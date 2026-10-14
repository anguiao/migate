use super::{new_device, outlet};
use crate::device::{
    Availability, Capability, Category, Command, CommandReceiver, CompositionError, DeviceKey,
    DeviceRegistry, FunctionDescription, GroupConfiguration, PhysicalDevice,
    PhysicalDeviceDescription,
};
use futures_lite::future::{block_on, poll_once};

fn light(key: &str, label: &str) -> PhysicalDeviceDescription {
    PhysicalDeviceDescription {
        key: DeviceKey::new("xiaomi", key),
        label: label.into(),
        functions: vec![FunctionDescription {
            id: "main".into(),
            label: "Main light".into(),
            category: Category::Light,
            capabilities: vec![Capability::OnOff],
        }],
    }
}

fn ready_light(key: &str, label: &str) -> (PhysicalDevice, CommandReceiver) {
    let (device, mut receivers) = PhysicalDevice::new(light(key, label));
    device.functions()[0].observe(false);
    device.set_availability(Availability::Available);
    (device, receivers.remove(0))
}

fn group(key: &str, label: &str, members: &[&PhysicalDevice]) -> GroupConfiguration {
    GroupConfiguration {
        capabilities: vec![Capability::OnOff],
        key: DeviceKey::new("xiaomi", key),
        label: label.into(),
        members: members.iter().map(|member| member.key().clone()).collect(),
    }
}

fn apply_groups(
    registry: &DeviceRegistry,
    version: u64,
    groups: Vec<GroupConfiguration>,
) -> Result<(), CompositionError> {
    registry.apply_configuration(version, groups, &[], &[])
}

fn relations(
    snapshot: &crate::device::RegistrySnapshot,
) -> Vec<(crate::device::GroupDescription, Vec<DeviceKey>)> {
    snapshot
        .applied
        .groups
        .iter()
        .map(|group| (group.description(), group.members.clone()))
        .collect()
}

#[test]
fn composition_update_rejects_empty_groups_and_device_key_collisions() {
    let registry = DeviceRegistry::new();
    let (first, _first_receiver) = ready_light("first", "First");
    let (second, _second_receiver) = ready_light("second", "Second");
    registry.insert(first.clone());
    registry.insert(second.clone());

    assert_eq!(
        apply_groups(
            &registry,
            1,
            vec![GroupConfiguration {
                capabilities: vec![Capability::OnOff],
                key: DeviceKey::new("xiaomi", "empty"),
                label: "Empty".into(),
                members: Vec::new(),
            }]
        ),
        Err(CompositionError::EmptyGroup(DeviceKey::new(
            "xiaomi", "empty"
        )))
    );
    assert_eq!(
        apply_groups(&registry, 1, vec![group("first", "Collides", &[&second])]),
        Err(CompositionError::GroupKeyConflict(DeviceKey::new(
            "xiaomi", "first"
        )))
    );
    let snapshot = registry.snapshot();
    assert!(snapshot.applied.groups.is_empty());
    assert_eq!(snapshot.devices.len(), 2);
    assert_eq!(snapshot.revision, 2);
}

