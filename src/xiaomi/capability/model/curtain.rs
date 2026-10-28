use super::{CapabilityError, NumericFormat, PropertyBinding, numeric::raw_integer};
use crate::device::{CurtainMotion, NumericRange};
use serde_json::Value;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CurtainPositionBinding {
    property: PropertyBinding,
    format: NumericFormat,
    range: NumericRange,
}
impl CurtainPositionBinding {
    pub(crate) fn new(
        property: PropertyBinding,
        format: NumericFormat,
        range: NumericRange,
    ) -> Result<Self, CapabilityError> {
        if property.siid == 0
            || property.piid == 0
            || format == NumericFormat::Float
            || format.maximum() < 100
            || range.min() != 0
            || range.max() != 100
            || range.effective_max() != 100
        {
            return Err(CapabilityError::InvalidDescription(
                "curtain position range or format",
            ));
        }
        Ok(Self {
            property,
            format,
            range,
        })
    }
    pub fn property(&self) -> PropertyBinding {
        self.property
    }
    pub fn format(&self) -> NumericFormat {
        self.format
    }
    pub fn range(&self) -> NumericRange {
        self.range
    }
    /// Observations retain their reported precision; the step governs writes.
    pub fn read_raw(&self, value: &Value) -> Result<u32, CapabilityError> {
        let raw = raw_integer(value)?;
        if raw > 100 {
            return Err(CapabilityError::InvalidValue("curtain position"));
        }
        Ok(raw)
    }
    #[allow(dead_code)] // Used by the curtain adapter in the next slice.
    pub fn quantize(&self, percent: f64) -> u32 {
        self.range.quantize(percent)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CurtainMotorBinding {
    property: PropertyBinding,
    format: NumericFormat,
    pause: u32,
}
impl CurtainMotorBinding {
    pub(crate) fn new(
        property: PropertyBinding,
        format: NumericFormat,
        pause: u32,
    ) -> Result<Self, CapabilityError> {
        if property.siid == 0
            || property.piid == 0
            || format == NumericFormat::Float
            || pause > format.maximum()
        {
            return Err(CapabilityError::InvalidDescription("curtain motor"));
        }
        Ok(Self {
            property,
            format,
            pause,
        })
    }
    pub fn property(&self) -> PropertyBinding {
        self.property
    }
    pub fn format(&self) -> NumericFormat {
        self.format
    }
    pub fn pause(&self) -> u32 {
        self.pause
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CurtainStatusBinding {
    property: PropertyBinding,
    format: NumericFormat,
    opening: u32,
    closing: u32,
    stopped: u32,
}
impl CurtainStatusBinding {
    pub(crate) fn new(
        property: PropertyBinding,
        format: NumericFormat,
        opening: u32,
        closing: u32,
        stopped: u32,
    ) -> Result<Self, CapabilityError> {
        if property.siid == 0
            || property.piid == 0
            || format == NumericFormat::Float
            || [opening, closing, stopped]
                .into_iter()
                .any(|value| value > format.maximum())
            || opening == closing
            || opening == stopped
            || closing == stopped
        {
            return Err(CapabilityError::InvalidDescription(
                "curtain status enumeration",
            ));
        }
        Ok(Self {
            property,
            format,
            opening,
            closing,
            stopped,
        })
    }
    pub fn property(&self) -> PropertyBinding {
        self.property
    }
    pub fn format(&self) -> NumericFormat {
        self.format
    }
    pub fn opening(&self) -> u32 {
        self.opening
    }
    pub fn closing(&self) -> u32 {
        self.closing
    }
    pub fn stopped(&self) -> u32 {
        self.stopped
    }
    pub fn read_value(&self, value: &Value) -> Result<CurtainMotion, CapabilityError> {
        let raw = raw_integer(value)?;
        if raw == self.opening {
            Ok(CurtainMotion::Opening)
        } else if raw == self.closing {
            Ok(CurtainMotion::Closing)
        } else if raw == self.stopped {
            Ok(CurtainMotion::Stopped)
        } else {
            Err(CapabilityError::InvalidValue("curtain status"))
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CurtainBindings {
    motor: CurtainMotorBinding,
    status: CurtainStatusBinding,
    current: CurtainPositionBinding,
    target: CurtainPositionBinding,
}
impl CurtainBindings {
    pub(crate) fn new(
        motor: CurtainMotorBinding,
        status: CurtainStatusBinding,
        current: CurtainPositionBinding,
        target: CurtainPositionBinding,
    ) -> Self {
        Self {
            motor,
            status,
            current,
            target,
        }
    }
    pub fn motor(&self) -> &CurtainMotorBinding {
        &self.motor
    }
    pub fn status(&self) -> &CurtainStatusBinding {
        &self.status
    }
    pub fn current(&self) -> &CurtainPositionBinding {
        &self.current
    }
    pub fn target(&self) -> &CurtainPositionBinding {
        &self.target
    }
    pub(crate) fn bindings(&self) -> Vec<PropertyBinding> {
        vec![
            self.motor.property(),
            self.status.property(),
            self.current.property(),
            self.target.property(),
        ]
    }
}
