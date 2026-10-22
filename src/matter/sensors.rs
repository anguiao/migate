//! Read-only Matter measurement clusters for temperature, humidity and the
//! optional shared physical battery.
//!
//! Projection, range validation and report ownership live here. The source
//! state is already validated at the Xiaomi boundary; these handlers never
//! perform writes or fabricate optional capabilities.

use crate::{
    device::{BatterySnapshot, Function, PhysicalDevice},
    matter::units,
};
use rs_matter::{
    dm::{
        ArrayAttributeRead, Cluster, Dataver, HandlerContext, ReadContext,
        clusters::decl::{
            power_source, relative_humidity_measurement as humidity,
            temperature_measurement as temperature,
        },
    },
    error::{Error, ErrorCode},
    im::EndptId,
    tlv::{Nullable, TLVBuilderParent, ToTLVArrayBuilder, ToTLVBuilder, Utf8StrBuilder},
    with,
};
use std::cell::Cell;

pub(super) struct TemperatureHandler {
    function: Function,
    endpoint: u16,
    dataver: Dataver,
    min: i16,
    max: i16,
    versioned: Cell<Option<i16>>,
    observed: Cell<Option<i16>>,
}
impl TemperatureHandler {
    pub(super) fn new(function: Function, endpoint: u16, dataver: Dataver) -> Result<Self, Error> {
        let range = function
            .description()
            .measurement_range()
            .ok_or(ErrorCode::InvalidState)?;
        let (min, max) = units::temperature_matter_range(range).ok_or(ErrorCode::InvalidState)?;
        let versioned = project(&function);
        Ok(Self {
            function,
            endpoint,
            dataver,
            min,
            max,
            versioned: Cell::new(versioned),
            observed: Cell::new(versioned),
        })
    }
    fn sync_version(&self) -> Option<i16> {
        let current = project(&self.function);
        if self.versioned.replace(current) != current {
            self.dataver.changed();
        }
        current
    }
}
fn project(function: &Function) -> Option<i16> {
    function
        .snapshot()
        .measurement()?
        .value
        .and_then(units::temperature_matter_value)
}

impl temperature::ClusterHandler for TemperatureHandler {
    const CLUSTER: Cluster<'static> = temperature::FULL_CLUSTER
        .with_features(0)
        .with_attrs(|a, _, _| matches!(a.id, 0..=2 | 0xfff8..=0xfffd))
        .with_cmds(with!());

    fn dataver(&self) -> u32 {
        self.sync_version();
        self.dataver.get()
    }
    fn dataver_changed(&self) {
        self.dataver.changed();
    }
    fn measured_value(&self, _ctx: impl ReadContext) -> Result<Nullable<i16>, Error> {
        Ok(Nullable::new(self.sync_version()))
    }
    fn min_measured_value(&self, _ctx: impl ReadContext) -> Result<Nullable<i16>, Error> {
        Ok(Nullable::new(Some(self.min)))
    }
    fn max_measured_value(&self, _ctx: impl ReadContext) -> Result<Nullable<i16>, Error> {
        Ok(Nullable::new(Some(self.max)))
    }
    async fn run(&self, ctx: impl HandlerContext) -> Result<(), Error> {
        let mut changes = self.function.subscribe();
        let mut observed = self.observed.get();
        loop {
            let current = self.sync_version();
            if observed != current {
                observed = current;
                self.observed.set(current);
                self.dataver.changed();
                ctx.notify_attr_changed(
                    self.endpoint,
                    Self::CLUSTER.id,
                    temperature::AttributeId::MeasuredValue as _,
                );
            }
            changes.changed().await;
        }
    }
}

