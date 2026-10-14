use migate as library;
#[path = "../src/storage/tests/support.rs"]
mod configuration_support;
use configuration_support::stage_configuration;
use migate::{
    device::{Capability, Composition, DeviceKey},
    storage::{
        AdmissionError, CachedPhysicalDevice, CachedScope, CompositionCommitError,
        ConfirmedComposition, Store,
    },
    xiaomi::{capability::parse_device_for_model, cloud::group_id},
};
use rusqlite::Connection;
use serde_json::Value;

fn light(did: &str) -> CachedPhysicalDevice {
    light_in("account", "home", did)
}

fn light_in(uid: &str, home: &str, did: &str) -> CachedPhysicalDevice {
    let instance: Value =
        serde_json::from_str(include_str!("fixtures/miot/xiaomi-light-ceil04.json")).unwrap();
    let model = "xiaomi.light.ceil04";
    CachedPhysicalDevice {
        uid: uid.into(),
        home_id: home.into(),
        group_id: group_id(uid, home),
        model: model.into(),
        instance_type: instance["type"].as_str().unwrap().into(),
        device: parse_device_for_model(DeviceKey::new("xiaomi", did), model, &instance).unwrap(),
    }
}

fn group_like(did: &str, model: &str) -> CachedPhysicalDevice {
    let mut cached = light("placeholder");
    cached.device.key = DeviceKey::new("xiaomi", did);
    cached.model = model.into();
    cached
}

fn scope() -> CachedScope {
    scope_in("account", "home")
}

fn scope_in(uid: &str, home: &str) -> CachedScope {
    CachedScope {
        uid: uid.into(),
        home_id: home.into(),
        group_id: group_id(uid, home),
    }
}

fn commit_groups(
    store: &Store,
    scope: &CachedScope,
    groups: &[Composition],
) -> Result<ConfirmedComposition, CompositionCommitError> {
    commit(store, scope, groups, &[], &[])
}

fn commit(
    store: &Store,
    scope: &CachedScope,
    groups: &[Composition],
    removed: &[DeviceKey],
    devices: &[&str],
) -> Result<ConfirmedComposition, CompositionCommitError> {
    let (registry, _receivers) = stage_configuration(
        &store.devices(),
        scope,
        groups,
        removed,
        &devices.iter().map(|id| key(id)).collect::<Vec<_>>(),
    )?;
    Ok(store
        .devices()
        .apply_pending_configuration(&registry, |_| Ok((Vec::new(), Vec::new())))?
        .unwrap())
}

#[test]
fn incompatible_member_description_preserves_confirmed_groups() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    for did in ["a", "b", "c"] {
        store.devices().publish_cached(&light(did), did).unwrap();
    }
    let initial = commit_groups(
        &store,
        &scope(),
        &[group("g1", "One", &["a", "b"]), group("g2", "Two", &["c"])],
    )
    .unwrap();
    let original = store.devices().load_published(&key("a")).unwrap().unwrap();
    let mut incompatible = light("a");
    incompatible.device.functions[0].power = None;
    incompatible.device.functions[0]
        .description
        .capabilities
        .retain(|capability| *capability != Capability::OnOff);
    assert!(matches!(
        store.devices().publish_cached(&incompatible, "Changed"),
        Err(AdmissionError::IncompatibleFunctions)
    ));
    assert_eq!(
        store.devices().restore_groups("account", "home").unwrap(),
        initial
    );
    assert_eq!(
        store.devices().load_published(&key("a")).unwrap(),
        Some(original)
    );
    assert_eq!(
        store
            .devices()
            .load_parsed("account", "home", &key("a"))
            .unwrap(),
        Some(light("a"))
    );
}

#[test]
fn committed_group_restores_after_restart_without_a_main_light_identity() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    for did in ["a", "b"] {
        store.devices().publish_cached(&light(did), did).unwrap();
    }
    let confirmed = commit_groups(
        &store,
        &scope(),
        &[group("group.1", "Ceiling", &["a", "b"])],
    )
    .unwrap();
    assert_eq!(confirmed.version, 1);
    assert_eq!(confirmed.groups.len(), 1);
    assert_eq!(confirmed.groups[0].key, key("group.1"));
    assert_eq!(confirmed.groups[0].label, "Ceiling");
    assert_eq!(confirmed.groups[0].members, [key("a"), key("b")]);

    drop(store);
    let store = Store::open(dir.path()).unwrap();
    assert_eq!(
        store.devices().restore_groups("account", "home").unwrap(),
        confirmed
    );
    assert_eq!(
        store
            .devices()
            .restore_groups("account", "home")
            .unwrap()
            .groups
            .iter()
            .find(|group| group.key == key("group.1")),
        Some(&confirmed.groups[0])
    );
    assert!(
        store
            .devices()
            .load_published(&key("group.1"))
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .devices()
            .restore_cached("account", "home")
            .unwrap()
            .iter()
            .all(|(_, stored)| stored.key != key("group.1"))
    );
}

