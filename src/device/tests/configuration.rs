use super::new_device;
use crate::device::{
    AppliedGroup, Availability, Capability, Category, Command, CommandReceiver, Composition,
    ConfigurationScope, DeviceKey, DeviceRegistry, FunctionDescription, GroupConfiguration,
    LightPatch, PhysicalDevice, PhysicalDeviceDescription, Power, derive_group_capabilities,
};
use futures_lite::future::block_on;

fn scope() -> ConfigurationScope {
    ConfigurationScope {
        uid: "account".into(),
        home_id: "home".into(),
        group_id: "group-scope".into(),
    }
}

fn key(id: &str) -> DeviceKey {
    DeviceKey::new("xiaomi", id)
}

fn light(key_id: &str, capabilities: Vec<Capability>) -> PhysicalDeviceDescription {
    PhysicalDeviceDescription {
        key: key(key_id),
        label: key_id.into(),
        functions: vec![FunctionDescription {
            id: "main".into(),
            label: "Main light".into(),
            category: Category::Light,
            capabilities,
        }],
    }
}

fn new_light(key_id: &str, capabilities: Vec<Capability>) -> (PhysicalDevice, CommandReceiver) {
    new_device(light(key_id, capabilities))
}

fn ready_light(key_id: &str) -> (PhysicalDevice, CommandReceiver) {
    let (device, receiver) = new_light(key_id, vec![Capability::OnOff]);
    device.functions()[0].observe(false);
    device.set_availability(Availability::Available);
    (device, receiver)
}

fn ready_dimmable(key_id: &str) -> (PhysicalDevice, CommandReceiver) {
    let (device, receiver) = new_light(key_id, vec![Capability::OnOff, Capability::Brightness]);
    device.functions()[0].observe(false);
    device.set_availability(Availability::Available);
    (device, receiver)
}

fn group(key_id: &str, members: &[&PhysicalDevice]) -> Composition {
    Composition {
        key: key(key_id),
        label: key_id.into(),
        members: members.iter().map(|member| member.key().clone()).collect(),
    }
}

fn configuration(composition: &Composition, members: &[&PhysicalDevice]) -> GroupConfiguration {
    let functions = members
        .iter()
        .map(|member| member.functions()[0].description().clone())
        .collect::<Vec<_>>();
    GroupConfiguration {
        key: composition.key.clone(),
        label: composition.label.clone(),
        members: composition.members.clone(),
        capabilities: derive_group_capabilities(&functions),
    }
}

fn applied_group(registry: &DeviceRegistry, id: &str) -> AppliedGroup {
    registry
        .snapshot()
        .applied
        .groups
        .into_iter()
        .find(|group| group.key == key(id))
        .expect("applied group")
}

#[test]
fn staged_configuration_keeps_the_applied_view_until_the_boundary() {
    let registry = DeviceRegistry::new();
    let (first, _first_receiver) = ready_light("first");
    let (second, _second_receiver) = ready_light("second");
    registry.insert(first.clone());
    registry.insert(second.clone());
    let composition = group("group", &[&first, &second]);
    let applied = configuration(&composition, &[&first, &second]);
    registry
        .apply_configuration(1, vec![applied], &[], &[])
        .unwrap();

    // A newer confirmation keeps the complete old relation and shape applied.
    registry
        .stage_configuration(scope(), vec![group("group", &[&second])], Vec::new())
        .unwrap();
    let snapshot = registry.snapshot();
    let pending = snapshot.pending.expect("pending configuration");
    assert_eq!(pending.scope, scope());
    assert_eq!(pending.groups[0].members, vec![key("second")]);
    assert_eq!(
        snapshot.applied.groups[0].members,
        vec![key("first"), key("second")]
    );
    assert_eq!(snapshot.applied.confirmed, Some(1));
    assert_eq!(
        snapshot.applied.groups[0].members,
        vec![key("first"), key("second")]
    );
    assert!(snapshot.applied.devices.is_empty());
}

