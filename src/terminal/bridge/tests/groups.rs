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
        assert!(
            list.contains(
                "~ group.1 (Reading lamp): members: one (Lamp one), three (Lamp three) (applied: one (Lamp one), two (Lamp two))."
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
        assert!(selected.contains("~ group.1"), "{selected}");
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
        assert!(
            list.contains(
                "~ group.1 (Reading lamp): members: two (Lamp two) (applied: one (Lamp one), two (Lamp two))."
            ),
            "{list}"
        );
        assert!(
            list.contains("confirmed removed devices: one (Lamp one)"),
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
        // The pending summary names the surviving member relation, but only
        // the selected member gets a state block.
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

        let list = handle_line(&registry, &store.devices(), &status, "list", 0)
            .await
            .unwrap()
            .unwrap();
        assert!(
            list.contains(
                "Configuration: confirmed changes are pending application; no committed version is applied."
            ),
            "{list}"
        );
        assert!(
            list.contains(
                "+ group.1 (Reading lamp): new group; members: one (Lamp one), two (Lamp two); capabilities: on/off, brightness."
            ),
            "{list}"
        );

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

#[test]
fn pending_apply_failure_keeps_the_applied_relation_until_a_successful_apply() {
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
        observe(&three, true, Some(90.0), None);
        registry
            .stage_configuration(
                scope(),
                vec![composition("group.1", "Reading lamp", &[&one, &three])],
                Vec::new(),
            )
            .unwrap();

        let failure = "cannot persist the applied configuration: disk is read-only";
        registry.set_apply_error(failure);

        let expected = format!(
            "Last configuration apply failed: {failure}; the applied configuration is still in use, retrying."
        );
        let list = handle_line(&registry, &store.devices(), &status, "list", 0)
            .await
            .unwrap()
            .unwrap();
        assert!(list.contains(&expected), "{list}");
        assert!(
            list.contains(
                "group.1 (Reading lamp): group; members: one (Lamp one), two (Lamp two)\n  Capabilities: on/off, brightness.\n  State: power on; brightness 40%; representative: unknown; reachable: true."
            ),
            "{list}"
        );
        // The candidate member was observed on at 90%; only the applied
        // members aggregate while the apply keeps failing.
        assert!(!list.contains("State: power on; brightness 90%"), "{list}");
        assert!(!list.contains("State: power on; brightness 65%"), "{list}");
        // Reading the diagnostic never clears it.
        assert_eq!(registry.apply_error().as_deref(), Some(failure));

        let selected = handle_line(&registry, &store.devices(), &status, "status group.1", 0)
            .await
            .unwrap()
            .unwrap();
        assert!(selected.contains(&expected), "{selected}");
        assert!(
            selected.contains(
                "  State: power on; brightness 40%; representative: unknown; reachable: true."
            ),
            "{selected}"
        );
        assert_eq!(registry.apply_error().as_deref(), Some(failure));

        // A successful commit clears the diagnostic and only then switches
        // the applied relation and aggregate.
        registry.clear_apply_error();
        registry
            .apply_configuration(
                2,
                vec![configuration("group.1", "Reading lamp", &[&one, &three])],
                &[],
                &[],
            )
            .unwrap();
        let recovered = handle_line(&registry, &store.devices(), &status, "list", 0)
            .await
            .unwrap()
            .unwrap();
        assert!(
            !recovered.contains("Last configuration apply failed"),
            "{recovered}"
        );
        assert!(
            recovered.contains(
                "group.1 (Reading lamp): group; members: one (Lamp one), three (Lamp three)"
            ),
            "{recovered}"
        );
        assert!(recovered.contains("brightness 65%"), "{recovered}");
        assert!(
            recovered.contains("three (Lamp three): available\n  Group: group.1 (Reading lamp)."),
            "{recovered}"
        );
        assert_eq!(registry.apply_error(), None);
    });
}

#[test]
fn an_apply_error_without_a_pending_configuration_is_not_shown() {
    block_on(async {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let status = status_for(&directory);
        let registry = DeviceRegistry::new();
        let capabilities = vec![Capability::OnOff, Capability::Brightness];
        let (one, _one_receiver) = light("one", "Lamp one", capabilities.clone());
        let (two, _two_receiver) = light("two", "Lamp two", capabilities);
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
        observe(&one, true, Some(40.0), None);
        observe(&two, false, None, None);
        // The registry can still hold the last failure while nothing is
        // pending; the terminal must not present it as the current state.
        registry.set_apply_error("cannot persist the applied configuration: disk is read-only");

        for line in ["list", "status"] {
            let output = handle_line(&registry, &store.devices(), &status, line, 0)
                .await
                .unwrap()
                .unwrap();
            assert!(
                !output.contains("Last configuration apply failed"),
                "{line}: {output}"
            );
            assert!(!output.contains("Configuration:"), "{line}: {output}");
        }
        let group = handle_line(&registry, &store.devices(), &status, "status group.1", 0)
            .await
            .unwrap()
            .unwrap();
        assert!(
            !group.contains("Last configuration apply failed"),
            "{group}"
        );
        assert!(
            group.contains("  State: power on; brightness 40%"),
            "{group}"
        );
        // Reading the diagnostic does not clear it.
        assert!(registry.apply_error().is_some());
    });
}

#[test]
fn pending_summary_only_names_groups_whose_observable_fields_changed() {
    block_on(async {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let status = status_for(&directory);
        let registry = DeviceRegistry::new();
        let capabilities = vec![Capability::OnOff, Capability::Brightness];
        let (one, _one_receiver) = light("one", "Lamp one", capabilities.clone());
        let (two, _two_receiver) = light("two", "Lamp two", capabilities.clone());
        let (three, _three_receiver) = light("three", "Lamp three", capabilities.clone());
        let (four, _four_receiver) = light("four", "Lamp four", capabilities);
        for device in [&one, &two, &three, &four] {
            registry.insert(device.clone());
        }
        registry
            .apply_configuration(
                1,
                vec![
                    configuration("group.1", "Reading lamp", &[&one, &two]),
                    configuration("group.2", "Hall lamp", &[&three, &four]),
                ],
                &[],
                &[],
            )
            .unwrap();
        observe(&one, true, Some(40.0), None);

        // The complete candidate set is staged again; only group.2 changed.
        registry
            .stage_configuration(
                scope(),
                vec![
                    composition("group.1", "Reading lamp", &[&one, &two]),
                    composition("group.2", "Cloud rename", &[&three]),
                ],
                Vec::new(),
            )
            .unwrap();

        let list = handle_line(&registry, &store.devices(), &status, "list", 0)
            .await
            .unwrap()
            .unwrap();
        assert!(
            list.contains("~ group.2 (Hall lamp): members: three (Lamp three) (applied: four (Lamp four), three (Lamp three))."),
            "{list}"
        );
        assert!(!list.contains("~ group.1"), "{list}");
        assert!(!list.contains("source instances changed"), "{list}");

        let unchanged = handle_line(&registry, &store.devices(), &status, "status group.1", 0)
            .await
            .unwrap()
            .unwrap();
        assert!(
            unchanged.contains("group.1 (Reading lamp): relation and description unchanged."),
            "{unchanged}"
        );
        assert!(
            !unchanged.contains("source instances changed"),
            "{unchanged}"
        );

        let changed = handle_line(&registry, &store.devices(), &status, "status group.2", 0)
            .await
            .unwrap()
            .unwrap();
        assert!(
            changed.contains("~ group.2 (Hall lamp): members: three (Lamp three) (applied: four (Lamp four), three (Lamp three))."),
            "{changed}"
        );
    });
}

#[test]
fn pending_dissolution_is_visible_on_the_applied_group_status() {
    block_on(async {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let status = status_for(&directory);
        let registry = DeviceRegistry::new();
        let capabilities = vec![Capability::OnOff, Capability::Brightness];
        let (one, _one_receiver) = light("one", "Lamp one", capabilities.clone());
        let (two, _two_receiver) = light("two", "Lamp two", capabilities);
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
        registry
            .stage_configuration(scope(), Vec::new(), Vec::new())
            .unwrap();

        let selected = handle_line(&registry, &store.devices(), &status, "status group.1", 0)
            .await
            .unwrap()
            .unwrap();
        assert!(
            selected.contains(
                "Configuration: confirmed changes are pending application; applied version 1."
            ),
            "{selected}"
        );
        assert!(
            selected.contains("- group.1 (Reading lamp): group removed."),
            "{selected}"
        );
        assert!(
            selected
                .contains("group.1 (Reading lamp): group; members: one (Lamp one), two (Lamp two)"),
            "{selected}"
        );
    });
}

#[test]
fn moving_a_member_shows_both_the_old_and_new_relations_on_status() {
    block_on(async {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let status = status_for(&directory);
        let registry = DeviceRegistry::new();
        let capabilities = vec![Capability::OnOff, Capability::Brightness];
        let (one, _one_receiver) = light("one", "Lamp one", capabilities.clone());
        let (two, _two_receiver) = light("two", "Lamp two", capabilities.clone());
        let (three, _three_receiver) = light("three", "Lamp three", capabilities.clone());
        let (four, _four_receiver) = light("four", "Lamp four", capabilities);
        for device in [&one, &two, &three, &four] {
            registry.insert(device.clone());
        }
        registry
            .apply_configuration(
                1,
                vec![
                    configuration("group.1", "Reading lamp", &[&one, &two]),
                    configuration("group.2", "Hall lamp", &[&three, &four]),
                ],
                &[],
                &[],
            )
            .unwrap();
        registry
            .stage_configuration(
                scope(),
                vec![
                    composition("group.1", "Reading lamp", &[&two]),
                    composition("group.2", "Hall lamp", &[&three, &four, &one]),
                ],
                Vec::new(),
            )
            .unwrap();

        let member = handle_line(&registry, &store.devices(), &status, "status one", 0)
            .await
            .unwrap()
            .unwrap();
        assert!(
            member.contains(
                "~ group.1 (Reading lamp): members: two (Lamp two) (applied: one (Lamp one), two (Lamp two))."
            ),
            "{member}"
        );
        assert!(
            member.contains(
                "~ group.2 (Hall lamp): members: four (Lamp four), one (Lamp one), three (Lamp three) (applied: four (Lamp four), three (Lamp three))."
            ),
            "{member}"
        );
    });
}

#[test]
fn pending_derived_capability_change_is_summarized_against_the_applied_shape() {
    block_on(async {
        use crate::device::NumericRange;
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let status = status_for(&directory);
        let registry = DeviceRegistry::new();
        let (one, _one_receiver) = light(
            "one",
            "Lamp one",
            vec![Capability::OnOff, Capability::Brightness],
        );
        let (two, _two_receiver) = light(
            "two",
            "Lamp two",
            vec![
                Capability::OnOff,
                Capability::Brightness,
                Capability::ColorTemperature(NumericRange {
                    min: 2700,
                    max: 6500,
                    step: 1,
                }),
                Capability::ColorRgb,
            ],
        );
        let (four, _four_receiver) = light("four", "Lamp four", vec![Capability::OnOff]);
        registry.insert(one.clone());
        registry.insert(two.clone());
        registry.insert(four.clone());
        registry
            .apply_configuration(
                1,
                vec![configuration("group.1", "Reading lamp", &[&one, &two])],
                &[],
                &[],
            )
            .unwrap();
        registry
            .stage_configuration(
                scope(),
                vec![Composition {
                    key: key("group.1"),
                    label: "Reading lamp".into(),
                    members: vec![key("one"), key("four")],
                }],
                Vec::new(),
            )
            .unwrap();

        let list = handle_line(&registry, &store.devices(), &status, "list", 0)
            .await
            .unwrap()
            .unwrap();
        assert!(
            list.contains(
                "~ group.1 (Reading lamp): members: four (Lamp four), one (Lamp one) (applied: one (Lamp one), two (Lamp two)); capabilities: on/off (applied: on/off, brightness)."
            ),
            "{list}"
        );
        assert!(
            list.contains(
                "group.1 (Reading lamp): group; members: one (Lamp one), two (Lamp two)\n  Capabilities: on/off, brightness."
            ),
            "{list}"
        );
    });
}
