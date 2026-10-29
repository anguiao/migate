use super::super::{
    AppliedPatch, BrightnessBinding, CapabilityError, ColorBinding, LightBindings, ModeBinding,
    NumericFormat, ParsedDevice, ParsedFunction, PropertyBinding, RecognitionDiagnostic,
    TemperatureBinding,
    description::{Description, Service, Urn, array},
    light::{access, color_binding, format, standard_property},
};
use super::BuiltinPatch;

pub(super) const PATCH: BuiltinPatch = BuiltinPatch {
    id: "devcea.ls2307.rgb",
    stage: "recognition and control/feedback",
};

const MODEL: &str = "devcea.light.ls2307";
const POWER: PropertyBinding = PropertyBinding { siid: 2, piid: 1 };
const BRIGHTNESS: PropertyBinding = PropertyBinding { siid: 2, piid: 2 };
const TEMPERATURE: PropertyBinding = PropertyBinding { siid: 2, piid: 3 };
const COLOR_PROPERTY: PropertyBinding = PropertyBinding { siid: 4, piid: 1 };
const MODE_PROPERTY: PropertyBinding = PropertyBinding { siid: 4, piid: 2 };
const MODE_FORMAT: NumericFormat = NumericFormat::Uint8;
const MODE_TEMPERATURE_VALUE: u32 = 0;
const MODE_RGB_VALUE: u32 = 1;

fn matches_vendor(urn: &Urn<'_>) -> bool {
    urn.vendor == "devcea-ls2307"
}
fn main_bindings(
    power: PropertyBinding,
    brightness: &BrightnessBinding,
    temperature: &TemperatureBinding,
) -> bool {
    power == POWER && brightness.property() == BRIGHTNESS && temperature.property() == TEMPERATURE
}
fn is_color(binding: &ColorBinding) -> bool {
    binding.property() == COLOR_PROPERTY && binding.format() == NumericFormat::Uint32
}
fn is_mode(binding: &ModeBinding) -> bool {
    binding.property() == MODE_PROPERTY
        && binding.format() == MODE_FORMAT
        && binding.temperature_value() == MODE_TEMPERATURE_VALUE
        && binding.rgb_value() == MODE_RGB_VALUE
}

fn matches(model: &str, urn: &Urn<'_>) -> bool {
    model == MODEL && urn.standard("device", "light", 0xa001) && matches_vendor(urn)
}

pub(in crate::xiaomi::capability) fn apply(
    model: &str,
    instance: &Description<'_>,
    functions: &mut [ParsedFunction],
    diagnostics: &mut Vec<RecognitionDiagnostic>,
    patches: &mut Vec<AppliedPatch>,
) {
    if model != MODEL {
        return;
    }
    let Some(function) = functions
        .iter_mut()
        .find(|function| function.id() == "light:2")
    else {
        diagnostics.push(PATCH.failed("light:2", "required light binding is missing or invalid"));
        return;
    };
    let service = instance
        .services
        .iter()
        .find(|service| service.iid == POWER.siid)
        .unwrap();
    let power = function.power().unwrap();
    let light = function.lighting().unwrap();
    let brightness = light.brightness().copied();
    let mut temperature = light.temperature().copied();
    let mut color = light.color().copied();
    let mut mode = None;
    let result = match (brightness, temperature) {
        (Some(brightness), Some(temperature)) => {
            parse_ls2307(instance, service, power, brightness, temperature)
        }
        _ => Err(CapabilityError::IncompatibleModel(MODEL)),
    };
    match result {
        Ok((patched_color, patched_mode)) => {
            color = Some(patched_color);
            mode = Some(patched_mode);
            patches.push(PATCH.applied(function.id()));
        }
        Err(error) => {
            diagnostics.push(PATCH.failed(function.id(), error));
            // This model remembers white temperature while RGB is active.
            // Without confirmed mode feedback, that value cannot identify white mode.
            if matches(model, &instance.urn) {
                if temperature.take().is_some() {
                    diagnostics.push(RecognitionDiagnostic {
                        target: function.id().into(),
                        reason: "color temperature feedback requires confirmed color mode".into(),
                    });
                }
                color = None;
            }
        }
    }
    *function = ParsedFunction::light(
        function.id(),
        "Light",
        power,
        LightBindings::new(brightness, temperature, color, mode)
            .expect("patch preserves light dependencies"),
    );
}

pub(super) fn validate_applied(
    model: &str,
    urn: &Urn<'_>,
    function: &ParsedFunction,
) -> Result<(), &'static str> {
    if matches(model, urn)
        && function
            .lighting()
            .is_some_and(|light| light.mode().is_some())
    {
        Ok(())
    } else {
        Err("Xiaomi applied patch is invalid")
    }
}

pub(super) fn validate_bindings(
    device: &ParsedDevice,
    model: &str,
    urn: &Urn<'_>,
) -> Result<(), &'static str> {
    for function in device.functions() {
        if let Some(light) = function.lighting()
            && let Some(mode) = light.mode()
        {
            let (Some(brightness), Some(temperature), Some(color)) =
                (light.brightness(), light.temperature(), light.color())
            else {
                return Err("Xiaomi ls2307 adapter is incompatible");
            };
            if !matches(model, urn)
                || function.power() != Some(POWER)
                || !main_bindings(POWER, brightness, temperature)
                || !is_color(color)
                || !is_mode(mode)
            {
                return Err("Xiaomi ls2307 adapter is incompatible");
            }
        }
    }
    Ok(())
}

fn parse_ls2307(
    instance: &Description<'_>,
    main: &Service<'_>,
    power: PropertyBinding,
    brightness: BrightnessBinding,
    temperature: TemperatureBinding,
) -> Result<(ColorBinding, ModeBinding), CapabilityError> {
    let incompatible = || CapabilityError::IncompatibleModel(MODEL);
    if !instance.urn.standard("device", "light", 0x0000a001)
        || !matches_vendor(&instance.urn)
        || main.iid != POWER.siid
        || !matches_vendor(&main.urn)
        || !main_bindings(power, &brightness, &temperature)
    {
        return Err(incompatible());
    }
    let ambient = instance
        .services
        .iter()
        .find(|service| service.iid == COLOR_PROPERTY.siid)
        .ok_or_else(incompatible)?;
    if !ambient.urn.standard("service", "ambient-light", 0x0000789d)
        || !matches_vendor(&ambient.urn)
    {
        return Err(incompatible());
    }
    let color_property = standard_property(ambient, "color", 0x0e)?;
    let mode_property = standard_property(ambient, "mode", 8)?;
    if color_property.iid != COLOR_PROPERTY.piid
        || mode_property.iid != MODE_PROPERTY.piid
        || !matches_vendor(&color_property.urn)
        || !matches_vendor(&mode_property.urn)
    {
        return Err(incompatible());
    }
    let color = color_binding(COLOR_PROPERTY.siid, color_property)?;
    if !is_color(&color) {
        return Err(incompatible());
    }
    access(mode_property)?;
    if format(mode_property)? != MODE_FORMAT {
        return Err(incompatible());
    }
    let values = array(mode_property.value, "value-list")?;
    if !values
        .iter()
        .any(|value| value["value"].as_u64() == Some(MODE_TEMPERATURE_VALUE as u64))
        || !values
            .iter()
            .any(|value| value["value"].as_u64() == Some(MODE_RGB_VALUE as u64))
    {
        return Err(incompatible());
    }
    let mode = ModeBinding::new(
        MODE_PROPERTY,
        MODE_FORMAT,
        MODE_TEMPERATURE_VALUE,
        MODE_RGB_VALUE,
    )?;
    Ok((color, mode))
}