pub(super) struct HumidityHandler {
    function: Function,
    endpoint: u16,
    dataver: Dataver,
    min: u16,
    max: u16,
    versioned: Cell<Option<u16>>,
    observed: Cell<Option<u16>>,
}
impl HumidityHandler {
    pub(super) fn new(function: Function, endpoint: u16, dataver: Dataver) -> Result<Self, Error> {
        let range = function
            .description()
            .measurement_range()
            .ok_or(ErrorCode::InvalidState)?;
        let (min, max) = units::humidity_matter_range(range).ok_or(ErrorCode::InvalidState)?;
        let versioned = project_humidity(&function);
        Ok(Self {
            function,
            endpoint,
            dataver,
            min,
            max,
            versioned: Cell::new(versioned),
            observed: Cell::new(versioned),
        })
    }
    fn sync_version(&self) -> Option<u16> {
        let current = project_humidity(&self.function);
        if self.versioned.replace(current) != current {
            self.dataver.changed();
        }
        current
    }
}
fn project_humidity(function: &Function) -> Option<u16> {
    function
        .snapshot()
        .measurement()?
        .value
        .and_then(units::humidity_matter_value)
}

impl humidity::ClusterHandler for HumidityHandler {
    const CLUSTER: Cluster<'static> = humidity::FULL_CLUSTER
        .with_features(0)
        .with_attrs(|a, _, _| matches!(a.id, 0..=2 | 0xfff8..=0xfffd))
        .with_cmds(with!());

