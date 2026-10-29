//! Pure planning contract tests: versions, explicit operations, metadata-only
//! updates, rebuild causes and exhaustion boundaries.

use super::super::*;
use crate::xiaomi::capability::tests::source::SourceFixture;
use crate::{device::DeviceKey, matter};
use serde_json::json;

const OUTLET_TYPE: &str = "urn:miot-spec-v2:device:outlet:0000A002:vendor-v1:1";
const LIGHT_TYPE: &str = "urn:miot-spec-v2:device:light:0000A001:vendor-v1:1";

fn scope() -> Scope {
    Scope {
        uid: "account".into(),
        home_id: "home".into(),
    }
}

fn other_scope() -> Scope {
    Scope {
        uid: "account".into(),
        home_id: "other".into(),
    }
}

fn key(id: &str) -> DeviceKey {
    DeviceKey::new("xiaomi", id)
}

fn unique(id: &str) -> String {
    let value = id.bytes().fold(1_u128, |value, byte| {
        value.wrapping_mul(31).wrapping_add(byte as u128)
    });
    format!("{value:0>32x}")
}

fn outlet_native(id: &str, siid: u32) -> SourceFixture {
    outlet_native_with_piid(id, siid, 1)
}
fn outlet_native_with_piid(id: &str, siid: u32, piid: u32) -> SourceFixture {
    SourceFixture::new(
        id,
        "vendor.plug",
        json!({"type": OUTLET_TYPE, "services": [{
            "iid": siid, "type": "urn:miot-spec-v2:service:switch:0000780C:vendor-v1:1",
            "properties": [{"iid": piid, "type": "urn:miot-spec-v2:property:on:00000006:vendor-v1:1", "format": "bool", "access": ["read", "write", "notify"]}]
        }]}),
    )
}
fn ct_light_native(id: &str, min: u32, max: u32) -> SourceFixture {
    ct_light_native_with_power(id, min, max, 1)
}
fn ct_light_native_with_power(id: &str, min: u32, max: u32, piid: u32) -> SourceFixture {
    SourceFixture::new(
        id,
        "vendor.light",
        json!({"type": LIGHT_TYPE, "services": [{
            "iid": 2, "type": "urn:miot-spec-v2:service:light:00007802:vendor-v1:1", "properties": [
                {"iid": piid, "type": "urn:miot-spec-v2:property:on:00000006:vendor-v1:1", "format": "bool", "access": ["read", "write", "notify"]},
                {"iid": 3, "type": "urn:miot-spec-v2:property:brightness:0000000D:vendor-v1:1", "format": "uint8", "unit": "percentage", "value-range": [1, 100, 1], "access": ["read", "write", "notify"]},
                {"iid": 4, "type": "urn:miot-spec-v2:property:color-temperature:0000000F:vendor-v1:1", "format": "uint16", "unit": "kelvin", "value-range": [min, max, 1], "access": ["read", "write", "notify"]}
            ]
        }]}),
    )
}

fn committed(
    id: &str,
    native: SourceFixture,
    label: &str,
    configuration_version: u32,
) -> CommittedDevice {
    committed_at(id, native, label, configuration_version, 2)
}

fn committed_at(
    id: &str,
    native: SourceFixture,
    label: &str,
    configuration_version: u32,
    endpoint: u16,
) -> CommittedDevice {
    let functions = native
        .device()
        .functions()
        .iter()
        .enumerate()
        .map(|(index, function)| CommittedFunction {
            id: function.id().to_owned(),
            endpoint: endpoint + 1 + index as u16,
        })
        .collect();
    CommittedDevice {
        key: key(id),
        unique_id: unique(id),
        endpoint,
        label: label.into(),
        configuration_version,
        functions,
        native: Some(native.native),
    }
}

fn historical(id: &str, endpoint: u16) -> CommittedDevice {
    CommittedDevice {
        key: key(id),
        unique_id: unique(id),
        endpoint,
        label: String::new(),
        configuration_version: 1,
        functions: vec![CommittedFunction {
            id: "main".into(),
            endpoint,
        }],
        native: None,
    }
}

#[derive(Clone)]
struct TestSource {
    fixture: SourceFixture,
    name: Option<String>,
    fresh: bool,
}
fn source(id: &str, fixture: SourceFixture, name: Option<&str>) -> TestSource {
    assert_eq!(fixture.raw.key, key(id));
    TestSource {
        fixture,
        name: name.map(str::to_owned),
        fresh: true,
    }
}
fn facts(scope: Scope, devices: Vec<TestSource>, relations: Vec<GroupRelation>) -> SourceFacts {
    SourceFacts {
        scope: Some(scope),
        devices: devices
            .into_iter()
            .map(|source| SourceDevice {
                description: source.fixture.into(),
                name: source.name,
                fresh: source.fresh,
            })
            .collect(),
        relations,
    }
}

