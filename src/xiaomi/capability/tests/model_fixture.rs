//! Synthetic source inputs for tests that start from logical device shapes.

use super::source::SourceFixture;
use crate::device::{FunctionKind, LightProfile, PhysicalDeviceDescription};
use serde_json::{Value, json};

pub(crate) fn native_for(description: &PhysicalDeviceDescription) -> SourceFixture {
    let mut services = std::collections::BTreeMap::<u32, Value>::new();
    for function in description.functions() {
        let (siid, name, kind, properties) = match function.kind {
            FunctionKind::Outlet => {
                let siid = match function.id.as_str() {
                    "outlet_1" | "outlet:3" => 3,
                    "outlet_2" | "outlet:4" => 4,
                    "usb" | "outlet:9" => 9,
                    _ => 2,
                };
                (siid, "switch", 0x780c, vec![power()])
            }
            FunctionKind::Light(profile) => {
                let mut properties = vec![power()];
                if profile != LightProfile::OnOff {
                    properties.push(numeric(
                        3,
                        "brightness",
                        0xd,
                        "uint8",
                        "percentage",
                        json!([1, 100, 1]),
                    ));
                }
                if let LightProfile::ColorTemperature(interval)
                | LightProfile::ExtendedColor(interval) = profile
                {
                    properties.push(numeric(
                        4,
                        "color-temperature",
                        0xf,
                        "uint16",
                        "kelvin",
                        json!([interval.min(), interval.max(), 1]),
                    ));
                }
                if matches!(profile, LightProfile::ExtendedColor(_)) {
                    properties.push(numeric(
                        5,
                        "color",
                        0xe,
                        "uint32",
                        "rgb",
                        json!([0, 16777215, 1]),
                    ));
                }
                (2, "light", 0x7802, properties)
            }
            FunctionKind::Fan {
                speed_count,
                horizontal_swing: swing,
            } => {
                let mut properties = vec![
                    power(),
                    numeric(
                        2,
                        "fan-level",
                        0x16,
                        "uint8",
                        "none",
                        json!([1, speed_count, 1]),
                    ),
                ];
                if swing {
                    properties.push(property(3, "horizontal-swing", 0x17, "bool"));
                }
                (2, "fan", 0x7808, properties)
            }
            FunctionKind::Curtain => {
                let input: Value = serde_json::from_str(include_str!(
                    "../../../../tests/fixtures/miot/xiaomi-acn010.json"
                ))
                .unwrap();
                let service = input["services"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|service| {
                        service["type"]
                            .as_str()
                            .unwrap()
                            .contains(":service:curtain:")
                    })
                    .unwrap()
                    .clone();
                services.insert(2, service);
                continue;
            }
            FunctionKind::Temperature(range)
            | FunctionKind::Humidity(range)
            | FunctionKind::CarbonDioxide(range)
            | FunctionKind::Illuminance(range) => {
                let (siid, piid, name, id, unit) = match function.kind {
                    FunctionKind::Temperature(_) => (3, 1001, "temperature", 0x20, "celsius"),
                    FunctionKind::Humidity(_) => (3, 1008, "relative-humidity", 0xc, "percentage"),
                    FunctionKind::CarbonDioxide(_) => (8, 1029, "co2-density", 0x4b, "ppm"),
                    _ => (42, 1, "illumination", 0x4e, "lux"),
                };
                (
                    siid,
                    "environment",
                    0x780a,
                    vec![numeric(
                        piid,
                        name,
                        id,
                        "float",
                        unit,
                        json!([range.min(), range.max(), 1]),
                    )],
                )
            }
            FunctionKind::Contact => (
                50,
                "magnet-sensor",
                0x7827,
                vec![property(1, "contact-state", 0x7c, "bool")],
            ),
            FunctionKind::Motion => {
                let mut service = service(
                    51,
                    "motion-sensor",
                    0x7825,
                    vec![numeric(
                        1,
                        "no-motion-duration",
                        0xcb,
                        "uint16",
                        "seconds",
                        json!([0, 65535, 1]),
                    )],
                );
                service["events"] = json!([{"iid": 1, "type": urn("event", "motion-detected", 0x5001), "arguments": []}]);
                services.insert(51, service);
                continue;
            }
            FunctionKind::Occupancy => {
                let mut state = property(1, "occupancy-status", 0x166, "uint8");
                state["value-list"] = json!([
                    {"value": 0, "description": "No One"},
                    {"value": 1, "description": "Has Someone"},
                    {"value": 2, "description": "Unknown"}
                ]);
                (52, "occupancy-sensor", 0x78c0, vec![state])
            }
        };
        services
            .entry(siid)
            .or_insert_with(|| service(siid, name, kind, vec![]))["properties"]
            .as_array_mut()
            .unwrap()
            .extend(properties);
    }
    if description.has_battery() {
        let siid = if services.contains_key(&2) { 99 } else { 2 };
        services.insert(
            siid,
            service(
                siid,
                "battery",
                0x7805,
                vec![numeric(
                    1003,
                    "battery-level",
                    0x14,
                    "uint8",
                    "percentage",
                    json!([0, 100, 1]),
                )],
            ),
        );
    }
    let (category, kind) = if description
        .functions()
        .iter()
        .any(|function| function.kind == FunctionKind::Outlet)
    {
        ("outlet", 0xa002)
    } else if description
        .functions()
        .iter()
        .any(|function| function.kind.is_light())
    {
        ("light", 0xa001)
    } else {
        ("temperature-humidity-sensor", 0xa00a)
    };
    SourceFixture::new(
        &description.key().id,
        "test.composite",
        json!({
            "type": urn("device", category, kind),
            "services": services.into_values().collect::<Vec<_>>()
        }),
    )
}

fn urn(entity: &str, name: &str, kind: u32) -> String {
    format!("urn:miot-spec-v2:{entity}:{name}:{kind:08X}:test-v1:1")
}
fn property(iid: u32, name: &str, kind: u32, format: &str) -> Value {
    json!({"iid": iid, "type": urn("property", name, kind), "format": format, "access": ["read", "write", "notify"]})
}
fn power() -> Value {
    property(1, "on", 6, "bool")
}
fn numeric(iid: u32, name: &str, kind: u32, format: &str, unit: &str, range: Value) -> Value {
    let mut property = property(iid, name, kind, format);
    property["unit"] = unit.into();
    property["value-range"] = range;
    property
}
fn service(iid: u32, name: &str, kind: u32, properties: Vec<Value>) -> Value {
    json!({"iid": iid, "type": urn("service", name, kind), "properties": properties})
}
