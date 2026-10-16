//! Concrete repaired-group command dispatch.
//!
//! Native on/off, brightness and color temperature stay on the group DID.
//! Repaired RGB and the member color mode travel to every effective member in
//! parallel, each through its own existing DID gate. The logical request is
//! accepted as soon as all required native writes and at least one full member
//! batch are accepted; remaining owned member sends continue independently.

use super::super::super::{
    model::{AdapterError, CommandTarget},
    state::{Context, DeviceRecord},
    work::{ConfirmedLight, DeviceWork, RepairMemberRef},
};
use super::super::LightWriteGate;
use super::{
    Batch, BatchAttempt, BatchReceipt, accept, alive, feedback_bindings, reject, skip,
    submit_properties,
};
use crate::{
    device::{ActionToken, CommandError, DeviceRequest, LightColorTarget, LightMode, LightTarget},
    xiaomi::capability::{PropertyBinding, RepairPlan},
};
use event_listener::Event;
use serde_json::{Value, json};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

#[derive(Default)]
struct MemberResults {
    full: Cell<bool>,
    done: Cell<usize>,
    changed: Event,
}

/// Dispatch one frame of a committed repaired group.
pub(super) async fn send(
    request: DeviceRequest,
    context: Rc<Context>,
    record: Rc<DeviceRecord>,
    id: &str,
    gate: Rc<LightWriteGate>,
    plan: Rc<RepairPlan>,
) -> Result<(), AdapterError> {
    let did = record.physical.key().id.clone();
    let target = match &request {
        DeviceRequest::Power(request) => LightTarget {
            power: Some(request.target),
            ..LightTarget::default()
        },
        DeviceRequest::Light(request) => request.target,
    };
    let final_frame = matches!(&request, DeviceRequest::Light(request) if request.final_frame);
    let queued_at = request.queued_at();
    let Some(work) = context
        .live
        .borrow()
        .devices
        .get(&did)
        .and_then(|slot| slot.work.clone())
    else {
        reject(
            request,
            CommandError::Unavailable("no usable control path".into()),
        );
        return Ok(());
    };
    let generation = record.binding_generation.get();
    let request = Rc::new(RefCell::new(Some(request)));
    let request_current: Rc<dyn Fn() -> Result<(), CommandError>> = {
        let request = request.clone();
        Rc::new(move || {
            request
                .borrow()
                .as_ref()
                .map_or(Err(CommandError::Cancelled), super::current_result)
        })
    };
    let take_request = {
        let request = request.clone();
        move || request.borrow_mut().take()
    };
    let submitted = Rc::new(Cell::new(false));
    let mark: Rc<dyn Fn() -> Result<(), CommandError>> = {
        let request = request.clone();
        let submitted = submitted.clone();
        Rc::new(move || {
            let borrow = request.borrow();
            let Some(request) = borrow.as_ref() else {
                return Ok(());
            };
            let result = super::mark_sent(request);
            if result.is_ok() {
                submitted.set(true);
            }
            result
        })
    };
    if !alive(&work, generation) || request_current().is_err() {
        resolve(&take_request, Resolution::Reject(CommandError::Cancelled));
        return Ok(());
    }
    let (token, epoch, members, missing) = match request.borrow().as_ref().unwrap() {
        DeviceRequest::Light(request) => {
            let token = request.token;
            let snapshot = || snapshot_members(&context, &plan);
            let (epoch, members, missing) =
                work.light_frames.borrow_mut().next_light(token, snapshot);
            (Some(token), epoch, members, missing)
        }
        DeviceRequest::Power(_) => {
            let snapshot = || snapshot_members(&context, &plan);
            let (epoch, members, missing) = work.light_frames.borrow_mut().next_power(snapshot);
            (None, epoch, members, missing)
        }
    };
    let Some(logical_range) = record
        .physical
        .function(id)
        .and_then(|function| function.description().temperature_range())
    else {
        resolve(&take_request, Resolution::Reject(CommandError::Unsupported));
        return Ok(());
    };
    let native = match encode_native(&record, id, target, logical_range) {
        Ok(native) => native,
        Err(error) => {
            cancel_frame(&work, token, epoch);
            resolve(&take_request, Resolution::Reject(error));
            return Ok(());
        }
    };
    let native_wrote = !native.is_empty();
    let logical_power = target.power.or_else(|| {
        record
            .physical
            .function(id)
            .and_then(|function| function.snapshot().power.value())
    });
    let results = Rc::new(MemberResults::default());
    if !native.is_empty() {
        let history = feedback_bindings(&record, id, true);
        let operation = context.commands.begin_operation(
            &did,
            id,
            native[0].0,
            CommandTarget::Light(target),
            native.clone(),
            queued_at,
        );
        let receipt = loop {
            let echoes = register_member_echoes(&record, id, &members, &native);
            match submit_properties(
                &context,
                &work,
                Batch {
                    function_id: id,
                    properties: native.clone(),
                    history_bindings: &history,
                    echoes,
                    gate: &gate,
                    operation: &operation.0,
                    mark: &mark,
                    current: &request_current,
                    frame_current: None,
                    optimistic: false,
                },
            )
            .await
            {
                BatchAttempt::Withdrawn => continue,
                BatchAttempt::Settled(receipt) => break receipt,
            }
        };
        let result = receipt.result.clone();
        operation.0.finish(result.clone());
        if let Err(error) = result {
            cancel_frame(&work, token, epoch);
            resolve(&take_request, Resolution::Reject(error));
            // A maintenance report only accompanies a failed cloud report,
            // whose actual receipt was just settled.
            return receipt.maintenance.map_or(Ok(()), Err);
        }
    }
    let total = members.len() + missing;
    for member in members.iter() {
        let task_context = context.clone();
        let member = member.clone();
        let member_gate = task_context
            .light_writes
            .for_device(&member.record.physical.key().id, task_context.clock.clone());
        let results = results.clone();
        let group_work = work.clone();
        let mark = mark.clone();
        context.tasks.spawn(async move {
            let (result, maintenance) = member_task(
                &task_context,
                &member,
                &member_gate,
                &group_work,
                target,
                logical_power,
                native_wrote,
                token,
                epoch,
                &mark,
            )
            .await;
            if result.is_ok() {
                results.full.set(true);
            }
            results.done.set(results.done.get() + 1);
            results.changed.notify(usize::MAX);
            maintenance.map_or(Ok(()), Err)
        });
    }
    loop {
        if request_current().is_err() {
            cancel_frame(&work, token, epoch);
            resolve(&take_request, Resolution::Reject(CommandError::Cancelled));
            return Ok(());
        }
        if results.full.get() {
            if let Some(token) = token {
                work.light_frames
                    .borrow_mut()
                    .settle_light(token, epoch, final_frame);
            }
            resolve(
                &take_request,
                if submitted.get() {
                    Resolution::Accept
                } else {
                    Resolution::Skip
                },
            );
            return Ok(());
        }
        if results.done.get() + missing >= total {
            cancel_frame(&work, token, epoch);
            resolve(
                &take_request,
                Resolution::Reject(CommandError::Rejected(
                    "all effective members failed".into(),
                )),
            );
            return Ok(());
        }
        let listener = results.changed.listen();
        if results.full.get() || results.done.get() + missing >= total || request_current().is_err()
        {
            continue;
        }
        listener.await;
    }
}

