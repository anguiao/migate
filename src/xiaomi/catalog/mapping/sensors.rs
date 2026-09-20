use super::*;

pub(super) fn compile_temperature_humidity(
    services: &[Service<'_>],
    output: &mut Vec<FeatureDescriptor>,
) {
    let Some(service) = services
        .iter()
        .find(|service| service.kind == "temperature-humidity-sensor")
    else {
        return;
    };
    for (kind, property, role, capability, unit) in [
        (
            "temperature",
            Property::Temperature,
            FeatureRole::TemperatureSensor,
            CapabilityKind::Temperature,
            NumericUnit::Celsius,
        ),
        (
            "relative-humidity",
            Property::Humidity,
            FeatureRole::HumiditySensor,
            CapabilityKind::Humidity,
            NumericUnit::Percent,
        ),
    ] {
        if let Some(wire) = readable_property(service, kind)
            && let Some(range) = numeric_range(wire, unit)
        {
            let mut feature = base_feature(service, role);
            feature
                .definition
                .capabilities
                .0
                .push(capability.value(range));
            feature.binding.properties.push(mapping(
                service.iid,
                wire,
                property,
                ValueCodec::NumberRange {
                    minimum: range.minimum,
                    maximum: range.maximum,
                    step: range.step,
                },
            ));
            attach_battery(services, &mut feature);
            output.push(feature);
        }
    }
}
pub(super) fn compile_motion(
    model: &str,
    services: &[Service<'_>],
    output: &mut Vec<FeatureDescriptor>,
) -> Result<(), CompileError> {
    let Some(service) = services
        .iter()
        .find(|service| service.kind == "motion-sensor")
    else {
        return Ok(());
    };
    if !service
        .events
        .iter()
        .any(|event| event.kind == "motion-detected")
    {
        return Ok(());
    }
    let mut feature = base_feature(service, FeatureRole::MotionSensor);
    feature.definition.capabilities.0.push(Capability::Motion);
    feature
        .definition
        .capabilities
        .0
        .push(Capability::SensingModalities(sensing_modalities(model)));
    if let Some(duration) = readable_property(service, "no-motion-duration")
        .filter(|property| property.unit == Some("seconds"))
        && let Some((minimum, maximum, step)) = duration.range
        && minimum > 0.
        && maximum >= minimum
        && step > 0.
    {
        feature.binding.properties.push(mapping(
            service.iid,
            duration,
            Property::Motion,
            ValueCodec::MotionDuration {
                minimum,
                maximum,
                step,
            },
        ));
    }
    let mut illuminance = base_feature(service, FeatureRole::IlluminanceSensor);
    if let Some(illumination) =
        readable_property(service, "illumination").filter(|property| property.unit == Some("lux"))
        && let Some(range) = numeric_range(illumination, NumericUnit::Lux)
    {
        illuminance
            .definition
            .capabilities
            .0
            .push(Capability::Illuminance(range));
        illuminance.binding.properties.push(mapping(
            service.iid,
            illumination,
            Property::Illuminance,
            ValueCodec::NumberRange {
                minimum: range.minimum,
                maximum: range.maximum,
                step: range.step,
            },
        ));
    }
    attach_battery(services, &mut feature);
    for event in service
        .events
        .iter()
        .filter(|event| event.kind == "motion-detected")
    {
        let arguments = event_argument_mappings(service, event, PropertyClass::Core);
        for argument in &arguments {
            if argument.mapping.property == Property::Illuminance
                && !illuminance
                    .definition
                    .capabilities
                    .0
                    .iter()
                    .any(|capability| matches!(capability, Capability::Illuminance(_)))
                && let ValueCodec::NumberRange {
                    minimum,
                    maximum,
                    step,
                } = argument.mapping.codec
            {
                illuminance
                    .definition
                    .capabilities
                    .0
                    .push(Capability::Illuminance(NumericRange {
                        minimum,
                        maximum,
                        step,
                        unit: NumericUnit::Lux,
                    }));
            }
        }
        feature.binding.events.push(EventMapping {
            siid: service.iid,
            eiid: event.iid,
            argument_iids: event.arguments.clone(),
            argument_count: event.arguments.len(),
            effect: EventEffect::Motion(true),
            arguments: arguments
                .iter()
                .filter(|argument| argument.mapping.property != Property::Illuminance)
                .cloned()
                .collect(),
        });
        let lux_arguments = arguments
            .into_iter()
            .filter(|argument| argument.mapping.property == Property::Illuminance)
            .collect::<Vec<_>>();
        if !lux_arguments.is_empty() {
            illuminance.binding.events.push(EventMapping {
                siid: service.iid,
                eiid: event.iid,
                argument_iids: event.arguments.clone(),
                argument_count: event.arguments.len(),
                effect: EventEffect::ArgumentsOnly,
                arguments: lux_arguments,
            });
        }
    }
    if model == "xiaomi.motion.pir1"
        && let Some(custom) = services.iter().find(|service| service.iid == 5)
        && let Some(event) = custom.events.iter().find(|event| event.iid == 1022)
    {
        feature.binding.events.push(EventMapping {
            siid: custom.iid,
            eiid: event.iid,
            argument_iids: event.arguments.clone(),
            argument_count: event.arguments.len(),
            effect: EventEffect::Motion(false),
            arguments: vec![],
        });
    }
    output.push(feature);
    if illuminance
        .definition
        .capabilities
        .0
        .iter()
        .any(|capability| matches!(capability, Capability::Illuminance(_)))
    {
        attach_battery(services, &mut illuminance);
        output.push(illuminance);
    }
    Ok(())
}
pub(super) fn compile_occupancy(
    model: &str,
    services: &[Service<'_>],
    output: &mut Vec<FeatureDescriptor>,
) {
    let Some(service) = services
        .iter()
        .filter(|service| service.kind == "occupancy-sensor")
        .min_by_key(|service| service.iid)
    else {
        return;
    };
    let Some(status) = readable_property(service, "occupancy-status") else {
        return;
    };
    let mut feature = base_feature(service, FeatureRole::OccupancySensor);
    feature
        .definition
        .capabilities
        .0
        .push(Capability::Occupancy);
    feature
        .definition
        .capabilities
        .0
        .push(Capability::SensingModalities(sensing_modalities(model)));
    feature.binding.properties.push(mapping(
        service.iid,
        status,
        Property::Occupancy,
        ValueCodec::Occupancy {
            vacant: enum_values(status, &["no one", "noshow", "vacant"]),
            occupied: enum_values(status, &["has one", "show", "occupied"]),
        },
    ));
    let mut illuminance = base_feature(service, FeatureRole::IlluminanceSensor);
    if let Some(illumination) =
        readable_property(service, "illumination").filter(|property| property.unit == Some("lux"))
        && let Some(range) = numeric_range(illumination, NumericUnit::Lux)
    {
        illuminance
            .definition
            .capabilities
            .0
            .push(Capability::Illuminance(range));
        illuminance.binding.properties.push(mapping(
            service.iid,
            illumination,
            Property::Illuminance,
            ValueCodec::NumberRange {
                minimum: range.minimum,
                maximum: range.maximum,
                step: range.step,
            },
        ));
    }
    attach_battery(services, &mut feature);
    output.push(feature);
    if !illuminance.binding.properties.is_empty() {
        attach_battery(services, &mut illuminance);
        output.push(illuminance);
    }
}
pub(super) fn compile_contact(services: &[Service<'_>], output: &mut Vec<FeatureDescriptor>) {
    let Some(service) = services
        .iter()
        .find(|service| service.kind == "magnet-sensor")
    else {
        return;
    };
    let Some(status) = readable_property(service, "contact-state")
        .or_else(|| readable_property(service, "status"))
    else {
        return;
    };
    let codec = if status.format == "bool" {
        ValueCodec::ContactBool
    } else {
        ValueCodec::ContactEnum {
            open: enum_values(status, &["open"]),
            closed: enum_values(status, &["closed", "close"]),
        }
    };
    let mut feature = base_feature(service, FeatureRole::ContactSensor);
    feature.definition.capabilities.0.push(Capability::Contact);
    feature
        .binding
        .properties
        .push(mapping(service.iid, status, Property::Contact, codec));
    attach_battery(services, &mut feature);
    output.push(feature);
}
fn event_argument_mappings(
    service: &Service<'_>,
    event: &ActionSpec<'_>,
    class: PropertyClass,
) -> Vec<EventArgumentMapping> {
    // MIoT event arguments refer to property IIDs in their own service. They may
    // have no standalone access, so only this event mapping consumes them.
    event
        .arguments
        .iter()
        .enumerate()
        .filter_map(|(index, iid)| {
            service
                .properties
                .iter()
                .find(|property| property.iid == *iid)
                .map(|property| (index, property))
        })
        .filter_map(|(index, property)| {
            let (core, codec) = match property.kind {
                "illumination" if property.unit == Some("lux") => {
                    let range = numeric_range(property, NumericUnit::Lux)?;
                    (
                        Property::Illuminance,
                        ValueCodec::NumberRange {
                            minimum: range.minimum,
                            maximum: range.maximum,
                            step: range.step,
                        },
                    )
                }
                "occupancy-status" => (
                    Property::Occupancy,
                    ValueCodec::Occupancy {
                        vacant: enum_values(property, &["no one", "noshow", "vacant"]),
                        occupied: enum_values(property, &["has one", "show", "occupied"]),
                    },
                ),
                _ => return None,
            };
            Some(EventArgumentMapping {
                index,
                mapping: PropertyMapping {
                    property: core,
                    siid: service.iid,
                    piid: property.iid,
                    readable: false,
                    notify: false,
                    class,
                    codec,
                },
            })
        })
        .collect()
}

fn sensing_modalities(model: &str) -> Vec<SensingModality> {
    match model {
        "xiaomi.motion.pir1" => vec![SensingModality::Pir],
        "linp.sensor_occupy.hb01" | "izq.sensor_occupy.trio" => {
            vec![SensingModality::Radar]
        }
        "xiaomi.sensor_occupy.03" | "xiaomi.sensor_occupy.p1" => {
            vec![SensingModality::Pir, SensingModality::Radar]
        }
        // Generic models remain explicitly unclassified. Matter represents
        // this protocol-neutral value using its Other modality feature.
        _ => vec![SensingModality::Unspecified],
    }
}