#[test]
fn grouping_a_mixed_light_hides_only_control_and_preserves_sensors_and_battery() {
    let light = ct_light_native("member", 2700, 6500);
    let sensor: serde_json::Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/miot/miaomiaoce-t6.json"
    ))
    .unwrap();
    let mut instance = light.raw.instance;
    instance["services"].as_array_mut().unwrap().extend(
        sensor["services"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|service| [3, 4].contains(&service["iid"].as_u64().unwrap()))
            .cloned(),
    );
    let contact: serde_json::Value =
        serde_json::from_str(include_str!("../../../tests/fixtures/miot/isa-dw2hl.json")).unwrap();
    let mut contact = contact["services"][1].clone();
    contact["iid"] = json!(50);
    instance["services"].as_array_mut().unwrap().push(contact);
    let mixed = SourceFixture::new("member", "vendor.light", instance);
    let mut battery = sensor["services"][1].clone();
    battery["iid"] = json!(40);
    let mut instance = mixed.raw.instance;
    instance["services"].as_array_mut().unwrap().push(battery);
    let mixed = SourceFixture::new("member", "vendor.light", instance);
    let source_devices = vec![
        source("member", mixed, None),
        source("group", ct_light_native("group", 2700, 6500), None),
    ];
    let initial = plan(
        CommittedConfiguration::empty(),
        facts(scope(), source_devices.clone(), vec![]),
        BTreeMap::from([
            (key("member"), unique("member")),
            (key("group"), unique("group")),
        ]),
    )
    .unwrap();
    let grouped = plan(
        initial.configuration.clone(),
        facts(
            scope(),
            source_devices,
            vec![GroupRelation {
                group_did: "group".into(),
                record_did: "group".into(),
                model: "vendor.group".into(),
                instance_type: Some(LIGHT_TYPE.into()),
                is_physical: true,
                active: true,
                error: None,
                members: vec!["member".into()],
            }],
        ),
        BTreeMap::new(),
    )
    .unwrap();
    assert!(grouped.derived.published.contains(&key("member")));
    assert!(
        grouped.derived.served[&key("member")]
            .description
            .has_battery()
    );
    assert_eq!(
        grouped
            .derived
            .visible_functions(&key("member"))
            .iter()
            .map(|function| function.id.as_str())
            .collect::<Vec<_>>(),
        ["temperature:3:1001", "humidity:3:1008", "contact:50:2"]
    );
    assert_eq!(
        grouped.derived.served[&key("member")]
            .description
            .functions()
            .len(),
        4
    );
    let before = initial.configuration.device(&key("member")).unwrap();
    let after = grouped.configuration.device(&key("member")).unwrap();
    assert_eq!(before.endpoint, after.endpoint);
    assert_eq!(before.functions, after.functions);
    assert_eq!(
        after.configuration_version,
        before.configuration_version + 1
    );
    grouped.configuration.validate().unwrap();
}

