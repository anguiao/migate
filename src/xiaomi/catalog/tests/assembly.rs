use super::*;

#[test]
fn split_service_entries_merge_into_parent_while_groups_stay_independent() {
    use crate::xiaomi::cloud::{CloudDevice, OwnedCatalog, OwnedHome, OwnedRoom};
    let device = |did: &str, name: &str, model: &str, urn: &str| CloudDevice {
        did: did.into(),
        uid: Some("42".into()),
        name: name.into(),
        model: model.into(),
        spec_type: Some(urn.into()),
        pid: Some(8),
        token: None,
        online: None,
        local_ip: None,
        parent_id: None,
    };
    let light_urn =
        serde_json::from_str::<Value>(&public_spec("yeelink.light.light3")).unwrap()["type"]
            .as_str()
            .unwrap()
            .to_owned();
    let group_urn =
        serde_json::from_str::<Value>(&public_spec("mijia.light.group3")).unwrap()["type"]
            .as_str()
            .unwrap()
            .to_owned();
    let cloud = OwnedCatalog {
        uid: "42".into(),
        homes: vec![OwnedHome {
            id: "1".into(),
            name: "Home".into(),
            group_id: "group".into(),
            dids: vec!["d1".into(), "d1.s2".into(), "group1".into()],
            rooms: vec![OwnedRoom {
                id: "r1".into(),
                name: "Room".into(),
                dids: vec!["d1".into(), "d1.s2".into()],
            }],
        }],
        devices: vec![
            device("d1", "Ceiling", "yeelink.light.light3", &light_urn),
            device(
                "d1.s2",
                "Bedside channel",
                "yeelink.light.light3",
                &light_urn,
            ),
            device("group1", "All lights", "mijia.light.group3", &group_urn),
        ],
    };
    let specs = [
        (light_urn.clone(), public_spec("yeelink.light.light3")),
        (group_urn.clone(), public_spec("mijia.light.group3")),
    ]
    .into_iter()
    .collect();
    let catalog = assemble_catalog(&cloud, &specs).unwrap();
    assert_eq!(catalog.devices.len(), 2);
    let parent = catalog
        .devices
        .iter()
        .find(|device| device.parent_did == "d1")
        .unwrap();
    assert_eq!(parent.features[0].definition.name, "Bedside channel");
    assert_eq!(parent.room_id.as_deref(), Some("r1"));
    assert!(
        catalog
            .devices
            .iter()
            .any(|device| device.parent_did == "group1")
    );
}

fn child_only_cloud_catalog(
    home_did: &str,
) -> (crate::xiaomi::cloud::OwnedCatalog, HashMap<String, String>) {
    use crate::storage::DeviceToken;
    use crate::xiaomi::cloud::{CloudDevice, OwnedCatalog, OwnedHome};
    let document = public_spec("yeelink.light.light3");
    let urn = serde_json::from_str::<Value>(&document).unwrap()["type"]
        .as_str()
        .unwrap()
        .to_owned();
    (
        OwnedCatalog {
            uid: "42".into(),
            homes: vec![OwnedHome {
                id: "1".into(),
                name: "Home".into(),
                group_id: "0123456789abcdef".into(),
                dids: vec![home_did.into()],
                rooms: vec![],
            }],
            devices: vec![CloudDevice {
                did: "d1.s2".into(),
                uid: Some("42".into()),
                name: "Diagnostic child".into(),
                model: "yeelink.light.light3".into(),
                spec_type: Some(urn.clone()),
                pid: Some(8),
                token: Some(DeviceToken(vec![7; 16])),
                online: Some(true),
                local_ip: Some("192.168.1.2".into()),
                parent_id: Some("d1".into()),
            }],
        },
        [(urn, document)].into_iter().collect(),
    )
}

#[test]
fn parent_membership_keeps_child_only_detail_as_unrecognized_parent_candidate() {
    let (cloud, specs) = child_only_cloud_catalog("d1");
    let catalog = assemble_catalog(&cloud, &specs).unwrap();
    assert_eq!(catalog.devices.len(), 1);
    let candidate = &catalog.devices[0];
    assert_eq!(candidate.parent_did, "d1");
    assert_eq!(candidate.name, "Diagnostic child");
    assert!(candidate.features.is_empty());
    assert_eq!(candidate.token, None);
    assert_eq!(candidate.local_ip, None);
}

#[test]
fn child_membership_does_not_promote_child_spec_to_complete_parent_features() {
    let (cloud, specs) = child_only_cloud_catalog("d1.s2");
    let catalog = assemble_catalog(&cloud, &specs).unwrap();
    assert_eq!(catalog.devices.len(), 1);
    assert!(catalog.devices[0].features.is_empty());
    assert_eq!(catalog.devices[0].token, None);
    assert_eq!(catalog.devices[0].local_ip, None);
}