#[test]
fn applying_new_members_installs_a_new_binding_and_uses_current_brightness() {
    let registry = DeviceRegistry::new();
    let (first, _first_receiver) = ready_dimmable("first");
    let (second, _second_receiver) = ready_dimmable("second");
    registry.insert(first.clone());
    registry.insert(second.clone());
    let composition = group("group", &[&first, &second]);
    registry
        .apply_configuration(
            1,
            vec![configuration(&composition, &[&first, &second])],
            &[],
            &[],
        )
        .unwrap();
    for (member, brightness) in [(&first, 40.0), (&second, 60.0)] {
        member.functions()[0]
            .observe_light(LightPatch {
                brightness_percent: Some(brightness),
                ..Default::default()
            })
            .unwrap();
    }
    let previous = applied_group(&registry, "group");
    assert_eq!(
        previous.snapshot().lighting.unwrap().brightness_percent,
        Some(50.0)
    );

    // A captured applied view retains its binding; the new configuration uses
    // the current observations of its own member set, including while all off.
    let updated = group("group", &[&second]);
    let groups = vec![configuration(&updated, &[&second])];
    registry.apply_configuration(2, groups, &[], &[]).unwrap();
    let group = applied_group(&registry, "group");
    assert_eq!(group.members, vec![key("second")]);
    assert_eq!(
        group.snapshot().lighting.unwrap().brightness_percent,
        Some(60.0)
    );
    assert_eq!(previous.handle().keys().len(), 2);
    assert_eq!(
        previous.snapshot().lighting.unwrap().brightness_percent,
        Some(50.0)
    );
    assert_eq!(registry.snapshot().applied.confirmed, Some(2));
    assert!(registry.pending_configuration().is_none());
}

#[test]
fn revoking_a_member_keeps_the_applied_relation_with_a_closed_handle() {
    let registry = DeviceRegistry::new();
    let (member, _member_receiver) = ready_light("member");
    registry.insert(member.clone());
    let composition = group("group", &[&member]);
    registry
        .apply_configuration(1, vec![configuration(&composition, &[&member])], &[], &[])
        .unwrap();

    registry.revoke(&key("member"));
    let snapshot = registry.snapshot();
    assert!(snapshot.devices.is_empty());
    // The old published view remains readable without dangling panics.
    assert_eq!(snapshot.applied.groups[0].members, vec![key("member")]);
    let group = &snapshot.applied.groups[0];
    assert_eq!(group.members, vec![key("member")]);
    assert!(!group.functions()[0].fresh_snapshot().reachable);
    assert_eq!(group.snapshot().power, Power::Unknown);
    assert!(!group.snapshot().reachable);
}

#[test]
fn a_same_key_replacement_forms_a_candidate_and_rebinds_after_apply() {
    let registry = DeviceRegistry::new();
    let (member, _member_receiver) = ready_light("member");
    registry.insert(member.clone());
    let composition = group("group", &[&member]);
    registry
        .apply_configuration(1, vec![configuration(&composition, &[&member])], &[], &[])
        .unwrap();

    // Substitute the physical instance under the same key. The old handle is
    // closed but the applied group metadata stays until the apply boundary.
    let (replacement, _replacement_receiver) = ready_light("member");
    let previous = registry.revoke(replacement.key()).unwrap();
    // A pending deletion must not block a later confirmation of the same
    // group and a newly admitted physical instance under the same keys.
    registry
        .stage_configuration(scope(), Vec::new(), vec![key("group"), key("member")])
        .unwrap();
    registry.insert(replacement.clone());
    assert!(previous.same_instance(&member));
    assert!(
        registry
            .get(&key("member"))
            .unwrap()
            .same_instance(&replacement)
    );
    registry
        .stage_configuration(scope(), vec![composition.clone()], Vec::new())
        .unwrap();
    assert!(
        registry.pending_configuration().is_some(),
        "an instance change must form a candidate even when keys and caps match"
    );
    let snapshot = registry.snapshot();
    assert!(snapshot.pending.as_ref().unwrap().removed.is_empty());
    assert_eq!(snapshot.applied.confirmed, Some(1));
    let stale = applied_group(&registry, "group");
    assert!(!stale.functions()[0].fresh_snapshot().reachable);
    assert_eq!(stale.snapshot().power, Power::Unknown);
    // A command on the stale handle cannot resurrect the old member action.
    let stale_function = stale.functions()[0].clone();
    block_on(async move {
        assert!(stale_function.execute(Command::On).await.is_err());
    });
    replacement.functions()[0]
        .observe_light(LightPatch {
            power: Some(true),
            ..Default::default()
        })
        .unwrap();
    replacement.set_availability(Availability::Available);
    // The stale applied handle still cannot see the replacement.
    assert_eq!(
        applied_group(&registry, "group").snapshot().power,
        Power::Unknown
    );

    registry
        .apply_configuration(
            2,
            vec![configuration(&composition, &[&replacement])],
            &[],
            &[],
        )
        .unwrap();
    let rebound = applied_group(&registry, "group");
    assert!(rebound.functions()[0].fresh_snapshot().reachable);
    assert_eq!(rebound.snapshot().power, Power::On);
    assert!(registry.pending_configuration().is_none());
}

