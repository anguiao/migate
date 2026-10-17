//! RGB-only dispatch for admitted ls2307 native groups.
//!
//! Each currently ON member receives one real RGB property write. The first
//! acceptance settles the group request; other owned sends keep their place
//! in the shared per-device queue without retaining skipped color targets.

use super::super::super::{
    model::{AdapterError, CommandTarget},
    state::{Context, DeviceRecord},
    work::{DeviceWork, RepairMemberRef},
};
use super::{Batch, BatchAttempt, submit_properties};
use crate::{
    device::{ActionToken, CommandError, LightColorTarget, LightRequest, LightTarget, Rgb},
    xiaomi::capability::RepairPlan,
};
use event_listener::Event;
use serde_json::json;
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

#[derive(Default)]
struct MemberResults {
    accepted: Cell<bool>,
    done: Cell<usize>,
    changed: Event,
}

pub(super) async fn send(
    request: LightRequest,
    rgb: Rgb,
    context: Rc<Context>,
    record: Rc<DeviceRecord>,
    plan: Rc<RepairPlan>,
) -> Result<(), AdapterError> {
    let work = context
        .live
        .borrow()
        .devices
        .get(&record.physical.key().id)
        .and_then(|slot| slot.work.clone());
    let Some(work) = work else {
        request.reject(CommandError::Unavailable("no usable control path".into()));
        return Ok(());
    };
    if let Err(error) = request.current_result() {
        request.reject(error);
        return Ok(());
    }
    let token = request.token;
    let (epoch, members) = work
        .light_frames
        .borrow_mut()
        .next_light(token, || snapshot_members(&context, &plan));
    let pending = Rc::new(RefCell::new(Some(request)));
    let results = Rc::new(MemberResults::default());
    for member in members.iter() {
        let context = context.clone();
        let member = member.clone();
        let work = work.clone();
        let pending = pending.clone();
        let results = results.clone();
        context.clone().tasks.spawn(async move {
            let (result, maintenance) =
                member_task(&context, member, work, rgb, token, epoch, pending).await;
            if result.is_ok() {
                results.accepted.set(true);
            }
            results.done.set(results.done.get() + 1);
            results.changed.notify(usize::MAX);
            maintenance.map_or(Ok(()), Err)
        });
    }
    loop {
        let changed = results.changed.listen();
        let result = if results.accepted.get() {
            Ok(())
        } else if results.done.get() == members.len() {
            Err(CommandError::Rejected("no member accepted RGB".into()))
        } else {
            changed.await;
            continue;
        };
        let request = pending.borrow_mut().take().unwrap();
        match result {
            Ok(()) => {
                let _ = request.accept();
            }
            Err(error) => {
                work.light_frames.borrow_mut().cancel_light(token, epoch);
                request.reject(error);
            }
        }
        return Ok(());
    }
}

fn snapshot_members(context: &Context, plan: &RepairPlan) -> Vec<RepairMemberRef> {
    let live = context.live.borrow();
    plan.members
        .iter()
        .filter_map(|key| {
            let slot = live.devices.get(&key.id)?;
            Some(RepairMemberRef {
                record: slot.admitted.clone()?,
                work: slot.work.clone()?,
            })
        })
        .collect()
}

async fn member_task(
    context: &Rc<Context>,
    member: RepairMemberRef,
    group_work: Rc<DeviceWork>,
    rgb: Rgb,
    token: ActionToken,
    epoch: u64,
    pending: Rc<RefCell<Option<LightRequest>>>,
) -> (Result<(), CommandError>, Option<AdapterError>) {
    let gate = context
        .light_writes
        .for_device(&member.record.physical.key().id, context.clock.clone());
    let _permit = gate.acquire().await;
    let current: Rc<dyn Fn() -> Result<(), CommandError>> = {
        let member = member.clone();
        let pending = pending.clone();
        Rc::new(move || {
            if let Some(request) = pending.borrow().as_ref() {
                request.current_result()?;
            }
            if group_work.life.is_closed()
                || group_work.record.life.is_closed()
                || member.work.life.is_closed()
                || member.record.life.is_closed()
            {
                return Err(CommandError::Cancelled);
            }
            let frames = group_work.light_frames.borrow();
            if !frames.current_light(token, epoch)
                || !group_work
                    .record
                    .physical
                    .function("main")
                    .unwrap()
                    .light_action_owns_writes(token)
            {
                return Err(CommandError::Superseded);
            }
            if member
                .record
                .physical
                .function("main")
                .unwrap()
                .snapshot()
                .power
                .value()
                != Some(true)
            {
                return Err(CommandError::Rejected("member is not confirmed on".into()));
            }
            Ok(())
        })
    };
    if let Err(error) = current() {
        return (Err(error), None);
    }
    let property = {
        let cached = member.record.cached.borrow();
        cached
            .device
            .functions
            .iter()
            .find(|function| function.description.id == "main")
            .unwrap()
            .lighting
            .as_ref()
            .unwrap()
            .color
            .as_ref()
            .unwrap()
            .property
    };
    let properties = vec![(property, json!(rgb.packed()))];
    let operation = context.commands.begin_operation(
        &member.record.physical.key().id,
        "main",
        property,
        CommandTarget::Light(LightTarget {
            color: Some(LightColorTarget::Rgb(rgb)),
            ..LightTarget::default()
        }),
        properties.clone(),
        std::time::Instant::now(),
    );
    let mark: Rc<dyn Fn() -> Result<(), CommandError>> = Rc::new(move || {
        pending
            .borrow()
            .as_ref()
            .map_or(Ok(()), LightRequest::mark_sent)
    });
    let receipt = loop {
        match submit_properties(
            context,
            &member.work,
            Batch {
                function_id: "main",
                properties: properties.clone(),
                retain_feedback: false,
                gate: &gate,
                operation: &operation.0,
                mark: &mark,
                current: &current,
                frame_current: None,
            },
        )
        .await
        {
            BatchAttempt::Withdrawn => continue,
            BatchAttempt::Settled(receipt) => break receipt,
        }
    };
    refresh_mode(&member.work);
    operation.0.finish(receipt.result.clone());
    (receipt.result, receipt.maintenance)
}

/// Automatic mode changes still need real feedback when no notification arrives.
fn refresh_mode(work: &DeviceWork) {
    if work.life.is_closed() || work.record.life.is_closed() {
        return;
    }
    let mode = work
        .record
        .cached
        .borrow()
        .device
        .functions
        .iter()
        .find(|function| function.description.id == "main")
        .unwrap()
        .lighting
        .as_ref()
        .unwrap()
        .mode
        .unwrap()
        .property;
    work.request_dirty_read("main", mode);
}

pub(super) fn refresh_native_color_mode(context: &Context, did: &str) {
    if let Some(plan) = context.group_repair(did) {
        for member in snapshot_members(context, &plan) {
            refresh_mode(&member.work);
        }
    }
}
