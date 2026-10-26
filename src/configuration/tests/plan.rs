//! Pure planning contract tests: versions, explicit operations, metadata-only
//! updates, rebuild causes and exhaustion boundaries.

use super::super::*;
use crate::{
    device::{DeviceKey, NumericRange},
    matter::{self, FixedShape},
    xiaomi::capability::{
        BrightnessBinding, LightBindings, NumericFormat, ParsedDevice, ParsedFunction,
        PropertyBinding, TemperatureBinding,
    },
};

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

fn native(
    id: &str,
    model: &str,
    instance_type: &str,
    functions: Vec<ParsedFunction>,
    refresh_trigger: Option<PropertyBinding>,
) -> NativeDescription {
    let device = ParsedDevice::new(key(id), functions, refresh_trigger).expect("valid native");
    NativeDescription::new(model.into(), instance_type.into(), device).unwrap()
}

fn outlet_native(id: &str, siid: u32) -> NativeDescription {
    native(
        id,
        "vendor.plug",
        OUTLET_TYPE,
        vec![ParsedFunction::outlet(
            "main",
            "Main power",
            PropertyBinding { siid, piid: 1 },
        )],
        None,
    )
}

fn ct_light_native(id: &str, min: u32, max: u32) -> NativeDescription {
    let power = PropertyBinding { siid: 2, piid: 1 };
    let brightness = BrightnessBinding::new(
        PropertyBinding { siid: 2, piid: 3 },
        NumericFormat::Uint8,
        NumericRange::new(1, 100, 1).unwrap(),
    )
    .unwrap();
    let temperature = TemperatureBinding::new(
        PropertyBinding { siid: 2, piid: 4 },
        NumericFormat::Uint16,
        NumericRange::new(min, max, 1).unwrap(),
    )
    .unwrap();
    let lighting = LightBindings::new(Some(brightness), Some(temperature), None, None).unwrap();
    native(
        id,
        "vendor.light",
        LIGHT_TYPE,
        vec![ParsedFunction::light("main", "Light", power, lighting)],
        None,
    )
}

fn committed(
    id: &str,
    native: NativeDescription,
    label: &str,
    configuration_version: u32,
) -> CommittedDevice {
    committed_at(id, native, label, configuration_version, 2)
}

