mod color;
mod level;
mod power;
mod scenes;
mod transition;

#[cfg(test)]
pub(super) use color::rgb_to_xy;

pub(super) use color::color_cluster;
pub(super) use level::LEVEL_CLUSTER;
use level::matter_level;
pub(super) use power::ON_OFF_CLUSTER;
pub(super) use scenes::{GROUPS_CLUSTER, SCENES_CLUSTER, SceneColor, SceneLevel, SceneOnOff};

use crate::device::{
    Capability, CommandIntent, CommandOutcome, DeviceCommand, DeviceService, FeatureIdentity,
    Property, PropertyValue,
};
use event_listener::Event;
use rs_matter::{
    dm::{
        Dataver,
        clusters::decl::{color_control, level_control},
    },
    error::{Error, ErrorCode},
    tlv::Nullable,
};
use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    num::NonZeroU8,
    time::{Duration, Instant},
};

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

    pub(super) fn dataver_for(&self, cluster: u32) -> Option<&Dataver> {
        match cluster {
            id if id == ON_OFF_CLUSTER.id => Some(&self.on_off_dataver),
            id if id == LEVEL_CLUSTER.id => Some(&self.level_dataver),
            id if id == color_control::FULL_CLUSTER.id => Some(&self.color_dataver),
            _ => None,
        }
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
}