fn outlet(did: &str) -> CachedPhysicalDevice {
    let instance: Value = serde_json::from_str(include_str!("fixtures/miot/outlet.json")).unwrap();
    let model = "vendor.plug";
    CachedPhysicalDevice {
        uid: "account".into(),
        home_id: "home".into(),
        group_id: group_id("account", "home"),
        model: model.into(),
        instance_type: instance["type"].as_str().unwrap().into(),
        device: parse_device_for_model(DeviceKey::new("xiaomi", did), model, &instance).unwrap(),
    }
}

fn group(key: &str, label: &str, members: &[&str]) -> Composition {
    Composition {
        key: DeviceKey::new("xiaomi", key),
        label: label.into(),
        members: members
            .iter()
            .map(|did| DeviceKey::new("xiaomi", *did))
            .collect(),
    }
}

fn key(id: &str) -> DeviceKey {
    DeviceKey::new("xiaomi", id)
}

#[test]
fn commit_rejects_invalid_sets_without_touching_storage() {
    use migate::device::CompositionError;
    use migate::storage::CompositionCommitError;

    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let scope = scope();
    for did in ["a", "b"] {
        store.devices().publish_cached(&light(did), did).unwrap();
    }
    store.devices().publish_cached(&outlet("o"), "O").unwrap();

    assert!(matches!(
        commit_groups(
            &store,
            &scope,
            &[Composition {
                key: key("empty"),
                label: "Empty".into(),
                members: Vec::new(),
            }]
        ),
        Err(CompositionCommitError::Invalid(
            CompositionError::EmptyGroup(_)
        ))
    ));
    assert!(matches!(
        commit_groups(&store, &scope, &[group("missing", "Missing", &["absent"])]),
        Err(CompositionCommitError::Invalid(
            CompositionError::MissingMember(_)
        ))
    ));
    assert!(matches!(
        commit_groups(&store, &scope, &[group("outlet", "Outlet", &["o"])]),
        Err(CompositionCommitError::Invalid(
            CompositionError::UnsupportedMember(_)
        ))
    ));
    assert!(matches!(
        commit(
            &store,
            &scope,
            &[group("group.1", "Group", &["a"])],
            &[key("a")],
            &[]
        ),
        Err(CompositionCommitError::Invalid(
            CompositionError::MissingMember(_)
        ))
    ));

    let restored = store.devices().restore_groups("account", "home").unwrap();
    assert_eq!(restored.version, 0);
    assert!(restored.groups.is_empty());
    let db = Connection::open(store.path()).unwrap();
    assert_eq!(
        db.query_row("SELECT count(*) FROM group_identity", [], |row| row
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert_eq!(
        db.query_row(
            "SELECT COALESCE(MAX(endpoint),1) FROM endpoint_allocation",
            [],
            |row| row.get::<_, u16>(0)
        )
        .unwrap(),
        4
    );
}

#[test]
fn dissolve_and_readd_reuse_parent_reservation_and_saved_label() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let scope = scope();
    for did in ["a", "b"] {
        store.devices().publish_cached(&light(did), did).unwrap();
    }
    let first = commit(
        &store,
        &scope,
        &[group("group.1", "Cloud name", &["a"])],
        &[],
        &["b"],
    )
    .unwrap();
    let stored = first.groups[0].clone();
    store
        .devices()
        .set_group_label(&stored.key, "Saved group")
        .unwrap();

    let dissolved = commit(&store, &scope, &[], &[], &["a", "b"]).unwrap();
    assert_eq!(dissolved.version, 2);
    assert!(dissolved.groups.is_empty());
    assert!(
        store
            .devices()
            .restore_groups("account", "home")
            .unwrap()
            .groups
            .iter()
            .all(|group| group.key != stored.key)
    );
    let db = Connection::open(store.path()).unwrap();
    let reservation = db
        .query_row(
            "SELECT unique_id, endpoint, label FROM group_identity WHERE device_id='group.1'",
            [],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, u16>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(
        reservation,
        (
            stored.unique_id.clone(),
            stored.endpoint,
            "Saved group".into()
        )
    );
    assert_eq!(
        db.query_row("SELECT count(*) FROM group_relation", [], |row| row
            .get::<_, i64>(0))
            .unwrap(),
        0
    );

    let rejoined = commit_groups(
        &store,
        &scope,
        &[group("group.1", "Later cloud name", &["b", "a"])],
    )
    .unwrap();
    assert_eq!(rejoined.version, 3);
    assert_eq!(rejoined.groups[0].unique_id, stored.unique_id);
    assert_eq!(rejoined.groups[0].endpoint, stored.endpoint);
    assert_eq!(rejoined.groups[0].label, "Saved group");
    assert_eq!(rejoined.groups[0].members, [key("a"), key("b")]);
}

#[test]
fn confirmed_member_deletion_revokes_relations_and_storage_failure_rolls_back() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let scope = scope();
    for did in ["a", "b", "c"] {
        store.devices().publish_cached(&light(did), did).unwrap();
    }
    commit_groups(
        &store,
        &scope,
        &[group("g1", "One", &["a", "b"]), group("g2", "Two", &["c"])],
    )
    .unwrap();

    let confirmed = commit(
        &store,
        &scope,
        &[group("g2", "Two", &["c"])],
        &[key("b")],
        &["a"],
    )
    .unwrap();
    assert_eq!(confirmed.version, 2);
    assert_eq!(confirmed.groups.len(), 1);
    assert_eq!(confirmed.groups[0].key, key("g2"));
    let db = Connection::open(store.path()).unwrap();
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM group_relation WHERE member_source='xiaomi' AND member_id='b'",
            [],
            |row| row.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
    assert!(
        !db.query_row(
            "SELECT published FROM device_identity WHERE source='xiaomi' AND device_id='b'",
            [],
            |row| row.get::<_, bool>(0)
        )
        .unwrap()
    );
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM group_identity WHERE device_id='g1'",
            [],
            |row| row.get::<_, i64>(0)
        )
        .unwrap(),
        1
    );

    db.execute_batch(
        "CREATE TRIGGER reject_group_relation BEFORE INSERT ON group_relation
         BEGIN SELECT RAISE(ABORT, 'private relation'); END;",
    )
    .unwrap();
    let error = commit_groups(
        &store,
        &scope,
        &[group("g1", "One", &["a"]), group("g2", "Two", &["c"])],
    )
    .unwrap_err();
    assert!(!format!("{error:?} {error}").contains("private relation"));
    assert_eq!(
        store.devices().restore_groups("account", "home").unwrap(),
        confirmed
    );
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM group_relation WHERE source='xiaomi' AND device_id='g1'",
            [],
            |row| row.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
}