#[test]
fn group_member_resolution_preserves_other_controls_and_rejects_ambiguity() {
    let member = ct_light_native("member", 2700, 6500);
    let fan_source = serde_json::json!({
        "type": "urn:miot-spec-v2:device:fan:0000A005:example:1", "services": [{
            "iid": 3, "type": "urn:miot-spec-v2:service:fan:00007808:example:1", "properties": [
                {"iid": 1, "type": "urn:miot-spec-v2:property:on:00000006:example:1", "format": "bool", "access": ["read", "write", "notify"]},
                {"iid": 2, "type": "urn:miot-spec-v2:property:fan-level:00000016:example:1", "format": "uint8", "access": ["read", "write", "notify"], "value-range": [1, 3, 1]}
            ]
        }]
    });
    let mut instance = member.raw.instance;
    instance["services"]
        .as_array_mut()
        .unwrap()
        .extend(fan_source["services"].as_array().unwrap().iter().cloned());
    let composite = SourceFixture::new("member", "vendor.light", instance);
    let group = ct_light_native("group", 2700, 6500);
    let relation = GroupRelation {
        group_did: "group".into(),
        record_did: "record".into(),
        model: "vendor.group".into(),
        instance_type: Some(LIGHT_TYPE.into()),
        is_physical: false,
        active: true,
        error: None,
        members: vec!["member".into()],
    };
    let configuration = CommittedConfiguration {
        scope: Some(scope()),
        root_version: 1,
        descriptions: vec![composite.raw.clone(), group.raw.clone()],
        devices: vec![
            committed_at("member", composite.clone(), "Member", 1, 2),
            committed_at("group", group.clone(), "Group", 1, 10),
        ],
        groups: vec![relation.clone()],
    };
    let derived = configuration.derive();
    assert!(derived.published.contains(&key("member")));
    assert_eq!(
        derived
            .visible_functions(&key("member"))
            .iter()
            .map(|f| f.id.as_str())
            .collect::<Vec<_>>(),
        ["fan:3"]
    );

    let mut instance = composite.raw.instance;
    let mut second = instance["services"][0].clone();
    second["iid"] = json!(9);
    instance["services"].as_array_mut().unwrap().push(second);
    let ambiguous = SourceFixture::new("member", "vendor.light", instance);
    let configuration = CommittedConfiguration {
        devices: vec![
            committed_at("member", ambiguous, "Member", 1, 2),
            committed_at("group", group, "Group", 1, 10),
        ],
        ..configuration
    };
    let derived = configuration.derive();
    assert!(derived.published.contains(&key("member")));
    assert_eq!(derived.visible_functions(&key("member")).len(), 3);
    assert!(derived.rules["group"].members.is_empty());
    assert!(
        derived.served[&key("group")]
            .repair_reason
            .as_deref()
            .unwrap()
            .contains("ambiguous")
    );
}

#[test]
fn native_rgb_stays_native_without_member_description() {
    let instance: serde_json::Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/miot/devcea-light-ls2307.json"
    ))
    .unwrap();
    let native = SourceFixture::new("group", "devcea.light.ls2307", instance);
    let configuration = CommittedConfiguration {
        scope: Some(scope()),
        root_version: 1,
        descriptions: vec![native.raw.clone()],
        devices: vec![committed("group", native, "Group", 1)],
        groups: vec![GroupRelation {
            group_did: "group".into(),
            record_did: "record".into(),
            model: "vendor.group".into(),
            instance_type: Some(LIGHT_TYPE.into()),
            is_physical: true,
            active: true,
            error: None,
            members: vec!["missing".into()],
        }],
    };
    let derived = configuration.derive();
    let served = &derived.served[&key("group")];
    assert!(served.description.functions()[0].supports_rgb());
    assert!(served.repair.is_none());
    assert!(served.repair_reason.is_none(), "native RGB needs no repair");
}

fn plan(
    current: CommittedConfiguration,
    facts: SourceFacts,
    new_identities: BTreeMap<DeviceKey, String>,
) -> Result<Plan, PlanError> {
    plan_with_revocations(current, facts, new_identities, Vec::new())
}

fn plan_with_revocations(
    current: CommittedConfiguration,
    facts: SourceFacts,
    new_identities: BTreeMap<DeviceKey, String>,
    revocations: Vec<Revocation>,
) -> Result<Plan, PlanError> {
    let current_derived = current.derive();
    super::super::plan(
        PlanInput {
            current: &current,
            current_derived: &current_derived,
            facts,
            revocations,
            new_identities,
        },
        |functions, battery| matter::fixed_shape_with_battery(functions.iter().copied(), battery),
    )
}

#[test]
fn new_identity_starts_at_version_one() {
    let native = outlet_native("outlet", 2);
    let facts = facts(
        scope(),
        vec![source("outlet", native, Some("Kitchen"))],
        Vec::new(),
    );
    let mut identities = BTreeMap::new();
    identities.insert(key("outlet"), unique("outlet"));
    let plan = plan(CommittedConfiguration::empty(), facts, identities).unwrap();
    assert!(plan.changed);
    assert!(plan.rebuild);
    let device = plan.configuration.device(&key("outlet")).unwrap();
    assert_eq!(device.configuration_version, 1);
    assert_eq!(device.endpoint, 2);
    assert_eq!(device.label, "Kitchen");
    assert_eq!(
        device.functions,
        [CommittedFunction {
            id: "outlet:2".into(),
            endpoint: 3
        }]
    );
    assert_eq!(plan.derived.published, BTreeSet::from([key("outlet")]));
    assert_eq!(
        plan.operations,
        [PlanOperation {
            key: key("outlet"),
            kind: OperationKind::Added,
        }]
    );
    assert_eq!(plan.configuration.root_version, 2);
}

