use super::{LightingHandler, TimedOff, TimedOffPhase};
use crate::device::{DeviceCommand, Property, PropertyState, PropertyValue};
use rs_matter::{
    dm::{Cluster, InvokeContext, ReadContext, WriteContext, clusters::decl::on_off},
    error::{Error, ErrorCode},
    tlv::Nullable,
    with,
};
use std::time::{Duration, Instant};

pub(in crate::matter) const ON_OFF_CLUSTER: Cluster<'static> = on_off::FULL_CLUSTER
    .with_features(on_off::Feature::LIGHTING.bits())
    .with_attrs(with!(
        required;
        on_off::AttributeId::OnOff
            | on_off::AttributeId::GlobalSceneControl
            | on_off::AttributeId::OnTime
            | on_off::AttributeId::OffWaitTime
            | on_off::AttributeId::StartUpOnOff
    ))
    .with_cmds(with!(
        on_off::CommandId::Off
            | on_off::CommandId::On
            | on_off::CommandId::Toggle
            | on_off::CommandId::OffWithEffect
            | on_off::CommandId::OnWithRecallGlobalScene
            | on_off::CommandId::OnWithTimedOff
    ))
    .with_events(with!());

impl on_off::ClusterHandler for LightingHandler {
    const CLUSTER: Cluster<'static> = ON_OFF_CLUSTER;
    fn dataver(&self) -> u32 {
        self.on_off_dataver.get()
    }
    fn dataver_changed(&self) {
        self.on_off_dataver.changed();
    }
    fn on_off(&self, _ctx: impl ReadContext) -> Result<bool, Error> {
        self.power().ok_or(ErrorCode::Failure.into())
    }
    fn global_scene_control(&self, _ctx: impl ReadContext) -> Result<bool, Error> {
        Ok(self.global_scene.borrow().is_none())
    }
    fn on_time(&self, _ctx: impl ReadContext) -> Result<u16, Error> {
        Ok(self
            .timed_off
            .borrow()
            .as_ref()
            .filter(|timer| timer.phase == TimedOffPhase::On)
            .map(|timer| {
                let remaining = timer.deadline.saturating_duration_since(Instant::now());
                remaining
                    .as_millis()
                    .div_ceil(100)
                    .min(u128::from(u16::MAX)) as u16
            })
            .unwrap_or_else(|| self.on_time.get()))
    }
    fn off_wait_time(&self, _ctx: impl ReadContext) -> Result<u16, Error> {
        Ok(self.timed_off.borrow().as_ref().map_or_else(
            || self.off_wait_time.get(),
            |timer| {
                if timer.phase == TimedOffPhase::OffWait {
                    timer
                        .deadline
                        .saturating_duration_since(Instant::now())
                        .as_millis()
                        .div_ceil(100)
                        .min(u128::from(u16::MAX)) as u16
                } else {
                    timer.off_wait_time
                }
            },
        ))
    }
    fn start_up_on_off(
        &self,
        _ctx: impl ReadContext,
    ) -> Result<Nullable<on_off::StartUpOnOffEnum>, Error> {
        Err(ErrorCode::AttributeNotFound.into())
    }
    fn set_on_time(&self, ctx: impl WriteContext, value: u16) -> Result<(), Error> {
        self.on_time.set(value);
        if let Some(timer) = self.timed_off.borrow_mut().as_mut() {
            timer.deadline = Instant::now() + Duration::from_millis(u64::from(value) * 100);
        }
        ctx.notify_changed();
        self.wake.notify(usize::MAX);
        Ok(())
    }
    fn set_off_wait_time(&self, ctx: impl WriteContext, value: u16) -> Result<(), Error> {
        self.off_wait_time.set(value);
        if let Some(timer) = self.timed_off.borrow_mut().as_mut() {
            timer.off_wait_time = value;
        }
        ctx.notify_changed();
        Ok(())
    }
    fn set_start_up_on_off(
        &self,
        _ctx: impl WriteContext,
        _value: Nullable<on_off::StartUpOnOffEnum>,
    ) -> Result<(), Error> {
        Err(ErrorCode::UnsupportedAccess.into())
    }
    fn handle_off(&self, _ctx: impl InvokeContext) -> Result<(), Error> {
        Err(ErrorCode::Failure.into())
    }
    fn handle_on(&self, _ctx: impl InvokeContext) -> Result<(), Error> {
        Err(ErrorCode::Failure.into())
    }
    fn handle_toggle(&self, _ctx: impl InvokeContext) -> Result<(), Error> {
        Err(ErrorCode::Failure.into())
    }
    fn handle_off_with_effect(
        &self,
        _ctx: impl InvokeContext,
        _request: on_off::OffWithEffectRequest<'_>,
    ) -> Result<(), Error> {
        Err(ErrorCode::Failure.into())
    }
    fn handle_on_with_recall_global_scene(&self, _ctx: impl InvokeContext) -> Result<(), Error> {
        Err(ErrorCode::Failure.into())
    }
    fn handle_on_with_timed_off(
        &self,
        _ctx: impl InvokeContext,
        _request: on_off::OnWithTimedOffRequest<'_>,
    ) -> Result<(), Error> {
        Err(ErrorCode::Failure.into())
    }
}