enum Resolution {
    Accept,
    Skip,
    Reject(CommandError),
}

fn resolve(take: &impl Fn() -> Option<DeviceRequest>, resolution: Resolution) {
    let Some(request) = take() else {
        return;
    };
    match resolution {
        Resolution::Accept => {
            let _ = accept(request);
        }
        Resolution::Skip => {
            let _ = skip(request);
        }
        Resolution::Reject(error) => reject(request, error),
    }
}

fn cancel_frame(work: &Rc<DeviceWork>, token: Option<ActionToken>, epoch: u64) {
    match token {
        Some(token) => work.light_frames.borrow_mut().cancel_light(token, epoch),
        None => work.light_frames.borrow_mut().cancel_power(epoch),
    }
}

fn snapshot_members(context: &Context, plan: &RepairPlan) -> (Vec<RepairMemberRef>, usize) {
    let live = context.live.borrow();
    let mut members = Vec::new();
    let mut missing = 0;
    for key in &plan.members {
        match live.devices.get(&key.id) {
            Some(slot) if slot.admitted.is_some() && slot.work.is_some() => {
                members.push(RepairMemberRef {
                    record: slot.admitted.clone().unwrap(),
                    work: slot.work.clone().unwrap(),
                });
            }
            _ => missing += 1,
        }
    }
    (members, missing)
}

