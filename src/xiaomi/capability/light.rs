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
    let power = select_control(
        controls(std::iter::once(service), "on", 0x00000006)?,
        "multiple controllable power properties",
    )?
    .map(|(siid, property)| power_binding(siid, property))
    .transpose()?;
    let brightness = select_control(
        controls(std::iter::once(service), "brightness", 0x0000000d)?,
        "multiple controllable brightness properties",
    )?
    .map(|(siid, property)| numeric_binding(siid, property, NumericUnit::Percentage))
    .transpose()?;
    if brightness
        .as_ref()
        .is_some_and(|binding| binding.range.nonzero_bounds().is_none())
    {
        return Err(CapabilityError::InvalidLight(
            "nonzero brightness range is required",
        ));
    }
    let temperature = select_control(
        controls(std::iter::once(service), "color-temperature", 0x0000000f)?,
        "multiple controllable color temperature properties",
    )?
    .map(|(siid, property)| numeric_binding(siid, property, NumericUnit::Kelvin))
    .transpose()?;
    if temperature
        .as_ref()
        .is_some_and(|binding| binding.range.min == 0 || binding.range.max > 1_000_000)
    {
        return Err(CapabilityError::InvalidLight("Kelvin range is required"));
    }
    // A standard color property is admitted wherever it is declared in the
    // standard namespace, including the ambient-light service used by vendor
    // adapters; its declared encoding must be packed 24-bit RGB.
    let color = select_control(
        controls(instance.services.iter(), "color", 0x0000000e)?,
        "multiple controllable color properties",
    )?
    .map(|(siid, property)| numeric_binding(siid, property, NumericUnit::Rgb))
    .transpose()?;
    if color.as_ref().is_some_and(|binding| {
        binding.format != NumericFormat::Uint32
            || binding.range
                != (NumericRange {
                    min: 0,
                    max: 0xffffff,
                    step: 1,
                })
    }) {
        return Err(CapabilityError::InvalidLight("RGB encoding is required"));
    }
    let mode = if model == "devcea.light.ls2307" {
        Some(ls2307(
            instance,
            service,
            power,
            brightness.as_ref(),
            temperature.as_ref(),
            color.as_ref(),
        )?)
    } else {
        None
    };
    let mut capabilities = Vec::new();
    if power.is_some() {
        capabilities.push(Capability::OnOff);
    }
    if brightness.is_some() {
        capabilities.push(Capability::Brightness);
    }
    if let Some(temperature) = &temperature {
        capabilities.push(Capability::ColorTemperature(temperature.range));
    }
    if color.is_some() {
        capabilities.push(Capability::ColorRgb);
    }
    if capabilities.is_empty() {
        return Err(CapabilityError::InvalidLight(
            "at least one controllable property is required",
        ));
    }
    let lighting = (brightness.is_some() || temperature.is_some() || color.is_some()).then_some(
        LightBindings {
            brightness_conversion: brightness
                .is_some()
                .then_some(BrightnessConversion::NonzeroLinear),
            brightness,
            temperature,
            color,
            mode,
        },
    );
    Ok(ParsedDevice {
        key,
        functions: vec![ParsedFunction {
            description: FunctionDescription {
                id: "main".into(),
                label: "Light".into(),
                category: Category::Light,
                capabilities,
            },
            power,
            lighting,
        }],
        refresh_trigger: None,
    })
}

/// A property declares control only when it has write access. Read-only or
/// fixed parameters are not capabilities. Declared control missing read or
/// notify is malformed and never silently downgraded.
fn control_declared(property: &Property<'_>) -> Result<bool, CapabilityError> {
    let access = array(property.value, "access")?
        .iter()
        .map(|value| {
            value
                .as_str()
                .ok_or(CapabilityError::InvalidDescription("access"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    if !access.contains(&"write") {
        return Ok(false);
    }
    for permission in ["read", "notify"] {
        if !access.contains(&permission) {
            return Err(CapabilityError::MissingAccess(permission));
        }
    }
    Ok(true)
}

fn controls<'a>(
    services: impl Iterator<Item = &'a Service<'a>>,
    name: &str,
    type_id: u32,
) -> Result<Vec<(u32, &'a Property<'a>)>, CapabilityError> {
    let mut controls = Vec::new();
    for service in services {
        for property in &service.properties {
            if property.urn.standard("property", name, type_id) && control_declared(property)? {
                controls.push((service.iid, property));
            }
        }
    }
    Ok(controls)
}

fn select_control<'a>(
    controls: Vec<(u32, &'a Property<'a>)>,
    ambiguous: &'static str,
) -> Result<Option<(u32, &'a Property<'a>)>, CapabilityError> {
    match controls.as_slice() {
        [] => Ok(None),
        [candidate] => Ok(Some(*candidate)),
        _ => Err(CapabilityError::InvalidLight(ambiguous)),
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

fn require_full_access(property: &Property<'_>) -> Result<(), CapabilityError> {
    let access = array(property.value, "access")?
        .iter()
        .map(|value| {
            value
                .as_str()
                .ok_or(CapabilityError::InvalidDescription("access"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    for permission in ["read", "write", "notify"] {
        if !access.contains(&permission) {
            return Err(CapabilityError::MissingAccess(permission));
        }
    }
    Ok(())
}

fn numeric_binding(
    siid: u32,
    property: &Property<'_>,
    unit: NumericUnit,
) -> Result<NumericBinding, CapabilityError> {
    require_full_access(property)?;
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

/// The private ls2307 adapter validates its exact source layout and mode enum.
/// Generic parsing already admitted the standard RGB property; this function
/// only adds the vendor-specific mode semantics for the known model.
fn ls2307(
    instance: &Description<'_>,
    main: &Service<'_>,
    power: Option<PropertyBinding>,
    brightness: Option<&NumericBinding>,
    temperature: Option<&NumericBinding>,
    color: Option<&NumericBinding>,
) -> Result<ModeBinding, CapabilityError> {
    let incompatible = || CapabilityError::IncompatibleModel("devcea.light.ls2307");
    let exact =
        |urn: &Urn<'_>| urn.vendor == "devcea-ls2307" && urn.version == 1 && urn.suffix.is_none();
    if !exact(&instance.urn)
        || main.iid != 2
        || !exact(&main.urn)
        || power != Some(PropertyBinding { siid: 2, piid: 1 })
        || brightness
            .is_none_or(|binding| binding.property != (PropertyBinding { siid: 2, piid: 2 }))
        || temperature
            .is_none_or(|binding| binding.property != (PropertyBinding { siid: 2, piid: 3 }))
        || color.is_none_or(|binding| binding.property != (PropertyBinding { siid: 4, piid: 1 }))
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
    let mode_property = standard_property(ambient, "mode", 8)?;
    if mode_property.iid != 2 || !exact(&mode_property.urn) {
        return Err(incompatible());
    }
    require_full_access(mode_property)?;
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
    Ok(ModeBinding {
        property: PropertyBinding { siid: 4, piid: 2 },
        format: NumericFormat::Uint8,
        temperature_value: 0,
        rgb_value: 1,
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