#[test]
fn composition_update_rejects_malformed_member_relations() {
    let registry = DeviceRegistry::new();
    let (first, _first_receiver) = ready_light("first", "First");
    let (second, _second_receiver) = ready_light("second", "Second");
    let (third, _third_receiver) = ready_light("third", "Third");
    let (outlet_device, _outlet_receiver) = new_device(outlet());
    let mut multi = light("multi", "Multi");
    multi.functions.push(FunctionDescription {
        id: "second".into(),
        label: "Second".into(),
        category: Category::Outlet,
        capabilities: vec![Capability::OnOff],
    });
    let (multi_device, _multi_receiver) = new_device(multi);
    let mut auxiliary = light("auxiliary", "Auxiliary");
    auxiliary.functions[0].id = "aux".into();
    let (auxiliary_device, _auxiliary_receiver) = new_device(auxiliary);
    for device in [
        first.clone(),
        second.clone(),
        third.clone(),
        outlet_device.clone(),
        multi_device.clone(),
        auxiliary_device.clone(),
    ] {
        registry.insert(device);
    }

    let cases = [
        (
            vec![GroupConfiguration {
                capabilities: vec![Capability::OnOff],
                key: DeviceKey::new("xiaomi", "missing"),
                label: "Missing".into(),
                members: vec![DeviceKey::new("xiaomi", "absent")],
            }],
            CompositionError::MissingMember(DeviceKey::new("xiaomi", "absent")),
        ),
        (
            vec![group("outlet-group", "Outlet", &[&outlet_device])],
            CompositionError::UnsupportedMember(DeviceKey::new("xiaomi", "test-outlet")),
        ),
        (
            vec![group("multi-group", "Multi", &[&multi_device])],
            CompositionError::UnsupportedMember(DeviceKey::new("xiaomi", "multi")),
        ),
        (
            vec![group("aux-group", "Aux", &[&auxiliary_device])],
            CompositionError::UnsupportedMember(DeviceKey::new("xiaomi", "auxiliary")),
        ),
        (
            vec![GroupConfiguration {
                capabilities: vec![Capability::OnOff],
                key: DeviceKey::new("xiaomi", "self"),
                label: "Self".into(),
                members: vec![DeviceKey::new("xiaomi", "self")],
            }],
            CompositionError::SelfMember(DeviceKey::new("xiaomi", "self")),
        ),
        (
            vec![GroupConfiguration {
                capabilities: vec![Capability::OnOff],
                key: DeviceKey::new("xiaomi", "duplicate"),
                label: "Duplicate".into(),
                members: vec![first.key().clone(), first.key().clone()],
            }],
            CompositionError::DuplicateMember {
                group: DeviceKey::new("xiaomi", "duplicate"),
                member: DeviceKey::new("xiaomi", "first"),
            },
        ),
        (
            vec![
                GroupConfiguration {
                    capabilities: vec![Capability::OnOff],
                    key: DeviceKey::new("xiaomi", "nested-one"),
                    label: "Nested".into(),
                    members: vec![DeviceKey::new("xiaomi", "nested-two")],
                },
                group("nested-two", "Nested", &[&first]),
            ],
            CompositionError::NestedGroup(DeviceKey::new("xiaomi", "nested-two")),
        ),
    ];
    for (groups, expected) in cases {
        let before = registry.snapshot();
        assert_eq!(apply_groups(&registry, 1, groups), Err(expected));
        let after = registry.snapshot();
        assert_eq!(relations(&after), relations(&before));
        assert_eq!(after.applied.confirmed, before.applied.confirmed);
        assert_eq!(after.revision, before.revision);
        assert_eq!(after.devices.len(), before.devices.len());
    }
}

#[test]
fn invalid_composition_update_leaves_revision_event_and_relations_unchanged() {
    block_on(async {
        let registry = DeviceRegistry::new();
        let (first, _first_receiver) = ready_light("first", "First");
        let (second, _second_receiver) = ready_light("second", "Second");
        registry.insert(first.clone());
        registry.insert(second.clone());
        apply_groups(
            &registry,
            1,
            vec![group("group", "Group", &[&first, &second])],
        )
        .unwrap();
        let before = registry.snapshot();
        let mut changes = registry.subscribe();

        assert_eq!(
            registry.apply_configuration(
                1,
                vec![group("group", "Group", &[&first, &second])],
                &[],
                &[second.key().clone()],
            ),
            Err(CompositionError::MissingMember(DeviceKey::new(
                "xiaomi", "second"
            )))
        );
        let after = registry.snapshot();
        assert_eq!(relations(&after), relations(&before));
        assert_eq!(after.applied.confirmed, before.applied.confirmed);
        assert_eq!(after.revision, before.revision);
        assert!(poll_once(changes.changed()).await.is_none());
    });
}

#[test]
fn composition_updates_notify_and_idempotent_sets_do_not_revise() {
    block_on(async {
        let registry = DeviceRegistry::new();
        let (first, _first_receiver) = ready_light("first", "First");
        let (second, _second_receiver) = ready_light("second", "Second");
        let (third, _third_receiver) = ready_light("third", "Third");
        let mut changes = registry.subscribe();
        registry.insert(first.clone());
        let _ = changes.changed().await;
        registry.insert(second.clone());
        let _ = changes.changed().await;

        apply_groups(
            &registry,
            1,
            vec![group("group", "Group", &[&first, &second])],
        )
        .unwrap();
        let updated = changes.changed().await;
        assert_eq!(updated.applied.groups.len(), 1);
        assert_eq!(updated.revision, 3);

        registry.insert(third.clone());
        let inserted = changes.changed().await;
        assert_eq!(inserted.revision, 4);

        apply_groups(
            &registry,
            1,
            vec![group("group", "Group", &[&second, &first])],
        )
        .unwrap();
        assert!(poll_once(changes.changed()).await.is_none());
        assert_eq!(registry.snapshot().revision, 4);

        apply_groups(
            &registry,
            1,
            vec![
                group("another", "Another", &[&third]),
                group("group", "Group", &[&first, &second]),
            ],
        )
        .unwrap();
        let replaced = changes.changed().await;
        assert_eq!(replaced.applied.groups.len(), 2);
        assert_eq!(replaced.revision, 5);
    });
}

