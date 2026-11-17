use super::*;
use crate::device::DeviceKey;
use serde_json::{Value, json};

const MODEL: &str = "yeelink.bhf_light.v13";
const HEATER: &str = "heater:3";

fn v13() -> Value {
    serde_json::from_str(include_str!(
        "../../../../tests/fixtures/miot/yeelink-bhf-light-v13.json"
    ))
    .unwrap()
}

fn parse(instance: &Value) -> Result<crate::xiaomi::capability::ParsedDevice, CapabilityError> {
    crate::xiaomi::capability::parse_device_for_model(
        DeviceKey::new("xiaomi", "bath"),
        MODEL,
        instance,
    )
}

fn heater_service(instance: &mut Value) -> &mut Value {
    instance["services"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|service| service["iid"] == 3)
        .unwrap()
}

fn change_vendor(value: &mut Value) {
    let urn = value["type"].as_str().unwrap();
    let prefix = urn.split(':').take(5).collect::<Vec<_>>().join(":");
    value["type"] = json!(format!("{prefix}:other-vendor:1"));
}

fn property(service: &mut Value, iid: u32) -> &mut Value {
    service["properties"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|property| property["iid"] == iid)
        .unwrap()
}

#[test]
fn public_v13_description_yields_lightning_identity_and_only_heating_thermostat() {
    let parsed = parse(&v13()).unwrap();
    assert_eq!(
        parsed
            .functions()
            .iter()
            .map(|function| (function.id(), function.label()))
            .collect::<Vec<_>>(),
        [
            ("light:2", "Light"),
            (HEATER, "暖风"),
            ("fan:3:2", "吹风"),
            ("fan:3:4", "换气"),
        ]
    );
    let heater = parsed
        .functions()
        .iter()
        .find(|function| function.id() == HEATER)
        .unwrap();
    assert_eq!(heater.power(), Some(PropertyBinding { siid: 3, piid: 3 }));
    assert_eq!(
        heater.bindings(),
        [
            PropertyBinding { siid: 3, piid: 3 },
            PropertyBinding { siid: 3, piid: 8 },
            PropertyBinding { siid: 3, piid: 1 },
            PropertyBinding { siid: 3, piid: 5 },
        ]
    );
    assert_eq!(
        heater.kind().capabilities(),
        [
            crate::device::Capability::OnOff,
            crate::device::Capability::Heater,
        ]
    );
}

#[test]
fn invalid_heating_or_target_temperature_descriptions_are_not_admitted() {
    type Mutation = Box<dyn Fn(&mut Value)>;
    let cases: Vec<(&str, Mutation)> = vec![
        (
            "missing heating",
            Box::new(|instance| {
                heater_service(instance)["properties"]
                    .as_array_mut()
                    .unwrap()
                    .retain(|property| property["iid"] != 3);
            }),
        ),
        (
            "heating boolean format",
            Box::new(|instance| {
                property(heater_service(instance), 3)["format"] = json!("uint8");
            }),
        ),
        (
            "heating write access",
            Box::new(|instance| {
                property(heater_service(instance), 3)["access"] = json!(["read", "notify"]);
            }),
        ),
        (
            "missing target temperature",
            Box::new(|instance| {
                heater_service(instance)["properties"]
                    .as_array_mut()
                    .unwrap()
                    .retain(|property| property["iid"] != 8);
            }),
        ),
        (
            "celsius target",
            Box::new(|instance| {
                property(heater_service(instance), 8)["unit"] = json!("fahrenheit");
            }),
        ),
        (
            "integral target",
            Box::new(|instance| {
                let target = property(heater_service(instance), 8);
                target["format"] = json!("float");
                target["value-range"] = json!([25.0, 45.0, 1.0]);
            }),
        ),
        (
            "temperature range shape",
            Box::new(|instance| {
                property(heater_service(instance), 8)["value-list"] = json!([
                    {"value": 25, "description": "25"},
                    {"value": 45, "description": "45"}
                ]);
            }),
        ),
        (
            "ambiguous heating",
            Box::new(|instance| {
                let service = heater_service(instance);
                let mut duplicate = property(service, 3).clone();
                duplicate["iid"] = json!(9);
                service["properties"]
                    .as_array_mut()
                    .unwrap()
                    .push(duplicate);
            }),
        ),
        (
            "ambiguous target temperature",
            Box::new(|instance| {
                let service = heater_service(instance);
                let mut duplicate = property(service, 8).clone();
                duplicate["iid"] = json!(9);
                service["properties"]
                    .as_array_mut()
                    .unwrap()
                    .push(duplicate);
            }),
        ),
    ];
    for (name, mutate) in cases {
        let mut instance = v13();
        mutate(&mut instance);
        let parsed = parse(&instance).unwrap_or_else(|error| panic!("{name}: {error}"));
        assert!(
            parsed
                .functions()
                .iter()
                .all(|function| function.id() != HEATER),
            "{name}"
        );
        let diagnostics = parsed.recognition().diagnostics();
        assert!(
            diagnostics
                .iter()
                .any(|issue| issue.target() == HEATER && issue.reason() != "unsupported device"),
            "{name}: {diagnostics:?}"
        );
    }
}

#[test]
fn another_standard_ptc_service_uses_the_same_contract() {
    let mut instance = v13();
    // Recognition is anchored on the standard service and property semantics,
    // not on the representative model, vendor suffix, or description strings.
    for service in instance["services"].as_array_mut().unwrap() {
        service["description"] = json!("Unknown");
        if let Some(properties) = service["properties"].as_array_mut() {
            for property in properties {
                property["description"] = json!("Unknown");
            }
        }
        change_vendor(service);
        for property in service["properties"].as_array_mut().unwrap() {
            change_vendor(property);
        }
    }
    let parsed = crate::xiaomi::capability::parse_device_for_model(
        DeviceKey::new("xiaomi", "bath"),
        "other.vendor.ptc",
        &instance,
    )
    .unwrap();
    let ids = parsed
        .functions()
        .iter()
        .map(|function| function.id())
        .collect::<Vec<_>>();
    assert!(ids.contains(&HEATER));
}

#[test]
fn heating_thermostat_uses_integer_temperature_encoding() {
    let parsed = parse(&v13()).unwrap();
    let heater = parsed
        .functions()
        .iter()
        .find_map(|function| function.heater())
        .unwrap();
    let target = heater.temperature();
    assert_eq!(target.encode(26), json!(26));
    assert!(target.encode(26).is_i64());
    assert_eq!(target.read_value(&json!(26)), Ok(26));
}