#[test]
fn identical_facts_reuse_without_change() {
    let native = outlet_native("outlet", 2);
    let current = CommittedConfiguration {
        scope: Some(scope()),
        root_version: 2,
        descriptions: vec![native.raw.clone()],
        devices: vec![committed("outlet", native.clone(), "Kitchen", 1)],
        groups: Vec::new(),
    };
    let facts = facts(scope(), vec![source("outlet", native, None)], Vec::new());
    let plan = plan(current.clone(), facts, BTreeMap::new()).unwrap();
    assert!(!plan.changed);
    assert!(!plan.rebuild);
    assert_eq!(
        plan.operations,
        [PlanOperation {
            key: key("outlet"),
            kind: OperationKind::Reused,
        }]
    );
    assert_eq!(plan.configuration, current);
}

#[test]
fn hidden_member_republishes_with_a_version_bump() {
    let group_native = ct_light_native("group-1", 2700, 6500);
    let member_native = ct_light_native("member", 2700, 6500);
    let current = CommittedConfiguration {
        scope: Some(scope()),
        root_version: 2,
        descriptions: vec![group_native.raw.clone(), member_native.raw.clone()],
        devices: vec![
            committed("group-1", group_native, "Group", 1),
            committed_at("member", member_native.clone(), "Member", 1, 6),
        ],
        groups: vec![GroupRelation {
            group_did: "group-1".into(),
            record_did: "record-1".into(),
            model: "vendor.group".into(),
            instance_type: Some(LIGHT_TYPE.into()),
            is_physical: true,
            active: true,
            error: None,
            members: vec!["member".into()],
        }],
    };
    let facts = facts(
        scope(),
        vec![source("member", member_native, None)],
        Vec::new(),
    );
    let plan = plan(current, facts, BTreeMap::new()).unwrap();
    assert!(plan.rebuild);
    let member = plan.configuration.device(&key("member")).unwrap();
    assert_eq!(member.configuration_version, 2);
    assert!(plan.derived.published.contains(&key("member")));
    let group = plan.configuration.device(&key("group-1")).unwrap();
    assert_eq!(group.configuration_version, 2);
    assert_eq!(group.native, None);
    assert!(!plan.derived.published.contains(&key("group-1")));
    assert_eq!(plan.configuration.root_version, 3);
}

#[test]
fn urn_only_change_is_reused_without_rebuild_or_a_version_bump() {
    let before = outlet_native("outlet", 2);
    let mut instance = before.raw.instance.clone();
    instance["type"] = "urn:miot-spec-v2:device:outlet:0000A002:vendor-v2:2".into();
    let metadata_only = SourceFixture::new("outlet", "vendor.plug", instance);
    let current = CommittedConfiguration {
        scope: Some(scope()),
        root_version: 2,
        descriptions: vec![before.raw.clone()],
        devices: vec![committed("outlet", before, "Kitchen", 1)],
        groups: Vec::new(),
    };
    let facts = facts(
        scope(),
        vec![source("outlet", metadata_only, None)],
        Vec::new(),
    );
    let plan = plan(current, facts, BTreeMap::new()).unwrap();
    assert!(plan.changed);
    assert!(!plan.rebuild);
    assert_eq!(
        plan.operations,
        [PlanOperation {
            key: key("outlet"),
            kind: OperationKind::Reused,
        }]
    );
    let device = plan.configuration.device(&key("outlet")).unwrap();
    assert_eq!(device.configuration_version, 1);
    assert_eq!(
        device.native.as_ref().unwrap().instance_type(),
        "urn:miot-spec-v2:device:outlet:0000A002:vendor-v2:2"
    );
    assert_eq!(plan.configuration.root_version, 2);
}

#[test]
fn native_control_change_replaces_and_rebuilds_without_a_version_bump() {
    let before = outlet_native("outlet", 2);
    let after = outlet_native_with_piid("outlet", 2, 7);
    let current = CommittedConfiguration {
        scope: Some(scope()),
        root_version: 2,
        descriptions: vec![before.raw.clone()],
        devices: vec![committed("outlet", before, "Kitchen", 1)],
        groups: Vec::new(),
    };
    let facts = facts(scope(), vec![source("outlet", after, None)], Vec::new());
    let plan = plan(current, facts, BTreeMap::new()).unwrap();
    assert!(plan.rebuild);
    assert_eq!(
        plan.operations,
        [PlanOperation {
            key: key("outlet"),
            kind: OperationKind::ReplacedNative,
        }]
    );
    assert_eq!(
        plan.configuration
            .device(&key("outlet"))
            .unwrap()
            .configuration_version,
        1
    );
    assert_eq!(plan.configuration.root_version, 2);
}

