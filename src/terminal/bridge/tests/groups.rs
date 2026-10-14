//! Applied-group and pending-configuration presentation tests.
//!
//! Every case drives `handle_line` over one registry snapshot. The rendered
//! group shape and aggregate must come from the applied handle, while member
//! blocks read only fresh observations and source diagnostics.

use super::{handle_line, light, publish_diagnostics, report, status_for};
use crate::{
    device::{
        Capability, Composition, ConfigurationScope, DeviceKey, DeviceRegistry, GroupConfiguration,
        LightPatch, PhysicalDevice, derive_group_capabilities,
    },
    storage::Store,
    terminal::AuthStatus,
    xiaomi::adapter::{AdapterStatus, DeviceStatus, GatewayStatus, PathKind},
};
use futures_lite::future::{block_on, poll_once};

fn key(id: &str) -> DeviceKey {
    DeviceKey::new("xiaomi", id)
}

fn scope() -> ConfigurationScope {
    ConfigurationScope {
        uid: "uid".into(),
        home_id: "home".into(),
        group_id: "scope".into(),
    }
}

fn configuration(group_key: &str, label: &str, members: &[&PhysicalDevice]) -> GroupConfiguration {
    let descriptions = members
        .iter()
        .map(|member| member.functions()[0].description().clone())
        .collect::<Vec<_>>();
    GroupConfiguration {
        key: key(group_key),
        label: label.into(),
        members: members.iter().map(|member| member.key().clone()).collect(),
        capabilities: derive_group_capabilities(&descriptions),
    }
}

fn composition(group_key: &str, label: &str, members: &[&PhysicalDevice]) -> Composition {
    Composition {
        key: key(group_key),
        label: label.into(),
        members: members.iter().map(|member| member.key().clone()).collect(),
    }
}

fn device_status(did: &str, unavailable: Option<&str>) -> DeviceStatus {
    DeviceStatus {
        did: did.into(),
        usable_control_paths: if unavailable.is_some() {
            Vec::new()
        } else {
            vec![PathKind::Cloud]
        },
        selected_control: if unavailable.is_some() {
            None
        } else {
            Some(PathKind::Cloud)
        },
        notification_source: if unavailable.is_some() {
            None
        } else {
            Some(PathKind::Local)
        },
        unavailable_reason: unavailable.map(str::to_owned),
        sync_limited_reason: None,
        last_switch_reason: None,
        commands: Vec::new(),
    }
}

fn diagnostics(status: &AuthStatus, devices: Vec<DeviceStatus>) {
    status.update_adapter(AdapterStatus {
        authentication: report(),
        gateway: GatewayStatus::Connected("hub".into()),
        catalog_problem: None,
        pending: Vec::new(),
        routing: Default::default(),
        devices,
    });
}

fn observe(
    device: &PhysicalDevice,
    power: bool,
    brightness: Option<f64>,
    temperature: Option<u32>,
) {
    device.functions()[0]
        .observe_light(LightPatch {
            power: Some(power),
            brightness_percent: brightness,
            temperature_kelvin: temperature,
            ..Default::default()
        })
        .unwrap();
}