fn committed_at(
    id: &str,
    native: NativeDescription,
    label: &str,
    configuration_version: u32,
    endpoint: u16,
) -> CommittedDevice {
    let single = native.device().functions().len() == 1
        && !native.device().functions()[0].kind().is_measurement();
    let functions = native
        .device()
        .functions()
        .iter()
        .enumerate()
        .map(|(index, function)| CommittedFunction {
            id: function.id().to_owned(),
            endpoint: if single {
                endpoint
            } else {
                endpoint + 1 + index as u16
            },
        })
        .collect();
    CommittedDevice {
        key: key(id),
        unique_id: unique(id),
        endpoint,
        label: label.into(),
        configuration_version,
        functions,
        native: Some(native),
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

fn source(id: &str, native: NativeDescription, name: Option<&str>) -> SourceDevice {
    SourceDevice {
        key: key(id),
        native,
        name: name.map(str::to_owned),
        fresh: true,
    }
}

fn facts(scope: Scope, devices: Vec<SourceDevice>, relations: Vec<GroupRelation>) -> SourceFacts {
    SourceFacts {
        scope: Some(scope),
        devices,
        relations,
    }
}

fn shapes(derived: &DerivedConfiguration) -> BTreeMap<DeviceKey, FixedShape> {
    derived
        .served
        .iter()
        .map(|(key, served)| {
            (
                key.clone(),
                matter::fixed_shape_with_battery(
                    &derived
                        .visible_functions(key)
                        .into_iter()
                        .cloned()
                        .collect::<Vec<_>>(),
                    served.description.has_battery(),
                ),
            )
        })
        .collect()
}

#[test]
fn grouping_a_mixed_light_hides_only_control_and_preserves_sensors_and_battery() {
    let light = ct_light_native("member", 2700, 6500);
    let sensor: serde_json::Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/miot/miaomiaoce-t6.json"
    ))
    .unwrap();
    let sensor =
        crate::xiaomi::capability::parse_device_for_model(key("member"), "sample", &sensor)
            .unwrap();
    let mut functions = light.device().functions().to_vec();
    functions.extend_from_slice(sensor.functions());
    let contact: serde_json::Value =
        serde_json::from_str(include_str!("../../../tests/fixtures/miot/isa-dw2hl.json")).unwrap();
    let contact =
        crate::xiaomi::capability::parse_device_for_model(key("member"), "sample", &contact)
            .unwrap();
    functions.extend_from_slice(contact.functions());
    let mixed = NativeDescription::new(
        "vendor.light".into(),
        LIGHT_TYPE.into(),
        ParsedDevice::new_with_battery(key("member"), functions, None, sensor.battery().cloned())
            .unwrap(),
    )
    .unwrap();
    let source_devices = vec![
        source("member", mixed, None),
        source("group", ct_light_native("group", 2700, 6500), None),
    ];
    let initial = plan(input(
        CommittedConfiguration::empty(),
        facts(scope(), source_devices.clone(), vec![]),
        BTreeMap::from([
            (key("member"), unique("member")),
            (key("group"), unique("group")),
        ]),
    ))
    .unwrap();
    let grouped = plan(input(
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
    ))
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
        ["temperature", "humidity", "contact"]
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

fn input(
    current: CommittedConfiguration,
    facts: SourceFacts,
    new_identities: BTreeMap<DeviceKey, String>,
) -> PlanInput {
    input_with(current, facts, new_identities, Vec::new())
}

fn input_with(
    current: CommittedConfiguration,
    facts: SourceFacts,
    new_identities: BTreeMap<DeviceKey, String>,
    revocations: Vec<Revocation>,
) -> PlanInput {
    let current_derived = current.derive();
    PlanInput {
        current_shapes: shapes(&current_derived),
        current,
        current_derived,
        facts_shapes: shapes(&facts.derive()),
        facts,
        revocations,
        new_identities,
    }
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
    let plan = plan(input(CommittedConfiguration::empty(), facts, identities)).unwrap();
    assert!(plan.changed);
    assert!(plan.rebuild);
    let device = plan.configuration.device(&key("outlet")).unwrap();
    assert_eq!(device.configuration_version, 1);
    assert_eq!(device.endpoint, 2);
    assert_eq!(device.label, "Kitchen");
    assert_eq!(
        device.functions,
        [CommittedFunction {
            id: "main".into(),
            endpoint: 2
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
        devices: vec![committed("outlet", native.clone(), "Kitchen", 1)],
        groups: Vec::new(),
    };
    let facts = facts(scope(), vec![source("outlet", native, None)], Vec::new());
    let plan = plan(input(current.clone(), facts, BTreeMap::new())).unwrap();
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
    let member_native = outlet_native("member", 2);
    let current = CommittedConfiguration {
        scope: Some(scope()),
        root_version: 2,
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
    let plan = plan(input(current, facts, BTreeMap::new())).unwrap();
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
    let mut metadata_only = before.clone();
    metadata_only = crate::xiaomi::capability::NativeDescription::new(
        metadata_only.model().to_owned(),
        "urn:miot-spec-v2:device:outlet:0000A002:vendor-v2:2".into(),
        metadata_only.device().clone(),
    )
    .unwrap();
    let current = CommittedConfiguration {
        scope: Some(scope()),
        root_version: 2,
        devices: vec![committed("outlet", before, "Kitchen", 1)],
        groups: Vec::new(),
    };
    let facts = facts(
        scope(),
        vec![source("outlet", metadata_only, None)],
        Vec::new(),
    );
    let plan = plan(input(current, facts, BTreeMap::new())).unwrap();
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
    let after = outlet_native("outlet", 7);
    let current = CommittedConfiguration {
        scope: Some(scope()),
        root_version: 2,
        devices: vec![committed("outlet", before, "Kitchen", 1)],
        groups: Vec::new(),
    };
    let facts = facts(scope(), vec![source("outlet", after, None)], Vec::new());
    let plan = plan(input(current, facts, BTreeMap::new())).unwrap();
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
        devices: vec![committed("outlet", native, "Kitchen", 1)],
        groups: Vec::new(),
    };
    let facts = facts(scope(), Vec::new(), Vec::new());
    let plan = plan(input(current, facts, BTreeMap::new())).unwrap();
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
        devices: vec![committed("outlet", native.clone(), "Kitchen", 1)],
        groups: Vec::new(),
    };
    let facts = facts(
        other_scope(),
        vec![source("outlet", native, None)],
        Vec::new(),
    );
    let plan = plan(input(current, facts, BTreeMap::new())).unwrap();
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
    let error = plan(input(current, facts, identities)).unwrap_err();
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
        devices: vec![device],
        groups: Vec::new(),
    };
    let facts = facts(scope(), vec![source("light", after, None)], Vec::new());
    let error = plan(input(current, facts, BTreeMap::new())).unwrap_err();
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
    let plan = plan(input(current, facts, BTreeMap::new())).unwrap();
    assert!(!plan.changed);
    assert_eq!(
        plan.configuration.device(&key("outlet")).unwrap().label,
        previous.label
    );
    assert!(plan.configuration.validate().is_ok());
}

#[test]
fn revoked_startup_removal_bumps_versions_against_the_committed_graph() {
    let native = outlet_native("outlet", 2);
    let current = CommittedConfiguration {
        scope: Some(scope()),
        root_version: 2,
        devices: vec![committed("outlet", native, "Kitchen", 1)],
        groups: Vec::new(),
    };
    // Startup facts exclude the revoked device; the tombstone stays durable.
    let facts = facts(scope(), Vec::new(), Vec::new());
    let plan = plan(input_with(
        current,
        facts,
        BTreeMap::new(),
        vec![Revocation {
            key: key("outlet"),
            reason: "confirmed removal".into(),
        }],
    ))
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
fn new_identity_imports_a_bounded_utf8_directory_name() {
    let name = "米家厨房吸顶灯超长名称需要按字节边界截断";
    let mut identities = BTreeMap::new();
    identities.insert(key("outlet"), unique("outlet"));
    let facts = facts(
        scope(),
        vec![source("outlet", outlet_native("outlet", 2), Some(name))],
        Vec::new(),
    );
    let plan = plan(input(CommittedConfiguration::empty(), facts, identities)).unwrap();
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
    let plan = plan(input(current, facts, BTreeMap::new())).unwrap();
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
        devices: vec![committed("outlet", native.clone(), "Kitchen", 1)],
        groups: Vec::new(),
    };
    let facts = facts(scope(), vec![source("outlet", native, None)], Vec::new());
    let plan = plan(input_with(
        current,
        facts,
        BTreeMap::new(),
        vec![Revocation {
            key: key("outlet"),
            reason: "confirmed removal".into(),
        }],
    ))
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
        devices: vec![committed("outlet", native.clone(), "Kitchen", 1)],
        groups: Vec::new(),
    };
    let facts = facts(
        other_scope(),
        vec![source("outlet", native, None)],
        Vec::new(),
    );
    let plan = plan(input(current, facts, BTreeMap::new())).unwrap();
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
    let before = outlet_native("member", 2);
    let after = outlet_native("member", 7);
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
    let plan = plan(input(current, facts, BTreeMap::new())).unwrap();
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
    let plan = plan(input(current, facts, BTreeMap::new())).unwrap();
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
