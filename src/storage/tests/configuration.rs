use super::support::stage_configuration;
use crate::{
    device::{
        Availability, Capability, Category, Composition, ConfigurationScope, DeviceKey,
        DeviceRegistry, FunctionDescription, Power,
    },
    storage::{
        BlobChange, CachedPhysicalDevice, CachedScope, CompositionCommitError,
        ConfigurationVersion, ConfirmedComposition, Store,
    },
    xiaomi::{
        capability::{ParsedDevice, ParsedFunction, PropertyBinding},
        cloud::group_id,
    },
};

fn cached_light(uid: &str, did: &str) -> CachedPhysicalDevice {
    CachedPhysicalDevice {
        uid: uid.into(),
        home_id: "local".into(),
        group_id: group_id(uid, "local"),
        model: "yeelink.light.light3".into(),
        instance_type: "urn:miot-spec-v2:device:light:0000A001:yeelink-light3:1:0000C802".into(),
        device: ParsedDevice {
            key: DeviceKey::new("xiaomi", did),
            functions: vec![ParsedFunction {
                description: FunctionDescription {
                    id: "main".into(),
                    label: "Light".into(),
                    category: Category::Light,
                    capabilities: vec![Capability::OnOff],
                },
                power: Some(PropertyBinding { siid: 2, piid: 1 }),
                lighting: None,
            }],
            refresh_trigger: None,
        },
    }
}

fn scope() -> CachedScope {
    CachedScope {
        uid: "uid".into(),
        home_id: "local".into(),
        group_id: group_id("uid", "local"),
    }
}

fn key(id: &str) -> DeviceKey {
    DeviceKey::new("xiaomi", id)
}

fn group(members: &[&str]) -> Composition {
    Composition {
        key: key("group-1"),
        label: "Group".into(),
        members: members.iter().map(|member| key(member)).collect(),
    }
}

fn publish(store: &Store, ids: &[&str]) {
    for id in ids {
        store
            .devices()
            .publish_cached(&cached_light("uid", id), id)
            .unwrap();
    }
}

fn confirm(registry: &DeviceRegistry, members: &[&str], removed: Vec<DeviceKey>) {
    registry
        .stage_configuration(
            ConfigurationScope {
                uid: "uid".into(),
                home_id: "local".into(),
                group_id: group_id("uid", "local"),
            },
            vec![group(members)],
            removed,
        )
        .unwrap();
}

fn apply(store: &Store, registry: &DeviceRegistry) -> ConfirmedComposition {
    store
        .devices()
        .apply_pending_configuration(registry, |_| Ok((Vec::new(), Vec::new())))
        .unwrap()
        .unwrap()
}

#[test]
fn mapping_sees_reserved_identity_and_commits_state_and_metadata_together() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    publish(&store, &["member", "standalone"]);
    let (registry, _receivers) = stage_configuration(
        &store.devices(),
        &scope(),
        &[group(&["member"])],
        &[],
        &[key("standalone")],
    )
    .unwrap();
    let member = registry.get(&key("member")).unwrap();
    member.functions()[0].observe(true);
    member.set_availability(Availability::Available);
    let confirmed = store
        .devices()
        .apply_pending_configuration(&registry, |prepared| {
            let group = &prepared.groups()[0];
            assert_eq!(group.unique_id.len(), 32);
            assert!(group.endpoint >= 2);
            assert_eq!(prepared.pending().devices[0].key(), &key("standalone"));
            assert!(store.devices().load_group_identity(&group.key)?.is_none());
            Ok((
                vec![ConfigurationVersion {
                    endpoint: group.endpoint,
                    version: 3,
                }],
                vec![BlobChange {
                    key: 99,
                    value: Some(b"fixed-shape".to_vec()),
                }],
            ))
        })
        .unwrap()
        .unwrap();
    assert_eq!(confirmed.version, 1);
    assert_eq!(confirmed.groups[0].capabilities, vec![Capability::OnOff]);
    assert_eq!(confirmed.devices[0].key, key("standalone"));
    assert_eq!(
        registry.snapshot().applied.groups[0].snapshot().power,
        Power::On
    );
    assert_eq!(
        store
            .devices()
            .configuration_version(confirmed.groups[0].endpoint)
            .unwrap(),
        Some(3)
    );
    assert_eq!(
        store.matter().get(99).unwrap().as_deref(),
        Some(b"fixed-shape".as_slice())
    );
    assert_eq!(
        store.devices().restore_groups("uid", "local").unwrap(),
        confirmed
    );
}

fn show_rollback(store: &Store, initial: &ConfirmedComposition) {
    assert_eq!(
        &store.devices().restore_groups("uid", "local").unwrap(),
        initial
    );
    assert_eq!(store.devices().configuration_version(7).unwrap(), None);
    assert_eq!(store.matter().get(99).unwrap(), None);
}

fn metadata() -> (Vec<ConfigurationVersion>, Vec<BlobChange>) {
    (
        vec![ConfigurationVersion {
            endpoint: 7,
            version: 2,
        }],
        vec![BlobChange {
            key: 99,
            value: Some(b"new".to_vec()),
        }],
    )
}

#[test]
fn failed_commit_preserves_relations_shape_metadata_applied_instances_and_pending_candidate() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    publish(&store, &["member", "standalone"]);
    let (registry, _receivers) = stage_configuration(
        &store.devices(),
        &scope(),
        &[group(&["member"])],
        &[],
        &[key("standalone")],
    )
    .unwrap();
    let member = registry.get(&key("member")).unwrap();
    member.functions()[0].observe(true);
    member.set_availability(Availability::Available);
    let initial = apply(&store, &registry);
    confirm(&registry, &["standalone"], Vec::new());
    rusqlite::Connection::open(store.path())
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER fail_blob BEFORE INSERT ON blobs
         WHEN NEW.key=99 BEGIN SELECT RAISE(ABORT, 'blob failed'); END;",
        )
        .unwrap();
    let (versions, blobs) = metadata();
    let error = store
        .devices()
        .apply_pending_configuration(&registry, |_| Ok((versions, blobs)))
        .unwrap_err();
    assert!(!format!("{error:?} {error}").contains("blob failed"));
    show_rollback(&store, &initial);
    let snapshot = registry.snapshot();
    assert_eq!(snapshot.applied.groups[0].snapshot().power, Power::On);
    assert_eq!(snapshot.applied.confirmed, Some(initial.version));
    assert_eq!(
        snapshot.pending.unwrap().groups[0].members,
        vec![key("standalone")]
    );
    apply(&store, &registry);
    assert_eq!(
        registry.snapshot().applied.groups[0].members,
        vec![key("standalone")]
    );
}

#[test]
fn configuration_versions_cannot_regress_and_stay_atomic() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let commit_version = |version| {
        store.devices().commit_protocol_configuration(
            vec![ConfigurationVersion {
                endpoint: 7,
                version,
            }],
            Vec::new(),
        )
    };
    commit_version(5).unwrap();
    assert!(matches!(
        commit_version(4),
        Err(CompositionCommitError::VersionRegression {
            endpoint: 7,
            current: 5,
            requested: 4
        })
    ));
    assert_eq!(store.devices().configuration_version(7).unwrap(), Some(5));
    assert_eq!(
        store
            .connection()
            .prepare("SELECT endpoint, version FROM configuration_version ORDER BY endpoint")
            .unwrap()
            .query_map([], |row| Ok(ConfigurationVersion {
                endpoint: row.get(0)?,
                version: row.get(1)?
            }))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap(),
        vec![ConfigurationVersion {
            endpoint: 7,
            version: 5
        }]
    );
}
