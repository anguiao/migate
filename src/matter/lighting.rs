use crate::device::{
    Capability, CommandIntent, CommandOutcome, DeviceCommand, DeviceService, FeatureIdentity,
    NumericRange, Percent, Property, PropertyState, PropertyValue, RgbColor,
};
use event_listener::Event;
use futures_lite::future;
use rs_matter::{
    dm::{
        Cluster, Dataver, HandlerContext, InvokeContext, ReadContext, WriteContext,
        clusters::app::color_control::{RgbGamma, SetDeviceColor},
        clusters::decl::{color_control, groups, level_control, on_off, scenes_management},
        clusters::scenes::{
            AttributeValuePairStruct, AttributeValuePairStructArrayBuilder, SceneClusterHandler,
        },
    },
    error::{Error, ErrorCode},
    tlv::{Nullable, TLVArray, TLVBuilderParent},
    with,
};
use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    num::NonZeroU8,
    time::{Duration, Instant},
};

pub(super) const GROUPS_CLUSTER: Cluster<'static> = groups::FULL_CLUSTER
    .with_features(groups::Feature::GROUP_NAMES.bits())
    .with_attrs(with!(required))
    .with_cmds(with!(all))
    .with_events(with!());

pub(super) const SCENES_CLUSTER: Cluster<'static> = scenes_management::FULL_CLUSTER
    .with_features(scenes_management::Feature::empty().bits())
    .with_attrs(with!(required))
    .with_cmds(with!(all))
    .with_events(with!());

pub(super) const ON_OFF_CLUSTER: Cluster<'static> = on_off::FULL_CLUSTER
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

pub(super) const LEVEL_CLUSTER: Cluster<'static> = level_control::FULL_CLUSTER
    .with_features(level_control::Feature::ON_OFF.bits() | level_control::Feature::LIGHTING.bits())
    .with_attrs(with!(
        required;
        level_control::AttributeId::CurrentLevel
            | level_control::AttributeId::RemainingTime
            | level_control::AttributeId::MinLevel
            | level_control::AttributeId::MaxLevel
            | level_control::AttributeId::Options
            | level_control::AttributeId::OnLevel
            | level_control::AttributeId::StartUpCurrentLevel
    ))
    .with_cmds(with!(
        level_control::CommandId::MoveToLevel
            | level_control::CommandId::Move
            | level_control::CommandId::Step
            | level_control::CommandId::Stop
            | level_control::CommandId::MoveToLevelWithOnOff
            | level_control::CommandId::MoveWithOnOff
            | level_control::CommandId::StepWithOnOff
            | level_control::CommandId::StopWithOnOff
    ))
    .with_events(with!());

pub(super) const fn color_cluster(xy: bool, temperature: bool) -> Cluster<'static> {
    let mut features = color_control::Feature::empty();
    if xy {
        features = features.union(color_control::Feature::XY);
    }
    if temperature {
        features = features.union(color_control::Feature::COLOR_TEMPERATURE);
    }
    let cluster = color_control::FULL_CLUSTER.with_features(features.bits());
    let cluster = if xy && temperature {
        cluster.with_attrs(with!(
            required;
            color_control::AttributeId::CurrentX
                | color_control::AttributeId::CurrentY
                | color_control::AttributeId::RemainingTime
                | color_control::AttributeId::ColorTemperatureMireds
                | color_control::AttributeId::ColorMode
                | color_control::AttributeId::Options
                | color_control::AttributeId::EnhancedColorMode
                | color_control::AttributeId::ColorCapabilities
                | color_control::AttributeId::ColorTempPhysicalMinMireds
                | color_control::AttributeId::ColorTempPhysicalMaxMireds
                | color_control::AttributeId::CoupleColorTempToLevelMinMireds
                | color_control::AttributeId::StartUpColorTemperatureMireds
        ))
    } else if xy {
        cluster.with_attrs(with!(
            required;
            color_control::AttributeId::CurrentX
                | color_control::AttributeId::CurrentY
                | color_control::AttributeId::RemainingTime
                | color_control::AttributeId::ColorMode
                | color_control::AttributeId::Options
                | color_control::AttributeId::EnhancedColorMode
                | color_control::AttributeId::ColorCapabilities
        ))
    } else {
        cluster.with_attrs(with!(
            required;
            color_control::AttributeId::RemainingTime
                | color_control::AttributeId::ColorTemperatureMireds
                | color_control::AttributeId::ColorMode
                | color_control::AttributeId::Options
                | color_control::AttributeId::EnhancedColorMode
                | color_control::AttributeId::ColorCapabilities
                | color_control::AttributeId::ColorTempPhysicalMinMireds
                | color_control::AttributeId::ColorTempPhysicalMaxMireds
                | color_control::AttributeId::CoupleColorTempToLevelMinMireds
                | color_control::AttributeId::StartUpColorTemperatureMireds
        ))
    };
    let cluster = if xy && temperature {
        cluster.with_cmds(with!(
            color_control::CommandId::MoveToColor
                | color_control::CommandId::MoveColor
                | color_control::CommandId::StepColor
                | color_control::CommandId::MoveToColorTemperature
                | color_control::CommandId::StopMoveStep
                | color_control::CommandId::MoveColorTemperature
                | color_control::CommandId::StepColorTemperature
        ))
    } else if xy {
        cluster.with_cmds(with!(
            color_control::CommandId::MoveToColor
                | color_control::CommandId::MoveColor
                | color_control::CommandId::StepColor
                | color_control::CommandId::StopMoveStep
        ))
    } else {
        cluster.with_cmds(with!(
            color_control::CommandId::MoveToColorTemperature
                | color_control::CommandId::StopMoveStep
                | color_control::CommandId::MoveColorTemperature
                | color_control::CommandId::StepColorTemperature
        ))
    };
    cluster.with_events(with!())
}

pub(super) struct LightingHandler {
    service: DeviceService,
    feature: FeatureIdentity,
    endpoint: u16,
    capabilities: RefCell<Vec<Capability>>,
    on_off_dataver: Dataver,
    level_dataver: Dataver,
    color_dataver: Dataver,
    on_time: Cell<u16>,
    off_wait_time: Cell<u16>,
    level_options: Cell<level_control::OptionsBitmap>,
    color_options: Cell<color_control::OptionsBitmap>,
    on_level: RefCell<Nullable<u8>>,
    global_scene: RefCell<Option<Vec<DeviceCommand>>>,
    scene_intent: RefCell<Option<CommandIntent>>,
    scene_confirmation: RefCell<Option<SceneConfirmation>>,
    timed_off: RefCell<Option<TimedOff>>,
    adjustment: RefCell<Option<Adjustment>>,
    last_level_report: Cell<Option<Instant>>,
    pending_level_report: Cell<Option<Instant>>,
    next_remaining_report: Cell<Option<Instant>>,
    next_operation: Cell<u64>,
    wake: Event,
}

#[derive(Clone)]
struct TimedOff {
    operation: u64,
    intent: CommandIntent,
    deadline: Instant,
    off_wait_time: u16,
    phase: TimedOffPhase,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TimedOffPhase {
    On,
    OffWait,
}

#[derive(Clone)]
struct Adjustment {
    operation: u64,
    intent: CommandIntent,
    started: Instant,
    duration: Duration,
    last_steps: Option<Vec<u32>>,
    kinds: Vec<AdjustmentKind>,
}

struct SceneConfirmation {
    fabric: NonZeroU8,
    group: u16,
    scene: u8,
    expected: BTreeMap<Property, PropertyValue>,
    confirmed: bool,
}

#[derive(Clone, Copy)]
enum AdjustmentKind {
    Brightness {
        start: u8,
        target: u8,
        with_on_off: bool,
    },
    ColorTemperature {
        start_mireds: u16,
        target_mireds: u16,
    },
    Xy {
        start_x: u16,
        start_y: u16,
        target_x: u16,
        target_y: u16,
    },
    XyMove {
        start_x: u16,
        start_y: u16,
        rate_x: i16,
        rate_y: i16,
    },
}

impl LightingHandler {
    pub(super) fn new(
        service: DeviceService,
        feature: FeatureIdentity,
        capabilities: Vec<Capability>,
        endpoint: u16,
        seed: u32,
    ) -> Self {
        Self {
            service,
            feature,
            endpoint,
            capabilities: RefCell::new(capabilities),
            on_off_dataver: Dataver::new(seed),
            level_dataver: Dataver::new(seed.wrapping_add(1)),
            color_dataver: Dataver::new(seed.wrapping_add(2)),
            on_time: Cell::new(0),
            off_wait_time: Cell::new(0),
            level_options: Cell::new(level_control::OptionsBitmap::empty()),
            color_options: Cell::new(color_control::OptionsBitmap::empty()),
            on_level: RefCell::new(Nullable::none()),
            global_scene: RefCell::new(None),
            scene_intent: RefCell::new(None),
            scene_confirmation: RefCell::new(None),
            timed_off: RefCell::new(None),
            adjustment: RefCell::new(None),
            last_level_report: Cell::new(None),
            pending_level_report: Cell::new(None),
            next_remaining_report: Cell::new(None),
            next_operation: Cell::new(0),
            wake: Event::new(),
        }
    }

