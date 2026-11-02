//! Read-only Matter measurement clusters for temperature, humidity, CO₂,
//! illuminance and the optional shared physical battery.
//!
//! Projection, range validation and report ownership live here. The source
//! state is already validated at the Xiaomi boundary; these handlers never
//! perform writes or fabricate optional capabilities.

mod battery;

pub(super) use battery::BatteryHandler;

use crate::{
    device::{AmbientTemperature, Function, MeasurementRange},
    matter::units,
};
use rs_matter::{
    dm::{
        Async, AsyncHandler, Cluster, Dataver, EmptyHandler, HandlerContext, InvokeContext,
        InvokeReply, MatchContext, ReadContext, ReadReply, WriteContext,
        clusters::decl::{
            air_quality, carbon_dioxide_concentration_measurement as co2,
            globals::{MeasurementMediumEnum, MeasurementUnitEnum},
            illuminance_measurement as illuminance, relative_humidity_measurement as humidity,
            temperature_measurement as temperature,
        },
    },
    error::{Error, ErrorCode},
    tlv::Nullable,
    with,
};
use std::cell::Cell;

pub(super) type TemperatureHandler = MeasurementHandler<i16>;
pub(super) type HumidityHandler = MeasurementHandler<u16>;
pub(super) type IlluminanceHandler = MeasurementHandler<u16>;
pub(super) type Co2Handler = MeasurementHandler<f32>;

pub(crate) fn measurement_mapping_issue(function: &Function) -> Option<&'static str> {
    let snapshot = function.snapshot();
    if let Some(AmbientTemperature::Available(reading)) = snapshot.ambient_temperature() {
        return units::temperature_matter_value(
            reading
                .value
                .expect("available ambient temperature has a value"),
        )
        .is_none()
        .then_some("native ambient temperature cannot be represented");
    }
    let value = snapshot.measurement()?.value;
    let (range_supported, value_supported) = match function.description().kind {
        crate::device::FunctionKind::Temperature(range) => (
            units::temperature_matter_bounds(range).is_some(),
            value.is_none_or(|value| units::temperature_matter_value(value).is_some()),
        ),
        crate::device::FunctionKind::Humidity(range) => (
            units::humidity_matter_bounds(range).is_some(),
            value.is_none_or(|value| units::humidity_matter_value(value).is_some()),
        ),
        crate::device::FunctionKind::Illuminance(range) => (
            units::illuminance_matter_bounds(range).is_some(),
            value.is_none_or(|value| units::illuminance_matter_value(value).is_some()),
        ),
        crate::device::FunctionKind::CarbonDioxide(range) => (
            units::co2_matter_bounds(range).is_some(),
            value.is_none_or(|value| units::co2_matter_value(value).is_some()),
        ),
        _ => return None,
    };
    if !range_supported {
        Some("native range cannot be represented")
    } else if !value_supported {
        Some("native reading cannot be represented")
    } else {
        None
    }
}

pub(super) struct MeasurementHandler<T> {
    function: Function,
    endpoint: u16,
    dataver: Dataver,
    bounds: Option<(T, T)>,
    project: fn(f64) -> Option<T>,
    versioned: Cell<Option<T>>,
    observed: Cell<Option<T>>,
}
impl<T: Copy + PartialEq> MeasurementHandler<T> {
    fn with_projection(
        function: Function,
        endpoint: u16,
        dataver: Dataver,
        bounds: Option<(T, T)>,
        project: fn(f64) -> Option<T>,
    ) -> Self {
        let versioned = bounds.and_then(|_| {
            function
                .snapshot()
                .measurement()
                .and_then(|measurement| measurement.value)
                .and_then(project)
        });
        Self {
            function,
            endpoint,
            dataver,
            bounds,
            project,
            versioned: Cell::new(versioned),
            observed: Cell::new(versioned),
        }
    }
    fn sync_version(&self) -> Option<T> {
        let current = self.bounds.and_then(|_| {
            self.function
                .snapshot()
                .measurement()
                .and_then(|measurement| measurement.value)
                .and_then(self.project)
        });
        if self.versioned.replace(current) != current {
            self.dataver.changed();
        }
        current
    }
    fn measured_value(&self) -> Result<Nullable<T>, Error> {
        if self.bounds.is_none() {
            return Err(ErrorCode::ConstraintError.into());
        }
        let value = self.sync_version();
        if value.is_none()
            && self
                .function
                .snapshot()
                .measurement()
                .unwrap()
                .value
                .is_some()
        {
            return Err(ErrorCode::ConstraintError.into());
        }
        Ok(Nullable::new(value))
    }
    fn min_measured_value(&self) -> Result<Nullable<T>, Error> {
        Ok(Nullable::new(Some(
            self.bounds.ok_or(ErrorCode::ConstraintError)?.0,
        )))
    }
    fn max_measured_value(&self) -> Result<Nullable<T>, Error> {
        Ok(Nullable::new(Some(
            self.bounds.ok_or(ErrorCode::ConstraintError)?.1,
        )))
    }
    async fn run_measurement(
        &self,
        ctx: impl HandlerContext,
        cluster: u32,
        attribute: u32,
    ) -> Result<(), Error> {
        let mut changes = self.function.subscribe();
        let mut observed = self.observed.get();
        loop {
            let current = self.sync_version();
            if observed != current {
                observed = current;
                self.observed.set(current);
                self.dataver.changed();
                ctx.notify_attr_changed(self.endpoint, cluster, attribute);
            }
            changes.changed().await;
        }
    }
}

