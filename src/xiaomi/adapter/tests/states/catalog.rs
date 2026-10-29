//! Atomic catalog upgrades and current-model restart restoration.
use super::*;

#[test]
fn native_state_directory_upgrade_is_atomic_and_restores_current_model_identity() {
    let rig = Rig::light("xiaomi.light.ceil04");
    {
        let mut cloud = rig.cloud.lock().unwrap();
        cloud.online.insert("one".into());
        cloud.raw_values.insert(("one".into(), 2, 1), json!(false));
        cloud.raw_values.insert(("one".into(), 2, 2), json!(50));
        cloud.raw_values.insert(("one".into(), 2, 3), json!(4000));
    }
    block_on(rig.run(|c| async move {
        let key = DeviceKey::new("xiaomi", "one");
        until(|| {
            function(&c.context.registry, "one", "light:2")
                .snapshot()
                .lighting()
                .unwrap()
                .temperature_kelvin
                == Some(4000)
        })
        .await;
        c.publication.rename(&key, "Saved state device").unwrap();
        c.store
            .matter()
            .put(rs_matter::persist::BASIC_INFO_KEY, &[0x11, 0x22])
            .unwrap();
        let before = c.publication.current().configuration.clone();
        let old = before.device(&key).unwrap().clone();
        let old_physical = c.context.registry.get(&key).unwrap();
        let db = rusqlite::Connection::open(c.store.path()).unwrap();
        db.execute_batch(
            "CREATE TRIGGER fail_state_upgrade BEFORE UPDATE ON bridge_configuration
             BEGIN SELECT RAISE(ABORT, 'injected state upgrade failure'); END;",
        )
        .unwrap();

        let new_urn = "urn:miot-spec-v2:device:light:0000A001:state-upgrade:2";
        let mut instance: Value = serde_json::from_str(include_str!(
            "../../../../../tests/fixtures/miot/xiaomi-light-ceil04.json"
        ))
        .unwrap();
        let contact: Value = serde_json::from_str(include_str!(
            "../../../../../tests/fixtures/miot/isa-dw2hl.json"
        ))
        .unwrap();
        let mut service = contact["services"]
            .as_array()
            .unwrap()
            .iter()
            .find(|service| {
                service["type"]
                    .as_str()
                    .unwrap()
                    .contains(":magnet-sensor:")
            })
            .unwrap()
            .clone();
        service["iid"] = json!(42);
        instance["type"] = json!(new_urn);
        instance["services"].as_array_mut().unwrap().push(service);
        {
            let mut cloud = c.cloud.lock().unwrap();
            cloud.instance_types.insert("one".into(), new_urn.into());
            cloud.catalog_instances.insert(new_urn.into(), instance);
            cloud.raw_values.insert(("one".into(), 42, 2), json!(true));
        }
        c.context.request_sync();
        until(|| c.publication.failure().is_some()).await;
        assert!(
            c.cloud
                .lock()
                .unwrap()
                .instance_reads
                .get(new_urn)
                .is_some_and(|reads| *reads >= 1)
        );
        assert_eq!(c.store.devices().load_configuration().unwrap(), before);
        assert!(
            c.context
                .registry
                .get(&key)
                .unwrap()
                .same_instance(&old_physical)
        );
        assert!(old_physical.function("contact:42:2").is_none());

        db.execute_batch("DROP TRIGGER fail_state_upgrade").unwrap();
        c.context.request_sync();
        until(|| {
            c.publication
                .current()
                .configuration
                .device(&key)
                .and_then(|device| device.native.as_ref())
                .is_some_and(|native| native.instance_type() == new_urn)
        })
        .await;
        until(|| {
            c.context
                .registry
                .get(&key)
                .and_then(|physical| physical.function("contact:42:2"))
                .is_some_and(|function| {
                    function.snapshot().state_sensor().unwrap().active == Some(false)
                })
        })
        .await;

        let current = c.publication.current().configuration.clone();
        let saved = current.device(&key).unwrap();
        assert_eq!(saved.unique_id, old.unique_id);
        assert_eq!(saved.endpoint, old.endpoint);
        assert_eq!(saved.label, old.label);
        assert_eq!(saved.configuration_version, old.configuration_version + 1);
        assert_eq!(current.root_version, before.root_version + 1);
        assert_eq!(
            saved
                .functions
                .iter()
                .map(|function| function.id.as_str())
                .collect::<Vec<_>>(),
            ["light:2", "contact:42:2"]
        );
        assert!(
            saved
                .functions
                .iter()
                .all(|function| function.endpoint != saved.endpoint)
        );
        assert_eq!(
            c.store
                .matter()
                .get(rs_matter::persist::BASIC_INFO_KEY)
                .unwrap(),
            Some(vec![0x11, 0x22])
        );

        let (_events, receiver) = flume::unbounded();
        let (discovery, _driver) = Discovery::with_events(receiver);
        let (restored, _) =
            XiaomiAdapter::new(c.store.clone(), c.context.cloud.clone(), discovery).unwrap();
        let app =
            Rc::new(crate::app::App::open(restored.source_handle(), c.store.clone()).unwrap());
        assert_eq!(app.current().configuration, current);
        let restored_physical = restored.context.registry.get(&key).unwrap();
        assert_eq!(
            restored_physical
                .function("contact:42:2")
                .unwrap()
                .snapshot()
                .state_sensor()
                .unwrap()
                .active,
            None
        );
        assert_eq!(
            restored.context.diagnostics().devices[0]
                .functions
                .iter()
                .find(|function| function.function == "contact:42:2")
                .unwrap()
                .observation_source,
            None
        );
    }));
}