#[test]
fn removed_device_unpublishes_and_bumps_its_version() {
    let native = outlet_native("outlet", 2);
    let current = CommittedConfiguration {
        scope: Some(scope()),
        root_version: 2,
        descriptions: vec![native.raw.clone()],
        devices: vec![committed("outlet", native, "Kitchen", 1)],
        groups: Vec::new(),
    };
    let facts = facts(scope(), Vec::new(), Vec::new());
    let plan = plan(current, facts, BTreeMap::new()).unwrap();
    assert!(plan.rebuild);
    assert_eq!(
        plan.operations,
        [PlanOperation {
            key: key("outlet"),
            kind: OperationKind::Removed,
        }]
    );
    let device = plan.configuration.device(&key("outlet")).unwrap();
    assert_eq!(device.configuration_version, 2);
    assert_eq!(device.native, None);
    assert!(plan.derived.published.is_empty());
    assert_eq!(plan.configuration.root_version, 3);
}

#[test]
fn scope_change_alone_does_not_bump_root_or_device_versions() {
    let native = outlet_native("outlet", 2);
    let current = CommittedConfiguration {
        scope: Some(scope()),
        root_version: 2,
        descriptions: vec![native.raw.clone()],
        devices: vec![committed("outlet", native.clone(), "Kitchen", 1)],
        groups: Vec::new(),
    };
    let facts = facts(
        other_scope(),
        vec![source("outlet", native, None)],
        Vec::new(),
    );
    let plan = plan(current, facts, BTreeMap::new()).unwrap();
    assert!(plan.rebuild);
    assert_eq!(
        plan.configuration
            .device(&key("outlet"))
            .unwrap()
            .configuration_version,
        1
    );
    assert_eq!(plan.configuration.root_version, 2);
}

#[test]
fn endpoint_exhaustion_is_explicit() {
    let current = CommittedConfiguration {
        scope: Some(scope()),
        root_version: 1,
        descriptions: Vec::new(),
        devices: vec![historical("history", 65534)],
        groups: Vec::new(),
    };
    let facts = facts(
        scope(),
        vec![source(
            "outlet",
            outlet_native("outlet", 2),
            Some("Kitchen"),
        )],
        Vec::new(),
    );
    let mut identities = BTreeMap::new();
    identities.insert(key("outlet"), unique("outlet"));
    let error = plan(current, facts, identities).unwrap_err();
    assert_eq!(error, PlanError::EndpointExhausted { key: key("outlet") });
}

#[test]
fn version_overflow_is_explicit() {
    let before = ct_light_native("light", 2700, 6500);
    let after = ct_light_native("light", 3000, 6000);
    let mut device = committed("light", before, "Light", u32::MAX);
    device.configuration_version = u32::MAX;
    let current = CommittedConfiguration {
        scope: Some(scope()),
        root_version: 2,
        descriptions: Vec::new(),
        devices: vec![device],
        groups: Vec::new(),
    };
    let facts = facts(scope(), vec![source("light", after, None)], Vec::new());
    let error = plan(current, facts, BTreeMap::new()).unwrap_err();
    assert_eq!(error, PlanError::VersionExhausted { key: key("light") });
}

#[test]
fn empty_saved_label_survives_plan_and_validation() {
    let native = outlet_native("outlet", 2);
    let mut previous = committed("outlet", native.clone(), "", 1);
    previous.label = String::new();
    let current = CommittedConfiguration {
        scope: Some(scope()),
        root_version: 2,
        descriptions: vec![native.raw.clone()],
        devices: vec![previous.clone()],
        groups: Vec::new(),
    };
    assert!(
        current.validate().is_ok(),
        "an allocated identity always has a saved name, even an empty one"
    );
    let facts = facts(
        scope(),
        vec![source("outlet", native, Some("Renamed in Mi Home"))],
        Vec::new(),
    );
    let plan = plan(current, facts, BTreeMap::new()).unwrap();
    assert!(!plan.changed);
    assert_eq!(
        plan.configuration.device(&key("outlet")).unwrap().label,
        previous.label
    );
    assert_eq!(plan.derived.served[&key("outlet")].description.label(), "");
    assert!(plan.configuration.validate().is_ok());
}