impl LightingHandler {
    pub(super) fn power(&self) -> Option<bool> {
        match self
            .service
            .snapshot(&self.feature)?
            .property(Property::Power)?
        {
            PropertyState::Current {
                value: PropertyValue::Power(value),
                ..
            } => Some(*value),
            _ => None,
        }
    }

    pub(super) fn on_commands(&self) -> Result<Vec<DeviceCommand>, Error> {
        let mut commands = Vec::new();
        if let Some(level) = self.on_level.borrow().clone().into_option() {
            commands.extend(self.commands_for_level(level, false)?);
        }
        commands.push(DeviceCommand::SetPower(true));
        Ok(commands)
    }

    pub(in crate::matter) async fn invoke_on_off(
        &self,
        ctx: impl InvokeContext,
    ) -> Result<(), Error> {
        ctx.data().structure()?;
        let command = on_off::CommandId::try_from(ctx.cmd().cmd_id)?;
        if command != on_off::CommandId::OnWithTimedOff
            && self.timed_off.borrow_mut().take().is_some()
        {
            self.wake.notify(usize::MAX);
            ctx.notify_attr_changed(
                self.endpoint,
                ON_OFF_CLUSTER.id,
                on_off::AttributeId::OnTime as _,
            );
            ctx.notify_attr_changed(
                self.endpoint,
                ON_OFF_CLUSTER.id,
                on_off::AttributeId::OffWaitTime as _,
            );
        }
        match command {
            on_off::CommandId::Off => {
                self.global_scene.borrow_mut().take();
                self.command(DeviceCommand::SetPower(false)).await
            }
            on_off::CommandId::On => {
                self.global_scene.borrow_mut().take();
                self.command_batch(self.on_commands()?).await
            }
            on_off::CommandId::OnWithRecallGlobalScene => {
                let commands = self
                    .global_scene
                    .borrow_mut()
                    .take()
                    .unwrap_or(self.on_commands()?);
                self.command_batch(commands).await
            }
            on_off::CommandId::Toggle => {
                self.command(DeviceCommand::SetPower(
                    !self.power().ok_or(ErrorCode::Failure)?,
                ))
                .await
            }
            on_off::CommandId::OffWithEffect => {
                let request = on_off::OffWithEffectRequest::new(ctx.data().clone());
                let identifier = request.effect_identifier()?;
                let variant = request.effect_variant()?;
                match identifier {
                    on_off::EffectIdentifierEnum::DelayedAllOff if variant <= 2 => {}
                    on_off::EffectIdentifierEnum::DyingLight if variant == 0 => {}
                    _ => return Err(ErrorCode::ConstraintError.into()),
                }
                *self.global_scene.borrow_mut() = Some(self.capture_global_scene()?);
                self.command(DeviceCommand::SetPower(false)).await
            }
            on_off::CommandId::OnWithTimedOff => {
                let request = on_off::OnWithTimedOffRequest::new(ctx.data().clone());
                let control = request.on_off_control()?;
                let on_time = request.on_time()?;
                let off_wait_time = request.off_wait_time()?;
                if control.contains(on_off::OnOffControlBitmap::ACCEPT_ONLY_WHEN_ON)
                    && self.power() != Some(true)
                {
                    return Ok(());
                }
                let current_timer = self.timed_off.borrow().as_ref().cloned();
                if let Some(current) = current_timer
                    && current.phase == TimedOffPhase::OffWait
                    && self.power() == Some(false)
                {
                    let remaining = current.deadline.saturating_duration_since(Instant::now());
                    let requested = Duration::from_millis(u64::from(off_wait_time) * 100);
                    let deadline = Instant::now() + remaining.min(requested);
                    let operation = current.operation;
                    let intent = current.intent.clone();
                    *self.timed_off.borrow_mut() = Some(TimedOff {
                        operation,
                        intent,
                        deadline,
                        off_wait_time,
                        phase: TimedOffPhase::OffWait,
                    });
                    self.off_wait_time.set(off_wait_time);
                    self.wake.notify(usize::MAX);
                    return Ok(());
                }
                let intent = self.service.begin_command_intent(
                    &self.feature,
                    [
                        Some(Property::Power),
                        self.on_level
                            .borrow()
                            .clone()
                            .into_option()
                            .map(|_| Property::Brightness),
                    ]
                    .into_iter()
                    .flatten(),
                );
                self.intent_command(&intent, self.on_commands()?).await?;
                let requested_deadline =
                    Instant::now() + Duration::from_millis(u64::from(on_time) * 100);
                let deadline = self
                    .timed_off
                    .borrow()
                    .as_ref()
                    .map_or(requested_deadline, |current| {
                        current.deadline.max(requested_deadline)
                    });
                let off_wait_time = self
                    .timed_off
                    .borrow()
                    .as_ref()
                    .map_or(off_wait_time, |current| {
                        current.off_wait_time.max(off_wait_time)
                    });
                *self.timed_off.borrow_mut() = (on_time > 0).then(|| TimedOff {
                    operation: self.next_operation(),
                    intent,
                    deadline,
                    off_wait_time,
                    phase: TimedOffPhase::On,
                });
                self.on_time.set(on_time);
                self.off_wait_time.set(off_wait_time);
                self.wake.notify(usize::MAX);
                Ok(())
            }
        }
    }
}