#[allow(clippy::too_many_arguments)]
async fn member_task(
    context: &Rc<Context>,
    member: &RepairMemberRef,
    gate: &Rc<LightWriteGate>,
    group_work: &Rc<DeviceWork>,
    target: LightTarget,
    logical_power: Option<bool>,
    native_wrote: bool,
    token: Option<ActionToken>,
    epoch: u64,
    mark: &Rc<dyn Fn() -> Result<(), CommandError>>,
) -> (Result<(), CommandError>, Option<AdapterError>) {
    if member.record.life.is_closed() {
        return (Err(CommandError::Cancelled), None);
    }
    let _permit = gate.acquire().await;
    if member.record.life.is_closed() || member.work.life.is_closed() {
        return (Err(CommandError::Cancelled), None);
    }
    // An unsent frame superseded by a newer frame, Stop or takeover retires
    // here. A successful final acceptance keeps its owned member sends.
    let (owned, accepted_final) = match token {
        Some(token) => {
            let frames = group_work.light_frames.borrow();
            (
                frames.current_light(token, epoch),
                frames.accepted_final_light(token, epoch),
            )
        }
        None => (group_work.light_frames.borrow().current_power(epoch), false),
    };
    if !owned {
        return (Err(CommandError::Superseded), None);
    }
    if let Some(token) = token
        && !accepted_final
    {
        let current = group_work
            .record
            .physical
            .function("main")
            .is_some_and(|function| function.light_action_current(token));
        if !current {
            return (Err(CommandError::Superseded), None);
        }
    }
    let confirmed = member.work.confirmed_light().unwrap_or_default();
    let writes = match encode_member(
        &member.record,
        target,
        logical_power,
        &confirmed,
        native_wrote,
    ) {
        Ok(writes) => writes,
        Err(error) => return (Err(error), None),
    };
    let mut maintenance = None;
    if !writes.primary.is_empty() {
        let history = writes
            .primary
            .iter()
            .map(|(binding, _)| *binding)
            .collect::<Vec<_>>();
        let receipt = send_member_batch(
            context,
            &member.work,
            &writes.primary,
            &history,
            target,
            gate,
            mark,
        )
        .await;
        maintenance = receipt.maintenance;
        if let Err(error) = receipt.result {
            return (Err(error), maintenance);
        }
    }
    if let Some((binding, value)) = writes.final_off {
        let receipt = send_member_batch(
            context,
            &member.work,
            &[(binding, value)],
            &[binding],
            target,
            gate,
            mark,
        )
        .await;
        if maintenance.is_none() {
            maintenance = receipt.maintenance;
        }
        if let Err(error) = receipt.result {
            return (Err(error), maintenance);
        }
    }
    (Ok(()), maintenance)
}