#[test]
fn revoked_startup_removal_bumps_versions_against_the_committed_graph() {
    let native = outlet_native("outlet", 2);
    let current = CommittedConfiguration {
        scope: Some(scope()),
        root_version: 2,
        descriptions: vec![native.raw.clone()],
        devices: vec![committed("outlet", native, "Kitchen", 1)],
        groups: Vec::new(),
    };
    // Startup facts exclude the revoked device; the tombstone stays durable.
    let facts = facts(scope(), Vec::new(), Vec::new());
    let plan = plan_with_revocations(
        current,
        facts,
        BTreeMap::new(),
        vec![Revocation {
            key: key("outlet"),
            reason: "confirmed removal".into(),
        }],
    )
    .unwrap();
    assert!(plan.rebuild);
    assert!(plan.derived.published.is_empty());
    assert_eq!(
        plan.operations,
        [PlanOperation {
            key: key("outlet"),
            kind: OperationKind::Removed,
        }]
    );
    assert_eq!(
        plan.configuration
            .device(&key("outlet"))
            .unwrap()
            .configuration_version,
        2
    );
    assert_eq!(plan.configuration.root_version, 3);
}

#[test]
fn stale_revoked_member_cannot_affect_candidate_group_profile_or_shape() {
    let group_native = ct_light_native("group", 2700, 6500);
    let member_native = ct_light_native("member", 3000, 6000);
    let relation = GroupRelation {
        group_did: "group".into(),
        record_did: "record-group".into(),
        model: "vendor.group".into(),
        instance_type: Some(LIGHT_TYPE.into()),
        is_physical: true,
        active: true,
        error: None,
        members: vec!["member".into()],
    };
    let current = CommittedConfiguration {
        scope: Some(scope()),
        root_version: 2,
        descriptions: vec![group_native.raw.clone(), member_native.raw.clone()],
        devices: vec![
            committed("group", group_native.clone(), "Saved group", 1),
            committed_at("member", member_native.clone(), "Member", 1, 6),
        ],
        groups: vec![relation.clone()],
    };
    let mut stale = source("member", member_native, None);
    stale.fresh = false;
    let plan = plan_with_revocations(
        current,
        facts(
            scope(),
            vec![
                source("group", group_native, Some("Directory group")),
                stale,
            ],
            vec![relation],
        ),
        BTreeMap::new(),
        vec![Revocation {
            key: key("member"),
            reason: "confirmed removal".into(),
        }],
    )
    .unwrap();
    let group = &plan.derived.served[&key("group")].description;
    let interval = group.functions()[0].temperature_interval().unwrap();
    assert_eq!((interval.min(), interval.max()), (2700, 6500));
    assert_eq!(group.label(), "Saved group");
    assert!(!plan.derived.served.contains_key(&key("member")));
    assert!(
        plan.configuration
            .device(&key("member"))
            .unwrap()
            .native
            .is_none()
    );
    assert!(plan.clear_revocations.is_empty());
    assert_eq!(
        plan.configuration
            .device(&key("group"))
            .unwrap()
            .configuration_version,
        2
    );
    assert_eq!(plan.configuration.root_version, 3);
    assert!(plan.rebuild);
    plan.configuration.validate().unwrap();
}

#[test]
fn new_identity_imports_a_bounded_utf8_directory_name() {
    let name = "米家厨房吸顶灯超长名称需要按字节边界截断";
    let mut identities = BTreeMap::new();
    identities.insert(key("outlet"), unique("outlet"));
    let facts = facts(
        scope(),
        vec![source("outlet", outlet_native("outlet", 2), Some(name))],
        Vec::new(),
    );
    let plan = plan(CommittedConfiguration::empty(), facts, identities).unwrap();
    let label = &plan.configuration.device(&key("outlet")).unwrap().label;
    assert_eq!(label, "米家厨房吸顶灯超长名");
    assert!(label.len() <= 32);
    assert!(!label.ends_with('\u{fffd}'));
}

