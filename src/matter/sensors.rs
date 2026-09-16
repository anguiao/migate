use crate::device::{
    Capability, DeviceService, FeatureIdentity, NumericRange, PresenceState, Property,
    PropertyState, PropertyValue, SensingModality,
};
use rs_matter::{
    dm::{
        Cluster, Dataver, ReadContext,
        clusters::decl::{
            boolean_state, illuminance_measurement, occupancy_sensing, power_source,
            relative_humidity_measurement, temperature_measurement,
        },
    },
    error::{Error, ErrorCode},
    tlv::Nullable,
    with,
};
use std::cell::RefCell;

pub(super) const TEMPERATURE_CLUSTER: Cluster<'static> = temperature_measurement::FULL_CLUSTER
    .with_attrs(with!(required))
    .with_cmds(with!())
    .with_events(with!());
pub(super) const HUMIDITY_CLUSTER: Cluster<'static> = relative_humidity_measurement::FULL_CLUSTER
    .with_attrs(with!(required))
    .with_cmds(with!())
    .with_events(with!());
pub(super) const ILLUMINANCE_CLUSTER: Cluster<'static> = illuminance_measurement::FULL_CLUSTER
    .with_attrs(with!(required))
    .with_cmds(with!())
    .with_events(with!());
pub(super) const BOOLEAN_STATE_CLUSTER: Cluster<'static> = boolean_state::FULL_CLUSTER
    .with_attrs(with!(required))
    .with_cmds(with!())
    .with_events(with!());
pub(super) const POWER_SOURCE_CLUSTER: Cluster<'static> = power_source::FULL_CLUSTER
    .with_features(power_source::Feature::BATTERY.bits())
    .with_attrs(with!(required; power_source::AttributeId::BatPercentRemaining))
    .with_cmds(with!())
    .with_events(with!());

pub(super) fn occupancy_cluster(modalities: &[SensingModality]) -> Cluster<'static> {
    let mut features = occupancy_sensing::Feature::empty();
    for modality in modalities {
        features |= match modality {
            SensingModality::Unspecified => occupancy_sensing::Feature::OTHER,
            SensingModality::Pir => occupancy_sensing::Feature::PASSIVE_INFRARED,
            SensingModality::Radar => occupancy_sensing::Feature::RADAR,
        };
    }
    if features.is_empty() {
        features = occupancy_sensing::Feature::OTHER;
    }
    occupancy_sensing::FULL_CLUSTER
        .with_features(features.bits())
        .with_attrs(with!(required))
        .with_cmds(with!())
        .with_events(with!())
}

pub(super) struct SensorHandler {
    service: DeviceService,
    feature: FeatureIdentity,
    capabilities: RefCell<Vec<Capability>>,
    endpoint: u16,
    datavers: [Dataver; 6],
}

impl SensorHandler {
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
            capabilities: RefCell::new(capabilities),
            endpoint,
            datavers: std::array::from_fn(|offset| Dataver::new(seed.wrapping_add(offset as u32))),
        }
    }

    pub(super) fn dataver(&self, cluster: u32) -> Option<&Dataver> {
        let index = match cluster {
            0x0402 => 0,
            0x0405 => 1,
            0x0400 => 2,
            0x0406 => 3,
            0x0045 => 4,
            0x002f => 5,
            _ => return None,
        };
        Some(&self.datavers[index])
    }

    fn range(&self, pick: fn(&Capability) -> Option<NumericRange>) -> Option<NumericRange> {
        self.capabilities.borrow().iter().find_map(pick)
    }

    pub(super) fn set_capabilities(&self, capabilities: Vec<Capability>) {
        *self.capabilities.borrow_mut() = capabilities;
    }

    pub(super) fn capabilities(&self) -> Vec<Capability> {
        self.capabilities.borrow().clone()
    }

    fn current(&self, property: Property) -> Option<PropertyValue> {
        match self.service.snapshot(&self.feature)?.property(property)? {
            PropertyState::Current { value, .. } => Some(value.clone()),
            PropertyState::LastKnown { .. } | PropertyState::Unknown { .. } => None,
        }
    }

    fn temperature_range(&self) -> Option<NumericRange> {
        self.range(|cap| match cap {
            Capability::Temperature(range) => Some(*range),
            _ => None,
        })
    }

    fn humidity_range(&self) -> Option<NumericRange> {
        self.range(|cap| match cap {
            Capability::Humidity(range) => Some(*range),
            _ => None,
        })
    }

    fn illuminance_range(&self) -> Option<NumericRange> {
        self.range(|cap| match cap {
            Capability::Illuminance(range) => Some(*range),
            _ => None,
        })
    }

    fn presence_property(&self) -> Property {
        if self
            .capabilities
            .borrow()
            .iter()
            .any(|capability| matches!(capability, Capability::Occupancy))
        {
            Property::Occupancy
        } else {
            Property::Motion
        }
    }
}

