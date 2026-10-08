use super::{
    BrightnessConversion, CapabilityError, LightBindings, ModeBinding, NumericBinding,
    NumericFormat, NumericUnit, ParsedDevice, ParsedFunction, PropertyBinding,
    description::{Description, Property, Service, Urn, array, string},
    outlet::power_binding,
};
use crate::device::{Capability, Category, DeviceKey, FunctionDescription, NumericRange};

pub(crate) fn is_ls2307_type(value: &str) -> bool {
    Urn::parse(value).is_ok_and(|urn| {
        urn.standard("device", "light", 0x0000a001)
            && urn.vendor == "devcea-ls2307"
            && urn.version == 1
            && urn.suffix.is_none()
    })
}
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
    let power = power_binding(service.iid, standard_property(service, "on", 6)?)?;
    let brightness = numeric_binding(
        service.iid,
        standard_property(service, "brightness", 0x0d)?,
        NumericUnit::Percentage,
    )?;
    let temperature = numeric_binding(
        service.iid,
        standard_property(service, "color-temperature", 0x0f)?,
        NumericUnit::Kelvin,
    )?;
    if brightness.range.nonzero_bounds().is_none()
        || temperature.range.min == 0
        || temperature.range.max > 1_000_000
        || temperature.range.mired_bounds().is_err()
    {
        return Err(CapabilityError::InvalidLight(
            "nonzero brightness and Kelvin ranges are required",
        ));
    }
    let (color, mode) = if model == "devcea.light.ls2307" {
        ls2307(instance, service, power, brightness, temperature)?
    } else {
        (None, None)
    };
    let mut capabilities = vec![
        Capability::OnOff,
        Capability::Brightness,
        Capability::ColorTemperature(temperature.range),
    ];
    if color.is_some() {
        capabilities.push(Capability::ColorRgb);
    }
    Ok(ParsedDevice {
        key,
        functions: vec![ParsedFunction {
            description: FunctionDescription {
                id: "main".into(),
                label: "灯光".into(),
                category: Category::Light,
                capabilities,
            },
            power,
            lighting: Some(LightBindings {
                brightness,
                temperature,
                color,
                mode,
                brightness_conversion: BrightnessConversion::NonzeroLinear,
            }),
        }],
        refresh_trigger: None,
    })
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
fn numeric_binding(
    siid: u32,
    property: &Property<'_>,
    unit: NumericUnit,
) -> Result<NumericBinding, CapabilityError> {
    access(property)?;
    let format = format(property)?;
    let expected = match unit {
        NumericUnit::Percentage => "percentage",
        NumericUnit::Kelvin => "kelvin",
        NumericUnit::Rgb => "rgb",
    };
    if string(property.value, "unit")? != expected {
        return Err(CapabilityError::InvalidLight("unit"));
    }
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
    let binding = NumericBinding {
        property: PropertyBinding {
            siid,
            piid: property.iid,
        },
        format,
        unit,
        range: NumericRange {
            min: *min,
            max: *max,
            step: *step,
        },
    };
    if !binding.valid(unit) {
        return Err(CapabilityError::InvalidLight("range or format"));
    }
    Ok(binding)
}
fn ls2307(
    instance: &Description<'_>,
    main: &Service<'_>,
    power: PropertyBinding,
    brightness: NumericBinding,
    temperature: NumericBinding,
) -> Result<(Option<NumericBinding>, Option<ModeBinding>), CapabilityError> {
    let incompatible = || CapabilityError::IncompatibleModel("devcea.light.ls2307");
    let exact =
        |urn: &Urn<'_>| urn.vendor == "devcea-ls2307" && urn.version == 1 && urn.suffix.is_none();
    if !exact(&instance.urn)
        || main.iid != 2
        || !exact(&main.urn)
        || power != (PropertyBinding { siid: 2, piid: 1 })
        || brightness.property != (PropertyBinding { siid: 2, piid: 2 })
        || temperature.property != (PropertyBinding { siid: 2, piid: 3 })
    {
        return Err(incompatible());
    }
    let ambient = instance
        .services
        .iter()
        .find(|service| service.iid == 4)
        .ok_or_else(incompatible)?;
    if !ambient.urn.standard("service", "ambient-light", 0x0000789d) || !exact(&ambient.urn) {
        return Err(incompatible());
    }
    let color_property = standard_property(ambient, "color", 0x0e)?;
    let mode_property = standard_property(ambient, "mode", 8)?;
    if color_property.iid != 1
        || mode_property.iid != 2
        || !exact(&color_property.urn)
        || !exact(&mode_property.urn)
    {
        return Err(incompatible());
    }
    let color = numeric_binding(4, color_property, NumericUnit::Rgb)?;
    if color.format != NumericFormat::Uint32
        || color.range
            != (NumericRange {
                min: 0,
                max: 0xffffff,
                step: 1,
            })
    {
        return Err(incompatible());
    }
    access(mode_property)?;
    if format(mode_property)? != NumericFormat::Uint8 {
        return Err(incompatible());
    }
    let values = array(mode_property.value, "value-list")?;
    if values.len() != 2
        || !values.iter().any(|value| {
            value["value"].as_u64() == Some(0) && value["description"].as_str() == Some("Warm")
        })
        || !values.iter().any(|value| {
            value["value"].as_u64() == Some(1) && value["description"].as_str() == Some("Color")
        })
    {
        return Err(incompatible());
    }
    Ok((
        Some(color),
        Some(ModeBinding {
            property: PropertyBinding { siid: 4, piid: 2 },
            format: NumericFormat::Uint8,
            temperature_value: 0,
            rgb_value: 1,
        }),
    ))
}
