use super::{CapabilityError, NumericFormat, PropertyBinding};
use crate::device::MeasurementRange;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MeasurementUnit {
    Celsius,
    Percentage,
    Ppm,
    Lux,
}

/// One real read-only measurement binding. Format, unit, range and access are
/// fixed at the source boundary; Matter projects its own target representation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MeasurementBinding {
    property: PropertyBinding,
    format: NumericFormat,
    unit: MeasurementUnit,
    range: MeasurementRange,
}
impl MeasurementBinding {
    pub(crate) fn new(
        property: PropertyBinding,
        format: NumericFormat,
        unit: MeasurementUnit,
        range: MeasurementRange,
    ) -> Result<Self, CapabilityError> {
        if property.siid == 0 || property.piid == 0 {
            return Err(CapabilityError::InvalidMeasurement("range"));
        }
        if let Some((min, max)) = format.measurement_bounds()
            && (range.min() < min || range.max() > max)
        {
            return Err(CapabilityError::InvalidMeasurement("range"));
        }
        Ok(Self {
            property,
            format,
            unit,
            range,
        })
    }
    pub fn property(&self) -> PropertyBinding {
        self.property
    }
    pub fn format(&self) -> NumericFormat {
        self.format
    }
    pub fn unit(&self) -> MeasurementUnit {
        self.unit
    }
    pub fn range(&self) -> MeasurementRange {
        self.range
    }
    /// Decode a real source value in its declared unit. Integer formats do not
    /// accept fractional JSON values; every format is finite and in range.
    pub fn read_value(&self, value: &Value) -> Result<f64, CapabilityError> {
        let value = match self.format {
            NumericFormat::Float => value.as_f64(),
            _ => value
                .as_i64()
                .map(|value| value as f64)
                .or_else(|| value.as_u64().map(|value| value as f64))
                .or_else(|| {
                    value
                        .as_f64()
                        .filter(|value| value.is_finite() && value.fract() == 0.0)
                }),
        }
        .filter(|value| self.range.contains(*value))
        .ok_or(CapabilityError::InvalidValue("measurement"))?;
        Ok(value)
    }
}

/// One optional physical battery binding. The service/property semantics are
/// resolved before construction; this type only carries the real binding and
/// its supported percentage representation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BatteryBinding {
    measurement: MeasurementBinding,
}
impl BatteryBinding {
    pub(crate) fn new(measurement: MeasurementBinding) -> Result<Self, CapabilityError> {
        if measurement.unit() != MeasurementUnit::Percentage
            || measurement.range().min() < 0.0
            || measurement.range().max() > 100.0
        {
            return Err(CapabilityError::InvalidMeasurement("battery"));
        }
        Ok(Self { measurement })
    }
    pub fn property(&self) -> PropertyBinding {
        self.measurement.property()
    }
    pub fn format(&self) -> NumericFormat {
        self.measurement.format()
    }
    pub fn range(&self) -> MeasurementRange {
        self.measurement.range()
    }
    pub fn read_value(&self, value: &Value) -> Result<f64, CapabilityError> {
        self.measurement
            .read_value(value)
            .map_err(|_| CapabilityError::InvalidValue("battery"))
    }
}
