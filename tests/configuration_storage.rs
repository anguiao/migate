use migate::{
    device::{
        Capability, Category, ConfigurationScope, DeviceKey, DeviceRegistry, FunctionDescription,
        PhysicalDevice, PublishedEndpoint,
    },
    storage::{BlobChange, CachedPhysicalDevice, ConfigurationVersion, Store},
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

fn key(id: &str) -> DeviceKey {
    DeviceKey::new("xiaomi", id)
}

fn scope() -> ConfigurationScope {
    ConfigurationScope {
        uid: "uid".into(),
        home_id: "local".into(),
        group_id: group_id("uid", "local"),
    }
}

#[test]
fn pending_admission_restart_keeps_the_last_applied_set_and_versions() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let registry = DeviceRegistry::new();
    let stored = store
        .devices()
        .publish_cached(&cached_light("uid", "first"), "First")
        .unwrap();
    registry.insert(PhysicalDevice::new(stored.description()).0);
    registry
        .stage_configuration(scope(), Vec::new(), Vec::new())
        .unwrap();
    let applied = store
        .devices()
        .apply_pending_configuration(&registry, |_| Ok((Vec::new(), Vec::new())))
        .unwrap()
        .unwrap();
    assert_eq!(applied.version, 1);
    assert_eq!(
        applied.devices,
        vec![PublishedEndpoint {
            key: key("first"),
            endpoint: 2
        }]
    );
    // The latest committed root version and fixed shape blob belong to the
    // applied set, not to any later admission.
    store
        .devices()
        .commit_protocol_configuration(
            vec![ConfigurationVersion {
                endpoint: 2,
                version: 3,
            }],
            vec![BlobChange {
                key: 50,
                value: Some(b"old-shape".to_vec()),
            }],
        )
        .unwrap();
    assert_eq!(
        store
            .devices()
            .restore_groups("uid", "local")
            .unwrap()
            .version,
        1
    );

    // A second device is admitted (its identity reservation is durable) and a
    // candidate set is prepared, but the configuration is not committed.
    let stored = store
        .devices()
        .publish_cached(&cached_light("uid", "second"), "Second")
        .unwrap();
    registry.insert(PhysicalDevice::new(stored.description()).0);
    registry
        .stage_configuration(scope(), Vec::new(), Vec::new())
        .unwrap();
    assert_eq!(registry.pending_configuration().unwrap().devices.len(), 2);

    drop(store);
    let store = Store::open(dir.path()).unwrap();
    let restored = store.devices().restore_groups("uid", "local").unwrap();
    assert_eq!(restored.version, 1);
    assert_eq!(
        restored.devices,
        vec![PublishedEndpoint {
            key: key("first"),
            endpoint: 2
        }],
        "a pending admission must not enter the applied set"
    );
    // The identity and endpoint reservation survive for the next candidate.
    let reservation = store
        .devices()
        .load_published(&key("second"))
        .unwrap()
        .expect("reservation survives restart");
    assert_eq!(reservation.endpoint, 3);
    // The applied Matter version and blob are still the last committed ones.
    assert_eq!(store.devices().configuration_version(2).unwrap(), Some(3));
    assert_eq!(
        store.matter().get(50).unwrap().as_deref(),
        Some(b"old-shape".as_slice())
    );
}
