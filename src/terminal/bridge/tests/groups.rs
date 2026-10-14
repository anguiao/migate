//! Applied-group and pending-configuration presentation tests.
//!
//! Every case drives `handle_line` over one registry snapshot. The rendered
//! group shape and aggregate must come from the applied handle, while member
//! blocks read only fresh observations and source diagnostics.

use super::{Terminal, light, publish_diagnostics, report, status_for};
use crate::{
    device::{
        Capability, CommandError, Composition, ConfigurationScope, DeviceKey, DeviceRegistry,
        GroupConfiguration, LightPatch, PhysicalDevice, derive_group_capabilities,
    },
    lighting::LightingExecutor,
    storage::Store,
    terminal::AuthStatus,
    xiaomi::adapter::{AdapterStatus, DeviceStatus, GatewayStatus, PathKind},
};
use futures_lite::future::{self, block_on, poll_once};
use std::rc::Rc;

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
        let terminal = Terminal::new(&registry, &store.devices(), &status);
        assert_eq!(
            snapshot.applied.groups[0].members,
            vec![key("one"), key("two")]
        );
        assert_eq!(
            snapshot.pending.as_ref().unwrap().groups[0].members,
            vec![key("one"), key("three")]
        );

        let list = terminal.handle_line("list", 0).await.unwrap().unwrap();
        assert!(
            list.contains(
                "group.1 (Reading lamp): group; members: one (Lamp one), two (Lamp two)\n  Capabilities: on/off, brightness.\n  State: power on; brightness 40%; representative: unknown; reachable: true."
            ),
            "{list}"
        );
        assert!(
            list.contains(
                "Configuration: confirmed changes are pending application; applied version 1."
            ),
            "{list}"
        );
        // The candidate member keeps its applied standalone block; it never
        // replaces the applied group member set.
        assert!(list.contains("three (Lamp three): available"), "{list}");

        let selected = terminal
            .handle_line("status group.1", 0)
            .await
            .unwrap()
            .unwrap();
        assert!(
            selected.contains(
                "  State: power on; brightness 40%; representative: unknown; reachable: true."
            ),
            "{selected}"
        );
        assert!(
            selected.contains("confirmed changes are pending application"),
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
            let terminal = Terminal::new(&registry, &store.devices(), &status);
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

            let list = terminal.handle_line("list", 0).await.unwrap().unwrap();
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

            let member = terminal
                .handle_line("status one", 0)
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
        let terminal = Terminal::new(&registry, &store.devices(), &status);
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

        let list = terminal.handle_line("list", 0).await.unwrap().unwrap();
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

        let selected = terminal
            .handle_line("status group.1", 0)
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

        let member = terminal
            .handle_line("status one", 0)
            .await
            .unwrap()
            .unwrap();
        assert!(
            member.contains("one (Lamp one): removed from source"),
            "{member}"
        );
        // Only the selected member gets a state block.
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
        let terminal = Terminal::new(&registry, &store.devices(), &status);
        let pending = terminal
            .handle_line("status one", 0)
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
        let applied = terminal
            .handle_line("status one", 0)
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
        let terminal = Terminal::new(&registry, &store.devices(), &status);
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

        let list = terminal.handle_line("list", 0).await.unwrap().unwrap();
        assert!(
            list.contains(
                "Configuration: confirmed changes are pending application; no committed version is applied."
            ),
            "{list}"
        );
        assert!(
            list.contains("group.1 (Reading lamp): waiting to apply; not published."),
            "{list}"
        );

        let selected = terminal
            .handle_line("status group.1", 0)
            .await
            .unwrap()
            .unwrap();
        assert!(
            selected.contains("group.1 (Reading lamp): waiting to apply; not published."),
            "{selected}"
        );
        assert!(!selected.contains("command accepted"), "{selected}");

        for line in ["on group.1", "off group.1"] {
            let output = terminal.handle_line(line, 0).await.unwrap().unwrap();
            assert!(output.contains("waiting to apply"), "{line}: {output}");
            assert!(output.contains("not published"), "{line}: {output}");
            assert!(!output.contains("command accepted"), "{line}: {output}");
            assert!(poll_once(one_receiver.next()).await.is_none(), "{line}");
            assert!(poll_once(two_receiver.next()).await.is_none(), "{line}");
        }
    });
}

