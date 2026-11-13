use super::*;

const PANEL: &str = "urn:miot-spec-v2:device:control-panel:0000A099:xiaomi-86v1:1";
const SWITCH: &str = "urn:miot-spec-v2:device:switch:0000A003:zimi-dhkg05:1:0000C810";

fn record(did: &str, urn: &str, name: &str, split: Option<Value>) -> Value {
    let mut value = json!({
        "did": did, "model": "xiaomi.controller.86v1", "spec_type": urn,
        "isOnline": true, "name": name,
    });
    if let Some(split) = split {
        value["extra"] = json!({"split": split});
    }
    value
}

fn alias(did: &str, parent: &str, siid: u32, name: &str) -> Value {
    record(
        did,
        PANEL,
        name,
        Some(json!({"parentId": parent, "moduleId": siid})),
    )
}

fn stored_json(store: &Store) -> Value {
    let db = rusqlite::Connection::open(store.path()).unwrap();
    let json: String = db
        .query_row(
            "SELECT json FROM bridge_configuration WHERE id=1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    serde_json::from_str(&json).unwrap()
}

#[test]
fn complete_catalog_binds_names_and_filters_aliases_before_description_requests() {
    let rig = Rig::new(
        false,
        &["panel", "left-alias", "middle-alias", "single", "other"],
    );
    {
        let mut cloud = rig.cloud.lock().unwrap();
        cloud.names.insert("panel".into(), "Panel".into());
        cloud.names.insert("single".into(), "Single key".into());
        cloud
            .catalog_overrides
            .insert("panel".into(), record("panel", PANEL, "Panel", None));
        cloud.catalog_overrides.insert(
            "single".into(),
            record("single", SWITCH, "Single key", None),
        );
        cloud.catalog_overrides.insert(
            "left-alias".into(),
            alias("left-alias", "panel", 10, "Left load"),
        );
        cloud.catalog_overrides.insert(
            "middle-alias".into(),
            alias("middle-alias", "panel", 11, "Middle load"),
        );
    }
    block_on(rig.run(|c| async move {
        c.announce();
        until(|| c.publication.current().published.list().len() >= 3).await;
        let json = stored_json(&c.store);
        let channels = json["split_devices"]
            .as_array()
            .expect("persisted split records");
        assert_eq!(channels.len(), 2);
        assert!(channels.iter().any(|item| item["did"] == "left-alias"
            && item["parent_did"] == "panel"
            && item["siid"] == 10
            && item["name"] == "Left load"));
        assert!(channels.iter().any(|item| item["did"] == "middle-alias"
            && item["parent_did"] == "panel"
            && item["siid"] == 11
            && item["name"] == "Middle load"));
        assert!(
            json["descriptions"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["device_id"] == "single" && item["name"] == "Single key")
        );
        for did in ["left-alias", "middle-alias"] {
            assert!(!c.context.live.borrow().devices.contains_key(did));
            assert!(
                c.context
                    .registry
                    .get(&DeviceKey::new("xiaomi", did))
                    .is_none()
            );
            assert!(
                !c.cloud
                    .lock()
                    .unwrap()
                    .detail_requests
                    .iter()
                    .flatten()
                    .any(|target| target == did)
            );
        }
        assert!(
            c.context
                .registry
                .get(&DeviceKey::new("xiaomi", "other"))
                .is_some()
        );

        // A failed full directory read preserves the last names and existing
        // physical devices, then retries a name-only change with the same URN.
        let reads = c.cloud.lock().unwrap().catalogs;
        {
            let mut cloud = c.cloud.lock().unwrap();
            cloud.catalog_status = 503;
            cloud.catalog_overrides.get_mut("left-alias").unwrap()["name"] = json!("Renamed load");
        }
        c.context.request_sync();
        until(|| c.cloud.lock().unwrap().catalogs > reads).await;
        assert!(
            stored_json(&c.store)["split_devices"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["did"] == "left-alias" && item["name"] == "Left load")
        );
        assert!(
            c.context
                .registry
                .get(&DeviceKey::new("xiaomi", "other"))
                .is_some()
        );
        c.cloud.lock().unwrap().catalog_status = 200;
        c.clock.advance(Duration::from_secs(3));
        c.context.request_sync();
        until(|| {
            stored_json(&c.store)["split_devices"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["did"] == "left-alias" && item["name"] == "Renamed load")
        })
        .await;
    }));
}

#[test]
fn cached_alias_never_restores_as_a_physical_device_even_when_catalog_is_offline() {
    let mut rig = Rig::new(true, &["panel", "left-alias", "other"]);
    let db = rusqlite::Connection::open(rig.store.path()).unwrap();
    db.execute("UPDATE bridge_configuration SET json=json_set(json, '$.split_devices', json(?1)) WHERE id=1", [json!([{"did":"left-alias","parent_did":"panel","siid":10,"name":"Left load"}]).to_string()]).unwrap();
    drop(db);
    rig.cloud.lock().unwrap().catalog_status = 503;
    let (source, ready) = XiaomiAdapter::build(
        rig.store.clone(),
        rig.source.cloud.clone(),
        rig.discovery.clone(),
        rig.clock.clone(),
    )
    .unwrap();
    assert!(
        source
            .context
            .registry
            .get(&DeviceKey::new("xiaomi", "panel"))
            .is_some()
    );
    assert!(
        source
            .context
            .registry
            .get(&DeviceKey::new("xiaomi", "other"))
            .is_some()
    );
    assert!(
        source
            .context
            .registry
            .get(&DeviceKey::new("xiaomi", "left-alias"))
            .is_none()
    );
    assert!(
        !source
            .context
            .live
            .borrow()
            .devices
            .contains_key("left-alias")
    );
    rig.source = source;
    rig.ready = ready;
    block_on(rig.run(|c| async move {
        c.announce();
        until(|| {
            c.cloud.lock().unwrap().catalogs > 0
                && c.context.live.borrow().devices["panel"].confirmed
        })
        .await;
        assert!(!c.context.live.borrow().devices.contains_key("left-alias"));
        assert!(
            c.context
                .registry
                .get(&DeviceKey::new("xiaomi", "other"))
                .is_some()
        );
        assert!(
            c.cloud
                .lock()
                .unwrap()
                .detail_requests
                .iter()
                .flatten()
                .all(|did| did != "left-alias")
        );
    }));
}

#[test]
fn another_home_cannot_contribute_panel_names_or_turn_an_alias_into_a_device() {
    let rig = Rig::new(false, &["panel", "foreign-alias", "own-alias", "other"]);
    {
        let mut cloud = rig.cloud.lock().unwrap();
        cloud
            .catalog_overrides
            .insert("panel".into(), record("panel", PANEL, "Panel", None));
        cloud.catalog_overrides.insert(
            "own-alias".into(),
            alias("own-alias", "panel", 10, "Own load"),
        );
        cloud.catalog_overrides.insert(
            "foreign-alias".into(),
            alias("foreign-alias", "foreign-panel", 10, "Foreign load"),
        );
        cloud
            .catalog_groups
            .push(record("foreign-panel", PANEL, "Foreign panel", None));
        cloud
            .catalog_groups
            .push(alias("outside-alias", "panel", 11, "Outside load"));
    }
    block_on(rig.run(|c| async move {
        c.announce();
        until(|| c.publication.current().published.list().len() >= 2).await;
        let json = stored_json(&c.store);
        let channels = json["split_devices"].as_array().unwrap();
        assert!(channels.iter().any(|item| item["did"] == "own-alias"));
        assert!(
            channels
                .iter()
                .any(|item| item["did"] == "foreign-alias" && item["name"].is_null())
        );
        assert!(!channels.iter().any(|item| item["did"] == "outside-alias"));
        for did in [
            "own-alias",
            "foreign-alias",
            "outside-alias",
            "foreign-panel",
        ] {
            assert!(!c.context.live.borrow().devices.contains_key(did));
        }
        assert!(
            c.cloud
                .lock()
                .unwrap()
                .detail_requests
                .iter()
                .flatten()
                .all(|did| !did.contains("alias"))
        );
    }));
}

#[test]
fn failed_first_catalog_defers_unknown_devices_until_the_retry_classifies_them() {
    let rig = Rig::new(false, &["panel", "left-alias", "other"]);
    {
        let mut cloud = rig.cloud.lock().unwrap();
        cloud.catalog_status = 503;
        cloud
            .catalog_overrides
            .insert("panel".into(), record("panel", PANEL, "Panel", None));
        cloud.catalog_overrides.insert(
            "left-alias".into(),
            alias("left-alias", "panel", 10, "Left load"),
        );
    }
    block_on(rig.run(|c| async move {
        c.announce();
        until(|| c.context.live.borrow().scope.is_some()).await;
        assert!(c.context.live.borrow().devices.is_empty());
        assert!(c.cloud.lock().unwrap().detail_requests.is_empty());
        assert!(c.context.status.borrow().catalog_problem.is_some());
        c.cloud.lock().unwrap().catalog_status = 200;
        c.clock.advance(Duration::from_secs(3));
        c.context.request_sync();
        until(|| c.publication.current().published.list().len() == 2).await;
        assert!(!c.context.live.borrow().devices.contains_key("left-alias"));
        assert!(
            c.context
                .registry
                .get(&DeviceKey::new("xiaomi", "other"))
                .is_some()
        );
    }));
}