#[test]
fn shared_groups_keep_the_same_member_instance_commands_and_state() {
    block_on(async {
        let registry = DeviceRegistry::new();
        let (first, mut first_receiver) = ready_light("first", "First");
        let (second, _second_receiver) = ready_light("second", "Second");
        registry.insert(first.clone());
        registry.insert(second.clone());
        let function = first.function("main").unwrap();
        let before = function.snapshot();
        let mut command = Box::pin(function.execute(Command::On));
        assert!(poll_once(command.as_mut()).await.is_none());

        apply_groups(
            &registry,
            1,
            vec![
                group("one", "One", &[&first, &second]),
                group("two", "Two", &[&first]),
            ],
        )
        .unwrap();

        let stored = registry.get(first.key()).unwrap();
        assert!(stored.same_instance(&first));
        assert_eq!(stored.label(), "First");
        assert_eq!(function.snapshot(), before);
        let request = first_receiver.next().await.unwrap();
        request.accept().unwrap();
        assert_eq!(command.await.unwrap().power, crate::device::Power::On);
        let groups = registry.snapshot().applied.groups;
        for group in &groups {
            assert_eq!(group.snapshot().power, crate::device::Power::On);
        }
        let shared = groups[1].functions()[0].clone();
        let mut next = Box::pin(shared.execute(Command::Off));
        assert!(poll_once(next.as_mut()).await.is_none());
        let request = first_receiver.next().await.unwrap();
        request.accept().unwrap();
        assert_eq!(next.await.unwrap().power, crate::device::Power::Off);
        for group in &groups {
            assert_eq!(group.snapshot().power, crate::device::Power::Off);
        }
        assert_eq!(function.snapshot().power, crate::device::Power::Off);
    });
}

#[test]
fn confirmed_deletion_revokes_the_affected_group_and_keeps_unrelated_groups() {
    let registry = DeviceRegistry::new();
    let (first, _first_receiver) = ready_light("first", "First");
    let (second, _second_receiver) = ready_light("second", "Second");
    let (third, _third_receiver) = ready_light("third", "Third");
    registry.insert(first.clone());
    registry.insert(second.clone());
    registry.insert(third.clone());
    apply_groups(
        &registry,
        1,
        vec![
            group("affected", "Affected", &[&first, &second]),
            group("unrelated", "Unrelated", &[&third]),
        ],
    )
    .unwrap();

    registry
        .apply_configuration(
            1,
            vec![group("unrelated", "Unrelated", &[&third])],
            &[first.key().clone()],
            &[second.key().clone()],
        )
        .unwrap();

    let snapshot = registry.snapshot();
    assert_eq!(snapshot.applied.groups.len(), 1);
    assert_eq!(
        snapshot.applied.groups[0].key,
        DeviceKey::new("xiaomi", "unrelated")
    );
    assert!(registry.get(second.key()).is_none());
    assert!(registry.get(first.key()).is_some());
    assert!(snapshot.applied.devices[0].same_instance(&first));
    assert!(registry.get(third.key()).is_some());
}

#[test]
fn ordinary_removal_revokes_a_referencing_group_without_trimming_it() {
    let registry = DeviceRegistry::new();
    let (first, _first_receiver) = ready_light("first", "First");
    let (second, _second_receiver) = ready_light("second", "Second");
    let (third, _third_receiver) = ready_light("third", "Third");
    registry.insert(first.clone());
    registry.insert(second.clone());
    registry.insert(third.clone());
    apply_groups(
        &registry,
        1,
        vec![
            group("affected", "Affected", &[&first, &second]),
            group("unrelated", "Unrelated", &[&third]),
        ],
    )
    .unwrap();

    assert!(registry.remove(second.key()).is_some());
    let snapshot = registry.snapshot();
    assert_eq!(snapshot.applied.groups.len(), 1);
    assert_eq!(
        snapshot.applied.groups[0].key,
        DeviceKey::new("xiaomi", "unrelated")
    );
    assert!(registry.get(first.key()).is_some());
}