#[test]
fn devices_without_specs_remain_diagnostic_candidates() {
    use crate::xiaomi::cloud::{CloudDevice, OwnedCatalog, OwnedHome};
    let cloud = OwnedCatalog {
        uid: "42".into(),
        homes: vec![OwnedHome {
            id: "1".into(),
            name: "Home".into(),
            group_id: "group".into(),
            dids: vec!["legacy".into()],
            rooms: vec![],
        }],
        devices: vec![CloudDevice {
            did: "legacy".into(),
            uid: Some("42".into()),
            name: "Legacy".into(),
            model: "vendor.legacy.x".into(),
            spec_type: None,
            pid: None,
            token: None,
            online: None,
            local_ip: None,
            parent_id: None,
        }],
    };
    let catalog = assemble_catalog(&cloud, &HashMap::new()).unwrap();
    assert_eq!(catalog.devices.len(), 1);
    assert_eq!(catalog.devices[0].spec_type, None);
    assert!(catalog.devices[0].features.is_empty());
}

#[test]
fn catalog_debug_output_redacts_device_tokens() {
    use crate::storage::DeviceToken;
    let device = CatalogDevice {
        home_id: "1".into(),
        room_id: None,
        parent_did: "did".into(),
        name: "Device".into(),
        model: "vendor.device.x".into(),
        spec_type: None,
        pid: None,
        token: Some(DeviceToken(vec![0x11; 16])),
        online: None,
        local_ip: None,
        parent_id: None,
        features: vec![],
    };
    let debug = format!("{device:?}");
    assert!(debug.contains("[REDACTED]"));
    assert!(!debug.contains("17, 17"));
}

#[test]
fn persisted_catalog_restores_compiled_mappings_and_split_names_after_logout_and_reopen() {
    use crate::storage::{Store, TokenSet, XiaomiRecord};
    use crate::xiaomi::cloud::{CloudDevice, OwnedCatalog, OwnedHome};

    let document = public_spec("yeelink.light.light3");
    let urn = serde_json::from_str::<Value>(&document).unwrap()["type"]
        .as_str()
        .unwrap()
        .to_owned();
    let bath_document = public_spec("yeelink.bhf_light.v13");
    let bath_urn = serde_json::from_str::<Value>(&bath_document).unwrap()["type"]
        .as_str()
        .unwrap()
        .to_owned();
    let cloud = OwnedCatalog {
        uid: "42".into(),
        homes: vec![OwnedHome {
            id: "1".into(),
            name: "Home".into(),
            group_id: "0123456789abcdef".into(),
            dids: vec!["d1".into(), "d1.s2".into(), "bath".into(), "ignored".into()],
            rooms: vec![],
        }],
        devices: vec![
            CloudDevice {
                did: "d1".into(),
                uid: Some("42".into()),
                name: "Light".into(),
                model: "yeelink.light.light3".into(),
                spec_type: Some(urn.clone()),
                pid: Some(8),
                token: None,
                online: Some(false),
                local_ip: None,
                parent_id: None,
            },
            CloudDevice {
                did: "d1.s2".into(),
                uid: Some("42".into()),
                name: "Named channel".into(),
                model: "yeelink.light.light3".into(),
                spec_type: Some(urn.clone()),
                pid: Some(8),
                token: None,
                online: Some(false),
                local_ip: None,
                parent_id: Some("d1".into()),
            },
            CloudDevice {
                did: "bath".into(),
                uid: Some("42".into()),
                name: "Bath heater".into(),
                model: "yeelink.bhf_light.v13".into(),
                spec_type: Some(bath_urn.clone()),
                pid: Some(8),
                token: None,
                online: None,
                local_ip: None,
                parent_id: None,
            },
            CloudDevice {
                did: "ignored".into(),
                uid: Some("42".into()),
                name: "Deferred".into(),
                model: "yeelink.light.light3".into(),
                spec_type: Some(urn.clone()),
                pid: Some(8),
                token: None,
                online: None,
                local_ip: None,
                parent_id: None,
            },
        ],
    };
    let specs = [(urn, document), (bath_urn, bath_document)]
        .into_iter()
        .collect();
    let mut catalog = assemble_catalog(&cloud, &specs).unwrap();
    for feature in &mut catalog
        .devices
        .iter_mut()
        .find(|device| device.parent_did == "bath")
        .unwrap()
        .features
    {
        feature.definition.name = format!("Custom {}", feature.definition.role.as_str());
    }
    catalog
        .devices
        .iter_mut()
        .find(|device| device.parent_did == "ignored")
        .unwrap()
        .features
        .clear();
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    store
        .xiaomi()
        .replace(&XiaomiRecord {
            uid: "42".into(),
            region: "cn".into(),
            oauth_client_uuid: "550e8400-e29b-41d4-a716-446655440000".into(),
            redirect_uri: "http://homeassistant.local/callback".into(),
            tokens: TokenSet {
                access_token: "access".into(),
                refresh_token: "refresh".into(),
                expires_at: 20,
                refresh_at: 10,
            },
            virtual_did: "1".into(),
            private_key_pem: "private".into(),
            certificate_pem: "certificate".into(),
        })
        .unwrap();
    let revision = store.xiaomi().snapshot().unwrap().revision;
    assert!(persist_catalog(&store.devices(), &catalog, &specs, 10, revision).unwrap());
    store.xiaomi().logout().unwrap();
    drop(store);

    let reopened = Store::open(directory.path()).unwrap();
    let restored = restore_catalog(&reopened.devices(), "42").unwrap();
    assert_eq!(restored.homes, catalog.homes);
    assert_eq!(restored.devices, catalog.devices);
}