impl power_source::ClusterHandler for SensorHandler {
    const CLUSTER: Cluster<'static> = POWER_SOURCE_CLUSTER;
    fn dataver(&self) -> u32 {
        self.datavers[5].get()
    }
    fn dataver_changed(&self) {
        self.datavers[5].changed();
    }
    fn status(&self, _ctx: impl ReadContext) -> Result<power_source::PowerSourceStatusEnum, Error> {
        Ok(power_source::PowerSourceStatusEnum::Unspecified)
    }
    fn order(&self, _ctx: impl ReadContext) -> Result<u8, Error> {
        Ok(0)
    }
    fn description<P: rs_matter::tlv::TLVBuilderParent>(
        &self,
        _ctx: impl ReadContext,
        builder: rs_matter::tlv::Utf8StrBuilder<P>,
    ) -> Result<P, Error> {
        builder.set("Battery")
    }
    fn bat_percent_remaining(&self, _ctx: impl ReadContext) -> Result<Nullable<u8>, Error> {
        Ok(Nullable::new(match self.current(Property::Battery) {
            Some(PropertyValue::Percent(value)) => {
                let encoded = (value.get() * 2.0).round();
                let valid = self
                    .range(|capability| match capability {
                        Capability::Battery(range) => Some(*range),
                        _ => None,
                    })
                    .is_some_and(|range| range.accepts(value.get()));
                (valid && (0.0..=200.0).contains(&encoded)).then_some(encoded as u8)
            }
            _ => None,
        }))
    }
    fn bat_charge_level(
        &self,
        _ctx: impl ReadContext,
    ) -> Result<power_source::BatChargeLevelEnum, Error> {
        Err(ErrorCode::Failure.into())
    }
    fn bat_replacement_needed(&self, _ctx: impl ReadContext) -> Result<bool, Error> {
        Err(ErrorCode::Failure.into())
    }
    fn bat_replaceability(
        &self,
        _ctx: impl ReadContext,
    ) -> Result<power_source::BatReplaceabilityEnum, Error> {
        Ok(power_source::BatReplaceabilityEnum::Unspecified)
    }
    fn endpoint_list<P: rs_matter::tlv::TLVBuilderParent>(
        &self,
        _ctx: impl ReadContext,
        builder: rs_matter::dm::ArrayAttributeRead<
            rs_matter::tlv::ToTLVArrayBuilder<P, u16>,
            rs_matter::tlv::ToTLVBuilder<P, u16>,
        >,
    ) -> Result<P, Error> {
        match builder {
            rs_matter::dm::ArrayAttributeRead::ReadAll(builder) => {
                builder.push(&self.endpoint)?.end()
            }
            rs_matter::dm::ArrayAttributeRead::ReadOne(0, builder) => builder.set(&self.endpoint),
            rs_matter::dm::ArrayAttributeRead::ReadOne(_, _) => {
                Err(ErrorCode::ConstraintError.into())
            }
            rs_matter::dm::ArrayAttributeRead::ReadNone(builder) => builder.end(),
        }
    }
}

