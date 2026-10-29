//! Publication of synthetic source capabilities through real configuration planning.
use super::*;
use crate::configuration::{
    self, CommittedConfiguration, Plan, PlanInput, Scope, SourceDevice, SourceFacts,
};
use crate::xiaomi::capability::tests::source::SourceFixture;
use serde_json::{Value, json};
use std::collections::BTreeMap;

fn instance(composite: bool) -> Value {
    let light = |iid| {
        json!({"iid": iid, "type": "urn:miot-spec-v2:service:light:00007802:example:1", "properties": [
            {"iid": 1, "type": "urn:miot-spec-v2:property:on:00000006:example:1", "format": "bool", "access": ["read", "write", "notify"]}
        ]})
    };
    let mut services = vec![
        light(2),
        json!({"iid": 8, "type": "urn:miot-spec-v2:service:battery:00007805:example:1", "properties": [
            {"iid": 1, "type": "urn:miot-spec-v2:property:battery-level:00000014:example:1", "format": "uint8", "access": ["read", "notify"], "unit": "percentage", "value-range": [0, 100, 1]}
        ]}),
    ];
    if composite {
        services.push(json!({"iid": 3, "type": "urn:miot-spec-v2:service:fan:00007808:example:1", "properties": [
            {"iid": 1, "type": "urn:miot-spec-v2:property:on:00000006:example:1", "format": "bool", "access": ["read", "write", "notify"]},
            {"iid": 2, "type": "urn:miot-spec-v2:property:fan-level:00000016:example:1", "format": "uint8", "access": ["read", "write", "notify"], "value-range": [1, 3, 1]}
        ]}));
        services.push(light(4));
    }
    json!({"type": "urn:miot-spec-v2:device:air-conditioner:0000A004:example:1", "services": services})
}

fn plan(current: &CommittedConfiguration, value: Value) -> Plan {
    let key = DeviceKey::new("xiaomi", "composite");
    let description = SourceFixture::new("composite", "example.composite", value).into();
    let derived = current.derive();
    let facts = SourceFacts {
        scope: Some(Scope {
            uid: "account".into(),
            home_id: "home".into(),
        }),
        devices: vec![SourceDevice {
            description,
            name: Some("Composite".into()),
            fresh: true,
        }],
        relations: vec![],
    };
    let new_identities = if current.device(&key).is_some() {
        BTreeMap::new()
    } else {
        BTreeMap::from([(key, "0123456789abcdef0123456789abcdef".into())])
    };
    configuration::plan(
        PlanInput {
            current,
            current_derived: &derived,
            facts,
            revocations: vec![],
            new_identities,
        },
        |functions, battery| {
            crate::matter::fixed_shape_with_battery(functions.iter().copied(), battery)
        },
    )
    .unwrap()
}

#[test]
fn composition_keeps_existing_endpoint_and_publishes_each_child_and_shared_battery() {
    let first = plan(&CommittedConfiguration::empty(), instance(false)).configuration;
    let key = DeviceKey::new("xiaomi", "composite");
    let old = first.device(&key).unwrap();
    let old_parent = old.endpoint;
    let old_light = old.functions[0].endpoint;
    let next = plan(&first, instance(true));
    let saved = next.configuration.device(&key).unwrap();
    assert_eq!(saved.endpoint, old_parent);
    assert_eq!(
        saved
            .functions
            .iter()
            .find(|f| f.id == "light:2")
            .unwrap()
            .endpoint,
        old_light
    );
    assert_eq!(saved.functions.len(), 3);
    let (physical, _receivers) = PhysicalDevice::new(next.derived.served[&key].description.clone());
    let endpoints = saved
        .functions
        .iter()
        .map(|f| (f.id.as_str(), f.endpoint))
        .collect::<Vec<_>>();
    let model = model_handler(vec![entry(&physical, saved.endpoint, &endpoints)]);
    model.access(|node| {
        let parent = node.endpoint(saved.endpoint).unwrap();
        assert!(parent.cluster(0x39).is_some());
        assert!(parent.cluster(0x2f).is_some());
        assert!(parent.cluster(6).is_none());
        assert!(parent.cluster(0x202).is_none());
        for function in &saved.functions {
            let child = node.endpoint(function.endpoint).unwrap();
            assert!(child.cluster(6).is_some());
            assert!(child.cluster(0x2f).is_none());
            assert_eq!(child.cluster(0x202).is_some(), function.id == "fan:3");
        }
    });
}