#[test]
fn commit_rejects_promoting_device_identities_to_groups() {
    use migate::device::CompositionError;

    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let cached = light("ordinary");
    let old = store
        .devices()
        .publish_cached(&cached, "Saved light")
        .unwrap();
    store
        .devices()
        .publish_cached(&light("member"), "Member")
        .unwrap();
    let before = std::fs::read(store.path()).unwrap();
    assert!(matches!(
        commit_groups(&store, &scope(), &[group("ordinary", "Group", &["member"])]),
        Err(CompositionCommitError::Invalid(
            CompositionError::GroupKeyConflict(_)
        ))
    ));
    assert_eq!(std::fs::read(store.path()).unwrap(), before);
    assert_eq!(
        store.devices().load_published(&cached.device.key).unwrap(),
        Some(old)
    );
}

#[test]
fn physical_cache_rejects_light_group_keys_and_models_on_write_and_read() {
    for (did, model) in [
        ("group.42", "xiaomi.light.ceil04"),
        ("42", "mijia.light.group3"),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let before = std::fs::read(store.path()).unwrap();
        assert!(
            store
                .devices()
                .publish_cached(&group_like(did, model), "Group")
                .is_err()
        );
        assert_eq!(std::fs::read(store.path()).unwrap(), before);
        store
            .devices()
            .publish_cached(&light("original"), "Light")
            .unwrap();
        Connection::open(store.path())
            .unwrap()
            .execute(
                "UPDATE device_cache SET device_id=?1, model=?2",
                [did, model],
            )
            .unwrap();
        assert!(
            store
                .devices()
                .load_parsed("account", "home", &key(did))
                .is_err()
        );
    }
}

