use super::{new_device, outlet};
use crate::device::{
    Availability, Capability, Category, Command, CommandReceiver, Composition, CompositionError,
    DeviceKey, DeviceRegistry, FunctionDescription, NumericRange, PhysicalDevice,
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
            capabilities: vec![
                Capability::OnOff,
                Capability::Brightness,
                Capability::ColorTemperature(NumericRange {
                    min: 2700,
                    max: 6500,
                    step: 1,
                }),
            ],
        }],
    }
}

fn ready_light(key: &str, label: &str) -> (PhysicalDevice, CommandReceiver) {
    let (device, mut receivers) = PhysicalDevice::new(light(key, label));
    device.functions()[0].observe(false);
    device.set_availability(Availability::Available);
    (device, receivers.remove(0))
}

fn group(key: &str, label: &str, members: &[&PhysicalDevice]) -> Composition {
    Composition {
        key: DeviceKey::new("xiaomi", key),
        label: label.into(),
        members: members.iter().map(|member| member.key().clone()).collect(),
    }
}

#[test]
fn composition_update_rejects_empty_groups_and_device_key_collisions() {
    let registry = DeviceRegistry::new();
    let (first, _first_receiver) = ready_light("first", "First");
    let (second, _second_receiver) = ready_light("second", "Second");
    registry.insert(first.clone());
    registry.insert(second.clone());

    assert_eq!(
        registry.update_composition(
            vec![Composition {
                key: DeviceKey::new("xiaomi", "empty"),
                label: "Empty".into(),
                members: Vec::new(),
            }],
            &[],
        ),
        Err(CompositionError::EmptyGroup(DeviceKey::new(
            "xiaomi", "empty"
        )))
    );
    assert_eq!(
        registry.update_composition(vec![group("first", "Collides", &[&second])], &[]),
        Err(CompositionError::GroupKeyConflict(DeviceKey::new(
            "xiaomi", "first"
        )))
    );
    let snapshot = registry.snapshot();
    assert_eq!(snapshot.composition.revision, 0);
    assert!(snapshot.composition.groups.is_empty());
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
            vec![Composition {
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
            vec![Composition {
                key: DeviceKey::new("xiaomi", "self"),
                label: "Self".into(),
                members: vec![DeviceKey::new("xiaomi", "self")],
            }],
            CompositionError::SelfMember(DeviceKey::new("xiaomi", "self")),
        ),
        (
            vec![Composition {
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
                group("overlap-one", "One", &[&first]),
                group("overlap-two", "Two", &[&first]),
            ],
            CompositionError::OverlappingMember(DeviceKey::new("xiaomi", "first")),
        ),
        (
            vec![
                Composition {
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
        assert_eq!(registry.update_composition(groups, &[]), Err(expected));
        let after = registry.snapshot();
        assert_eq!(after.composition, before.composition);
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
        registry
            .update_composition(vec![group("group", "Group", &[&first, &second])], &[])
            .unwrap();
        let before = registry.snapshot();
        let mut changes = registry.subscribe();

        assert_eq!(
            registry.update_composition(
                vec![group("group", "Group", &[&first, &second])],
                &[second.key().clone()],
            ),
            Err(CompositionError::MissingMember(DeviceKey::new(
                "xiaomi", "second"
            )))
        );
        let after = registry.snapshot();
        assert_eq!(after.composition, before.composition);
        assert_eq!(after.revision, before.revision);
        assert!(poll_once(changes.changed()).await.is_none());
    });
}

#[test]
fn composition_revision_changes_independently_and_idempotent_sets_do_not_revise() {
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

        registry
            .update_composition(vec![group("group", "Group", &[&first, &second])], &[])
            .unwrap();
        let updated = changes.changed().await;
        assert_eq!(updated.composition.revision, 1);
        assert_eq!(updated.composition.groups.len(), 1);
        assert_eq!(updated.revision, 3);

        registry.insert(third.clone());
        let inserted = changes.changed().await;
        assert_eq!(inserted.composition.revision, 1);
        assert_eq!(inserted.revision, 4);

        registry
            .update_composition(vec![group("group", "Group", &[&second, &first])], &[])
            .unwrap();
        assert!(poll_once(changes.changed()).await.is_none());
        assert_eq!(registry.snapshot().composition.revision, 1);
        assert_eq!(registry.snapshot().revision, 4);

        registry
            .update_composition(
                vec![
                    group("another", "Another", &[&third]),
                    group("group", "Group", &[&first, &second]),
                ],
                &[],
            )
            .unwrap();
        let replaced = changes.changed().await;
        assert_eq!(replaced.composition.revision, 2);
        assert_eq!(replaced.composition.groups.len(), 2);
        assert_eq!(replaced.revision, 5);
    });
}

#[test]
fn composition_changes_keep_member_instances_commands_and_state() {
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

        registry
            .update_composition(vec![group("group", "Group", &[&first, &second])], &[])
            .unwrap();

        let stored = registry.get(first.key()).unwrap();
        assert!(stored.same_instance(&first));
        assert_eq!(stored.label(), "First");
        assert_eq!(function.snapshot(), before);
        let request = first_receiver.next().await.unwrap();
        request.accept().unwrap();
        assert_eq!(command.await.unwrap().power, crate::device::Power::On);
        let mut next = Box::pin(function.execute(Command::Off));
        assert!(poll_once(next.as_mut()).await.is_none());
        let request = first_receiver.next().await.unwrap();
        request.accept().unwrap();
        assert_eq!(next.await.unwrap().power, crate::device::Power::Off);
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
    registry
        .update_composition(
            vec![
                group("affected", "Affected", &[&first, &second]),
                group("unrelated", "Unrelated", &[&third]),
            ],
            &[],
        )
        .unwrap();

    registry
        .update_composition(
            vec![group("unrelated", "Unrelated", &[&third])],
            &[second.key().clone()],
        )
        .unwrap();

    let snapshot = registry.snapshot();
    assert_eq!(snapshot.composition.groups.len(), 1);
    assert_eq!(
        snapshot.composition.groups[0].key,
        DeviceKey::new("xiaomi", "unrelated")
    );
    assert!(registry.get(second.key()).is_none());
    assert!(registry.get(first.key()).is_some());
    assert!(registry.get(third.key()).is_some());
    assert_eq!(snapshot.composition.revision, 2);
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
    registry
        .update_composition(
            vec![
                group("affected", "Affected", &[&first, &second]),
                group("unrelated", "Unrelated", &[&third]),
            ],
            &[],
        )
        .unwrap();

    assert!(registry.remove(second.key()).is_some());
    let snapshot = registry.snapshot();
    assert_eq!(snapshot.composition.revision, 2);
    assert_eq!(snapshot.composition.groups.len(), 1);
    assert_eq!(
        snapshot.composition.groups[0].key,
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
    registry
        .update_composition(
            vec![
                group("one", "Same group", &[&first]),
                group("two", "Same group", &[&second]),
            ],
            &[],
        )
        .unwrap();
    let snapshot = registry.snapshot();
    assert_eq!(snapshot.composition.groups.len(), 2);
    assert_eq!(
        snapshot.composition.groups[0].members,
        vec![DeviceKey::new("xiaomi", "first")]
    );
    assert_eq!(
        snapshot.composition.groups[1].members,
        vec![DeviceKey::new("xiaomi", "second")]
    );
    let revision = snapshot.composition.revision;
    registry
        .update_composition(
            vec![
                group("two", "Same group", &[&second]),
                group("one", "Same group", &[&first]),
            ],
            &[],
        )
        .unwrap();
    assert_eq!(registry.snapshot().composition.revision, revision);
}
