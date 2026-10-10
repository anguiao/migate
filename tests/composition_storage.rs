use migate::{
    device::{Composition, DeviceKey},
    storage::{CachedPhysicalDevice, CachedScope, CompositionCommitError, Store},
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

#[test]
fn commit_publishes_confirmed_group_and_restores_it() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    for did in ["a", "b"] {
        store.devices().publish_cached(&light(did), did).unwrap();
    }
    let confirmed = store
        .devices()
        .commit_composition(
            &scope(),
            &[Composition {
                key: DeviceKey::new("xiaomi", "group.1"),
                label: "Ceiling".into(),
                members: vec![DeviceKey::new("xiaomi", "a"), DeviceKey::new("xiaomi", "b")],
            }],
            &[],
        )
        .unwrap();
    assert_eq!(confirmed.version, 1);
    assert_eq!(confirmed.groups.len(), 1);
    assert_eq!(confirmed.groups[0].label, "Ceiling");
    assert_eq!(
        confirmed.groups[0].members,
        [DeviceKey::new("xiaomi", "a"), DeviceKey::new("xiaomi", "b")]
    );
    assert_eq!(
        store.devices().restore_groups("account", "home").unwrap(),
        confirmed
    );
    assert_eq!(
        store
            .devices()
            .load_published_group("account", "home", &DeviceKey::new("xiaomi", "group.1"))
            .unwrap()
            .as_ref(),
        Some(&confirmed.groups[0])
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
    store.devices().publish_cached(&light("c"), "C").unwrap();
    store.devices().remove(&key("c")).unwrap();

    assert!(matches!(
        store.devices().commit_composition(
            &scope,
            &[Composition {
                key: key("empty"),
                label: "Empty".into(),
                members: Vec::new(),
            }],
            &[],
        ),
        Err(CompositionCommitError::Invalid(
            CompositionError::EmptyGroup(_)
        ))
    ));
    assert!(matches!(
        store.devices().commit_composition(
            &scope,
            &[group("one", "One", &["a"]), group("two", "Two", &["a"])],
            &[],
        ),
        Err(CompositionCommitError::Invalid(
            CompositionError::OverlappingMember(_)
        ))
    ));
    assert!(matches!(
        store.devices().commit_composition(
            &scope,
            &[
                Composition {
                    key: key("nested-one"),
                    label: "Nested".into(),
                    members: vec![key("nested-two")],
                },
                group("nested-two", "Nested", &["a"]),
            ],
            &[],
        ),
        Err(CompositionCommitError::Invalid(
            CompositionError::NestedGroup(_)
        ))
    ));
    assert!(matches!(
        store.devices().commit_composition(
            &scope,
            &[group("missing", "Missing", &["absent"])],
            &[]
        ),
        Err(CompositionCommitError::UnsupportedMember(_))
    ));
    assert!(matches!(
        store
            .devices()
            .commit_composition(&scope, &[group("outlet", "Outlet", &["o"])], &[]),
        Err(CompositionCommitError::UnsupportedMember(_))
    ));
    assert!(matches!(
        store.devices().commit_composition(
            &scope,
            &[group("unpublished", "Unpublished", &["c"])],
            &[]
        ),
        Err(CompositionCommitError::UnsupportedMember(_))
    ));
    assert!(matches!(
        store.devices().commit_composition(
            &scope,
            &[group("group.1", "Group", &["a"])],
            &[key("a")],
        ),
        Err(CompositionCommitError::RemovedMember(_))
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
        5
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
    let first = store
        .devices()
        .commit_composition(&scope, &[group("group.1", "Cloud name", &["a"])], &[])
        .unwrap();
    let stored = first.groups[0].clone();
    store
        .devices()
        .set_group_label(&stored.key, "Saved group")
        .unwrap();

    let dissolved = store
        .devices()
        .commit_composition(&scope, &[], &[])
        .unwrap();
    assert_eq!(dissolved.version, 2);
    assert!(dissolved.groups.is_empty());
    assert!(
        store
            .devices()
            .load_published_group("account", "home", &stored.key)
            .unwrap()
            .is_none()
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

    let rejoined = store
        .devices()
        .commit_composition(
            &scope,
            &[group("group.1", "Later cloud name", &["b", "a"])],
            &[],
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
    store
        .devices()
        .commit_composition(
            &scope,
            &[group("g1", "One", &["a", "b"]), group("g2", "Two", &["c"])],
            &[],
        )
        .unwrap();

    let confirmed = store
        .devices()
        .commit_composition(&scope, &[group("g2", "Two", &["c"])], &[key("b")])
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
    let error = store
        .devices()
        .commit_composition(
            &scope,
            &[group("g1", "One", &["a"]), group("g2", "Two", &["c"])],
            &[],
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
fn commit_migrates_only_known_legacy_group_identities_to_parent_role() {
    for (did, model) in [
        ("group.42", "xiaomi.light.ceil04"),
        ("42", "mijia.light.group3"),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let scope = scope();
        let cached = group_like(did, model);
        let old = store
            .devices()
            .publish_cached(&cached, "Old light")
            .unwrap();
        store
            .devices()
            .publish_cached(&light("member"), "Member")
            .unwrap();
        let confirmed = store
            .devices()
            .commit_composition(&scope, &[group(did, "Group", &["member"])], &[])
            .unwrap();
        assert_eq!(confirmed.groups[0].unique_id, old.unique_id);
        assert_eq!(confirmed.groups[0].endpoint, old.endpoint);
        assert_eq!(confirmed.groups[0].label, "Old light");
        assert!(store.devices().load_published(&key(did)).unwrap().is_none());
        let db = Connection::open(store.path()).unwrap();
        assert_eq!(
            db.query_row(
                "SELECT count(*) FROM device_identity WHERE source='xiaomi' AND device_id=?1",
                [did],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
            0
        );
        assert_eq!(
            db.query_row(
                "SELECT slot FROM endpoint_allocation WHERE source='xiaomi' AND device_id=?1",
                [did],
                |row| row.get::<_, String>(0)
            )
            .unwrap(),
            "group"
        );
        assert!(matches!(
            store
                .devices()
                .publish(&migate::device::PhysicalDeviceDescription {
                    key: key(did),
                    label: "Taken over".into(),
                    functions: cached
                        .device
                        .functions
                        .iter()
                        .map(|function| function.description.clone())
                        .collect(),
                }),
            Err(migate::storage::AdmissionError::IncompatibleFunctions)
        ));
        let again = store
            .devices()
            .commit_composition(&scope, &[group(did, "Cloud rename", &["member"])], &[])
            .unwrap();
        assert_eq!(again, confirmed);
    }
}

#[test]
fn commit_rejects_promoting_an_ordinary_device_identity_to_group() {
    use migate::device::CompositionError;
    use migate::storage::CompositionCommitError;

    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let scope = scope();
    let old = store
        .devices()
        .publish_cached(&light("42"), "Ordinary light")
        .unwrap();
    store
        .devices()
        .publish_cached(&light("member"), "Member")
        .unwrap();
    assert!(matches!(
        store
            .devices()
            .commit_composition(&scope, &[group("42", "Group", &["member"])], &[]),
        Err(CompositionCommitError::Invalid(
            CompositionError::GroupKeyConflict(_)
        ))
    ));
    assert_eq!(
        store.devices().load_published(&key("42")).unwrap(),
        Some(old)
    );
    let db = Connection::open(store.path()).unwrap();
    assert_eq!(
        db.query_row("SELECT count(*) FROM group_identity", [], |row| row
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
}

#[test]
fn commit_rejects_known_group_identities_as_members() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let scope = scope();
    store
        .devices()
        .publish_cached(&group_like("group.42", "xiaomi.light.ceil04"), "Group-like")
        .unwrap();
    store
        .devices()
        .publish_cached(&group_like("42", "mijia.light.group3"), "Group-like")
        .unwrap();
    store
        .devices()
        .publish_cached(&light("member"), "Member")
        .unwrap();
    for member in ["group.42", "42"] {
        assert!(matches!(
            store.devices().commit_composition(
                &scope,
                &[group("parent", "Parent", &[member])],
                &[]
            ),
            Err(CompositionCommitError::UnsupportedMember(_))
        ));
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
        store
            .devices()
            .commit_composition(&scope, &[group("g", "G", &["a"])], &[]),
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
        store
            .devices()
            .commit_composition(&scope, &[group("g", "G", &["a"])], &[key("g")]),
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
    let first = store
        .devices()
        .commit_composition(&scope, &[group("g", "Saved name", &["a", "b"])], &[])
        .unwrap();
    let db = Connection::open(store.path()).unwrap();
    db.execute_batch(
        "CREATE TRIGGER reject_relation_insert BEFORE INSERT ON group_relation
         BEGIN SELECT RAISE(ABORT, 'no-op relation'); END;
         CREATE TRIGGER reject_identity_update BEFORE UPDATE ON group_identity
         BEGIN SELECT RAISE(ABORT, 'no-op identity'); END;",
    )
    .unwrap();
    let again = store
        .devices()
        .commit_composition(&scope, &[group("g", "Cloud rename", &["b", "a"])], &[])
        .unwrap();
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
    let home_confirmed = store
        .devices()
        .commit_composition(&home, &[group("group.1", "Home group", &["a"])], &[])
        .unwrap();
    let other_confirmed = store
        .devices()
        .commit_composition(&other, &[group("group.1", "Other group", &["b"])], &[])
        .unwrap();
    assert_eq!(
        other_confirmed.groups[0].unique_id,
        home_confirmed.groups[0].unique_id
    );
    assert_eq!(
        other_confirmed.groups[0].endpoint,
        home_confirmed.groups[0].endpoint
    );
    assert_eq!(other_confirmed.groups[0].label, "Home group");

    store.devices().commit_composition(&home, &[], &[]).unwrap();
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
            .load_published_group("account", "other", &key("group.1"))
            .unwrap()
            .is_some()
    );

    store
        .devices()
        .commit_composition(&other, &[], &[])
        .unwrap();
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
    let first = store
        .devices()
        .commit_composition(&scope, &[group("g", "G", &["a", "b"])], &[])
        .unwrap();
    store.devices().remove(&key("a")).unwrap();
    let revoked = store.devices().restore_groups("account", "home").unwrap();
    assert_eq!(revoked.version, 2);
    assert!(revoked.groups.is_empty());
    store.devices().publish_cached(&light("a"), "A").unwrap();
    let after = store.devices().restore_groups("account", "home").unwrap();
    assert_eq!(after.version, 2);
    assert!(after.groups.is_empty());

    let restored = store
        .devices()
        .commit_composition(&scope, &[group("g", "G", &["a", "b"])], &[])
        .unwrap();
    assert_eq!(restored.version, 3);
    assert_eq!(restored.groups[0].unique_id, first.groups[0].unique_id);
    assert_eq!(restored.groups[0].endpoint, first.groups[0].endpoint);
}

#[test]
fn restart_restores_the_confirmed_version() {
    let dir = tempfile::tempdir().unwrap();
    {
        let store = Store::open(dir.path()).unwrap();
        store.devices().publish_cached(&light("a"), "A").unwrap();
        store
            .devices()
            .commit_composition(&scope(), &[group("g", "G", &["a"])], &[])
            .unwrap();
    }
    let store = Store::open(dir.path()).unwrap();
    let confirmed = store.devices().restore_groups("account", "home").unwrap();
    assert_eq!(confirmed.version, 1);
    assert_eq!(confirmed.groups.len(), 1);
    assert_eq!(confirmed.groups[0].key, key("g"));
}

#[test]
fn restore_groups_propagates_corrupt_member_descriptions() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    store.devices().publish_cached(&light("a"), "A").unwrap();
    store
        .devices()
        .commit_composition(&scope(), &[group("g", "G", &["a"])], &[])
        .unwrap();
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
        store
            .devices()
            .commit_composition(&scope(), &[group("g", "G", &["a"])], &[])
            .unwrap();
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
        store
            .devices()
            .commit_composition(&scope(), &[group("g", "G", &["a"])], &[])
            .unwrap();
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
    store
        .devices()
        .commit_composition(&scope(), &[group("g", "G", &["a"])], &[])
        .unwrap();
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
fn stored_group_exposes_identity_without_a_main_light_function() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    store.devices().publish_cached(&light("a"), "A").unwrap();
    store
        .devices()
        .commit_composition(&scope(), &[group("g", "G", &["a"])], &[])
        .unwrap();
    let group = store
        .devices()
        .load_published_group("account", "home", &key("g"))
        .unwrap()
        .unwrap();
    assert_eq!(group.members, [key("a")]);
    assert!(
        store
            .devices()
            .load_published(&group.key)
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .devices()
            .restore_cached("account", "home")
            .unwrap()
            .iter()
            .all(|(_, stored)| stored.key != group.key)
    );
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
        store.devices().commit_composition(
            &scope(),
            &[Composition {
                key: DeviceKey::new("other", "group.42"),
                label: "Group".into(),
                members: vec![key("member")],
            }],
            &[],
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