impl TemperatureHandler {
    pub(super) fn new(
        function: Function,
        range: MeasurementRange,
        endpoint: u16,
        dataver: Dataver,
    ) -> Self {
        Self::with_projection(
            function,
            endpoint,
            dataver,
            units::temperature_matter_bounds(range),
            units::temperature_matter_value,
        )
    }
}
impl HumidityHandler {
    pub(super) fn new(
        function: Function,
        range: MeasurementRange,
        endpoint: u16,
        dataver: Dataver,
    ) -> Self {
        Self::with_projection(
            function,
            endpoint,
            dataver,
            units::humidity_matter_bounds(range),
            units::humidity_matter_value,
        )
    }
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
        self.measured_value()
    }
    fn min_measured_value(&self, _ctx: impl ReadContext) -> Result<Nullable<i16>, Error> {
        self.min_measured_value()
    }
    fn max_measured_value(&self, _ctx: impl ReadContext) -> Result<Nullable<i16>, Error> {
        self.max_measured_value()
    }
    async fn run(&self, ctx: impl HandlerContext) -> Result<(), Error> {
        self.run_measurement(
            ctx,
            Self::CLUSTER.id,
            temperature::AttributeId::MeasuredValue as _,
        )
        .await
    }
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
        self.measured_value()
    }
    fn min_measured_value(&self, _ctx: impl ReadContext) -> Result<Nullable<u16>, Error> {
        self.min_measured_value()
    }
    fn max_measured_value(&self, _ctx: impl ReadContext) -> Result<Nullable<u16>, Error> {
        self.max_measured_value()
    }
    async fn run(&self, ctx: impl HandlerContext) -> Result<(), Error> {
        self.run_measurement(
            ctx,
            Self::CLUSTER.id,
            humidity::AttributeId::MeasuredValue as _,
        )
        .await
    }
}

impl IlluminanceHandler {
    pub(super) fn new_illuminance(
        function: Function,
        range: MeasurementRange,
        endpoint: u16,
        dataver: Dataver,
    ) -> Self {
        Self::with_projection(
            function,
            endpoint,
            dataver,
            units::illuminance_matter_bounds(range),
            units::illuminance_matter_value,
        )
    }
}

impl illuminance::ClusterHandler for IlluminanceHandler {
    const CLUSTER: Cluster<'static> = illuminance::FULL_CLUSTER
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
        self.measured_value()
    }
    fn min_measured_value(&self, _ctx: impl ReadContext) -> Result<Nullable<u16>, Error> {
        self.min_measured_value()
    }
    fn max_measured_value(&self, _ctx: impl ReadContext) -> Result<Nullable<u16>, Error> {
        self.max_measured_value()
    }
    async fn run(&self, ctx: impl HandlerContext) -> Result<(), Error> {
        self.run_measurement(
            ctx,
            Self::CLUSTER.id,
            illuminance::AttributeId::MeasuredValue as _,
        )
        .await
    }
}