#[test]
fn member_reordering_is_canonical_and_does_not_rebuild() {
    let group_native = ct_light_native("group-1", 2700, 6500);
    let member_a = outlet_native("member-a", 2);
    let member_b = outlet_native("member-b", 3);
    let relation = |members: &[&str]| GroupRelation {
        group_did: "group-1".into(),
        record_did: "record-1".into(),
        model: "vendor.group".into(),
        instance_type: Some(LIGHT_TYPE.into()),
        is_physical: true,
        active: true,
        error: None,
        members: members.iter().map(|member| (*member).to_owned()).collect(),
    };
    let current = CommittedConfiguration {
        scope: Some(scope()),
        root_version: 2,
        descriptions: vec![
            group_native.raw.clone(),
            member_a.raw.clone(),
            member_b.raw.clone(),
        ],
        devices: vec![
            committed("group-1", group_native.clone(), "Group", 1),
            committed_at("member-a", member_a.clone(), "A", 1, 6),
            committed_at("member-b", member_b.clone(), "B", 1, 7),
        ],
        groups: vec![relation(&["member-a", "member-b"])],
    };
    let facts = facts(
        scope(),
        vec![
            source("group-1", group_native, None),
            source("member-a", member_a, None),
            source("member-b", member_b, None),
        ],
        vec![relation(&["member-b", "member-a"])],
    );
    let plan = plan(current, facts, BTreeMap::new()).unwrap();
    assert!(
        !plan.changed,
        "member order alone is not a configuration change"
    );
    assert!(!plan.rebuild);
    assert_eq!(
        plan.configuration.groups[0].members,
        ["member-a", "member-b"]
    );
}

#[test]
fn fresh_revocation_clear_replaces_the_native_instance() {
    let native = outlet_native("outlet", 2);
    let current = CommittedConfiguration {
        scope: Some(scope()),
        root_version: 2,
        descriptions: vec![native.raw.clone()],
        devices: vec![committed("outlet", native.clone(), "Kitchen", 1)],
        groups: Vec::new(),
    };
    let facts = facts(scope(), vec![source("outlet", native, None)], Vec::new());
    let plan = plan_with_revocations(
        current,
        facts,
        BTreeMap::new(),
        vec![Revocation {
            key: key("outlet"),
            reason: "confirmed removal".into(),
        }],
    )
    .unwrap();
    assert_eq!(plan.clear_revocations, [key("outlet")]);
    assert!(plan.rebuild);
    assert_eq!(
        plan.operations,
        [PlanOperation {
            key: key("outlet"),
            kind: OperationKind::ReplacedAuthority,
        }]
    );
    assert_eq!(
        plan.configuration
            .device(&key("outlet"))
            .unwrap()
            .configuration_version,
        1
    );
    assert_eq!(plan.configuration.root_version, 2);
}

#[test]
fn scope_change_replaces_the_native_instance_with_the_same_key() {
    let native = outlet_native("outlet", 2);
    let current = CommittedConfiguration {
        scope: Some(scope()),
        root_version: 2,
        descriptions: vec![native.raw.clone()],
        devices: vec![committed("outlet", native.clone(), "Kitchen", 1)],
        groups: Vec::new(),
    };
    let facts = facts(
        other_scope(),
        vec![source("outlet", native, None)],
        Vec::new(),
    );
    let plan = plan(current, facts, BTreeMap::new()).unwrap();
    assert!(plan.rebuild);
    assert_eq!(
        plan.operations,
        [PlanOperation {
            key: key("outlet"),
            kind: OperationKind::ReplacedAuthority,
        }]
    );
    assert_eq!(
        plan.configuration
            .device(&key("outlet"))
            .unwrap()
            .configuration_version,
        1
    );
    assert_eq!(plan.configuration.root_version, 2);
}

#[test]
fn hidden_member_replacement_rebuilds_without_a_version_bump() {
    let group_native = ct_light_native("group-1", 2700, 6500);
    let before = ct_light_native("member", 2700, 6500);
    let after = ct_light_native_with_power("member", 2700, 6500, 7);
    let relation = GroupRelation {
        group_did: "group-1".into(),
        record_did: "record-1".into(),
        model: "vendor.group".into(),
        instance_type: Some(LIGHT_TYPE.into()),
        is_physical: true,
        active: true,
        error: None,
        members: vec!["member".into()],
    };
    let current = CommittedConfiguration {
        scope: Some(scope()),
        root_version: 2,
        descriptions: vec![group_native.raw.clone(), before.raw.clone()],
        devices: vec![
            committed("group-1", group_native.clone(), "Group", 1),
            committed_at("member", before, "Member", 1, 6),
        ],
        groups: vec![relation.clone()],
    };
    let facts = facts(
        scope(),
        vec![
            source("group-1", group_native, None),
            source("member", after, None),
        ],
        vec![relation],
    );
    let plan = plan(current, facts, BTreeMap::new()).unwrap();
    assert!(plan.rebuild);
    assert!(!plan.derived.published.contains(&key("member")));
    assert_eq!(
        plan.configuration
            .device(&key("member"))
            .unwrap()
            .configuration_version,
        1
    );
    assert_eq!(
        plan.operations
            .iter()
            .find(|operation| operation.key == key("member"))
            .unwrap()
            .kind,
        OperationKind::ReplacedNative
    );
}