#[test]
fn a_standalone_source_change_forms_a_candidate_without_relation_changes() {
    let registry = DeviceRegistry::new();
    let (member, _member_receiver) = ready_light("member");
    let (standalone, _standalone_receiver) = ready_light("standalone");
    registry.insert(member.clone());
    let composition = group("group", &[&member]);
    registry
        .apply_configuration(1, vec![configuration(&composition, &[&member])], &[], &[])
        .unwrap();
    assert!(registry.snapshot().applied.devices.is_empty());

    // A newly admitted standalone device must form a candidate even though
    // the group relation set stayed identical.
    registry.insert(standalone);
    registry
        .stage_configuration(scope(), vec![composition], Vec::new())
        .unwrap();
    let pending = registry.pending_configuration().unwrap();
    assert_eq!(pending.devices[0].key(), &key("standalone"));
    assert!(registry.snapshot().applied.devices.is_empty());

    registry
        .apply_configuration(2, Vec::new(), &[key("standalone")], &[])
        .unwrap();
    assert_eq!(
        registry.snapshot().applied.devices[0].key(),
        &key("standalone")
    );
}

#[test]
fn shared_members_stay_hidden_until_the_last_group_releases_them() {
    let registry = DeviceRegistry::new();
    let (shared, _receiver) = ready_light("shared");
    registry.insert(shared.clone());
    for (version, group_ids) in [(1, vec!["one", "two"]), (2, vec!["two"]), (3, Vec::new())] {
        registry
            .stage_configuration(
                scope(),
                group_ids.iter().map(|id| group(id, &[&shared])).collect(),
                Vec::new(),
            )
            .unwrap();
        let pending = registry.pending_configuration().unwrap();
        assert_eq!(pending.devices.len(), usize::from(group_ids.is_empty()));
        registry
            .apply_pending::<_, ()>(|_| Ok(((), version)))
            .unwrap();
        let snapshot = registry.snapshot();
        assert_eq!(snapshot.applied.groups.len(), group_ids.len());
        assert_eq!(
            snapshot.applied.devices.len(),
            usize::from(group_ids.is_empty())
        );
        assert!(registry.get(shared.key()).unwrap().same_instance(&shared));
    }
    assert!(registry.snapshot().applied.devices[0].same_instance(&shared));
}

#[test]
fn restoring_configuration_installs_the_committed_shape() {
    let registry = DeviceRegistry::new();
    let (member, _receiver) = ready_dimmable("member");
    registry.insert(member.clone());
    let composition = group("group", &[&member]);
    let mut stored = configuration(&composition, &[&member]);
    stored.capabilities = vec![Capability::OnOff];
    registry
        .apply_configuration(1, vec![stored], &[], &[])
        .unwrap();
    let applied = applied_group(&registry, "group");
    assert_eq!(applied.description().capabilities, vec![Capability::OnOff]);
    assert!(applied.snapshot().lighting.is_none());
}
