use super::ls2307;
use super::{
    BrightnessBinding, CapabilityError, ColorBinding, LightBindings, ModeBinding, NumericFormat,
    ParsedFunction, PropertyBinding, TemperatureBinding,
    description::{Description, Property, Service, array, string},
    outlet::power_binding,
};
use crate::device::NumericRange;

pub(super) fn parse_light_service(
    model: &str,
    instance: &Description<'_>,
    service: &Service<'_>,
    diagnostics: &mut Vec<super::RecognitionDiagnostic>,
    patches: &mut Vec<super::AppliedPatch>,
) -> Result<ParsedFunction, CapabilityError> {
    let power = match optional_standard_property(service, "on", 6)? {
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
        optional_standard_property(service, "brightness", 0x0d)
            .and_then(|p| p.map(|p| brightness_binding(service.iid, p)).transpose()),
    );
    let mut temperature = optional(
        diagnostics,
        &target,
        "color temperature",
        optional_standard_property(service, "color-temperature", 0x0f)
            .and_then(|p| p.map(|p| temperature_binding(service.iid, p)).transpose()),
    );
    let mut color = optional(
        diagnostics,
        &target,
        "RGB",
        optional_standard_property(service, "color", 0x0e)
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
    let mut mode = None;
    if super::patch::ls2307_target(model, service.iid) {
        let patched = match (brightness, temperature) {
            (Some(brightness), Some(temperature)) => {
                parse_ls2307(instance, service, power, brightness, temperature)
            }
            _ => Err(CapabilityError::IncompatibleModel(ls2307::MODEL)),
        };
        match patched {
            Ok((patched_color, patched_mode)) => {
                color = patched_color;
                mode = patched_mode;
                patches.push(super::patch::LS2307_RGB.applied(target.clone()));
            }
            Err(error) => {
                diagnostics.push(super::patch::LS2307_RGB.failed(target.clone(), error));
                // On this confirmed model, the native temperature can be a
                // remembered value while the lamp is in RGB mode. A plain
                // temperature profile would incorrectly infer white mode.
                if instance.urn.standard("device", "light", 0xa001)
                    && ls2307::matches_vendor(&instance.urn)
                {
                    if temperature.take().is_some() {
                        diagnostics.push(super::RecognitionDiagnostic {
                            target: target.clone(),
                            reason: "color temperature feedback requires confirmed color mode"
                                .into(),
                        });
                    }
                    color = None;
                }
            }
        }
    }
    let lighting = LightBindings::new(brightness, temperature, color, mode)?;
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
fn optional_standard_property<'a>(
    service: &'a Service<'a>,
    name: &str,
    type_id: u32,
) -> Result<Option<&'a Property<'a>>, CapabilityError> {
    let mut matches = service
        .properties
        .iter()
        .filter(|property| property.urn.standard("property", name, type_id));
    let first = matches.next();
    if matches.next().is_some() {
        return Err(CapabilityError::InvalidLight(
            "standard property is ambiguous",
        ));
    }
    Ok(first)
}
fn standard_property<'a>(
    service: &'a Service<'a>,
    name: &str,
    type_id: u32,
) -> Result<&'a Property<'a>, CapabilityError> {
    let properties = service
        .properties
        .iter()
        .filter(|property| property.urn.standard("property", name, type_id))
        .collect::<Vec<_>>();
    match properties.as_slice() {
        [property] => Ok(*property),
        _ => Err(CapabilityError::InvalidLight(
            "required standard property is missing or ambiguous",
        )),
    }
}
fn format(property: &Property<'_>) -> Result<NumericFormat, CapabilityError> {
    match string(property.value, "format")? {
        "uint8" => Ok(NumericFormat::Uint8),
        "uint16" => Ok(NumericFormat::Uint16),
        "uint32" => Ok(NumericFormat::Uint32),
        "int8" => Ok(NumericFormat::Int8),
        "int16" => Ok(NumericFormat::Int16),
        "int32" => Ok(NumericFormat::Int32),
        _ => Err(CapabilityError::InvalidLight("unsupported numeric format")),
    }
}
fn access(property: &Property<'_>) -> Result<(), CapabilityError> {
    let access = array(property.value, "access")?;
    for permission in ["read", "write", "notify"] {
        if !access
            .iter()
            .any(|value| value.as_str() == Some(permission))
        {
            return Err(CapabilityError::MissingAccess(permission));
        }
    }
    if access.iter().any(|value| value.as_str().is_none()) {
        return Err(CapabilityError::InvalidDescription("access"));
    }
    Ok(())
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
    access(property)?;
    if string(property.value, "unit")? != "percentage" {
        return Err(CapabilityError::InvalidLight("unit"));
    }
    BrightnessBinding::new(
        PropertyBinding {
            siid,
            piid: property.iid,
        },
        format(property)?,
        numeric_range(property)?,
    )
}
fn temperature_binding(
    siid: u32,
    property: &Property<'_>,
) -> Result<TemperatureBinding, CapabilityError> {
    access(property)?;
    if string(property.value, "unit")? != "kelvin" {
        return Err(CapabilityError::InvalidLight("unit"));
    }
    TemperatureBinding::new(
        PropertyBinding {
            siid,
            piid: property.iid,
        },
        format(property)?,
        numeric_range(property)?,
    )
}
fn color_binding(siid: u32, property: &Property<'_>) -> Result<ColorBinding, CapabilityError> {
    access(property)?;
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
        format(property)?,
    )
}
fn parse_ls2307(
    instance: &Description<'_>,
    main: &Service<'_>,
    power: PropertyBinding,
    brightness: BrightnessBinding,
    temperature: TemperatureBinding,
) -> Result<(Option<ColorBinding>, Option<ModeBinding>), CapabilityError> {
    let incompatible = || CapabilityError::IncompatibleModel(ls2307::MODEL);
    let matches_vendor = ls2307::matches_vendor;
    if !instance.urn.standard("device", "light", 0x0000a001)
        || !matches_vendor(&instance.urn)
        || main.iid != ls2307::POWER.siid
        || !matches_vendor(&main.urn)
        || !ls2307::main_bindings(power, &brightness, &temperature)
    {
        return Err(incompatible());
    }
    let ambient = instance
        .services
        .iter()
        .find(|service| service.iid == ls2307::COLOR_PROPERTY.siid)
        .ok_or_else(incompatible)?;
    if !ambient.urn.standard("service", "ambient-light", 0x0000789d)
        || !matches_vendor(&ambient.urn)
    {
        return Err(incompatible());
    }
    let color_property = standard_property(ambient, "color", 0x0e)?;
    let mode_property = standard_property(ambient, "mode", 8)?;
    if color_property.iid != ls2307::COLOR_PROPERTY.piid
        || mode_property.iid != ls2307::MODE_PROPERTY.piid
        || !matches_vendor(&color_property.urn)
        || !matches_vendor(&mode_property.urn)
    {
        return Err(incompatible());
    }
    let color = color_binding(ls2307::COLOR_PROPERTY.siid, color_property)?;
    if !ls2307::is_color(&color) {
        return Err(incompatible());
    }
    access(mode_property)?;
    if format(mode_property)? != ls2307::MODE_FORMAT {
        return Err(incompatible());
    }
    let values = array(mode_property.value, "value-list")?;
    if !values
        .iter()
        .any(|value| value["value"].as_u64() == Some(ls2307::MODE_TEMPERATURE_VALUE as u64))
        || !values
            .iter()
            .any(|value| value["value"].as_u64() == Some(ls2307::MODE_RGB_VALUE as u64))
    {
        return Err(incompatible());
    }
    let mode = ModeBinding::new(
        ls2307::MODE_PROPERTY,
        ls2307::MODE_FORMAT,
        ls2307::MODE_TEMPERATURE_VALUE,
        ls2307::MODE_RGB_VALUE,
    )?;
    Ok((Some(color), Some(mode)))
}