#[test]
fn commit_requires_member_functions_to_match_the_published_identity() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let scope = scope();
    store.devices().publish_cached(&light("a"), "A").unwrap();
    let db = Connection::open(store.path()).unwrap();
    db.execute(
        "UPDATE device_function SET capabilities=?1 WHERE source='xiaomi' AND device_id='a'",
        [r#"["on_off","brightness",{"color_temperature":{"min":3000,"max":6000,"step":1}}]"#],
    )
    .unwrap();
    assert!(matches!(
        commit_groups(&store, &scope, &[group("g", "G", &["a"])]),
        Err(CompositionCommitError::UnsupportedMember(_))
    ));
}

#[test]
fn commit_rejects_a_removed_key_that_is_also_a_proposed_group() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let scope = scope();
    store.devices().publish_cached(&light("a"), "A").unwrap();
    assert!(matches!(
        commit(&store, &scope, &[group("g", "G", &["a"])], &[key("g")], &[]),
        Err(CompositionCommitError::RemovedGroup(_))
    ));
    assert!(
        store
            .devices()
            .restore_groups("account", "home")
            .unwrap()
            .groups
            .is_empty()
    );
}

#[test]
fn no_op_commit_does_not_rewrite_relations_or_consume_a_version() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let scope = scope();
    for did in ["a", "b"] {
        store.devices().publish_cached(&light(did), did).unwrap();
    }
    let first = commit_groups(&store, &scope, &[group("g", "Saved name", &["a", "b"])]).unwrap();
    let db = Connection::open(store.path()).unwrap();
    db.execute_batch(
        "CREATE TRIGGER reject_relation_insert BEFORE INSERT ON group_relation
         BEGIN SELECT RAISE(ABORT, 'no-op relation'); END;
         CREATE TRIGGER reject_identity_update BEFORE UPDATE ON group_identity
         BEGIN SELECT RAISE(ABORT, 'no-op identity'); END;",
    )
    .unwrap();
    let again = commit_groups(&store, &scope, &[group("g", "Cloud rename", &["b", "a"])]).unwrap();
    assert_eq!(again, first);
    assert_eq!(again.groups[0].label, "Saved name");
}

#[test]
fn scope_isolation_keeps_same_group_key_confirmed_per_home() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let home = scope();
    let other = scope_in("account", "other");
    store
        .devices()
        .publish_cached(&light_in("account", "home", "a"), "A")
        .unwrap();
    store
        .devices()
        .publish_cached(&light_in("account", "other", "b"), "B")
        .unwrap();
    let home_confirmed =
        commit_groups(&store, &home, &[group("group.1", "Home group", &["a"])]).unwrap();
    let other_confirmed =
        commit_groups(&store, &other, &[group("group.1", "Other group", &["b"])]).unwrap();
    assert_eq!(
        other_confirmed.groups[0].unique_id,
        home_confirmed.groups[0].unique_id
    );
    assert_eq!(
        other_confirmed.groups[0].endpoint,
        home_confirmed.groups[0].endpoint
    );
    assert_eq!(other_confirmed.groups[0].label, "Home group");

    commit(&store, &home, &[], &[], &["a"]).unwrap();
    assert!(
        store
            .devices()
            .restore_groups("account", "home")
            .unwrap()
            .groups
            .is_empty()
    );
    let still_confirmed = store.devices().restore_groups("account", "other").unwrap();
    assert_eq!(still_confirmed, other_confirmed);
    assert!(
        store
            .devices()
            .restore_groups("account", "other")
            .unwrap()
            .groups
            .iter()
            .any(|group| group.key == key("group.1"))
    );

    commit(&store, &other, &[], &[], &["b"]).unwrap();
    assert!(
        store
            .devices()
            .restore_groups("account", "other")
            .unwrap()
            .groups
            .is_empty()
    );
    let db = Connection::open(store.path()).unwrap();
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM group_identity WHERE device_id='group.1'",
            [],
            |row| row.get::<_, i64>(0)
        )
        .unwrap(),
        1
    );
    assert_eq!(
        db.query_row("SELECT count(*) FROM group_relation", [], |row| row
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
}

#[test]
fn removing_a_member_revokes_the_group_and_republishing_does_not_resurrect_it() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let scope = scope();
    for did in ["a", "b"] {
        store.devices().publish_cached(&light(did), did).unwrap();
    }
    let first = commit_groups(&store, &scope, &[group("g", "G", &["a", "b"])]).unwrap();
    commit(&store, &scope, &[], &[key("a")], &["b"]).unwrap();
    let revoked = store.devices().restore_groups("account", "home").unwrap();
    assert_eq!(revoked.version, 2);
    assert!(revoked.groups.is_empty());
    store.devices().publish_cached(&light("a"), "A").unwrap();
    let after = store.devices().restore_groups("account", "home").unwrap();
    assert_eq!(after.version, 2);
    assert!(after.groups.is_empty());

    let restored = commit_groups(&store, &scope, &[group("g", "G", &["a", "b"])]).unwrap();
    assert_eq!(restored.version, 3);
    assert_eq!(restored.groups[0].unique_id, first.groups[0].unique_id);
    assert_eq!(restored.groups[0].endpoint, first.groups[0].endpoint);
}

