use super::{
    BrightnessBinding, ColorBinding, ModeBinding, NumericFormat, PropertyBinding,
    TemperatureBinding, description::Urn,
};

pub(crate) const MODEL: &str = "devcea.light.ls2307";
pub(crate) const POWER: PropertyBinding = PropertyBinding { siid: 2, piid: 1 };
pub(crate) const BRIGHTNESS: PropertyBinding = PropertyBinding { siid: 2, piid: 2 };
pub(crate) const TEMPERATURE: PropertyBinding = PropertyBinding { siid: 2, piid: 3 };
pub(crate) const COLOR_PROPERTY: PropertyBinding = PropertyBinding { siid: 4, piid: 1 };
pub(crate) const MODE_PROPERTY: PropertyBinding = PropertyBinding { siid: 4, piid: 2 };
pub(crate) const MODE_FORMAT: NumericFormat = NumericFormat::Uint8;
pub(crate) const MODE_TEMPERATURE_VALUE: u32 = 0;
pub(crate) const MODE_RGB_VALUE: u32 = 1;

pub(super) fn exact_urn(urn: &Urn<'_>) -> bool {
    urn.vendor == "devcea-ls2307" && urn.version == 1 && urn.suffix.is_none()
}
pub(crate) fn is_type(value: &str) -> bool {
    Urn::parse(value)
        .is_ok_and(|urn| urn.standard("device", "light", 0x0000a001) && exact_urn(&urn))
}
pub(crate) fn main_bindings(
    power: PropertyBinding,
    brightness: &BrightnessBinding,
    temperature: &TemperatureBinding,
) -> bool {
    power == POWER && brightness.property() == BRIGHTNESS && temperature.property() == TEMPERATURE
}
pub(crate) fn is_color(binding: &ColorBinding) -> bool {
    binding.property() == COLOR_PROPERTY && binding.format() == NumericFormat::Uint32
}
pub(crate) fn is_mode(binding: &ModeBinding) -> bool {
    binding.property() == MODE_PROPERTY
        && binding.format() == MODE_FORMAT
        && binding.temperature_value() == MODE_TEMPERATURE_VALUE
        && binding.rgb_value() == MODE_RGB_VALUE
}