fn centi(value: f64) -> Option<i16> {
    let value = (value * 100.0).round();
    (value.is_finite() && value >= -27_315.0 && value <= i16::MAX as f64).then_some(value as i16)
}

fn percent_hundredths(value: f64) -> Option<u16> {
    let value = (value * 100.0).round();
    (value.is_finite() && (0.0..=10_000.0).contains(&value)).then_some(value as u16)
}

pub(super) fn matter_lux(value: f64) -> Option<u16> {
    if !value.is_finite() || value < 0.0 {
        return None;
    }
    if value < 1.0 {
        return Some(0);
    }
    let encoded = (value.log10() * 10_000.0 + 1.0).round();
    (1.0..=65_534.0)
        .contains(&encoded)
        .then_some(encoded as u16)
}

pub(super) fn temperature_bounds(range: Option<NumericRange>) -> Option<(i16, i16)> {
    range.and_then(|range| {
        let minimum = centi(range.minimum)?;
        let maximum = centi(range.maximum)?;
        (minimum < maximum).then_some((minimum, maximum))
    })
}

fn range_centi(range: Option<NumericRange>, minimum: bool) -> Nullable<i16> {
    Nullable::new(temperature_bounds(range).map(|values| if minimum { values.0 } else { values.1 }))
}

pub(super) fn humidity_bounds(range: Option<NumericRange>) -> Option<(u16, u16)> {
    let range = range?;
    let minimum = percent_hundredths(range.minimum)?;
    let maximum = percent_hundredths(range.maximum)?;
    (minimum < maximum).then_some((minimum, maximum))
}

pub(super) fn illuminance_bounds(range: Option<NumericRange>) -> Option<(u16, u16)> {
    let range = range?;
    let minimum = matter_lux(range.minimum)?.max(1);
    let maximum = matter_lux(range.maximum)?;
    (minimum < maximum && maximum <= 65_534).then_some((minimum, maximum))
}

impl temperature_measurement::ClusterHandler for SensorHandler {
    const CLUSTER: Cluster<'static> = TEMPERATURE_CLUSTER;
    fn dataver(&self) -> u32 {
        self.datavers[0].get()
    }
    fn dataver_changed(&self) {
        self.datavers[0].changed();
    }
    fn measured_value(&self, _ctx: impl ReadContext) -> Result<Nullable<i16>, Error> {
        Ok(Nullable::new(match self.current(Property::Temperature) {
            Some(PropertyValue::Temperature(value))
                if self
                    .temperature_range()
                    .is_some_and(|range| range.accepts(value)) =>
            {
                centi(value)
            }
            _ => None,
        }))
    }
    fn min_measured_value(&self, _ctx: impl ReadContext) -> Result<Nullable<i16>, Error> {
        Ok(range_centi(self.temperature_range(), true))
    }
    fn max_measured_value(&self, _ctx: impl ReadContext) -> Result<Nullable<i16>, Error> {
        Ok(range_centi(self.temperature_range(), false))
    }
}

impl relative_humidity_measurement::ClusterHandler for SensorHandler {
    const CLUSTER: Cluster<'static> = HUMIDITY_CLUSTER;
    fn dataver(&self) -> u32 {
        self.datavers[1].get()
    }
    fn dataver_changed(&self) {
        self.datavers[1].changed();
    }
    fn measured_value(&self, _ctx: impl ReadContext) -> Result<Nullable<u16>, Error> {
        Ok(Nullable::new(match self.current(Property::Humidity) {
            Some(PropertyValue::Percent(value))
                if self
                    .humidity_range()
                    .is_some_and(|range| range.accepts(value.get())) =>
            {
                percent_hundredths(value.get())
            }
            _ => None,
        }))
    }
    fn min_measured_value(&self, _ctx: impl ReadContext) -> Result<Nullable<u16>, Error> {
        Ok(Nullable::new(
            humidity_bounds(self.humidity_range()).map(|values| values.0),
        ))
    }
    fn max_measured_value(&self, _ctx: impl ReadContext) -> Result<Nullable<u16>, Error> {
        Ok(Nullable::new(
            humidity_bounds(self.humidity_range()).map(|values| values.1),
        ))
    }
}

