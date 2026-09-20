use super::level::LEVEL_CLUSTER;
use super::power::ON_OFF_CLUSTER;
use super::{Adjustment, AdjustmentKind, LightingHandler, TimedOff, TimedOffPhase};
use crate::device::{CommandIntent, DeviceCommand, RgbColor};
use futures_lite::future;
use rs_matter::{
    dm::{
        HandlerContext,
        clusters::app::color_control::{RgbGamma, SetDeviceColor},
        clusters::decl::{color_control, level_control, on_off},
    },
    error::{Error, ErrorCode},
};
use std::time::{Duration, Instant};

impl LightingHandler {
    pub(in crate::matter) async fn run(&self, ctx: impl HandlerContext) -> Result<(), Error> {
        loop {
            let listener = self.wake.listen();
            let timed_deadline = self.timed_off.borrow().as_ref().map(|timer| timer.deadline);
            let adjustment_deadline = self
                .adjustment
                .borrow()
                .as_ref()
                .map(|_| Instant::now() + Duration::from_millis(100));
            let deadline = [
                timed_deadline,
                adjustment_deadline,
                self.pending_level_report.get(),
                self.next_remaining_report.get(),
            ]
            .into_iter()
            .flatten()
            .min();
            let Some(deadline) = deadline else {
                listener.await;
                continue;
            };
            let deadline_reached = future::or(
                async {
                    listener.await;
                    false
                },
                async {
                    async_io::Timer::at(deadline).await;
                    true
                },
            )
            .await;
            if !deadline_reached {
                continue;
            }
            let timer_due = self
                .timed_off
                .borrow()
                .as_ref()
                .is_some_and(|timer| timer.deadline <= Instant::now());
            if timer_due {
                let timer = self.timed_off.borrow().as_ref().unwrap().clone();
                let accepted = if timer.phase == TimedOffPhase::On && timer.intent.is_current() {
                    self.intent_command(&timer.intent, vec![DeviceCommand::SetPower(false)])
                        .await
                        .is_ok()
                } else {
                    timer.phase == TimedOffPhase::OffWait
                };
                if self
                    .timed_off
                    .borrow()
                    .as_ref()
                    .is_some_and(|current| current.operation == timer.operation)
                {
                    if accepted && timer.phase == TimedOffPhase::On && timer.off_wait_time > 0 {
                        *self.timed_off.borrow_mut() = Some(TimedOff {
                            deadline: Instant::now()
                                + Duration::from_millis(u64::from(timer.off_wait_time) * 100),
                            phase: TimedOffPhase::OffWait,
                            ..timer
                        });
                    } else {
                        self.timed_off.borrow_mut().take();
                    }
                }
                self.on_time.set(0);
                if self.timed_off.borrow().is_none() {
                    self.off_wait_time.set(0);
                }
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
            if self
                .pending_level_report
                .get()
                .is_some_and(|deadline| deadline <= Instant::now())
            {
                self.pending_level_report.set(None);
                self.last_level_report.set(Some(Instant::now()));
                ctx.notify_attr_changed(
                    self.endpoint,
                    LEVEL_CLUSTER.id,
                    level_control::AttributeId::CurrentLevel as _,
                );
            }
            if self
                .next_remaining_report
                .get()
                .is_some_and(|deadline| deadline <= Instant::now())
            {
                if self.adjustment.borrow().is_some() {
                    self.notify_remaining_time(&ctx);
                    self.next_remaining_report
                        .set(Some(Instant::now() + Duration::from_secs(1)));
                } else {
                    self.next_remaining_report.set(None);
                }
            }
            self.advance_adjustment(&ctx).await;
        }
    }

    pub(super) async fn advance_adjustment(&self, ctx: &impl HandlerContext) {
        let Some(mut adjustment) = self.adjustment.borrow().as_ref().cloned() else {
            return;
        };
        if !adjustment.intent.is_current() {
            self.clear_adjustment(adjustment.operation);
            self.notify_remaining_time(ctx);
            return;
        }
        let elapsed = Instant::now().saturating_duration_since(adjustment.started);
        let complete = elapsed >= adjustment.duration;
        let fraction = if complete || adjustment.duration.is_zero() {
            1.0
        } else {
            elapsed.as_secs_f64() / adjustment.duration.as_secs_f64()
        };
        let mut steps = Vec::with_capacity(adjustment.kinds.len());
        let mut commands = Vec::new();
        for kind in adjustment.kinds.iter().copied() {
            let (step, mut kind_commands) = match kind {
                AdjustmentKind::Brightness {
                    start,
                    target,
                    with_on_off,
                } => {
                    let value =
                        f64::from(start) + (f64::from(target) - f64::from(start)) * fraction;
                    let level = if complete && target == 0 {
                        0
                    } else {
                        value.round().clamp(1.0, 254.0) as u8
                    };
                    (
                        u32::from(level),
                        match self.commands_for_level(level, with_on_off) {
                            Ok(commands) => commands,
                            Err(_) => {
                                self.clear_adjustment(adjustment.operation);
                                self.notify_remaining_time(ctx);
                                return;
                            }
                        },
                    )
                }
                AdjustmentKind::ColorTemperature {
                    start_mireds,
                    target_mireds,
                } => {
                    let value = f64::from(start_mireds)
                        + (f64::from(target_mireds) - f64::from(start_mireds)) * fraction;
                    let mireds = value.round().clamp(1.0, f64::from(u16::MAX)) as u16;
                    let kelvin = match self.quantized_kelvin_for_mired(mireds) {
                        Ok(kelvin) => kelvin,
                        Err(_) => {
                            self.clear_adjustment(adjustment.operation);
                            self.notify_remaining_time(ctx);
                            return;
                        }
                    };
                    (
                        u32::from(mireds),
                        vec![DeviceCommand::SetColorTemperature(kelvin)],
                    )
                }
                AdjustmentKind::Xy {
                    start_x,
                    start_y,
                    target_x,
                    target_y,
                } => {
                    let interpolate = |start: u16, target: u16| {
                        (f64::from(start) + (f64::from(target) - f64::from(start)) * fraction)
                            .round()
                            .clamp(0.0, f64::from(0xfeff_u16)) as u16
                    };
                    let x = interpolate(start_x, target_x);
                    let y = interpolate(start_y, target_y);
                    let (red, green, blue) = SetDeviceColor::Xy { x, y }.to_rgb(RgbGamma::SRgb);
                    (
                        (u32::from(x) << 16) | u32::from(y),
                        vec![DeviceCommand::SetColor(RgbColor { red, green, blue })],
                    )
                }
                AdjustmentKind::XyMove {
                    start_x,
                    start_y,
                    rate_x,
                    rate_y,
                } => {
                    let elapsed = elapsed.as_secs_f64();
                    let advance = |start: u16, rate: i16| {
                        (f64::from(start) + f64::from(rate) * elapsed)
                            .round()
                            .clamp(0.0, f64::from(0xfeff_u16)) as u16
                    };
                    let x = advance(start_x, rate_x);
                    let y = advance(start_y, rate_y);
                    let (red, green, blue) = SetDeviceColor::Xy { x, y }.to_rgb(RgbGamma::SRgb);
                    (
                        (u32::from(x) << 16) | u32::from(y),
                        vec![DeviceCommand::SetColor(RgbColor { red, green, blue })],
                    )
                }
            };
            steps.push(step);
            commands.append(&mut kind_commands);
        }
        if adjustment.last_steps.as_ref() != Some(&steps) {
            if self
                .intent_command(&adjustment.intent, commands)
                .await
                .is_err()
            {
                self.clear_adjustment(adjustment.operation);
                self.notify_remaining_time(ctx);
                return;
            }
            adjustment.last_steps = Some(steps);
        }
        if complete {
            self.clear_adjustment(adjustment.operation);
            self.notify_remaining_time(ctx);
        } else if self
            .adjustment
            .borrow()
            .as_ref()
            .is_some_and(|current| current.operation == adjustment.operation)
        {
            *self.adjustment.borrow_mut() = Some(adjustment);
        }
    }

    pub(super) fn clear_adjustment(&self, operation: u64) {
        if self
            .adjustment
            .borrow()
            .as_ref()
            .is_some_and(|current| current.operation == operation)
        {
            self.adjustment.borrow_mut().take();
        }
    }

    pub(super) fn next_operation(&self) -> u64 {
        let operation = self.next_operation.get().wrapping_add(1);
        self.next_operation.set(operation);
        operation
    }

    pub(super) fn notify_remaining_time(&self, ctx: &impl HandlerContext) {
        if self.adjustment.borrow().is_none() {
            self.next_remaining_report.set(None);
        }
        ctx.notify_attr_changed(
            self.endpoint,
            LEVEL_CLUSTER.id,
            level_control::AttributeId::RemainingTime as _,
        );
        ctx.notify_attr_changed(
            self.endpoint,
            color_control::FULL_CLUSTER.id,
            color_control::AttributeId::RemainingTime as _,
        );
    }

    pub(in crate::matter) fn adjustment_active(&self) -> bool {
        self.adjustment.borrow().is_some()
    }

    pub(in crate::matter) fn adjustment_command_completed(
        &self,
        ctx: &impl HandlerContext,
        was_active: bool,
    ) {
        let active = self.adjustment_active();
        if was_active || active {
            self.next_remaining_report
                .set(active.then(|| Instant::now() + Duration::from_secs(1)));
            self.notify_remaining_time(ctx);
        }
    }

    pub(super) fn adjustment_remaining_ds(&self) -> u16 {
        self.adjustment.borrow().as_ref().map_or(0, |adjustment| {
            let elapsed = Instant::now().saturating_duration_since(adjustment.started);
            adjustment
                .duration
                .saturating_sub(elapsed)
                .as_millis()
                .div_ceil(100)
                .min(u128::from(u16::MAX)) as u16
        })
    }

    pub(super) fn start_adjustment(
        &self,
        intent: CommandIntent,
        duration_ds: u32,
        kind: AdjustmentKind,
    ) -> Result<(), Error> {
        if duration_ds == 0 {
            return Err(ErrorCode::ConstraintError.into());
        }
        *self.adjustment.borrow_mut() = Some(Adjustment {
            operation: self.next_operation(),
            intent,
            started: Instant::now(),
            duration: Duration::from_millis(u64::from(duration_ds) * 100),
            last_steps: None,
            kinds: vec![kind],
        });
        self.wake.notify(usize::MAX);
        Ok(())
    }
}
