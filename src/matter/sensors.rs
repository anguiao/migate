//! Read-only Matter measurement clusters for temperature and humidity.
//!
//! Projection, range validation and report ownership live here. The source
//! state is already validated at the Xiaomi boundary; these handlers never
//! perform writes or fabricate optional Tolerance capabilities.

use crate::{device::Function, matter::units};
use rs_matter::{
    dm::{
        Cluster, Dataver, HandlerContext, ReadContext,
        clusters::decl::{
            relative_humidity_measurement as humidity, temperature_measurement as temperature,
        },
    },
    error::{Error, ErrorCode},
    tlv::Nullable,
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