#[test]
fn pending_membership_keeps_the_applied_shape_and_aggregate() {
    block_on(async {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let status = status_for(&directory);
        let registry = DeviceRegistry::new();
        let capabilities = vec![Capability::OnOff, Capability::Brightness];
        let (one, _one_receiver) = light("one", "Lamp one", capabilities.clone());
        let (two, _two_receiver) = light("two", "Lamp two", capabilities.clone());
        let (three, _three_receiver) = light("three", "Lamp three", capabilities);
        registry.insert(one.clone());
        registry.insert(two.clone());
        registry.insert(three.clone());
        registry
            .apply_configuration(
                1,
                vec![configuration("group.1", "Reading lamp", &[&one, &two])],
                &[key("three")],
                &[],
            )
            .unwrap();
        observe(&one, true, Some(40.0), None);
        observe(&two, false, None, None);
        registry
            .stage_configuration(
                scope(),
                vec![composition("group.1", "Reading lamp", &[&one, &three])],
                Vec::new(),
            )
            .unwrap();

        let snapshot = registry.snapshot();
        assert_eq!(
            snapshot.applied.groups[0].members,
            vec![key("one"), key("two")]
        );
        assert_eq!(
            snapshot.pending.as_ref().unwrap().groups[0].members,
            vec![key("one"), key("three")]
        );

        let list = handle_line(&registry, &store.devices(), &status, "list", 0)
            .await
            .unwrap()
            .unwrap();
        assert!(
            list.contains(
                "group.1 (Reading lamp): group; members: one (Lamp one), two (Lamp two)\n  Capabilities: on/off, brightness.\n  State: power on; brightness 40%; representative: unknown; reachable: true."
            ),
            "{list}"
        );
        // The candidate member keeps its applied standalone block; it never
        // replaces the applied group member set.
        assert!(list.contains("three (Lamp three): available"), "{list}");

        let selected = handle_line(&registry, &store.devices(), &status, "status group.1", 0)
            .await
            .unwrap()
            .unwrap();
        assert!(
            selected.contains(
                "  State: power on; brightness 40%; representative: unknown; reachable: true."
            ),
            "{selected}"
        );
    });
}

#[test]
fn same_key_replacement_keeps_the_closed_handle_unknown_until_applied() {
    for grouped in [false, true] {
        block_on(async {
            let directory = tempfile::tempdir().unwrap();
            let store = Store::open(directory.path()).unwrap();
            let status = status_for(&directory);
            let registry = DeviceRegistry::new();
            let capabilities = vec![Capability::OnOff, Capability::Brightness];
            let (one, _one_receiver) = light("one", "Lamp one", capabilities.clone());
            let (two, _two_receiver) = light("two", "Lamp two", capabilities.clone());
            registry.insert(one.clone());
            registry.insert(two.clone());
            registry
                .apply_configuration(
                    1,
                    if grouped {
                        vec![configuration("group.1", "Reading lamp", &[&one, &two])]
                    } else {
                        Vec::new()
                    },
                    if grouped {
                        &[]
                    } else {
                        std::slice::from_ref(one.key())
                    },
                    &[],
                )
                .unwrap();
            observe(&one, true, Some(55.0), None);

            let (replacement, _replacement_receiver) = light("one", "Lamp one", capabilities);
            observe(&replacement, false, Some(10.0), None);
            registry.revoke(replacement.key());
            registry.insert(replacement);
            publish_diagnostics(&status, &registry);

            let list = handle_line(&registry, &store.devices(), &status, "list", 0)
                .await
                .unwrap()
                .unwrap();
            if grouped {
                assert!(
            list.contains(
                "  State: power unknown; brightness unknown; representative: unknown; reachable: true."
            ),
            "{list}"
        );
            }
            assert!(!list.contains("power on"), "{list}");
            assert!(!list.contains("main (Light): off"), "{list}");

            let member = handle_line(&registry, &store.devices(), &status, "status one", 0)
                .await
                .unwrap()
                .unwrap();
            assert!(member.contains("one (Lamp one): closed"), "{member}");
            assert!(member.contains("main (Light): unknown"), "{member}");
            assert!(!member.contains("main (Light): off"), "{member}");
        });
    }
}