impl illuminance_measurement::ClusterHandler for SensorHandler {
    const CLUSTER: Cluster<'static> = ILLUMINANCE_CLUSTER;
    fn dataver(&self) -> u32 {
        self.datavers[2].get()
    }
    fn dataver_changed(&self) {
        self.datavers[2].changed();
    }
    fn measured_value(&self, _ctx: impl ReadContext) -> Result<Nullable<u16>, Error> {
        Ok(Nullable::new(match self.current(Property::Illuminance) {
            Some(PropertyValue::Illuminance(value))
                if self
                    .illuminance_range()
                    .is_some_and(|range| range.accepts(value)) =>
            {
                matter_lux(value)
            }
            _ => None,
        }))
    }
    fn min_measured_value(&self, _ctx: impl ReadContext) -> Result<Nullable<u16>, Error> {
        Ok(Nullable::new(
            illuminance_bounds(self.illuminance_range()).map(|values| values.0),
        ))
    }
    fn max_measured_value(&self, _ctx: impl ReadContext) -> Result<Nullable<u16>, Error> {
        Ok(Nullable::new(
            illuminance_bounds(self.illuminance_range()).map(|values| values.1),
        ))
    }
}

impl occupancy_sensing::ClusterHandler for SensorHandler {
    const CLUSTER: Cluster<'static> = occupancy_sensing::FULL_CLUSTER;
    fn dataver(&self) -> u32 {
        self.datavers[3].get()
    }
    fn dataver_changed(&self) {
        self.datavers[3].changed();
    }
    fn occupancy(
        &self,
        _ctx: impl ReadContext,
    ) -> Result<occupancy_sensing::OccupancyBitmap, Error> {
        let occupied = match self.current(self.presence_property()) {
            Some(PropertyValue::Occupancy(PresenceState::Occupied))
            | Some(PropertyValue::Motion(true)) => true,
            Some(PropertyValue::Occupancy(PresenceState::Vacant))
            | Some(PropertyValue::Motion(false)) => false,
            _ => return Err(ErrorCode::Failure.into()),
        };
        Ok(if occupied {
            occupancy_sensing::OccupancyBitmap::OCCUPIED
        } else {
            occupancy_sensing::OccupancyBitmap::empty()
        })
    }
    fn occupancy_sensor_type(
        &self,
        _ctx: impl ReadContext,
    ) -> Result<occupancy_sensing::OccupancySensorTypeEnum, Error> {
        Ok(occupancy_sensing::OccupancySensorTypeEnum::PIR)
    }
    fn occupancy_sensor_type_bitmap(
        &self,
        _ctx: impl ReadContext,
    ) -> Result<occupancy_sensing::OccupancySensorTypeBitmap, Error> {
        Ok(occupancy_sensing::OccupancySensorTypeBitmap::PIR)
    }
}

impl boolean_state::ClusterHandler for SensorHandler {
    const CLUSTER: Cluster<'static> = BOOLEAN_STATE_CLUSTER;
    fn dataver(&self) -> u32 {
        self.datavers[4].get()
    }
    fn dataver_changed(&self) {
        self.datavers[4].changed();
    }
    fn state_value(&self, _ctx: impl ReadContext) -> Result<bool, Error> {
        match self.current(Property::Contact) {
            Some(PropertyValue::ContactOpen(open)) => Ok(!open),
            _ => Err(ErrorCode::Failure.into()),
        }
    }
}