#[test]
fn group_directory_metadata_change_does_not_rebuild() {
    let group_native = ct_light_native("group-1", 2700, 6500);
    let member_native = outlet_native("member", 2);
    let relation = GroupRelation {
        group_did: "group-1".into(),
        record_did: "record-1".into(),
        model: "vendor.group".into(),
        instance_type: Some(LIGHT_TYPE.into()),
        is_physical: true,
        active: true,
        error: None,
        members: vec!["member".into()],
    };
    let mut renamed = relation.clone();
    renamed.model = "vendor.group.v2".into();
    renamed.instance_type = Some(LIGHT_TYPE.replace(":vendor-v1:1", ":vendor-v2:2"));
    renamed.error = Some("catalog note".into());
    let current = CommittedConfiguration {
        scope: Some(scope()),
        root_version: 2,
        descriptions: vec![group_native.raw.clone(), member_native.raw.clone()],
        devices: vec![
            committed("group-1", group_native.clone(), "Group", 1),
            committed_at("member", member_native.clone(), "Member", 1, 6),
        ],
        groups: vec![relation],
    };
    let facts = facts(
        scope(),
        vec![
            source("group-1", group_native, None),
            source("member", member_native, None),
        ],
        vec![renamed.clone()],
    );
    let plan = plan(current, facts, BTreeMap::new()).unwrap();
    assert!(plan.changed);
    assert!(!plan.rebuild);
    assert!(
        plan.operations
            .iter()
            .all(|operation| { matches!(operation.kind, OperationKind::Reused) })
    );
    assert_eq!(plan.configuration.root_version, 2);
    assert_eq!(plan.configuration.groups, [renamed]);
}

fn state_native_from_instance(model: &str, instance: &serde_json::Value) -> SourceFixture {
    SourceFixture::new("state", model, instance.clone())
}

fn assert_state_semantics_replaced(before: SourceFixture, after: SourceFixture) {
    let initial = plan(
        CommittedConfiguration::empty(),
        facts(
            scope(),
            vec![source("state", before, Some("Entry sensor"))],
            Vec::new(),
        ),
        BTreeMap::from([(key("state"), unique("state"))]),
    )
    .unwrap()
    .configuration;
    let updated = plan(
        initial.clone(),
        facts(
            scope(),
            vec![source("state", after.clone(), None)],
            Vec::new(),
        ),
        BTreeMap::new(),
    )
    .unwrap();
    assert_eq!(
        updated.operations,
        [PlanOperation {
            key: key("state"),
            kind: OperationKind::ReplacedNative,
        }]
    );
    assert!(updated.changed);
    assert!(updated.rebuild);
    assert_eq!(updated.configuration.root_version, initial.root_version);
    let old = initial.device(&key("state")).unwrap();
    let new = updated.configuration.device(&key("state")).unwrap();
    assert_eq!(new.key, old.key);
    assert_eq!(new.unique_id, old.unique_id);
    assert_eq!(new.label, old.label);
    assert_eq!(new.endpoint, old.endpoint);
    assert_eq!(new.functions, old.functions);
    assert_eq!(new.configuration_version, old.configuration_version);
    assert_eq!(new.native.as_ref(), Some(&after.native));
    updated.configuration.validate().unwrap();
}

#[test]
fn native_state_semantics_mapping_change_replaces_runtime_without_changing_publication() {
    let before: serde_json::Value =
        serde_json::from_str(include_str!("../../../tests/fixtures/miot/linp-m1.json")).unwrap();
    let mut after = before.clone();
    // Keep the property and native values; the new description changes their meaning.
    after["services"][1]["properties"][0]["value-list"][0]["description"] =
        serde_json::json!("Closed");
    after["services"][1]["properties"][0]["value-list"][1]["description"] =
        serde_json::json!("Open");
    assert_state_semantics_replaced(
        state_native_from_instance("linp.magnet.m1", &before),
        state_native_from_instance("linp.magnet.m1", &after),
    );
}

#[test]
fn native_state_semantics_event_arguments_change_replaces_runtime_without_changing_publication() {
    let after: serde_json::Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/miot/xiaomi-pir1.json"
    ))
    .unwrap();
    let mut before = after.clone();
    before["services"][1]["events"][1]["arguments"] = serde_json::json!([]);
    assert_state_semantics_replaced(
        state_native_from_instance("xiaomi.motion.pir1", &before),
        state_native_from_instance("xiaomi.motion.pir1", &after),
    );
}