/// Submit one member batch through a fresh per-DID command record.
async fn send_member_batch(
    context: &Rc<Context>,
    work: &Rc<DeviceWork>,
    properties: &[(PropertyBinding, Value)],
    history: &[PropertyBinding],
    target: LightTarget,
    gate: &Rc<LightWriteGate>,
    mark: &Rc<dyn Fn() -> Result<(), CommandError>>,
) -> BatchReceipt {
    // Ownership was proven at the gate; the accepted task now owns its batches
    // so an ordinary settle cannot cancel half of the required auxiliary work.
    let current: Rc<dyn Fn() -> Result<(), CommandError>> = Rc::new(|| Ok(()));
    let did = work.record.physical.key().id.clone();
    let operation = context.commands.begin_operation(
        &did,
        "main",
        properties
            .first()
            .map_or(work.record.binding("main"), |(binding, _)| *binding),
        CommandTarget::Light(target),
        properties.to_vec(),
        std::time::Instant::now(),
    );
    let receipt = loop {
        match submit_properties(
            context,
            work,
            Batch {
                function_id: "main",
                properties: properties.to_vec(),
                history_bindings: history,
                echoes: Vec::new(),
                gate,
                operation: &operation.0,
                mark,
                current: &current,
                frame_current: None,
                optimistic: false,
            },
        )
        .await
        {
            BatchAttempt::Withdrawn => continue,
            BatchAttempt::Settled(receipt) => break receipt,
        }
    };
    operation.0.finish(receipt.result.clone());
    receipt
}

struct MemberWrites {
    primary: Vec<(PropertyBinding, Value)>,
    /// Sent as a separate accepted step after mode and color writes.
    final_off: Option<(PropertyBinding, Value)>,
}

fn encode_native(
    record: &DeviceRecord,
    id: &str,
    mut target: LightTarget,
    logical_range: crate::device::NumericRange,
) -> Result<Vec<(PropertyBinding, Value)>, CommandError> {
    target.color = match target.color {
        Some(LightColorTarget::Temperature(kelvin)) => Some(LightColorTarget::Temperature(
            kelvin.clamp(logical_range.min, logical_range.max),
        )),
        Some(LightColorTarget::Rgb(_)) | None => None,
    };
    // A repair plan has no native RGB or mode binding. Its native fields use
    // the same encoding and unchanged-value suppression as ordinary lights.
    super::super::super::lighting::encode(record, id, target)
}

fn encode_member(
    record: &DeviceRecord,
    target: LightTarget,
    logical_power: Option<bool>,
    confirmed: &ConfirmedLight,
    native_wrote: bool,
) -> Result<MemberWrites, CommandError> {
    let cached = record.cached.borrow();
    let function = cached
        .device
        .functions
        .iter()
        .find(|function| function.description.id == "main")
        .ok_or(CommandError::Unsupported)?;
    let light = function
        .lighting
        .as_ref()
        .ok_or(CommandError::Unsupported)?;
    let mut values = Vec::new();
    let mut content = false;
    // A native power or brightness write in the same frame can reset the
    // member mode, so a requested mode/color is rewritten even when the
    // confirmed observation already matches.
    let force = native_wrote && target.color.is_some();
    if let Some(LightColorTarget::Rgb(rgb)) = target.color {
        let color = light.color.as_ref().ok_or(CommandError::Unsupported)?;
        if force || confirmed.color != Some(rgb) {
            values.push((color.property, json!(rgb.packed())));
            content = true;
        }
    }
    let mode = match target.color {
        Some(LightColorTarget::Rgb(_)) => Some(LightMode::Rgb),
        Some(LightColorTarget::Temperature(_)) => Some(LightMode::Temperature),
        None => confirmed.mode,
    };
    if let Some(mode) = mode {
        let binding = light.mode.ok_or(CommandError::Unsupported)?;
        let requested = match mode {
            LightMode::Temperature => binding.temperature_value,
            LightMode::Rgb => binding.rgb_value,
        };
        // A native on/off or brightness write may reset the member mode, so
        // known modes are restored even when they currently match. Unknown
        // modes stay unknown.
        let restore = target.color.is_none() || force;
        if restore || confirmed.mode != Some(mode) {
            values.push((binding.property, json!(requested)));
            content = true;
        }
    }
    let mut final_off = None;
    if content && let Some(power) = logical_power {
        if power {
            if confirmed.power_value() != Some(true) {
                values.insert(0, (function.power, json!(true)));
            }
        } else {
            // Final off follows mode and color writes as its own accepted
            // step, even when the member was previously off: the writes
            // themselves can power the member on.
            final_off = Some((function.power, json!(false)));
        }
    }
    Ok(MemberWrites {
        primary: values,
        final_off,
    })
}

