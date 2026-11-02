use super::{Command, command};
use crate::device::{AirConditionerFanSpeed as Speed, Function};
use rs_matter::{
    dm::{
        Cluster, Dataver, HandlerContext, InvokeContext, ReadContext, WriteContext,
        clusters::decl::fan_control as fc,
    },
    error::{Error, ErrorCode},
    tlv::Nullable,
    with,
};
use std::cell::Cell;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct FanProjection {
    power: Option<bool>,
    speed: Option<Speed>,
    swing: Option<bool>,
}
impl FanProjection {
    fn read(function: &Function) -> Self {
        let value = function.snapshot().air_conditioner().unwrap();
        Self {
            power: value.power.value,
            speed: value.speed.value,
            swing: value.swing.value,
        }
    }
    fn mode(self) -> Option<fc::FanModeEnum> {
        if !self.power? {
            return Some(fc::FanModeEnum::Off);
        }
        Some(match self.speed? {
            Speed::Auto => fc::FanModeEnum::Auto,
            Speed::Low => fc::FanModeEnum::Low,
            Speed::Medium => fc::FanModeEnum::Medium,
            Speed::High => fc::FanModeEnum::High,
        })
    }
    fn setting(self) -> Result<Option<u8>, Error> {
        if !self.power.ok_or(ErrorCode::Failure)? {
            return Ok(Some(0));
        }
        Ok(match self.speed.ok_or(ErrorCode::Failure)? {
            Speed::Auto => None,
            Speed::Low => Some(1),
            Speed::Medium => Some(2),
            Speed::High => Some(3),
        })
    }
    fn current(self) -> Result<u8, Error> {
        // Only known Auto has no setting; 1 is a display placeholder, not a measurement.
        Ok(self.setting()?.unwrap_or(1))
    }
}
pub(in crate::matter) struct AirConditionerFan<const ROCKING: bool> {
    function: Function,
    endpoint: u16,
    dataver: Dataver,
    versioned: Cell<FanProjection>,
    reported: Cell<FanProjection>,
}
impl<const ROCKING: bool> AirConditionerFan<ROCKING> {
    pub(super) fn new(function: Function, endpoint: u16, dataver: Dataver) -> Self {
        let value = FanProjection::read(&function);
        Self {
            function,
            endpoint,
            dataver,
            versioned: Cell::new(value),
            reported: Cell::new(value),
        }
    }
    pub(super) fn sync(&self) -> FanProjection {
        let value = FanProjection::read(&self.function);
        let old = self.versioned.replace(value);
        if old.mode() != value.mode()
            || old.setting().ok() != value.setting().ok()
            || ROCKING && old.swing != value.swing
        {
            self.dataver.changed();
        }
        value
    }
    async fn set_speed(&self, value: Nullable<u8>, percent: bool) -> Result<(), Error> {
        let Some(value) = value.into_option() else {
            return Ok(());
        };
        if value > if percent { 100 } else { 3 } {
            return Err(ErrorCode::ConstraintError.into());
        }
        let speed = if percent {
            (u16::from(value) * 3).div_ceil(100) as u8
        } else {
            value
        };
        command(
            &self.function,
            match speed {
                0 => Command::Power(false),
                1 => Command::Speed(Speed::Low),
                2 => Command::Speed(Speed::Medium),
                3 => Command::Speed(Speed::High),
                _ => unreachable!(),
            },
        )
        .await
    }
}
impl<const ROCKING: bool> fc::ClusterAsyncHandler for AirConditionerFan<ROCKING> {
    const CLUSTER: Cluster<'static> = fc::FULL_CLUSTER
        .with_revision(6)
        .with_features(
            fc::Feature::MULTI_SPEED.bits()
                | fc::Feature::AUTO.bits()
                | if ROCKING {
                    fc::Feature::ROCKING.bits()
                } else {
                    0
                },
        )
        .with_attrs(|a, _, _| {
            matches!(a.id, 0..=6 | 0xfff8..=0xfffd) || ROCKING && matches!(a.id, 7 | 8)
        })
        .with_cmds(with!())
        .with_events(with!());
    fn dataver(&self) -> u32 {
        self.sync();
        self.dataver.get()
    }
    fn dataver_changed(&self) {
        self.dataver.changed();
    }
    async fn fan_mode(&self, _: impl ReadContext) -> Result<fc::FanModeEnum, Error> {
        self.sync().mode().ok_or_else(|| ErrorCode::Failure.into())
    }
    async fn fan_mode_sequence(
        &self,
        _: impl ReadContext,
    ) -> Result<fc::FanModeSequenceEnum, Error> {
        Ok(fc::FanModeSequenceEnum::OffLowMedHighAuto)
    }
    async fn percent_setting(&self, _: impl ReadContext) -> Result<Nullable<u8>, Error> {
        Ok(Nullable::new(
            self.sync()
                .setting()?
                .map(|speed| (u16::from(speed) * 100 / 3) as u8),
        ))
    }
    async fn percent_current(&self, _: impl ReadContext) -> Result<u8, Error> {
        Ok((u16::from(self.sync().current()?) * 100 / 3) as u8)
    }
    async fn speed_max(&self, _: impl ReadContext) -> Result<u8, Error> {
        Ok(3)
    }
    async fn speed_setting(&self, _: impl ReadContext) -> Result<Nullable<u8>, Error> {
        Ok(Nullable::new(self.sync().setting()?))
    }
    async fn speed_current(&self, _: impl ReadContext) -> Result<u8, Error> {
        self.sync().current()
    }
    async fn rock_support(&self, _: impl ReadContext) -> Result<fc::RockBitmap, Error> {
        Ok(fc::RockBitmap::ROCK_UP_DOWN)
    }
    async fn rock_setting(&self, _: impl ReadContext) -> Result<fc::RockBitmap, Error> {
        match self.sync().swing {
            Some(true) => Ok(fc::RockBitmap::ROCK_UP_DOWN),
            Some(false) => Ok(fc::RockBitmap::empty()),
            None => Err(ErrorCode::Failure.into()),
        }
    }
    async fn set_fan_mode(
        &self,
        _: impl WriteContext,
        value: fc::FanModeEnum,
    ) -> Result<(), Error> {
        command(
            &self.function,
            match value {
                fc::FanModeEnum::Off => Command::Power(false),
                fc::FanModeEnum::Low => Command::Speed(Speed::Low),
                fc::FanModeEnum::Medium => Command::Speed(Speed::Medium),
                fc::FanModeEnum::High | fc::FanModeEnum::On => Command::Speed(Speed::High),
                fc::FanModeEnum::Auto | fc::FanModeEnum::Smart => Command::Speed(Speed::Auto),
            },
        )
        .await
    }
    async fn set_percent_setting(
        &self,
        _: impl WriteContext,
        value: Nullable<u8>,
    ) -> Result<(), Error> {
        self.set_speed(value, true).await
    }
    async fn set_speed_setting(
        &self,
        _: impl WriteContext,
        value: Nullable<u8>,
    ) -> Result<(), Error> {
        self.set_speed(value, false).await
    }
    async fn set_rock_setting(
        &self,
        _: impl WriteContext,
        value: fc::RockBitmap,
    ) -> Result<(), Error> {
        match value.bits() {
            0 => command(&self.function, Command::Swing(false)).await,
            2 => command(&self.function, Command::Swing(true)).await,
            _ => Err(ErrorCode::ConstraintError.into()),
        }
    }
    async fn handle_step(
        &self,
        _: impl InvokeContext,
        _: fc::StepRequest<'_>,
    ) -> Result<(), Error> {
        Err(ErrorCode::CommandNotFound.into())
    }
    async fn run(&self, ctx: impl HandlerContext) -> Result<(), Error> {
        let mut changes = self.function.subscribe();
        loop {
            let value = self.sync();
            let previous = self.reported.replace(value);
            if previous.mode() != value.mode() {
                ctx.notify_attr_changed(self.endpoint, 0x202, 0);
            }
            if previous.setting().ok() != value.setting().ok() {
                for id in [2, 3, 5, 6] {
                    ctx.notify_attr_changed(self.endpoint, 0x202, id);
                }
            }
            if ROCKING && previous.swing != value.swing {
                ctx.notify_attr_changed(self.endpoint, 0x202, 8);
            }
            changes.changed().await;
        }
    }
}
