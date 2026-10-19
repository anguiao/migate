use super::{
    ModeBinding, NumericBinding, NumericFormat, NumericUnit, PropertyBinding, description::Urn,
};
use crate::device::NumericRange;

pub(crate) const MODEL: &str = "devcea.light.ls2307";
pub(crate) const POWER: PropertyBinding = PropertyBinding { siid: 2, piid: 1 };
pub(crate) const BRIGHTNESS: PropertyBinding = PropertyBinding { siid: 2, piid: 2 };
pub(crate) const TEMPERATURE: PropertyBinding = PropertyBinding { siid: 2, piid: 3 };
pub(crate) const COLOR: NumericBinding = NumericBinding {
    property: PropertyBinding { siid: 4, piid: 1 },
    format: NumericFormat::Uint32,
    unit: NumericUnit::Rgb,
    range: NumericRange {
        min: 0,
        max: 0xffffff,
        step: 1,
    },
};
pub(crate) const MODE: ModeBinding = ModeBinding {
    property: PropertyBinding { siid: 4, piid: 2 },
    format: NumericFormat::Uint8,
    temperature_value: 0,
    rgb_value: 1,
};

pub(super) fn exact_urn(urn: &Urn<'_>) -> bool {
    urn.vendor == "devcea-ls2307" && urn.version == 1 && urn.suffix.is_none()
}
pub(crate) fn is_type(value: &str) -> bool {
    Urn::parse(value)
        .is_ok_and(|urn| urn.standard("device", "light", 0x0000a001) && exact_urn(&urn))
}
pub(crate) fn main_bindings(
    power: PropertyBinding,
    brightness: NumericBinding,
    temperature: NumericBinding,
) -> bool {
    power == POWER && brightness.property == BRIGHTNESS && temperature.property == TEMPERATURE
}