#[allow(clippy::too_many_arguments)]
fn register_member_echoes(
    record: &DeviceRecord,
    id: &str,
    members: &[RepairMemberRef],
    native: &[(PropertyBinding, Value)],
) -> super::EchoTickets {
    let mut tickets = Vec::new();
    let cached = record.cached.borrow();
    let Some(group) = cached
        .device
        .functions
        .iter()
        .find(|function| function.description.id == id)
    else {
        return tickets;
    };
    let Some(group_light) = group.lighting.as_ref() else {
        return tickets;
    };
    for (binding, value) in native {
        for member in members {
            let Some(gate) = &member.work.light_write else {
                continue;
            };
            let cached = member.record.cached.borrow();
            let Some(function) = cached
                .device
                .functions
                .iter()
                .find(|function| function.description.id == "main")
            else {
                continue;
            };
            let Some(light) = function.lighting.as_ref() else {
                continue;
            };
            let mut expected = Vec::new();
            let mut native_switches_mode = false;
            if *binding == group.power {
                native_switches_mode = true;
                if let Some(value) = value.as_bool() {
                    expected.push((function.power, json!(value)));
                }
            } else if group_light
                .brightness
                .as_ref()
                .is_some_and(|brightness| brightness.property == *binding)
            {
                native_switches_mode = true;
                if let Some(member_brightness) = &light.brightness
                    && let Some(group_brightness) = &group_light.brightness
                    && let Ok(percent) = group_brightness.brightness_from_raw(value)
                    && let Ok(raw) = member_brightness.brightness_to_raw(percent)
                {
                    expected.push((member_brightness.property, json!(raw)));
                }
            } else if group_light
                .temperature
                .as_ref()
                .is_some_and(|temperature| temperature.property == *binding)
            {
                native_switches_mode = true;
                if let Some(member_temperature) = &light.temperature
                    && let Some(kelvin) = value.as_u64()
                    && let Ok(raw) = member_temperature.temperature_to_raw(kelvin as u32)
                {
                    expected.push((member_temperature.property, json!(raw)));
                }
            }
            // Only an actual native power/brightness/CT write may switch a
            // concrete ls2307 member to white; a native RGB/mode binding is
            // never treated as a white reset. Members without a genuine mode
            // binding get no guess.
            if native_switches_mode && let Some(mode) = &light.mode {
                expected.push((mode.property, json!(mode.temperature_value)));
            }
            if !expected.is_empty() {
                let ticket = gate.expect_pending(&expected);
                tickets.push((*binding, gate.clone(), ticket));
            }
        }
    }
    tickets
}

/// Register native-to-member expectations for an ordinary native group whose
/// installed members share the member DID; exact matcher and conversions only.
pub(super) fn register_group_echoes(
    context: &Context,
    record: &DeviceRecord,
    id: &str,
    native: &[(PropertyBinding, Value)],
) -> super::EchoTickets {
    let did = record.physical.key().id.clone();
    let members = {
        let live = context.live.borrow();
        let Some(slot) = live.devices.get(&did) else {
            return Vec::new();
        };
        let Some(group) = &slot.effective_group else {
            return Vec::new();
        };
        group.members.clone()
    };
    let mut refs = Vec::new();
    {
        let live = context.live.borrow();
        for member in members {
            let Some(slot) = live.devices.get(&member) else {
                continue;
            };
            let (Some(record), Some(work)) = (slot.admitted.clone(), slot.work.clone()) else {
                continue;
            };
            refs.push(RepairMemberRef { record, work });
        }
    }
    register_member_echoes(record, id, &refs, native)
}