#[test]
fn pending_deletion_renders_a_revoked_member_without_panicking() {
    block_on(async {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let status = status_for(&directory);
        let registry = DeviceRegistry::new();
        let capabilities = vec![Capability::OnOff, Capability::Brightness];
        let (one, _one_receiver) = light("one", "Lamp one", capabilities.clone());
        let (two, _two_receiver) = light("two", "Lamp two", capabilities);
        store.devices().publish(&one.description()).unwrap();
        registry.insert(one.clone());
        registry.insert(two.clone());
        registry
            .apply_configuration(
                1,
                vec![configuration("group.1", "Reading lamp", &[&one, &two])],
                &[],
                &[],
            )
            .unwrap();
        observe(&one, true, Some(30.0), None);
        observe(&two, true, Some(70.0), None);

        // A confirmed deletion revokes the source path immediately while the
        // applied group keeps its old member handle until the apply boundary.
        registry.revoke(&key("one"));
        registry
            .stage_configuration(
                scope(),
                vec![composition("group.1", "Reading lamp", &[&two])],
                vec![key("one")],
            )
            .unwrap();
        diagnostics(
            &status,
            vec![
                device_status("one", Some("cloud offline")),
                device_status("two", None),
            ],
        );

        let snapshot = registry.snapshot();
        assert!(
            snapshot
                .devices
                .iter()
                .all(|device| device.key().id != "one")
        );
        assert_eq!(
            snapshot.applied.groups[0].members,
            vec![key("one"), key("two")]
        );

        let list = handle_line(&registry, &store.devices(), &status, "list", 0)
            .await
            .unwrap()
            .unwrap();
        assert!(
            list.contains("group.1 (Reading lamp): group; members: one (Lamp one), two (Lamp two)"),
            "{list}"
        );
        assert!(
            list.contains("one (Lamp one): removed from source"),
            "{list}"
        );
        assert!(
            list.contains(
                "  State: power on; brightness unknown; representative: unknown; reachable: true."
            ),
            "{list}"
        );

        let selected = handle_line(&registry, &store.devices(), &status, "status group.1", 0)
            .await
            .unwrap()
            .unwrap();
        assert!(
            selected.contains("one (Lamp one): removed from source"),
            "{selected}"
        );
        assert!(selected.contains("one routing:"), "{selected}");
        assert!(
            selected.contains("Control unavailable: cloud offline."),
            "{selected}"
        );
        assert!(selected.contains("two routing:"), "{selected}");

        let member = handle_line(&registry, &store.devices(), &status, "status one", 0)
            .await
            .unwrap()
            .unwrap();
        assert!(
            member.contains("one (Lamp one): removed from source"),
            "{member}"
        );
        assert!(!member.contains("two (Lamp two): available"), "{member}");
    });
}

#[test]
fn group_state_formats_declared_values_and_unknowns() {
    use crate::device::{
        FunctionKey, GroupDescription, GroupSnapshot, LightMode, LightSnapshot, NumericRange, Power,
    };
    let description = GroupDescription {
        key: key("group.1"),
        label: "Reading lamp".into(),
        capabilities: vec![
            Capability::OnOff,
            Capability::Brightness,
            Capability::ColorTemperature(NumericRange {
                min: 2700,
                max: 6500,
                step: 1,
            }),
            Capability::ColorRgb,
        ],
    };
    let mut snapshot = GroupSnapshot {
        power: Power::On,
        lighting: Some(LightSnapshot {
            brightness_percent: Some(50.0),
            temperature_kelvin: Some(4000),
            color: None,
            mode: Some(LightMode::Temperature),
        }),
        reachable: true,
        representative: Some(FunctionKey::new(key("one"), "main")),
        members: Vec::new(),
    };
    assert_eq!(
        super::super::group_state(&description, &snapshot),
        "power on; brightness 50%; temperature 4000 K; RGB: unknown; mode temperature; representative: one; reachable: true"
    );
    snapshot.power = Power::Unknown;
    snapshot.lighting = None;
    snapshot.representative = None;
    snapshot.reachable = false;
    assert_eq!(
        super::super::group_state(&description, &snapshot),
        "power unknown; brightness unknown; temperature unknown; RGB: unknown; mode unknown; representative: unknown; reachable: false"
    );
}

