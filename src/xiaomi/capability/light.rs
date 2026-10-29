use super::ls2307;
use super::{
    BrightnessBinding, CapabilityError, ColorBinding, LightBindings, ModeBinding, NumericFormat,
    ParsedDevice, ParsedFunction, PropertyBinding, TemperatureBinding,
    description::{Description, Property, Service, Urn, array, string},
    outlet::power_binding,
};
use crate::device::{DeviceKey, NumericRange};

pub(crate) fn is_light_type(value: &str) -> bool {
    Urn::parse(value).is_ok_and(|urn| urn.standard("device", "light", 0x0000a001))
}
pub(super) fn parse_light(
    key: DeviceKey,
    model: &str,
    instance: &Description<'_>,
) -> Result<ParsedDevice, CapabilityError> {
    let services = instance
        .services
        .iter()
        .filter(|service| service.urn.standard("service", "light", 0x00007802))
        .collect::<Vec<_>>();
    let service = match services.as_slice() {
        [] => return Err(CapabilityError::UnsupportedDevice),
        [service] => *service,
        _ => {
            return Err(CapabilityError::InvalidLight(
                "one standard light service is required",
            ));
        }
    };
    let power = match optional_standard_property(service, "on", 6)? {
        Some(property) => power_binding(service.iid, property)?,
        None => {
            return Err(CapabilityError::InvalidLight(
                "standard light profiles require On/Off",
            ));
        }
    };
    let brightness = optional_standard_property(service, "brightness", 0x0d)?
        .map(|property| brightness_binding(service.iid, property))
        .transpose()?;
    let temperature = optional_standard_property(service, "color-temperature", 0x0f)?
        .map(|property| temperature_binding(service.iid, property))
        .transpose()?;
    let color = optional_standard_property(service, "color", 0x0e)?
        .map(|property| color_binding(service.iid, property))
        .transpose()?;
    let (color, mode) = if model == ls2307::MODEL {
        let (Some(brightness), Some(temperature)) = (brightness, temperature) else {
            return Err(CapabilityError::IncompatibleModel(ls2307::MODEL));
        };
        parse_ls2307(instance, service, power, brightness, temperature)?
    } else {
        (color, None)
    };
    let lighting = LightBindings::new(brightness, temperature, color, mode)?;
    ParsedDevice::new(
        key,
        vec![ParsedFunction::light(
            format!("light:{}", service.iid),
            "Light",
            power,
            lighting,
        )],
        None,
    )
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
    if !matches_vendor(&instance.urn)
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