impl Co2Handler {
    pub(super) fn new(
        function: Function,
        range: MeasurementRange,
        endpoint: u16,
        dataver: Dataver,
    ) -> Self {
        Self::with_projection(
            function,
            endpoint,
            dataver,
            units::co2_matter_bounds(range),
            units::co2_matter_value,
        )
    }
}
impl co2::ClusterHandler for Co2Handler {
    const CLUSTER: Cluster<'static> = co2::FULL_CLUSTER
        .with_features(co2::Feature::NUMERIC_MEASUREMENT.bits())
        .with_attrs(|attribute, _, _| matches!(attribute.id, 0..=2 | 8..=9 | 0xfff8..=0xfffd))
        .with_cmds(with!());
    fn dataver(&self) -> u32 {
        self.sync_version();
        self.dataver.get()
    }
    fn dataver_changed(&self) {
        self.dataver.changed();
    }
    fn measured_value(&self, _ctx: impl ReadContext) -> Result<Nullable<f32>, Error> {
        self.measured_value()
    }
    fn min_measured_value(&self, _ctx: impl ReadContext) -> Result<Nullable<f32>, Error> {
        self.min_measured_value()
    }
    fn max_measured_value(&self, _ctx: impl ReadContext) -> Result<Nullable<f32>, Error> {
        self.max_measured_value()
    }
    fn measurement_unit(&self, _ctx: impl ReadContext) -> Result<MeasurementUnitEnum, Error> {
        Ok(MeasurementUnitEnum::PPM)
    }
    fn measurement_medium(&self, _ctx: impl ReadContext) -> Result<MeasurementMediumEnum, Error> {
        Ok(MeasurementMediumEnum::Air)
    }
    async fn run(&self, ctx: impl HandlerContext) -> Result<(), Error> {
        self.run_measurement(ctx, Self::CLUSTER.id, co2::AttributeId::MeasuredValue as _)
            .await
    }
}

pub(super) struct AirQualityHandler {
    dataver: Dataver,
}
impl air_quality::ClusterHandler for AirQualityHandler {
    const CLUSTER: Cluster<'static> = air_quality::FULL_CLUSTER
        .with_features(0)
        .with_cmds(with!());
    fn dataver(&self) -> u32 {
        self.dataver.get()
    }
    fn dataver_changed(&self) {
        self.dataver.changed();
    }
    fn air_quality(&self, _ctx: impl ReadContext) -> Result<air_quality::AirQualityEnum, Error> {
        Ok(air_quality::AirQualityEnum::Unknown)
    }
}

/// Both mandatory clusters belong to the same CO₂ function endpoint.
pub(super) struct Co2Sensor {
    concentration: Co2Handler,
    air_quality: AirQualityHandler,
}
impl Co2Sensor {
    pub(super) fn new(
        function: Function,
        range: MeasurementRange,
        endpoint: u16,
        concentration: Dataver,
        air_quality: Dataver,
    ) -> Self {
        Self {
            concentration: Co2Handler::new(function, range, endpoint, concentration),
            air_quality: AirQualityHandler {
                dataver: air_quality,
            },
        }
    }
    fn handler(&self) -> impl AsyncHandler + '_ {
        EmptyHandler
            .chain(
                |_, cluster| cluster == 0x040d,
                Async(co2::HandlerAdaptor(&self.concentration)),
            )
            .chain(
                |_, cluster| cluster == 0x005b,
                Async(air_quality::HandlerAdaptor(&self.air_quality)),
            )
    }
    pub(super) fn bump(&self, cluster: Option<u32>) {
        if cluster.is_none_or(|id| id == 0x040d) {
            co2::ClusterHandler::dataver_changed(&self.concentration);
        }
        if cluster.is_none_or(|id| id == 0x005b) {
            air_quality::ClusterHandler::dataver_changed(&self.air_quality);
        }
    }
}
impl AsyncHandler for Co2Sensor {
    fn bump_dataver(&self, _ctx: impl MatchContext) {}
    async fn read(&self, ctx: impl ReadContext, reply: impl ReadReply) -> Result<(), Error> {
        self.handler().read(ctx, reply).await
    }
    async fn write(&self, ctx: impl WriteContext) -> Result<(), Error> {
        self.handler().write(ctx).await
    }
    async fn invoke(&self, ctx: impl InvokeContext, reply: impl InvokeReply) -> Result<(), Error> {
        self.handler().invoke(ctx, reply).await
    }
    async fn run(&self, ctx: impl HandlerContext) -> Result<(), Error> {
        self.handler().run(ctx).await
    }
}