    fn dataver(&self) -> u32 {
        self.sync_version();
        self.dataver.get()
    }
    fn dataver_changed(&self) {
        self.dataver.changed();
    }
    fn measured_value(&self, _ctx: impl ReadContext) -> Result<Nullable<u16>, Error> {
        Ok(Nullable::new(self.sync_version()))
    }
    fn min_measured_value(&self, _ctx: impl ReadContext) -> Result<Nullable<u16>, Error> {
        Ok(Nullable::new(Some(self.min)))
    }
    fn max_measured_value(&self, _ctx: impl ReadContext) -> Result<Nullable<u16>, Error> {
        Ok(Nullable::new(Some(self.max)))
    }
    async fn run(&self, ctx: impl HandlerContext) -> Result<(), Error> {
        let mut changes = self.function.subscribe();
        let mut observed = self.observed.get();
        loop {
            let current = self.sync_version();
            if observed != current {
                observed = current;
                self.observed.set(current);
                self.dataver.changed();
                ctx.notify_attr_changed(
                    self.endpoint,
                    Self::CLUSTER.id,
                    humidity::AttributeId::MeasuredValue as _,
                );
            }
            changes.changed().await;
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct BatteryProjection {
    percent: Option<u8>,
    charge: Option<power_source::BatChargeLevelEnum>,
    replacement: Option<bool>,
}
impl BatteryProjection {
    const UNKNOWN: Self = Self {
        percent: None,
        charge: None,
        replacement: None,
    };
    fn project(snapshot: BatterySnapshot) -> Self {
        let Some(percent) = snapshot.value else {
            return Self::UNKNOWN;
        };
        let Some(doubled) = units::battery_percent_matter_value(percent) else {
            return Self::UNKNOWN;
        };
        let (charge, replacement) = if percent > 10.0 {
            (power_source::BatChargeLevelEnum::OK, false)
        } else if percent > 5.0 {
            (power_source::BatChargeLevelEnum::Warning, false)
        } else {
            (power_source::BatChargeLevelEnum::Critical, true)
        };
        Self {
            percent: Some(doubled),
            charge: Some(charge),
            replacement: Some(replacement),
        }
    }
}

pub(super) struct BatteryHandler {
    physical: PhysicalDevice,
    dataver: Dataver,
    endpoint_list: Vec<u16>,
    versioned: std::cell::Cell<BatteryProjection>,
    observed: std::cell::Cell<BatteryProjection>,
}
impl BatteryHandler {
    pub(super) fn new(
        physical: PhysicalDevice,
        endpoint: u16,
        endpoint_list: Vec<u16>,
        dataver: Dataver,
    ) -> Result<Self, Error> {
        let snapshot = physical.battery_snapshot().ok_or(ErrorCode::InvalidState)?;
        let projection = BatteryProjection::project(snapshot);
        Ok(Self {
            physical,
            dataver,
            endpoint_list: std::iter::once(endpoint).chain(endpoint_list).collect(),
            versioned: std::cell::Cell::new(projection),
            observed: std::cell::Cell::new(projection),
        })
    }
    fn sync_version(&self) -> BatteryProjection {
        let current = self
            .physical
            .battery_snapshot()
            .map_or(BatteryProjection::UNKNOWN, BatteryProjection::project);
        if self.versioned.replace(current) != current {
            self.dataver.changed();
        }
        current
    }
}
impl power_source::ClusterHandler for BatteryHandler {
    const CLUSTER: Cluster<'static> = power_source::FULL_CLUSTER
        .with_features(power_source::Feature::BATTERY.bits())
        .with_attrs(|attribute, _, _| {
            matches!(
                attribute.id,
                0..=2 | 12 | 14..=16 | 31 | 0xfff8..=0xfffd
            )
        })
        .with_cmds(with!())
        .with_events(with!());

    fn dataver(&self) -> u32 {
        self.sync_version();
        self.dataver.get()
    }
    fn dataver_changed(&self) {
        self.dataver.changed();
    }
    fn status(&self, _ctx: impl ReadContext) -> Result<power_source::PowerSourceStatusEnum, Error> {
        Ok(power_source::PowerSourceStatusEnum::Unspecified)
    }
    fn order(&self, _ctx: impl ReadContext) -> Result<u8, Error> {
        Ok(0)
    }
    fn description<P: TLVBuilderParent>(
        &self,
        _ctx: impl ReadContext,
        out: Utf8StrBuilder<P>,
    ) -> Result<P, Error> {
        out.set("Battery")
    }
    fn bat_percent_remaining(&self, _ctx: impl ReadContext) -> Result<Nullable<u8>, Error> {
        Ok(Nullable::new(self.sync_version().percent))
    }
    fn bat_charge_level(
        &self,
        _ctx: impl ReadContext,
    ) -> Result<power_source::BatChargeLevelEnum, Error> {
        self.sync_version()
            .charge
            .ok_or_else(|| ErrorCode::Failure.into())
    }
    fn bat_replacement_needed(&self, _ctx: impl ReadContext) -> Result<bool, Error> {
        self.sync_version()
            .replacement
            .ok_or_else(|| ErrorCode::Failure.into())
    }
    fn bat_replaceability(
        &self,
        _ctx: impl ReadContext,
    ) -> Result<power_source::BatReplaceabilityEnum, Error> {
        Ok(power_source::BatReplaceabilityEnum::Unspecified)
    }
    fn endpoint_list<P: TLVBuilderParent>(
        &self,
        _ctx: impl ReadContext,
        builder: ArrayAttributeRead<ToTLVArrayBuilder<P, EndptId>, ToTLVBuilder<P, EndptId>>,
    ) -> Result<P, Error> {
        match builder {
            ArrayAttributeRead::ReadAll(mut builder) => {
                for endpoint in &self.endpoint_list {
                    builder = builder.push(endpoint)?;
                }
                builder.end()
            }
            ArrayAttributeRead::ReadOne(index, builder) => {
                let Some(endpoint) = self.endpoint_list.get(index as usize) else {
                    return Err(ErrorCode::ConstraintError.into());
                };
                builder.set(endpoint)
            }
            ArrayAttributeRead::ReadNone(builder) => builder.end(),
        }
    }
    async fn run(&self, ctx: impl HandlerContext) -> Result<(), Error> {
        let mut changes = self.physical.subscribe_battery();
        let mut observed = self.observed.get();
        loop {
            let current = self.sync_version();
            if observed != current {
                if observed.percent != current.percent {
                    ctx.notify_attr_changed(
                        self.physical_endpoint(),
                        Self::CLUSTER.id,
                        power_source::AttributeId::BatPercentRemaining as _,
                    );
                }
                if observed.charge != current.charge {
                    ctx.notify_attr_changed(
                        self.physical_endpoint(),
                        Self::CLUSTER.id,
                        power_source::AttributeId::BatChargeLevel as _,
                    );
                }
                if observed.replacement != current.replacement {
                    ctx.notify_attr_changed(
                        self.physical_endpoint(),
                        Self::CLUSTER.id,
                        power_source::AttributeId::BatReplacementNeeded as _,
                    );
                }
                observed = current;
                self.observed.set(current);
            }
            changes.changed().await;
        }
    }
}
impl BatteryHandler {
    fn physical_endpoint(&self) -> u16 {
        self.endpoint_list[0]
    }
}