#[test]
fn pending_apply_failure_keeps_the_applied_relation_until_a_successful_apply() {
    block_on(async {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let status = status_for(&directory);
        let registry = DeviceRegistry::new();
        let terminal = Terminal::new(&registry, &store.devices(), &status);
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
        let list = terminal.handle_line("list", 0).await.unwrap().unwrap();
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

        let selected = terminal
            .handle_line("status group.1", 0)
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
        let recovered = terminal.handle_line("list", 0).await.unwrap().unwrap();
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
        let terminal = Terminal::new(&registry, &store.devices(), &status);
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
            let output = terminal.handle_line(line, 0).await.unwrap().unwrap();
            assert!(
                !output.contains("Last configuration apply failed"),
                "{line}: {output}"
            );
            assert!(!output.contains("Configuration:"), "{line}: {output}");
        }
        let group = terminal
            .handle_line("status group.1", 0)
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

/// A production-shaped session with a private executor the module test can
/// still drive to finish member requests between command lines.
fn injecting_terminal<'a>(
    registry: &DeviceRegistry,
    store: &Store,
    status: &'a AuthStatus,
) -> (Terminal<'a>, Rc<LightingExecutor>) {
    let executor = LightingExecutor::new(Rc::new(crate::lighting::clock::SystemClock::new()));
    let terminal = Terminal::with_executor(registry, &store.devices(), status, executor.clone());
    (terminal, executor)
}

fn applied_pair(registry: &DeviceRegistry, one: &PhysicalDevice, two: &PhysicalDevice) {
    registry
        .apply_configuration(
            1,
            vec![configuration("group.1", "Reading lamp", &[one, two])],
            &[],
            &[],
        )
        .unwrap();
}

/// Poll the group command once and require that it is still waiting for member
/// outcomes. The fanout has already submitted every member by this point.
async fn pending_command<'a>(
    terminal: &'a Terminal<'a>,
    line: &'a str,
) -> std::pin::Pin<
    Box<
        dyn std::future::Future<Output = Result<Option<String>, crate::storage::StorageError>> + 'a,
    >,
> {
    let mut command = Box::pin(terminal.handle_line(line, 0));
    assert!(
        poll_once(command.as_mut()).await.is_none(),
        "{line}: group command returned before any member outcome"
    );
    command
}

#[test]
fn applied_group_on_returns_after_one_acceptance_and_the_other_member_continues() {
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
        applied_pair(&registry, &one, &two);
        let (terminal, executor) = injecting_terminal(&registry, &store, &status);

        let command = pending_command(&terminal, "on group.1").await;
        let request = poll_once(one_receiver.next())
            .await
            .expect("first member dispatched")
            .unwrap();
        assert!(request.target);
        request.accept().unwrap();
        let result = command.await.unwrap().unwrap();
        assert!(result.contains("at least one member accepted"), "{result}");

        // The response does not claim that every member has accepted.
        let pending_status = terminal
            .handle_line("status group.1", 0)
            .await
            .unwrap()
            .unwrap();
        assert!(
            pending_status.contains("State: power on; brightness unknown;"),
            "{pending_status}"
        );

        // The response and its waiter are gone; the second member keeps
        // executing through its normal control chain.
        let request = poll_once(two_receiver.next())
            .await
            .expect("second member dispatched")
            .unwrap();
        assert!(request.target);
        request.accept().unwrap();
        executor.advance();

        let settled = terminal
            .handle_line("status group.1", 0)
            .await
            .unwrap()
            .unwrap();
        assert!(
            settled.contains("State: power on; brightness unknown;"),
            "{settled}"
        );
    });
}

#[test]
fn applied_group_command_reports_the_overall_error_when_every_member_fails() {
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
        applied_pair(&registry, &one, &two);
        let (terminal, _executor) = injecting_terminal(&registry, &store, &status);

        let command = pending_command(&terminal, "off group.1").await;
        let request = poll_once(one_receiver.next())
            .await
            .expect("first member dispatched")
            .unwrap();
        request.reject(CommandError::Unavailable("gateway offline".into()));
        let request = poll_once(two_receiver.next())
            .await
            .expect("second member dispatched")
            .unwrap();
        request.reject(CommandError::Unavailable("gateway offline".into()));
        let result = command.await.unwrap().unwrap();
        assert!(
            result.contains("all members failed: device unavailable: gateway offline"),
            "{result}"
        );
    });
}

#[test]
fn group_member_manual_control_targets_only_that_member() {
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
        applied_pair(&registry, &one, &two);
        let (terminal, _executor) = injecting_terminal(&registry, &store, &status);

        let (result, ()) = future::zip(terminal.handle_line("on one", 0), async {
            let request = one_receiver.next().await.unwrap();
            request.accept().unwrap();
        })
        .await;
        assert_eq!(result.unwrap().unwrap(), "one: command accepted");
        // The other member of the published group is never part of the
        // standalone command.
        assert!(poll_once(two_receiver.next()).await.is_none());
        assert_eq!(
            one.functions()[0].snapshot().power,
            crate::device::Power::On
        );
    });
}

#[test]
fn consecutive_group_commands_keep_both_member_writes_alive() {
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
        applied_pair(&registry, &one, &two);
        let (terminal, executor) = injecting_terminal(&registry, &store, &status);

        for _ in 0..2 {
            let command = pending_command(&terminal, "on group.1").await;
            let request = poll_once(one_receiver.next())
                .await
                .expect("first member dispatched")
                .unwrap();
            assert!(request.target);
            request.accept().unwrap();
            let result = command.await.unwrap().unwrap();
            assert!(result.contains("at least one member accepted"), "{result}");
        }

        // Both line triggers committed their own member write to the second
        // member, in order, even though the first response already returned.
        for _ in 0..2 {
            let request = poll_once(two_receiver.next())
                .await
                .expect("second member keeps its own write")
                .unwrap();
            assert!(request.target);
            request.accept().unwrap();
            executor.advance();
        }

        assert_eq!(
            two.functions()[0].snapshot().power,
            crate::device::Power::On
        );
    });
}