#[test]
fn same_name_members_and_groups_are_distinct_by_stable_key() {
    let registry = DeviceRegistry::new();
    let (first, _first_receiver) = ready_light("first", "Same name");
    let (second, _second_receiver) = ready_light("second", "Same name");
    registry.insert(first.clone());
    registry.insert(second.clone());
    apply_groups(
        &registry,
        1,
        vec![
            group("one", "Same group", &[&first]),
            group("two", "Same group", &[&second]),
        ],
    )
    .unwrap();
    let snapshot = registry.snapshot();
    assert_eq!(snapshot.applied.groups.len(), 2);
    assert_eq!(
        snapshot.applied.groups[0].members,
        vec![DeviceKey::new("xiaomi", "first")]
    );
    assert_eq!(
        snapshot.applied.groups[1].members,
        vec![DeviceKey::new("xiaomi", "second")]
    );
    let revision = snapshot.revision;
    apply_groups(
        &registry,
        1,
        vec![
            group("two", "Same group", &[&second]),
            group("one", "Same group", &[&first]),
        ],
    )
    .unwrap();
    assert_eq!(registry.snapshot().revision, revision);
}

#[test]
fn confirmed_version_survives_device_revisions_and_clears_on_relation_revocation() {
    let registry = DeviceRegistry::new();
    let (first, _first_receiver) = ready_light("first", "First");
    let (second, _second_receiver) = ready_light("second", "Second");
    let (third, _third_receiver) = ready_light("third", "Third");
    registry.insert(first.clone());
    registry.insert(second.clone());
    apply_groups(
        &registry,
        7,
        vec![group("group", "Group", &[&first, &second])],
    )
    .unwrap();
    let snapshot = registry.snapshot();
    assert_eq!(snapshot.applied.confirmed, Some(7));

    // Ordinary device revisions never touch the confirmed composition version.
    registry.insert(third.clone());
    assert_eq!(registry.snapshot().applied.confirmed, Some(7));
    registry.remove(third.key());
    assert_eq!(registry.snapshot().applied.confirmed, Some(7));

    // An idempotent versioned publication keeps the version without revising.
    let revision = registry.snapshot().revision;
    apply_groups(
        &registry,
        7,
        vec![group("group", "Group", &[&second, &first])],
    )
    .unwrap();
    assert_eq!(registry.snapshot().revision, revision);

    // Revoking a referencing group through ordinary removal clears the version.
    registry.remove(second.key());
    let snapshot = registry.snapshot();
    assert!(snapshot.applied.groups.is_empty());
    assert_eq!(snapshot.applied.confirmed, None);
}

#[test]
fn composition_label_update_is_atomic_and_leaves_relations_untouched() {
    let registry = DeviceRegistry::new();
    let (first, _first_receiver) = ready_light("first", "First");
    registry.insert(first.clone());
    apply_groups(&registry, 7, vec![group("group", "Group", &[&first])]).unwrap();
    let before = registry.snapshot();
    let mut changes = registry.subscribe();

    assert!(registry.set_composition_label(&DeviceKey::new("xiaomi", "group"), "Renamed"));
    let after = registry.snapshot();
    assert_eq!(after.applied.groups[0].label, "Renamed");
    assert_eq!(
        after.applied.groups[0].members,
        before.applied.groups[0].members
    );
    assert_eq!(after.applied.confirmed, before.applied.confirmed);
    assert_eq!(after.revision, before.revision.wrapping_add(1));
    assert_eq!(
        block_on(changes.changed()).applied.groups[0].label,
        "Renamed"
    );

    // A repeated or unknown rename never advances the revisions.
    let after = registry.snapshot();
    assert!(registry.set_composition_label(&DeviceKey::new("xiaomi", "group"), "Renamed"));
    assert!(!registry.set_composition_label(&DeviceKey::new("xiaomi", "absent"), "Other"));
    let unchanged = registry.snapshot();
    assert_eq!(unchanged.revision, after.revision);
    assert_eq!(unchanged.applied.groups[0].label, "Renamed");
}