    fn power(&self) -> Option<bool> {
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

    pub(super) fn set_capabilities(&self, capabilities: Vec<Capability>) {
        *self.capabilities.borrow_mut() = capabilities;
    }

    pub(super) fn should_report_brightness(
        &self,
        previous: Option<&PropertyValue>,
        current: Option<&PropertyValue>,
    ) -> bool {
        let level = |value: Option<&PropertyValue>| match value {
            Some(PropertyValue::Percent(value)) => matter_level(*value),
            _ => None,
        };
        let now = Instant::now();
        if level(previous).is_none() != level(current).is_none()
            || self
                .last_level_report
                .get()
                .is_none_or(|last| now.saturating_duration_since(last) >= Duration::from_secs(1))
        {
            self.last_level_report.set(Some(now));
            self.pending_level_report.set(None);
            true
        } else {
            let deadline = self
                .last_level_report
                .get()
                .map_or(now, |last| last + Duration::from_secs(1));
            self.pending_level_report.set(Some(deadline));
            self.wake.notify(usize::MAX);
            false
        }
    }

    async fn command(&self, command: DeviceCommand) -> Result<(), Error> {
        match self.service.command(&self.feature, command).await {
            CommandOutcome::Accepted => Ok(()),
            CommandOutcome::Unsupported | CommandOutcome::Rejected(_) => {
                Err(ErrorCode::ConstraintError.into())
            }
            CommandOutcome::Unavailable
            | CommandOutcome::Expired
            | CommandOutcome::Cancelled
            | CommandOutcome::Superseded
            | CommandOutcome::Ambiguous => Err(ErrorCode::Failure.into()),
        }
    }

    fn on_commands(&self) -> Result<Vec<DeviceCommand>, Error> {
        let mut commands = Vec::new();
        if let Some(level) = self.on_level.borrow().clone().into_option() {
            commands.extend(self.commands_for_level(level, false)?);
        }
        commands.push(DeviceCommand::SetPower(true));
        Ok(commands)
    }

    fn capture_global_scene(&self) -> Result<Vec<DeviceCommand>, Error> {
        let mut commands = vec![DeviceCommand::SetPower(
            self.power().ok_or(ErrorCode::Failure)?,
        )];
        if let Some(value) = self.brightness() {
            commands.push(DeviceCommand::SetBrightness(value));
        }
        if let Some(value) = self.color_temperature() {
            commands.push(DeviceCommand::SetColorTemperature(value));
        }
        if let Some(value) = self.color() {
            commands.push(DeviceCommand::SetColor(value));
        }
        Ok(commands)
    }

    async fn intent_command(
        &self,
        intent: &CommandIntent,
        commands: Vec<DeviceCommand>,
    ) -> Result<(), Error> {
        match intent.command_batch(commands).await {
            CommandOutcome::Accepted => Ok(()),
            CommandOutcome::Unsupported | CommandOutcome::Rejected(_) => {
                Err(ErrorCode::ConstraintError.into())
            }
            _ => Err(ErrorCode::Failure.into()),
        }
    }

    pub(super) fn begin_scene_recall(&self, fabric: NonZeroU8, group: u16, scene: u8) {
        self.adjustment.borrow_mut().take();
        let capabilities = self.capabilities.borrow();
        let properties = [
            capabilities
                .iter()
                .any(|capability| matches!(capability, Capability::Power { writable: true }))
                .then_some(Property::Power),
            capabilities
                .iter()
                .any(|capability| matches!(capability, Capability::Brightness(_)))
                .then_some(Property::Brightness),
            capabilities
                .contains(&Capability::Color)
                .then_some(Property::Color),
            capabilities
                .iter()
                .any(|capability| matches!(capability, Capability::ColorTemperature(_)))
                .then_some(Property::ColorTemperature),
        ]
        .into_iter()
        .flatten();
        *self.scene_intent.borrow_mut() =
            Some(self.service.begin_command_intent(&self.feature, properties));
        *self.scene_confirmation.borrow_mut() = Some(SceneConfirmation {
            fabric,
            group,
            scene,
            expected: BTreeMap::new(),
            confirmed: false,
        });
    }

    pub(super) fn finish_scene_recall(&self, accepted: bool) -> i8 {
        self.scene_intent.borrow_mut().take();
        if !accepted {
            self.scene_confirmation.borrow_mut().take();
            -1
        } else {
            self.scene_state_changed()
        }
    }

    async fn scene_command(&self, commands: Vec<DeviceCommand>) -> Result<(), Error> {
        let intent = self
            .scene_intent
            .borrow()
            .as_ref()
            .cloned()
            .ok_or(ErrorCode::Failure)?;
        self.intent_command(&intent, commands.clone()).await?;
        if let Some(scene) = self.scene_confirmation.borrow_mut().as_mut() {
            for command in commands {
                if let Some((property, value)) = expected_command_value(command) {
                    scene.expected.insert(property, value);
                }
            }
        }
        Ok(())
    }

    pub(super) fn pending_scene(&self, fabric: NonZeroU8) -> Option<(u16, u8)> {
        self.scene_confirmation
            .borrow()
            .as_ref()
            .filter(|scene| scene.fabric == fabric && !scene.confirmed)
            .map(|scene| (scene.group, scene.scene))
    }

    pub(super) fn scene_state_changed(&self) -> i8 {
        if self.scene_intent.borrow().is_some() {
            return 0;
        }
        let mut confirmation = self.scene_confirmation.borrow_mut();
        let Some(scene) = confirmation.as_mut() else {
            return -1;
        };
        let snapshot = self.service.snapshot(&self.feature);
        let matches = !scene.expected.is_empty()
            && scene.expected.iter().all(|(property, expected)| {
                snapshot
                    .as_ref()
                    .and_then(|snapshot| snapshot.property(*property))
                    .is_some_and(|state| match state {
                        PropertyState::Current { value, .. } => {
                            self.scene_value_matches(*property, expected, value)
                        }
                        _ => false,
                    })
            });
        if matches {
            let changed = !scene.confirmed;
            scene.confirmed = true;
            i8::from(changed)
        } else if scene.confirmed {
            scene.confirmed = false;
            -1
        } else {
            0
        }
    }

    fn scene_value_matches(
        &self,
        property: Property,
        expected: &PropertyValue,
        actual: &PropertyValue,
    ) -> bool {
        match (property, expected, actual) {
            (
                Property::Brightness,
                PropertyValue::Percent(expected),
                PropertyValue::Percent(actual),
            ) => matter_level(*expected) == matter_level(*actual),
            (
                Property::ColorTemperature,
                PropertyValue::ColorTemperature(expected),
                PropertyValue::ColorTemperature(actual),
            ) => kelvin_to_mired(*expected) == kelvin_to_mired(*actual),
            _ => expected == actual,
        }
    }

    pub(super) async fn invoke_on_off(&self, ctx: impl InvokeContext) -> Result<(), Error> {
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

    pub(super) async fn run(&self, ctx: impl HandlerContext) -> Result<(), Error> {
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

    async fn advance_adjustment(&self, ctx: &impl HandlerContext) {
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

    fn clear_adjustment(&self, operation: u64) {
        if self
            .adjustment
            .borrow()
            .as_ref()
            .is_some_and(|current| current.operation == operation)
        {
            self.adjustment.borrow_mut().take();
        }
    }

    fn next_operation(&self) -> u64 {
        let operation = self.next_operation.get().wrapping_add(1);
        self.next_operation.set(operation);
        operation
    }

    fn notify_remaining_time(&self, ctx: &impl HandlerContext) {
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

    pub(super) fn adjustment_active(&self) -> bool {
        self.adjustment.borrow().is_some()
    }

    pub(super) fn adjustment_command_completed(&self, ctx: &impl HandlerContext, was_active: bool) {
        let active = self.adjustment_active();
        if was_active || active {
            self.next_remaining_report
                .set(active.then(|| Instant::now() + Duration::from_secs(1)));
            self.notify_remaining_time(ctx);
        }
    }

    pub(super) fn dataver_for(&self, cluster: u32) -> Option<&Dataver> {
        match cluster {
            id if id == ON_OFF_CLUSTER.id => Some(&self.on_off_dataver),
            id if id == LEVEL_CLUSTER.id => Some(&self.level_dataver),
            id if id == color_control::FULL_CLUSTER.id => Some(&self.color_dataver),
            _ => None,
        }
    }

    fn brightness(&self) -> Option<Percent> {
        match self
            .service
            .snapshot(&self.feature)?
            .property(Property::Brightness)?
        {
            PropertyState::Current {
                value: PropertyValue::Percent(value),
                ..
            } => Some(*value),
            _ => None,
        }
    }

    fn current_level_value(&self) -> Option<u8> {
        self.brightness().and_then(matter_level)
    }

    fn adjustment_remaining_ds(&self) -> u16 {
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

    fn color_temperature(&self) -> Option<u32> {
        match self
            .service
            .snapshot(&self.feature)?
            .property(Property::ColorTemperature)?
        {
            PropertyState::Current {
                value: PropertyValue::ColorTemperature(value),
                ..
            } => Some(*value),
            _ => None,
        }
    }

    fn current_mireds(&self) -> Option<u16> {
        kelvin_to_mired(self.color_temperature()?)
    }

    fn color(&self) -> Option<RgbColor> {
        match self
            .service
            .snapshot(&self.feature)?
            .property(Property::Color)?
        {
            PropertyState::Current {
                value: PropertyValue::Color(value),
                ..
            } => Some(*value),
            _ => None,
        }
    }

    fn color_temperature_range(&self) -> Option<NumericRange> {
        self.capabilities
            .borrow()
            .iter()
            .find_map(|capability| match capability {
                Capability::ColorTemperature(range) => Some(*range),
                _ => None,
            })
    }

    fn quantized_kelvin_for_mired(&self, mireds: u16) -> Result<u32, Error> {
        if mireds == 0 {
            return Err(ErrorCode::ConstraintError.into());
        }
        let range = self
            .color_temperature_range()
            .ok_or(ErrorCode::InvalidCommand)?;
        let kelvin = 1_000_000.0 / f64::from(mireds);
        if !kelvin.is_finite() || kelvin < range.minimum || kelvin > range.maximum {
            return Err(ErrorCode::ConstraintError.into());
        }
        let quantized =
            range.minimum + ((kelvin - range.minimum) / range.step).round() * range.step;
        Ok(quantized.clamp(range.minimum, range.maximum).round() as u32)
    }

    fn brightness_range(&self) -> Option<NumericRange> {
        self.capabilities
            .borrow()
            .iter()
            .find_map(|capability| match capability {
                Capability::Brightness(range) => Some(*range),
                _ => None,
            })
    }

    fn mired_bounds(&self) -> Option<(u16, u16)> {
        let range = self.color_temperature_range()?;
        Some((
            (1_000_000.0 / range.maximum).ceil() as u16,
            (1_000_000.0 / range.minimum).floor() as u16,
        ))
    }

    fn commands_for_level(
        &self,
        level: u8,
        with_on_off: bool,
    ) -> Result<Vec<DeviceCommand>, Error> {
        let range = self.brightness_range().ok_or(ErrorCode::InvalidCommand)?;
        let raw = f64::from(level) * 100.0 / 254.0;
        let quantized = (range.minimum + ((raw - range.minimum) / range.step).round() * range.step)
            .clamp(range.minimum, range.maximum);
        let percent = Percent::new(quantized).map_err(|_| ErrorCode::ConstraintError)?;
        let mut commands = if with_on_off {
            if level == 0 {
                vec![DeviceCommand::SetPower(false)]
            } else {
                vec![
                    DeviceCommand::SetBrightness(percent),
                    DeviceCommand::SetPower(true),
                ]
            }
        } else {
            vec![DeviceCommand::SetBrightness(percent)]
        };
        if level > 0
            && self
                .level_options
                .get()
                .contains(level_control::OptionsBitmap::COUPLE_COLOR_TEMP_TO_LEVEL)
            && !self.capabilities.borrow().contains(&Capability::Color)
            && let Some((minimum, maximum)) = self.mired_bounds()
        {
            let mireds =
                f64::from(maximum) - f64::from(maximum - minimum) * f64::from(level) / 254.0;
            let kelvin = 1_000_000.0 / mireds;
            let range = self
                .color_temperature_range()
                .ok_or(ErrorCode::InvalidCommand)?;
            let quantized = (range.minimum
                + ((kelvin - range.minimum) / range.step).round() * range.step)
                .clamp(range.minimum, range.maximum);
            commands.insert(
                commands.len().min(1),
                DeviceCommand::SetColorTemperature(quantized.round() as u32),
            );
        }
        Ok(commands)
    }

    async fn set_level(&self, level: u8, with_on_off: bool) -> Result<(), Error> {
        self.command_batch(self.commands_for_level(level, with_on_off)?)
            .await
    }

    fn start_adjustment(
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

    fn add_scene_adjustment(
        &self,
        intent: CommandIntent,
        duration_ds: u32,
        kind: AdjustmentKind,
    ) -> Result<(), Error> {
        if let Some(adjustment) = self.adjustment.borrow_mut().as_mut() {
            adjustment.kinds.push(kind);
            adjustment.last_steps = None;
            self.wake.notify(usize::MAX);
            Ok(())
        } else {
            self.start_adjustment(intent, duration_ds, kind)
        }
    }

    fn should_execute_level(
        &self,
        with_on_off: bool,
        mask: level_control::OptionsBitmap,
        override_value: level_control::OptionsBitmap,
    ) -> Result<bool, Error> {
        if with_on_off || self.power() == Some(true) {
            return Ok(true);
        }
        if self.power().is_none() {
            return Err(ErrorCode::Failure.into());
        }
        let effective = (self.level_options.get() & !mask) | (override_value & mask);
        Ok(effective.contains(level_control::OptionsBitmap::EXECUTE_IF_OFF))
    }

    fn should_execute_color(
        &self,
        mask: color_control::OptionsBitmap,
        override_value: color_control::OptionsBitmap,
    ) -> Result<bool, Error> {
        if self.power() == Some(true) {
            return Ok(true);
        }
        if self.power().is_none() {
            return Err(ErrorCode::Failure.into());
        }
        let effective = (self.color_options.get() & !mask) | (override_value & mask);
        Ok(effective.contains(color_control::OptionsBitmap::EXECUTE_IF_OFF))
    }

    async fn command_batch(&self, commands: Vec<DeviceCommand>) -> Result<(), Error> {
        match self.service.command_batch(&self.feature, commands).await {
            CommandOutcome::Accepted => Ok(()),
            CommandOutcome::Unsupported | CommandOutcome::Rejected(_) => {
                Err(ErrorCode::ConstraintError.into())
            }
            _ => Err(ErrorCode::Failure.into()),
        }
    }

    pub(super) async fn invoke_level(&self, ctx: &impl InvokeContext) -> Result<(), Error> {
        let command = level_control::CommandId::try_from(ctx.cmd().cmd_id)?;
        match command {
            level_control::CommandId::MoveToLevel => {
                ctx.data().structure()?;
                let request = level_control::MoveToLevelRequest::new(ctx.data().clone());
                let level = request.level()?;
                if level == u8::MAX {
                    return Err(ErrorCode::ConstraintError.into());
                }
                let transition = request.transition_time()?.into_option().unwrap_or(0);
                if !self.should_execute_level(
                    false,
                    request.options_mask()?,
                    request.options_override()?,
                )? {
                    return Ok(());
                }
                if transition == 0 {
                    self.set_level(level, false).await
                } else {
                    let start = self.current_level_value().ok_or(ErrorCode::Failure)?;
                    let intent = self
                        .service
                        .begin_command_intent(&self.feature, [Property::Brightness]);
                    self.start_adjustment(
                        intent,
                        u32::from(transition),
                        AdjustmentKind::Brightness {
                            start,
                            target: level.max(1),
                            with_on_off: false,
                        },
                    )
                }
            }
            level_control::CommandId::MoveToLevelWithOnOff => {
                ctx.data().structure()?;
                let request = level_control::MoveToLevelWithOnOffRequest::new(ctx.data().clone());
                let level = request.level()?;
                if level == u8::MAX {
                    return Err(ErrorCode::ConstraintError.into());
                }
                let transition = request.transition_time()?.into_option().unwrap_or(0);
                request.options_mask()?;
                request.options_override()?;
                if transition == 0 {
                    self.set_level(level, true).await
                } else {
                    let start = self.current_level_value().ok_or(ErrorCode::Failure)?;
                    let intent = self.service.begin_command_intent(
                        &self.feature,
                        [Property::Brightness, Property::Power],
                    );
                    self.start_adjustment(
                        intent,
                        u32::from(transition),
                        AdjustmentKind::Brightness {
                            start,
                            target: level,
                            with_on_off: true,
                        },
                    )
                }
            }
            level_control::CommandId::Step | level_control::CommandId::StepWithOnOff => {
                ctx.data().structure()?;
                let with_on_off = command == level_control::CommandId::StepWithOnOff;
                let (mode, size, transition, mask, override_value) = if with_on_off {
                    let request = level_control::StepWithOnOffRequest::new(ctx.data().clone());
                    (
                        request.step_mode()?,
                        request.step_size()?,
                        request.transition_time()?.into_option().unwrap_or(0),
                        request.options_mask()?,
                        request.options_override()?,
                    )
                } else {
                    let request = level_control::StepRequest::new(ctx.data().clone());
                    (
                        request.step_mode()?,
                        request.step_size()?,
                        request.transition_time()?.into_option().unwrap_or(0),
                        request.options_mask()?,
                        request.options_override()?,
                    )
                };
                if !self.should_execute_level(with_on_off, mask, override_value)? {
                    return Ok(());
                }
                let current = self.current_level_value().ok_or(ErrorCode::Failure)?;
                let target = match mode {
                    level_control::StepModeEnum::Up => current.saturating_add(size).min(254),
                    level_control::StepModeEnum::Down => {
                        let target = current.saturating_sub(size);
                        if with_on_off && target <= 1 {
                            0
                        } else {
                            target.max(1)
                        }
                    }
                };
                if transition == 0 {
                    self.set_level(target, with_on_off).await
                } else {
                    let properties = if with_on_off {
                        vec![Property::Brightness, Property::Power]
                    } else {
                        vec![Property::Brightness]
                    };
                    let intent = self.service.begin_command_intent(&self.feature, properties);
                    self.start_adjustment(
                        intent,
                        u32::from(transition),
                        AdjustmentKind::Brightness {
                            start: current,
                            target,
                            with_on_off,
                        },
                    )
                }
            }
            level_control::CommandId::Stop | level_control::CommandId::StopWithOnOff => {
                ctx.data().structure()?;
                if command == level_control::CommandId::StopWithOnOff {
                    let request = level_control::StopWithOnOffRequest::new(ctx.data().clone());
                    request.options_mask()?;
                    request.options_override()?;
                } else {
                    let request = level_control::StopRequest::new(ctx.data().clone());
                    request.options_mask()?;
                    request.options_override()?;
                }
                self.adjustment.borrow_mut().take();
                self.wake.notify(usize::MAX);
                self.service
                    .stop_adjustment(&self.feature, Property::Brightness);
                Ok(())
            }
            level_control::CommandId::Move | level_control::CommandId::MoveWithOnOff => {
                ctx.data().structure()?;
                let with_on_off = command == level_control::CommandId::MoveWithOnOff;
                let (mode, rate, mask, override_value) = if with_on_off {
                    let request = level_control::MoveWithOnOffRequest::new(ctx.data().clone());
                    (
                        request.move_mode()?,
                        request.rate()?.into_option(),
                        request.options_mask()?,
                        request.options_override()?,
                    )
                } else {
                    let request = level_control::MoveRequest::new(ctx.data().clone());
                    (
                        request.move_mode()?,
                        request.rate()?.into_option(),
                        request.options_mask()?,
                        request.options_override()?,
                    )
                };
                if !self.should_execute_level(with_on_off, mask, override_value)? {
                    return Ok(());
                }
                let rate = rate
                    .filter(|rate| *rate > 0)
                    .ok_or(ErrorCode::ConstraintError)?;
                let start = self.current_level_value().ok_or(ErrorCode::Failure)?;
                let target = match mode {
                    level_control::MoveModeEnum::Up => 254,
                    level_control::MoveModeEnum::Down if with_on_off => 0,
                    level_control::MoveModeEnum::Down => 1,
                };
                let duration = u16::from(start.abs_diff(target))
                    .saturating_mul(10)
                    .div_ceil(u16::from(rate));
                let properties = if with_on_off {
                    vec![Property::Brightness, Property::Power]
                } else {
                    vec![Property::Brightness]
                };
                let intent = self.service.begin_command_intent(&self.feature, properties);
                self.start_adjustment(
                    intent,
                    u32::from(duration.max(1)),
                    AdjustmentKind::Brightness {
                        start,
                        target,
                        with_on_off,
                    },
                )
            }
            level_control::CommandId::MoveToClosestFrequency => {
                Err(ErrorCode::InvalidCommand.into())
            }
        }
    }

    pub(super) async fn invoke_color(&self, ctx: &impl InvokeContext) -> Result<(), Error> {
        match color_control::CommandId::try_from(ctx.cmd().cmd_id)? {
            color_control::CommandId::MoveToColorTemperature => {
                ctx.data().structure()?;
                let request = color_control::MoveToColorTemperatureRequest::new(ctx.data().clone());
                let mireds = request.color_temperature_mireds()?;
                let transition = request.transition_time()?;
                if !self
                    .should_execute_color(request.options_mask()?, request.options_override()?)?
                {
                    return Ok(());
                }
                let kelvin = self.quantized_kelvin_for_mired(mireds)?;
                if transition == 0 {
                    self.command(DeviceCommand::SetColorTemperature(kelvin))
                        .await
                } else {
                    let start_mireds = self.current_mireds().ok_or(ErrorCode::Failure)?;
                    let intent = self
                        .service
                        .begin_command_intent(&self.feature, [Property::ColorTemperature]);
                    self.start_adjustment(
                        intent,
                        u32::from(transition),
                        AdjustmentKind::ColorTemperature {
                            start_mireds,
                            target_mireds: mireds,
                        },
                    )
                }
            }
            color_control::CommandId::MoveToColor => {
                ctx.data().structure()?;
                let request = color_control::MoveToColorRequest::new(ctx.data().clone());
                let x = request.color_x()?;
                let y = request.color_y()?;
                let transition = request.transition_time()?;
                if !self
                    .should_execute_color(request.options_mask()?, request.options_override()?)?
                {
                    return Ok(());
                }
                if x > 0xfeff || y > 0xfeff {
                    return Err(ErrorCode::ConstraintError.into());
                }
                if transition > 0 {
                    let (start_x, start_y) =
                        self.color().map(rgb_to_xy).ok_or(ErrorCode::Failure)?;
                    let intent = self
                        .service
                        .begin_command_intent(&self.feature, [Property::Color]);
                    return self.start_adjustment(
                        intent,
                        u32::from(transition),
                        AdjustmentKind::Xy {
                            start_x,
                            start_y,
                            target_x: x,
                            target_y: y,
                        },
                    );
                }
                let (red, green, blue) = SetDeviceColor::Xy { x, y }.to_rgb(RgbGamma::SRgb);
                self.command(DeviceCommand::SetColor(RgbColor { red, green, blue }))
                    .await
            }
            color_control::CommandId::StepColor => {
                ctx.data().structure()?;
                let request = color_control::StepColorRequest::new(ctx.data().clone());
                if !self
                    .should_execute_color(request.options_mask()?, request.options_override()?)?
                {
                    return Ok(());
                }
                let (start_x, start_y) = self.color().map(rgb_to_xy).ok_or(ErrorCode::Failure)?;
                let target_x = i32::from(start_x)
                    .saturating_add(i32::from(request.step_x()?))
                    .clamp(0, i32::from(0xfeff_u16)) as u16;
                let target_y = i32::from(start_y)
                    .saturating_add(i32::from(request.step_y()?))
                    .clamp(0, i32::from(0xfeff_u16)) as u16;
                let transition = request.transition_time()?;
                if transition == 0 {
                    let (red, green, blue) = SetDeviceColor::Xy {
                        x: target_x,
                        y: target_y,
                    }
                    .to_rgb(RgbGamma::SRgb);
                    self.command(DeviceCommand::SetColor(RgbColor { red, green, blue }))
                        .await
                } else {
                    let intent = self
                        .service
                        .begin_command_intent(&self.feature, [Property::Color]);
                    self.start_adjustment(
                        intent,
                        u32::from(transition),
                        AdjustmentKind::Xy {
                            start_x,
                            start_y,
                            target_x,
                            target_y,
                        },
                    )
                }
            }
            color_control::CommandId::MoveColor => {
                ctx.data().structure()?;
                let request = color_control::MoveColorRequest::new(ctx.data().clone());
                if !self
                    .should_execute_color(request.options_mask()?, request.options_override()?)?
                {
                    return Ok(());
                }
                let rate_x = request.rate_x()?;
                let rate_y = request.rate_y()?;
                if rate_x == 0 && rate_y == 0 {
                    return Err(ErrorCode::ConstraintError.into());
                }
                let (start_x, start_y) = self.color().map(rgb_to_xy).ok_or(ErrorCode::Failure)?;
                let target_x = if rate_x < 0 {
                    0
                } else if rate_x > 0 {
                    0xfeff
                } else {
                    start_x
                };
                let target_y = if rate_y < 0 {
                    0
                } else if rate_y > 0 {
                    0xfeff
                } else {
                    start_y
                };
                let axis_ds = |start: u16, target: u16, rate: i16| {
                    if rate == 0 {
                        0
                    } else {
                        (u32::from(start.abs_diff(target)) * 10)
                            .div_ceil(u32::from(rate.unsigned_abs()))
                    }
                };
                let duration = axis_ds(start_x, target_x, rate_x)
                    .max(axis_ds(start_y, target_y, rate_y))
                    .max(1);
                let intent = self
                    .service
                    .begin_command_intent(&self.feature, [Property::Color]);
                self.start_adjustment(
                    intent,
                    duration,
                    AdjustmentKind::XyMove {
                        start_x,
                        start_y,
                        rate_x,
                        rate_y,
                    },
                )
            }
            color_control::CommandId::StepColorTemperature => {
                ctx.data().structure()?;
                let request = color_control::StepColorTemperatureRequest::new(ctx.data().clone());
                if !self
                    .should_execute_color(request.options_mask()?, request.options_override()?)?
                {
                    return Ok(());
                }
                let (physical_min, physical_max) =
                    self.mired_bounds().ok_or(ErrorCode::InvalidCommand)?;
                let minimum = match request.color_temperature_minimum_mireds()? {
                    0 => physical_min,
                    value => value.max(physical_min),
                };
                let maximum = match request.color_temperature_maximum_mireds()? {
                    0 => physical_max,
                    value => value.min(physical_max),
                };
                if minimum > maximum {
                    return Err(ErrorCode::ConstraintError.into());
                }
                let start_mireds = self.current_mireds().ok_or(ErrorCode::Failure)?;
                let size = request.step_size()?;
                let target_mireds = match request.step_mode()? {
                    color_control::StepModeEnum::Up => {
                        start_mireds.saturating_add(size).min(maximum)
                    }
                    color_control::StepModeEnum::Down => {
                        start_mireds.saturating_sub(size).max(minimum)
                    }
                };
                let transition = request.transition_time()?;
                if transition == 0 {
                    self.command(DeviceCommand::SetColorTemperature(
                        self.quantized_kelvin_for_mired(target_mireds)?,
                    ))
                    .await
                } else {
                    let intent = self
                        .service
                        .begin_command_intent(&self.feature, [Property::ColorTemperature]);
                    self.start_adjustment(
                        intent,
                        u32::from(transition),
                        AdjustmentKind::ColorTemperature {
                            start_mireds,
                            target_mireds,
                        },
                    )
                }
            }
            color_control::CommandId::MoveColorTemperature => {
                ctx.data().structure()?;
                let request = color_control::MoveColorTemperatureRequest::new(ctx.data().clone());
                if !self
                    .should_execute_color(request.options_mask()?, request.options_override()?)?
                {
                    return Ok(());
                }
                let (physical_min, physical_max) =
                    self.mired_bounds().ok_or(ErrorCode::InvalidCommand)?;
                let minimum = match request.color_temperature_minimum_mireds()? {
                    0 => physical_min,
                    value => value.max(physical_min),
                };
                let maximum = match request.color_temperature_maximum_mireds()? {
                    0 => physical_max,
                    value => value.min(physical_max),
                };
                if minimum > maximum {
                    return Err(ErrorCode::ConstraintError.into());
                }
                let rate = request.rate()?;
                let start_mireds = self.current_mireds().ok_or(ErrorCode::Failure)?;
                match request.move_mode()? {
                    color_control::MoveModeEnum::Stop => {
                        self.adjustment.borrow_mut().take();
                        self.wake.notify(usize::MAX);
                        Ok(())
                    }
                    _ if rate == 0 => Err(ErrorCode::ConstraintError.into()),
                    mode
                    @ (color_control::MoveModeEnum::Up | color_control::MoveModeEnum::Down) => {
                        let target_mireds = if matches!(mode, color_control::MoveModeEnum::Up) {
                            maximum
                        } else {
                            minimum
                        };
                        let duration = (u32::from(start_mireds.abs_diff(target_mireds)) * 10
                            / u32::from(rate))
                        .max(1);
                        let intent = self
                            .service
                            .begin_command_intent(&self.feature, [Property::ColorTemperature]);
                        self.start_adjustment(
                            intent,
                            duration,
                            AdjustmentKind::ColorTemperature {
                                start_mireds,
                                target_mireds,
                            },
                        )
                    }
                }
            }
            color_control::CommandId::StopMoveStep => {
                ctx.data().structure()?;
                let request = color_control::StopMoveStepRequest::new(ctx.data().clone());
                request.options_mask()?;
                request.options_override()?;
                self.adjustment.borrow_mut().take();
                self.wake.notify(usize::MAX);
                self.service.stop_adjustment(&self.feature, Property::Color);
                self.service
                    .stop_adjustment(&self.feature, Property::ColorTemperature);
                Ok(())
            }
            _ => Err(ErrorCode::InvalidCommand.into()),
        }
    }
}

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

impl level_control::ClusterHandler for LightingHandler {
    const CLUSTER: Cluster<'static> = LEVEL_CLUSTER;

    fn dataver(&self) -> u32 {
        self.level_dataver.get()
    }

    fn dataver_changed(&self) {
        self.level_dataver.changed();
    }

    fn current_level(&self, _ctx: impl ReadContext) -> Result<Nullable<u8>, Error> {
        Ok(self
            .current_level_value()
            .map_or_else(Nullable::none, Nullable::some))
    }

    fn remaining_time(&self, _ctx: impl ReadContext) -> Result<u16, Error> {
        Ok(self.adjustment_remaining_ds())
    }

    fn min_level(&self, _ctx: impl ReadContext) -> Result<u8, Error> {
        Ok(1)
    }

    fn max_level(&self, _ctx: impl ReadContext) -> Result<u8, Error> {
        Ok(254)
    }

    fn options(&self, _ctx: impl ReadContext) -> Result<level_control::OptionsBitmap, Error> {
        Ok(self.level_options.get())
    }

    fn on_level(&self, _ctx: impl ReadContext) -> Result<Nullable<u8>, Error> {
        Ok(self.on_level.borrow().clone())
    }

    fn start_up_current_level(&self, _ctx: impl ReadContext) -> Result<Nullable<u8>, Error> {
        Err(ErrorCode::AttributeNotFound.into())
    }

    fn set_options(
        &self,
        _ctx: impl WriteContext,
        value: level_control::OptionsBitmap,
    ) -> Result<(), Error> {
        self.level_options.set(value);
        _ctx.notify_changed();
        Ok(())
    }

    fn set_on_level(&self, ctx: impl WriteContext, value: Nullable<u8>) -> Result<(), Error> {
        if value.clone().into_option().is_some_and(|value| value == 0) {
            return Err(ErrorCode::ConstraintError.into());
        }
        *self.on_level.borrow_mut() = value;
        ctx.notify_changed();
        Ok(())
    }

    fn set_start_up_current_level(
        &self,
        _ctx: impl WriteContext,
        _value: Nullable<u8>,
    ) -> Result<(), Error> {
        Err(ErrorCode::UnsupportedAccess.into())
    }

    fn handle_move_to_level(
        &self,
        _ctx: impl InvokeContext,
        _request: level_control::MoveToLevelRequest<'_>,
    ) -> Result<(), Error> {
        Err(ErrorCode::Failure.into())
    }

    fn handle_move(
        &self,
        _ctx: impl InvokeContext,
        _request: level_control::MoveRequest<'_>,
    ) -> Result<(), Error> {
        Err(ErrorCode::Failure.into())
    }

    fn handle_step(
        &self,
        _ctx: impl InvokeContext,
        _request: level_control::StepRequest<'_>,
    ) -> Result<(), Error> {
        Err(ErrorCode::Failure.into())
    }

    fn handle_stop(
        &self,
        _ctx: impl InvokeContext,
        _request: level_control::StopRequest<'_>,
    ) -> Result<(), Error> {
        Err(ErrorCode::Failure.into())
    }

    fn handle_move_to_level_with_on_off(
        &self,
        _ctx: impl InvokeContext,
        _request: level_control::MoveToLevelWithOnOffRequest<'_>,
    ) -> Result<(), Error> {
        Err(ErrorCode::Failure.into())
    }

    fn handle_move_with_on_off(
        &self,
        _ctx: impl InvokeContext,
        _request: level_control::MoveWithOnOffRequest<'_>,
    ) -> Result<(), Error> {
        Err(ErrorCode::Failure.into())
    }

    fn handle_step_with_on_off(
        &self,
        _ctx: impl InvokeContext,
        _request: level_control::StepWithOnOffRequest<'_>,
    ) -> Result<(), Error> {
        Err(ErrorCode::Failure.into())
    }

    fn handle_stop_with_on_off(
        &self,
        _ctx: impl InvokeContext,
        _request: level_control::StopWithOnOffRequest<'_>,
    ) -> Result<(), Error> {
        Err(ErrorCode::Failure.into())
    }

    fn handle_move_to_closest_frequency(
        &self,
        _ctx: impl InvokeContext,
        _request: level_control::MoveToClosestFrequencyRequest<'_>,
    ) -> Result<(), Error> {
        Err(ErrorCode::InvalidCommand.into())
    }
}

impl color_control::ClusterHandler for LightingHandler {
    const CLUSTER: Cluster<'static> = color_cluster(true, true);

    fn dataver(&self) -> u32 {
        self.color_dataver.get()
    }
    fn dataver_changed(&self) {
        self.color_dataver.changed();
    }
    fn current_x(&self, _ctx: impl ReadContext) -> Result<u16, Error> {
        self.color()
            .map(rgb_to_xy)
            .map(|value| value.0)
            .ok_or(ErrorCode::Failure.into())
    }
    fn current_y(&self, _ctx: impl ReadContext) -> Result<u16, Error> {
        self.color()
            .map(rgb_to_xy)
            .map(|value| value.1)
            .ok_or(ErrorCode::Failure.into())
    }
    fn remaining_time(&self, _ctx: impl ReadContext) -> Result<u16, Error> {
        Ok(self.adjustment_remaining_ds())
    }
    fn color_temperature_mireds(&self, _ctx: impl ReadContext) -> Result<u16, Error> {
        let kelvin = self.color_temperature().ok_or(ErrorCode::Failure)?;
        u16::try_from((1_000_000_u32 + kelvin / 2) / kelvin).map_err(|_| ErrorCode::Failure.into())
    }
    fn color_mode(&self, _ctx: impl ReadContext) -> Result<color_control::ColorModeEnum, Error> {
        if self.capabilities.borrow().contains(&Capability::Color)
            && self.color_temperature_range().is_some()
        {
            Err(ErrorCode::Failure.into())
        } else if self.capabilities.borrow().contains(&Capability::Color) {
            Ok(color_control::ColorModeEnum::CurrentXAndCurrentY)
        } else {
            Ok(color_control::ColorModeEnum::ColorTemperatureMireds)
        }
    }
    fn options(&self, _ctx: impl ReadContext) -> Result<color_control::OptionsBitmap, Error> {
        Ok(self.color_options.get())
    }
    fn number_of_primaries(&self, _ctx: impl ReadContext) -> Result<Nullable<u8>, Error> {
        Ok(Nullable::none())
    }
    fn enhanced_color_mode(
        &self,
        _ctx: impl ReadContext,
    ) -> Result<color_control::EnhancedColorModeEnum, Error> {
        if self.capabilities.borrow().contains(&Capability::Color)
            && self.color_temperature_range().is_some()
        {
            Err(ErrorCode::Failure.into())
        } else if self.capabilities.borrow().contains(&Capability::Color) {
            Ok(color_control::EnhancedColorModeEnum::CurrentXAndCurrentY)
        } else {
            Ok(color_control::EnhancedColorModeEnum::ColorTemperatureMireds)
        }
    }
    fn color_capabilities(
        &self,
        _ctx: impl ReadContext,
    ) -> Result<color_control::ColorCapabilitiesBitmap, Error> {
        let mut capabilities = color_control::ColorCapabilitiesBitmap::empty();
        if self.color_temperature_range().is_some() {
            capabilities |= color_control::ColorCapabilitiesBitmap::COLOR_TEMPERATURE;
        }
        if self.capabilities.borrow().contains(&Capability::Color) {
            capabilities |= color_control::ColorCapabilitiesBitmap::XY;
        }
        Ok(capabilities)
    }
    fn color_temp_physical_min_mireds(&self, _ctx: impl ReadContext) -> Result<u16, Error> {
        let range = self
            .color_temperature_range()
            .ok_or(ErrorCode::AttributeNotFound)?;
        Ok((1_000_000.0 / range.maximum).ceil() as u16)
    }
    fn color_temp_physical_max_mireds(&self, _ctx: impl ReadContext) -> Result<u16, Error> {
        let range = self
            .color_temperature_range()
            .ok_or(ErrorCode::AttributeNotFound)?;
        Ok((1_000_000.0 / range.minimum).floor() as u16)
    }
    fn couple_color_temp_to_level_min_mireds(&self, _ctx: impl ReadContext) -> Result<u16, Error> {
        self.mired_bounds()
            .map(|(minimum, _)| minimum)
            .ok_or(ErrorCode::AttributeNotFound.into())
    }
    fn start_up_color_temperature_mireds(
        &self,
        _ctx: impl ReadContext,
    ) -> Result<Nullable<u16>, Error> {
        Err(ErrorCode::AttributeNotFound.into())
    }
    fn set_options(
        &self,
        ctx: impl WriteContext,
        value: color_control::OptionsBitmap,
    ) -> Result<(), Error> {
        self.color_options.set(value);
        ctx.notify_changed();
        Ok(())
    }
    fn set_start_up_color_temperature_mireds(
        &self,
        _ctx: impl WriteContext,
        _value: Nullable<u16>,
    ) -> Result<(), Error> {
        Err(ErrorCode::UnsupportedAccess.into())
    }

    fn handle_move_to_hue(
        &self,
        _ctx: impl InvokeContext,
        _request: color_control::MoveToHueRequest<'_>,
    ) -> Result<(), Error> {
        Err(ErrorCode::InvalidCommand.into())
    }
    fn handle_move_hue(
        &self,
        _ctx: impl InvokeContext,
        _request: color_control::MoveHueRequest<'_>,
    ) -> Result<(), Error> {
        Err(ErrorCode::InvalidCommand.into())
    }
    fn handle_step_hue(
        &self,
        _ctx: impl InvokeContext,
        _request: color_control::StepHueRequest<'_>,
    ) -> Result<(), Error> {
        Err(ErrorCode::InvalidCommand.into())
    }
    fn handle_move_to_saturation(
        &self,
        _ctx: impl InvokeContext,
        _request: color_control::MoveToSaturationRequest<'_>,
    ) -> Result<(), Error> {
        Err(ErrorCode::InvalidCommand.into())
    }
    fn handle_move_saturation(
        &self,
        _ctx: impl InvokeContext,
        _request: color_control::MoveSaturationRequest<'_>,
    ) -> Result<(), Error> {
        Err(ErrorCode::InvalidCommand.into())
    }
    fn handle_step_saturation(
        &self,
        _ctx: impl InvokeContext,
        _request: color_control::StepSaturationRequest<'_>,
    ) -> Result<(), Error> {
        Err(ErrorCode::InvalidCommand.into())
    }
    fn handle_move_to_hue_and_saturation(
        &self,
        _ctx: impl InvokeContext,
        _request: color_control::MoveToHueAndSaturationRequest<'_>,
    ) -> Result<(), Error> {
        Err(ErrorCode::InvalidCommand.into())
    }
    fn handle_move_to_color(
        &self,
        _ctx: impl InvokeContext,
        _request: color_control::MoveToColorRequest<'_>,
    ) -> Result<(), Error> {
        Err(ErrorCode::Failure.into())
    }
    fn handle_move_color(
        &self,
        _ctx: impl InvokeContext,
        _request: color_control::MoveColorRequest<'_>,
    ) -> Result<(), Error> {
        Err(ErrorCode::Failure.into())
    }
    fn handle_step_color(
        &self,
        _ctx: impl InvokeContext,
        _request: color_control::StepColorRequest<'_>,
    ) -> Result<(), Error> {
        Err(ErrorCode::Failure.into())
    }
    fn handle_move_to_color_temperature(
        &self,
        _ctx: impl InvokeContext,
        _request: color_control::MoveToColorTemperatureRequest<'_>,
    ) -> Result<(), Error> {
        Err(ErrorCode::Failure.into())
    }
    fn handle_enhanced_move_to_hue(
        &self,
        _ctx: impl InvokeContext,
        _request: color_control::EnhancedMoveToHueRequest<'_>,
    ) -> Result<(), Error> {
        Err(ErrorCode::InvalidCommand.into())
    }
    fn handle_enhanced_move_hue(
        &self,
        _ctx: impl InvokeContext,
        _request: color_control::EnhancedMoveHueRequest<'_>,
    ) -> Result<(), Error> {
        Err(ErrorCode::InvalidCommand.into())
    }
    fn handle_enhanced_step_hue(
        &self,
        _ctx: impl InvokeContext,
        _request: color_control::EnhancedStepHueRequest<'_>,
    ) -> Result<(), Error> {
        Err(ErrorCode::InvalidCommand.into())
    }
    fn handle_enhanced_move_to_hue_and_saturation(
        &self,
        _ctx: impl InvokeContext,
        _request: color_control::EnhancedMoveToHueAndSaturationRequest<'_>,
    ) -> Result<(), Error> {
        Err(ErrorCode::InvalidCommand.into())
    }
    fn handle_color_loop_set(
        &self,
        _ctx: impl InvokeContext,
        _request: color_control::ColorLoopSetRequest<'_>,
    ) -> Result<(), Error> {
        Err(ErrorCode::InvalidCommand.into())
    }
    fn handle_stop_move_step(
        &self,
        _ctx: impl InvokeContext,
        _request: color_control::StopMoveStepRequest<'_>,
    ) -> Result<(), Error> {
        Err(ErrorCode::Failure.into())
    }
    fn handle_move_color_temperature(
        &self,
        _ctx: impl InvokeContext,
        _request: color_control::MoveColorTemperatureRequest<'_>,
    ) -> Result<(), Error> {
        Err(ErrorCode::Failure.into())
    }
    fn handle_step_color_temperature(
        &self,
        _ctx: impl InvokeContext,
        _request: color_control::StepColorTemperatureRequest<'_>,
    ) -> Result<(), Error> {
        Err(ErrorCode::Failure.into())
    }
}

pub(super) fn rgb_to_xy(color: RgbColor) -> (u16, u16) {
    let linear = |channel: u8| {
        let value = f64::from(channel) / 255.0;
        if value <= 0.04045 {
            value / 12.92
        } else {
            ((value + 0.055) / 1.055).powf(2.4)
        }
    };
    let red = linear(color.red);
    let green = linear(color.green);
    let blue = linear(color.blue);
    let x = 0.4124 * red + 0.3576 * green + 0.1805 * blue;
    let y = 0.2126 * red + 0.7152 * green + 0.0722 * blue;
    let z = 0.0193 * red + 0.1192 * green + 0.9505 * blue;
    let total = x + y + z;
    if total <= f64::EPSILON {
        return (0, 0);
    }
    (
        (x / total * f64::from(0xfeff_u16)).round() as u16,
        (y / total * f64::from(0xfeff_u16)).round() as u16,
    )
}

fn matter_level(value: Percent) -> Option<u8> {
    (value.get() > 0.0).then(|| (value.get() * 254.0 / 100.0).round().clamp(1.0, 254.0) as u8)
}

fn kelvin_to_mired(kelvin: u32) -> Option<u16> {
    (kelvin > 0)
        .then(|| (1_000_000_u32 + kelvin / 2) / kelvin)
        .and_then(|value| u16::try_from(value).ok())
}

fn expected_command_value(command: DeviceCommand) -> Option<(Property, PropertyValue)> {
    match command {
        DeviceCommand::SetPower(value) => Some((Property::Power, PropertyValue::Power(value))),
        DeviceCommand::SetBrightness(value) => {
            Some((Property::Brightness, PropertyValue::Percent(value)))
        }
        DeviceCommand::SetColorTemperature(value) => Some((
            Property::ColorTemperature,
            PropertyValue::ColorTemperature(value),
        )),
        DeviceCommand::SetColor(value) => Some((Property::Color, PropertyValue::Color(value))),
        _ => None,
    }
}

pub(super) struct SceneOnOff<'a>(pub(super) &'a LightingHandler);
pub(super) struct SceneLevel<'a>(pub(super) &'a LightingHandler);
pub(super) struct SceneColor<'a>(pub(super) &'a LightingHandler);

impl SceneClusterHandler for SceneOnOff<'_> {
    const CLUSTER_ID: u32 = ON_OFF_CLUSTER.id;

    fn endpoint_id(&self) -> u16 {
        self.0.endpoint
    }

    fn is_scenable_attribute(attribute_id: u32) -> bool {
        attribute_id == on_off::AttributeId::OnOff as u32
    }

    fn capture<P: TLVBuilderParent>(
        &self,
        values: AttributeValuePairStructArrayBuilder<P>,
    ) -> Result<AttributeValuePairStructArrayBuilder<P>, Error> {
        values.push_u8(
            on_off::AttributeId::OnOff as _,
            self.0.power().ok_or(ErrorCode::Failure)? as u8,
        )
    }

    async fn apply<C: HandlerContext>(
        &self,
        _ctx: &C,
        values: &TLVArray<'_, AttributeValuePairStruct<'_>>,
        _transition_time_ms: u32,
    ) -> Result<(), Error> {
        for value in values.iter() {
            let value = value?;
            if value.attribute_id()? == on_off::AttributeId::OnOff as u32
                && let Some(power) = value.value_unsigned_8()?
            {
                let power = match power {
                    0 => false,
                    1 => true,
                    _ => return Err(ErrorCode::ConstraintError.into()),
                };
                return self
                    .0
                    .scene_command(vec![DeviceCommand::SetPower(power)])
                    .await;
            }
        }
        Ok(())
    }
}

impl SceneClusterHandler for SceneLevel<'_> {
    const CLUSTER_ID: u32 = LEVEL_CLUSTER.id;

    fn endpoint_id(&self) -> u16 {
        self.0.endpoint
    }

    fn is_scenable_attribute(attribute_id: u32) -> bool {
        attribute_id == level_control::AttributeId::CurrentLevel as u32
    }

    fn capture<P: TLVBuilderParent>(
        &self,
        values: AttributeValuePairStructArrayBuilder<P>,
    ) -> Result<AttributeValuePairStructArrayBuilder<P>, Error> {
        values.push_u8(
            level_control::AttributeId::CurrentLevel as _,
            self.0.current_level_value().ok_or(ErrorCode::Failure)?,
        )
    }

    async fn apply<C: HandlerContext>(
        &self,
        _ctx: &C,
        values: &TLVArray<'_, AttributeValuePairStruct<'_>>,
        transition_time_ms: u32,
    ) -> Result<(), Error> {
        for value in values.iter() {
            let value = value?;
            if value.attribute_id()? == level_control::AttributeId::CurrentLevel as u32
                && let Some(level) = value.value_unsigned_8()?
            {
                if level == u8::MAX {
                    return Err(ErrorCode::ConstraintError.into());
                }
                if transition_time_ms >= 100 {
                    let intent = self
                        .0
                        .scene_intent
                        .borrow()
                        .as_ref()
                        .cloned()
                        .ok_or(ErrorCode::Failure)?;
                    if let Some(scene) = self.0.scene_confirmation.borrow_mut().as_mut() {
                        for command in self.0.commands_for_level(level.max(1), false)? {
                            if let Some((property, value)) = expected_command_value(command) {
                                scene.expected.insert(property, value);
                            }
                        }
                    }
                    return self.0.add_scene_adjustment(
                        intent,
                        transition_time_ms / 100,
                        AdjustmentKind::Brightness {
                            start: self.0.current_level_value().ok_or(ErrorCode::Failure)?,
                            target: level.max(1),
                            with_on_off: false,
                        },
                    );
                }
                return self
                    .0
                    .scene_command(self.0.commands_for_level(level.max(1), false)?)
                    .await;
            }
        }
        Ok(())
    }
}

impl SceneClusterHandler for SceneColor<'_> {
    const CLUSTER_ID: u32 = color_control::FULL_CLUSTER.id;

    fn endpoint_id(&self) -> u16 {
        self.0.endpoint
    }

    fn is_scenable_attribute(attribute_id: u32) -> bool {
        matches!(
            attribute_id,
            id if id == color_control::AttributeId::CurrentX as u32
                || id == color_control::AttributeId::CurrentY as u32
                || id == color_control::AttributeId::ColorTemperatureMireds as u32
                || id == color_control::AttributeId::EnhancedColorMode as u32
        )
    }

    fn capture<P: TLVBuilderParent>(
        &self,
        values: AttributeValuePairStructArrayBuilder<P>,
    ) -> Result<AttributeValuePairStructArrayBuilder<P>, Error> {
        if self.0.capabilities.borrow().contains(&Capability::Color)
            && self.0.color_temperature_range().is_some()
        {
            return Err(ErrorCode::Failure.into());
        }
        if self.0.capabilities.borrow().contains(&Capability::Color) {
            let (x, y) = rgb_to_xy(self.0.color().ok_or(ErrorCode::Failure)?);
            let values = values.push_u16(color_control::AttributeId::CurrentX as _, x)?;
            let values = values.push_u16(color_control::AttributeId::CurrentY as _, y)?;
            return values.push_u8(
                color_control::AttributeId::EnhancedColorMode as _,
                color_control::EnhancedColorModeEnum::CurrentXAndCurrentY as u8,
            );
        }
        let values = values.push_u16(
            color_control::AttributeId::ColorTemperatureMireds as _,
            self.0.current_mireds().ok_or(ErrorCode::Failure)?,
        )?;
        values.push_u8(
            color_control::AttributeId::EnhancedColorMode as _,
            color_control::EnhancedColorModeEnum::ColorTemperatureMireds as u8,
        )
    }

    async fn apply<C: HandlerContext>(
        &self,
        _ctx: &C,
        values: &TLVArray<'_, AttributeValuePairStruct<'_>>,
        transition_time_ms: u32,
    ) -> Result<(), Error> {
        let mut x = None;
        let mut y = None;
        for value in values.iter() {
            let value = value?;
            if value.attribute_id()? == color_control::AttributeId::CurrentX as u32 {
                x = value.value_unsigned_16()?;
            }
            if value.attribute_id()? == color_control::AttributeId::CurrentY as u32 {
                y = value.value_unsigned_16()?;
            }
            if value.attribute_id()? == color_control::AttributeId::ColorTemperatureMireds as u32
                && let Some(mireds) = value.value_unsigned_16()?
            {
                if mireds == 0 {
                    return Err(ErrorCode::ConstraintError.into());
                }
                let command =
                    DeviceCommand::SetColorTemperature(self.0.quantized_kelvin_for_mired(mireds)?);
                if transition_time_ms >= 100 {
                    if let Some(scene) = self.0.scene_confirmation.borrow_mut().as_mut()
                        && let Some((property, value)) = expected_command_value(command)
                    {
                        scene.expected.insert(property, value);
                    }
                    let intent = self
                        .0
                        .scene_intent
                        .borrow()
                        .as_ref()
                        .cloned()
                        .ok_or(ErrorCode::Failure)?;
                    return self.0.add_scene_adjustment(
                        intent,
                        transition_time_ms / 100,
                        AdjustmentKind::ColorTemperature {
                            start_mireds: self.0.current_mireds().ok_or(ErrorCode::Failure)?,
                            target_mireds: mireds,
                        },
                    );
                }
                return self.0.scene_command(vec![command]).await;
            }
        }
        if let (Some(x), Some(y)) = (x, y) {
            let (red, green, blue) = SetDeviceColor::Xy { x, y }.to_rgb(RgbGamma::SRgb);
            let command = DeviceCommand::SetColor(RgbColor { red, green, blue });
            if transition_time_ms >= 100 {
                if let Some(scene) = self.0.scene_confirmation.borrow_mut().as_mut()
                    && let Some((property, value)) = expected_command_value(command)
                {
                    scene.expected.insert(property, value);
                }
                let intent = self
                    .0
                    .scene_intent
                    .borrow()
                    .as_ref()
                    .cloned()
                    .ok_or(ErrorCode::Failure)?;
                let (start_x, start_y) = self.0.color().map(rgb_to_xy).ok_or(ErrorCode::Failure)?;
                return self.0.add_scene_adjustment(
                    intent,
                    transition_time_ms / 100,
                    AdjustmentKind::Xy {
                        start_x,
                        start_y,
                        target_x: x,
                        target_y: y,
                    },
                );
            }
            return self.0.scene_command(vec![command]).await;
        }
        Ok(())
    }
}
