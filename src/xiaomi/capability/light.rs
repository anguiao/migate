use super::{
    BrightnessBinding, CapabilityError, ColorBinding, LightBindings, ParsedFunction,
    PropertyBinding, TemperatureBinding,
    description::{Property, Service, array, string},
    outlet::power_binding,
};
use crate::device::NumericRange;

pub(super) fn parse_light_service(
    service: &Service<'_>,
    diagnostics: &mut Vec<super::RecognitionDiagnostic>,
) -> Result<ParsedFunction, CapabilityError> {
    let power = match service.optional_property("on", 6)? {
        Some(property) => power_binding(service.iid, property)?,
        None => {
            return Err(CapabilityError::InvalidLight(
                "standard light profiles require On/Off",
            ));
        }
    };
    let target = format!("light:{}", service.iid);
    let brightness = optional(
        diagnostics,
        &target,
        "brightness",
        service
            .optional_property("brightness", 0x0d)
            .and_then(|p| p.map(|p| brightness_binding(service.iid, p)).transpose()),
    );
    let mut temperature = optional(
        diagnostics,
        &target,
        "color temperature",
        service
            .optional_property("color-temperature", 0x0f)
            .and_then(|p| p.map(|p| temperature_binding(service.iid, p)).transpose()),
    );
    let mut color = optional(
        diagnostics,
        &target,
        "RGB",
        service
            .optional_property("color", 0x0e)
            .and_then(|p| p.map(|p| color_binding(service.iid, p)).transpose()),
    );
    if brightness.is_none() && temperature.take().is_some() {
        diagnostics.push(super::RecognitionDiagnostic {
            target: target.clone(),
            reason: "color temperature requires brightness".into(),
        });
    }
    if (brightness.is_none() || temperature.is_none()) && color.take().is_some() {
        diagnostics.push(super::RecognitionDiagnostic {
            target: target.clone(),
            reason: "RGB requires brightness and color temperature".into(),
        });
    }
    let lighting = LightBindings::new(brightness, temperature, color, None)?;
    Ok(ParsedFunction::light(
        format!("light:{}", service.iid),
        "Light",
        power,
        lighting,
    ))
}
fn optional<T>(
    diagnostics: &mut Vec<super::RecognitionDiagnostic>,
    target: &str,
    name: &str,
    result: Result<Option<T>, CapabilityError>,
) -> Option<T> {
    match result {
        Ok(value) => value,
        Err(error) => {
            diagnostics.push(super::RecognitionDiagnostic {
                target: target.into(),
                reason: format!("{name}: {error}"),
            });
            None
        }
    }
}
fn numeric_range(property: &Property<'_>) -> Result<NumericRange, CapabilityError> {
    let range = array(property.value, "value-range")?
        .iter()
        .map(|value| {
            value
                .as_u64()
                .and_then(|value| u32::try_from(value).ok())
                .ok_or(CapabilityError::InvalidLight("integer range"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let [min, max, step] = range.as_slice() else {
        return Err(CapabilityError::InvalidLight("range"));
    };
    NumericRange::new(*min, *max, *step)
        .map_err(|_| CapabilityError::InvalidLight("range or format"))
}
fn brightness_binding(
    siid: u32,
    property: &Property<'_>,
) -> Result<BrightnessBinding, CapabilityError> {
    property.control_access()?;
    if string(property.value, "unit")? != "percentage" {
        return Err(CapabilityError::InvalidLight("unit"));
    }
    BrightnessBinding::new(
        PropertyBinding {
            siid,
            piid: property.iid,
        },
        property.integer_format()?,
        numeric_range(property)?,
    )
}
fn temperature_binding(
    siid: u32,
    property: &Property<'_>,
) -> Result<TemperatureBinding, CapabilityError> {
    property.control_access()?;
    if string(property.value, "unit")? != "kelvin" {
        return Err(CapabilityError::InvalidLight("unit"));
    }
    TemperatureBinding::new(
        PropertyBinding {
            siid,
            piid: property.iid,
        },
        property.integer_format()?,
        numeric_range(property)?,
    )
}
pub(super) fn color_binding(
    siid: u32,
    property: &Property<'_>,
) -> Result<ColorBinding, CapabilityError> {
    property.control_access()?;
    if string(property.value, "unit")? != "rgb" {
        return Err(CapabilityError::InvalidLight("unit"));
    }
    let range = numeric_range(property)?;
    if range != NumericRange::new(0, 0xffffff, 1).expect("fixed rgb grid") {
        return Err(CapabilityError::InvalidLight("range or format"));
    }
    ColorBinding::new(
        PropertyBinding {
            siid,
            piid: property.iid,
        },
        property.integer_format()?,
    )
}