#[test]
fn shared_member_status_lists_all_groups_once_in_pending_and_applied_views() {
    block_on(async {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let status = status_for(&directory);
        let registry = DeviceRegistry::new();
        let (one, _one_receiver) = light("one", "Lamp one", vec![Capability::OnOff]);
        registry.insert(one.clone());
        registry
            .stage_configuration(
                scope(),
                vec![
                    composition("group.1", "Reading lamp", &[&one]),
                    composition("group.2", "Room lamp", &[&one]),
                ],
                Vec::new(),
            )
            .unwrap();
        let pending = handle_line(&registry, &store.devices(), &status, "status one", 0)
            .await
            .unwrap()
            .unwrap();
        assert!(
            pending.contains("candidate member of group.1, group.2 (waiting to apply)"),
            "{pending}"
        );
        registry
            .apply_configuration(
                1,
                vec![
                    configuration("group.1", "Reading lamp", &[&one]),
                    configuration("group.2", "Room lamp", &[&one]),
                ],
                &[],
                &[],
            )
            .unwrap();
        let applied = handle_line(&registry, &store.devices(), &status, "status one", 0)
            .await
            .unwrap()
            .unwrap();
        assert!(
            applied.contains("Groups: group.1 (Reading lamp), group.2 (Room lamp)."),
            "{applied}"
        );
        assert_eq!(
            applied.matches("one (Lamp one): available").count(),
            1,
            "{applied}"
        );
        assert_eq!(
            applied.matches("main (Light): unknown").count(),
            1,
            "{applied}"
        );
    });
}

#[test]
fn a_lone_pending_candidate_is_shown_as_not_published_and_never_commands_members() {
    block_on(async {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let status = status_for(&directory);
        let registry = DeviceRegistry::new();
        let capabilities = vec![Capability::OnOff, Capability::Brightness];
        let (one, mut one_receiver) = light("one", "Lamp one", capabilities.clone());
        let (two, mut two_receiver) = light("two", "Lamp two", capabilities);
        registry.insert(one.clone());
        registry.insert(two.clone());
        registry
            .stage_configuration(
                scope(),
                vec![composition("group.1", "Reading lamp", &[&one, &two])],
                Vec::new(),
            )
            .unwrap();

        let selected = handle_line(&registry, &store.devices(), &status, "status group.1", 0)
            .await
            .unwrap()
            .unwrap();
        assert!(
            selected.contains("group.1 (Reading lamp): waiting to apply; not published."),
            "{selected}"
        );
        assert!(
            selected.contains("  Capabilities: on/off, brightness."),
            "{selected}"
        );
        assert!(
            selected.contains("  Members: one (Lamp one), two (Lamp two)."),
            "{selected}"
        );
        assert!(!selected.contains("command accepted"), "{selected}");

        for line in ["on group.1", "off group.1"] {
            let output = handle_line(&registry, &store.devices(), &status, line, 0)
                .await
                .unwrap()
                .unwrap();
            assert!(output.contains("waiting to apply"), "{line}: {output}");
            assert!(output.contains("not published"), "{line}: {output}");
            assert!(!output.contains("command accepted"), "{line}: {output}");
            assert!(poll_once(one_receiver.next()).await.is_none(), "{line}");
            assert!(poll_once(two_receiver.next()).await.is_none(), "{line}");
        }
    });
}

#[test]
fn applied_without_a_candidate_never_claims_a_pending_apply() {
    block_on(async {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let status = status_for(&directory);
        let registry = DeviceRegistry::new();
        let (one, _one_receiver) = light(
            "one",
            "Lamp one",
            vec![Capability::OnOff, Capability::Brightness],
        );
        registry.insert(one.clone());
        registry
            .apply_configuration(1, Vec::new(), &[key("one")], &[])
            .unwrap();
        observe(&one, true, Some(25.0), None);
        publish_diagnostics(&status, &registry);

        let list = handle_line(&registry, &store.devices(), &status, "list", 0)
            .await
            .unwrap()
            .unwrap();
        assert!(list.contains("one (Lamp one): available"), "{list}");
        assert!(!list.contains("waiting to apply"), "{list}");
        assert!(!list.contains("Configuration:"), "{list}");
    });
}