#[test]
fn restore_groups_propagates_corrupt_member_descriptions() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    store.devices().publish_cached(&light("a"), "A").unwrap();
    commit_groups(&store, &scope(), &[group("g", "G", &["a"])]).unwrap();
    Connection::open(store.path())
        .unwrap()
        .execute(
            "UPDATE device_cache SET category='unknown' WHERE device_id='a'",
            [],
        )
        .unwrap();
    assert!(store.devices().restore_groups("account", "home").is_err());
}

#[test]
fn load_confirmed_rejects_corrupt_group_identity_and_orphan_relations() {
    for mutation in [
        "UPDATE group_identity SET unique_id='broken'",
        "UPDATE endpoint_allocation SET slot='main' WHERE slot='group'",
        "INSERT INTO group_relation VALUES('account','home','xiaomi','orphan','xiaomi','a')",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store.devices().publish_cached(&light("a"), "A").unwrap();
        commit_groups(&store, &scope(), &[group("g", "G", &["a"])]).unwrap();
        Connection::open(store.path())
            .unwrap()
            .execute_batch(mutation)
            .unwrap();
        assert!(
            store.devices().restore_groups("account", "home").is_err(),
            "{mutation}"
        );
    }
    for (label, expected_error) in [("x".repeat(33), true), ("valid".into(), false)] {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store.devices().publish_cached(&light("a"), "A").unwrap();
        commit_groups(&store, &scope(), &[group("g", "G", &["a"])]).unwrap();
        Connection::open(store.path())
            .unwrap()
            .execute("UPDATE group_identity SET label=?1", [&label])
            .unwrap();
        assert_eq!(
            store.devices().restore_groups("account", "home").is_err(),
            expected_error
        );
    }
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    store.devices().publish_cached(&light("a"), "A").unwrap();
    commit_groups(&store, &scope(), &[group("g", "G", &["a"])]).unwrap();
    Connection::open(store.path())
        .unwrap()
        .execute_batch(
            "INSERT INTO group_identity VALUES('xiaomi','g2','55555555555555555555555555555555',5,'G2');
             INSERT INTO endpoint_allocation VALUES(5,'xiaomi','g2','group');
             INSERT INTO group_relation VALUES('account','home','xiaomi','g2','xiaomi','a');",
        )
        .unwrap();
    assert!(store.devices().restore_groups("account", "home").is_err());
}

#[test]
fn commit_never_transitions_non_xiaomi_group_like_devices() {
    use migate::device::{CompositionError, PhysicalDeviceDescription};

    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let function = light("placeholder").device.functions[0].description.clone();
    let old = store
        .devices()
        .publish(&PhysicalDeviceDescription {
            key: DeviceKey::new("other", "group.42"),
            label: "Other light".into(),
            functions: vec![function],
        })
        .unwrap();
    store
        .devices()
        .publish_cached(&light("member"), "Member")
        .unwrap();
    assert!(matches!(
        commit_groups(
            &store,
            &scope(),
            &[Composition {
                key: DeviceKey::new("other", "group.42"),
                label: "Group".into(),
                members: vec![key("member")],
            }]
        ),
        Err(CompositionCommitError::Invalid(
            CompositionError::GroupKeyConflict(_)
        ))
    ));
    assert_eq!(
        store
            .devices()
            .load_published(&DeviceKey::new("other", "group.42"))
            .unwrap(),
        Some(old)
    );
}
