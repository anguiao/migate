//! Concrete repaired-group command dispatch.
//!
//! Native on/off, brightness and color temperature stay on the group DID.
//! Repaired RGB and the member color mode travel to every effective member in
//! parallel, each through its own existing DID gate. The logical request is
//! accepted as soon as all required native writes and at least one full member
//! batch are accepted; remaining owned member sends continue independently.

use super::super::super::{
    model::{AdapterError, CommandTarget, PathKind},
    routing::ControlPath,
    state::{Context, DeviceRecord},
    work::{ConfirmedLight, DeviceWork, RepairMemberRef},
};
use super::super::{CommandLease, CommandOperation, ExpectedTicket, LightWriteGate, command_error};
use super::{accept, alive, feedback_bindings, reject, skip};
use crate::{
    device::{ActionToken, CommandError, DeviceRequest, LightColorTarget, LightMode, LightTarget},
    xiaomi::{
        capability::{PropertyBinding, RepairPlan},
        central::{CentralError, RequestStage, SendDecision, SendHooks},
    },
};
use event_listener::Event;
use serde_json::{Value, json};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

/// Per-native-property settlement: `Some(true)` accepted, `Some(false)`
/// definitely rejected, `None` unknown after a submitted receipt.
pub(super) type Settlements = Rc<RefCell<Vec<(PropertyBinding, Option<bool>)>>>;

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
    let request_current: Rc<dyn Fn() -> bool> = {
        let request = request.clone();
        Rc::new(move || request.borrow().as_ref().is_some_and(super::current))
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
    if !alive(&work, generation) || !request_current() {
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
    if !native.is_empty() {
        let expectations = register_member_echoes(&record, id, &members, &native);
        let settlements = Rc::new(RefCell::new(Vec::new()));
        let history = feedback_bindings(&record, id, true);
        let operation = context.commands.begin_operation(
            &did,
            id,
            native[0].0,
            CommandTarget::Light(target),
            native.clone(),
            queued_at,
        );
        let result = submit_batch(
            &context,
            &work,
            &did,
            id,
            native,
            &history,
            &gate,
            &mark,
            &request_current,
            Some(operation.0.clone()),
            settlements.clone(),
        )
        .await;
        if let Err(error) = result {
            // A submitted transport/protocol-unknown receipt cannot prove no
            // change happened: retain its exact bounded expectation for the
            // same 3s window. Only definitely rejected or not-sent native
            // properties retire their expectations.
            for (native_binding, gate, ticket) in &expectations {
                match settlements
                    .borrow()
                    .iter()
                    .find(|(binding, _)| binding == native_binding)
                {
                    Some((_, Some(false))) => gate.clear_expected(*ticket),
                    Some((_, _)) => gate.expect_ack(*ticket),
                    None => gate.clear_expected(*ticket),
                }
            }
            cancel_frame(&work, token, epoch);
            resolve(&take_request, Resolution::Reject(error));
            return Ok(());
        }
        for (_, gate, ticket) in &expectations {
            gate.expect_ack(*ticket);
        }
    }
    let results = Rc::new(MemberResults::default());
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
            let full = member_task(
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
            .await
            .is_ok();
            if full {
                results.full.set(true);
            }
            results.done.set(results.done.get() + 1);
            results.changed.notify(usize::MAX);
            Ok(())
        });
    }
    loop {
        if !request_current() {
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
        if results.full.get() || results.done.get() + missing >= total || !request_current() {
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

/// Mirror ordinary cloud routing/auth coordination for repaired writes.
fn report_cloud(
    context: &Context,
    token: &str,
    did: Option<&str>,
    error: &crate::xiaomi::cloud::CloudError,
) -> Result<(), CommandError> {
    context
        .cloud_error(token, did, error)
        .map_err(|error| CommandError::SendFailed(error.to_string()))
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
) -> Result<(), CommandError> {
    if member.record.life.is_closed() {
        return Err(CommandError::Cancelled);
    }
    let _permit = gate.acquire().await;
    if member.record.life.is_closed() || member.work.life.is_closed() {
        return Err(CommandError::Cancelled);
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
        return Err(CommandError::Superseded);
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
            return Err(CommandError::Superseded);
        }
    }
    // Ownership was proven at the gate; the accepted task now owns its batches
    // so an ordinary settle cannot cancel half of the required auxiliary work.
    let owned_continue: Rc<dyn Fn() -> bool> = Rc::new(|| true);
    let confirmed = member.work.confirmed_light().unwrap_or_default();
    let writes = encode_member(
        &member.record,
        target,
        logical_power,
        &confirmed,
        native_wrote,
    )?;
    let member_did = member.record.physical.key().id.clone();
    if !writes.primary.is_empty() {
        let history = writes
            .primary
            .iter()
            .map(|(binding, _)| *binding)
            .collect::<Vec<_>>();
        let operation = member_operation(context, &member_did, target, &writes.primary);
        submit_batch(
            context,
            &member.work,
            &member_did,
            "main",
            writes.primary,
            &history,
            gate,
            mark,
            &owned_continue,
            operation.as_ref().map(|lease| lease.0.clone()),
            Rc::new(RefCell::new(Vec::new())),
        )
        .await?;
    }
    if let Some((binding, value)) = writes.final_off {
        let properties = vec![(binding, value)];
        let operation = member_operation(context, &member_did, target, &properties);
        submit_batch(
            context,
            &member.work,
            &member_did,
            "main",
            properties,
            &[binding],
            gate,
            mark,
            &owned_continue,
            operation.as_ref().map(|lease| lease.0.clone()),
            Rc::new(RefCell::new(Vec::new())),
        )
        .await?;
    }
    Ok(())
}

/// One existing per-DID command record for a real member batch.
fn member_operation(
    context: &Rc<Context>,
    did: &str,
    target: LightTarget,
    properties: &[(PropertyBinding, Value)],
) -> Option<CommandLease> {
    let (binding, _) = properties.first()?;
    Some(context.commands.begin_operation(
        did,
        "main",
        *binding,
        CommandTarget::Light(target),
        properties.to_vec(),
        std::time::Instant::now(),
    ))
}

struct MemberWrites {
    primary: Vec<(PropertyBinding, Value)>,
    /// Sent as a separate accepted step after mode and color writes.
    final_off: Option<(PropertyBinding, Value)>,
}

fn encode_native(
    record: &DeviceRecord,
    id: &str,
    target: LightTarget,
    logical_range: crate::device::NumericRange,
) -> Result<Vec<(PropertyBinding, Value)>, CommandError> {
    let cached = record.cached.borrow();
    let function = cached
        .device
        .functions
        .iter()
        .find(|function| function.description.id == id)
        .ok_or(CommandError::Unsupported)?;
    let light = function
        .lighting
        .as_ref()
        .ok_or(CommandError::Unsupported)?;
    let snapshot = record.physical.function(id).unwrap().snapshot();
    let state = snapshot.lighting.unwrap();
    let convert = |error: crate::xiaomi::capability::CapabilityError| {
        CommandError::Rejected(error.to_string())
    };
    let mut values = Vec::new();
    if let Some(power) = target.power
        && snapshot.power.value() != Some(power)
    {
        values.push((function.power, json!(power)));
    }
    if let Some(percent) = target.brightness_percent {
        let brightness = light.brightness.as_ref().ok_or(CommandError::Unsupported)?;
        let raw = brightness.brightness_to_raw(percent).map_err(convert)?;
        let current = state
            .brightness_percent
            .and_then(|value| brightness.brightness_to_raw(value).ok());
        if current != Some(raw) {
            values.push((brightness.property, json!(raw)));
        }
    }
    if let Some(LightColorTarget::Temperature(kelvin)) = target.color {
        let temperature = light
            .temperature
            .as_ref()
            .ok_or(CommandError::Unsupported)?;
        let clamped = kelvin.clamp(logical_range.min, logical_range.max);
        let raw = temperature.temperature_to_raw(clamped).map_err(convert)?;
        let current = state
            .temperature_kelvin
            .and_then(|value| temperature.temperature_to_raw(value).ok());
        if current != Some(raw) {
            values.push((temperature.property, json!(raw)));
        }
    }
    Ok(values)
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
) -> Vec<(PropertyBinding, Rc<LightWriteGate>, ExpectedTicket)> {
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
) -> Vec<(PropertyBinding, Rc<LightWriteGate>, ExpectedTicket)> {
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

/// Submit one exact property batch through the device's current path, with the
/// shared gate handling receipts and a real readback for actual state. `mark`
/// records the logical request as actually submitted at the transport boundary.
#[allow(clippy::too_many_arguments)]
async fn submit_batch(
    context: &Rc<Context>,
    work: &Rc<DeviceWork>,
    did: &str,
    id: &str,
    properties: Vec<(PropertyBinding, Value)>,
    history_bindings: &[PropertyBinding],
    gate: &Rc<LightWriteGate>,
    mark: &Rc<dyn Fn() -> Result<(), CommandError>>,
    still_current: &Rc<dyn Fn() -> bool>,
    operation: Option<Rc<CommandOperation>>,
    settlements: Settlements,
) -> Result<(), CommandError> {
    if properties.is_empty() {
        return Ok(());
    }
    let generation = work.record.binding_generation.get();
    loop {
        if work.life.is_closed()
            || work.record.life.is_closed()
            || work.record.binding_generation.get() != generation
        {
            return Err(CommandError::Cancelled);
        }
        let Some(path) = context.control_path(did) else {
            return Err(CommandError::Unavailable("no usable control path".into()));
        };
        let authority = gate.authority();
        match path {
            ControlPath::Local(handle) => {
                let result_outer = Rc::new(RefCell::new(None::<Result<(), CommandError>>));
                let result_slot = result_outer.clone();
                let pending = Rc::new(Cell::new(true));
                let pending_result = pending.clone();
                let capture_gate = gate.clone();
                let capture_properties = properties.clone();
                let capture_operation = operation.clone();
                let result_settlements = settlements.clone();
                let guard_work = work.clone();
                let guard_context = context.clone();
                let guard_handle = handle.clone();
                let guard_did = did.to_string();
                let guard_mark = mark.clone();
                let guard_request = still_current.clone();
                let result_context = context.clone();
                let result_handle = handle.clone();
                let result_work = work.clone();
                let result_gate = gate.clone();
                let result_properties = properties.clone();
                let result_history = history_bindings.to_vec();
                let result_id = id.to_string();
                let result_operation = operation.clone();
                let hooks = SendHooks::new(
                    move || {
                        if guard_work.life.is_closed()
                            || guard_work.record.life.is_closed()
                            || guard_work.record.binding_generation.get() != generation
                            || !guard_context.current_local(&guard_handle)
                            || !guard_request()
                        {
                            SendDecision::Cancel
                        } else if matches!(
                            guard_context.control_path(&guard_did),
                            Some(ControlPath::Local(selected)) if selected.same_connection(&guard_handle)
                        ) {
                            SendDecision::Send
                        } else {
                            SendDecision::Withdraw
                        }
                    },
                    move || {
                        // Ownership is recorded at the actual transmission boundary.
                        let _ = guard_mark();
                        if let Some(operation) = &capture_operation {
                            operation.submitted(PathKind::Local);
                        }
                        capture_gate.register_feedback_for(authority, &capture_properties);
                    },
                );
                handle
                    .set_properties_guarded(did, &properties, hooks, move |outcome| {
                        if outcome.result == Err(CentralError::Withdrawn)
                            && outcome.stage == RequestStage::NotSent
                        {
                            return;
                        }
                        pending_result.set(false);
                        result_context.local_device_outcome(
                            &result_handle,
                            &result_work,
                            generation,
                            &outcome,
                        );
                        let result = match outcome.result {
                            Ok(results) => {
                                {
                                    let mut settled = result_settlements.borrow_mut();
                                    for (index, (binding, _)) in
                                        result_properties.iter().enumerate()
                                    {
                                        let accepted = if results.protocol_error {
                                            None
                                        } else {
                                            results.items.get(index).map(|item| item.is_ok())
                                        };
                                        settled.push((*binding, accepted));
                                    }
                                }
                                result_gate.finish_feedback_for(
                                    authority,
                                    &result_properties,
                                    &results,
                                    &result_history,
                                );
                                for (binding, _) in &result_properties {
                                    result_work.request_dirty_read(&result_id, *binding);
                                }
                                if results.protocol_error {
                                    Err(CommandError::SendFailed("invalid gateway response".into()))
                                } else if let Some(error) =
                                    results.items.into_iter().find_map(Result::err)
                                {
                                    Err(command_error(error))
                                } else {
                                    Ok(())
                                }
                            }
                            Err(error) => {
                                if outcome.stage != RequestStage::NotSent {
                                    let mut settled = result_settlements.borrow_mut();
                                    for (binding, _) in &result_properties {
                                        settled.push((*binding, None));
                                    }
                                }
                                result_gate.cancel_feedback();
                                Err(command_error(error))
                            }
                        };
                        if let Some(operation) = &result_operation {
                            operation.finish(result.clone());
                        }
                        *result_slot.borrow_mut() = Some(result);
                    })
                    .await;
                if let Some(result) = result_outer.borrow_mut().take() {
                    return result;
                }
                // Withdrawn before transmission: retry with a fresh path.
                continue;
            }
            ControlPath::Cloud(credentials) => {
                if work.life.is_closed() || work.record.life.is_closed() {
                    return Err(CommandError::Cancelled);
                }
                if !still_current() || !context.current_token(&credentials.access_token) {
                    return Err(CommandError::Cancelled);
                }
                // Re-resolve immediately before the await so a route change is
                // never replayed across paths after an unknown send.
                if !matches!(
                    context.control_path(did),
                    Some(ControlPath::Cloud(selected))
                        if selected.uid == credentials.uid
                            && selected.access_token == credentials.access_token
                ) {
                    continue;
                }
                if !still_current() {
                    return Err(CommandError::Cancelled);
                }
                mark()?;
                if let Some(operation) = &operation {
                    operation.submitted(PathKind::Cloud);
                }
                gate.register_feedback_for(authority, &properties);
                let result = context
                    .cloud
                    .set_properties(&credentials.access_token, did, &properties)
                    .await;
                let outcome = match &result {
                    Ok(results) => {
                        {
                            let mut settled = settlements.borrow_mut();
                            for (index, (binding, _)) in properties.iter().enumerate() {
                                let accepted = if results.protocol_error {
                                    None
                                } else {
                                    results.items.get(index).map(|item| item.is_ok())
                                };
                                settled.push((*binding, accepted));
                            }
                        }
                        gate.finish_feedback_for(authority, &properties, results, history_bindings);
                        for (binding, _) in &properties {
                            work.request_dirty_read(id, *binding);
                        }
                        let current = !work.life.is_closed() && !work.record.life.is_closed();
                        for error in results.items.iter().filter_map(|item| item.as_ref().err()) {
                            report_cloud(
                                context,
                                &credentials.access_token,
                                current.then_some(did),
                                error,
                            )?;
                        }
                        if results.protocol_error {
                            Err(CommandError::SendFailed("invalid cloud response".into()))
                        } else if let Some(error) =
                            results.items.iter().find_map(|item| item.as_ref().err())
                        {
                            Err(super::cloud_command_error(error))
                        } else {
                            Ok(())
                        }
                    }
                    Err(error) => {
                        {
                            let mut settled = settlements.borrow_mut();
                            for (binding, _) in &properties {
                                settled.push((*binding, None));
                            }
                        }
                        gate.cancel_feedback();
                        let current = !work.life.is_closed() && !work.record.life.is_closed();
                        report_cloud(
                            context,
                            &credentials.access_token,
                            current.then_some(did),
                            error,
                        )?;
                        Err(super::cloud_command_error(error))
                    }
                };
                if let Some(operation) = &operation {
                    operation.finish(outcome.clone());
                }
                return outcome;
            }
        }
    }
}
