use super::*;
use crate::matter::tests::support::PublishedLightModel;

fn light_rig() -> Rig {
    let rig = Rig::light("xiaomi.light.ceil04");
    {
        let mut cloud = rig.cloud.lock().unwrap();
        cloud.online.insert("one".into());
        cloud.raw_values.insert(("one".into(), 2, 1), json!(true));
        cloud.raw_values.insert(("one".into(), 2, 2), json!(50));
        cloud.raw_values.insert(("one".into(), 2, 3), json!(4000));
    }
    rig
}

async fn ready(c: &Control) {
    until(|| {
        c.context.cloud_candidate("one", true).is_ok()
            && c.context
                .registry
                .get(&DeviceKey::new("xiaomi", "one"))
                .is_some_and(|physical| {
                    physical.functions()[0]
                        .snapshot()
                        .lighting
                        .and_then(|light| light.temperature_kelvin)
                        == Some(4000)
                })
    })
    .await;
}

#[test]
fn a_successful_native_replacement_restores_cloud_online_notifications() {
    let rig = Rig::lights(&[
        ("one", "xiaomi.light.ceil04"),
        ("two", "xiaomi.light.ceil04"),
    ]);
    rig.cloud
        .lock()
        .unwrap()
        .online
        .extend(["one".into(), "two".into()]);
    let listener = Async::<TcpListener>::bind(([127, 0, 0, 1], 0)).unwrap();
    rig.source.context.notification_connector.set(
        crate::xiaomi::adapter::notifications::NotificationConnector::Plaintext(
            listener.get_ref().local_addr().unwrap(),
        ),
    );
    let peer = Rc::new(CloudPeer::default());
    block_on(rig.run(|c| async move {
        future::or(peer.run(&listener), async {
            until(|| {
                cloud_active(&c, "one")
                    && cloud_active(&c, "two")
                    && peer.count("device/one/state/#") == 1
            })
            .await;
            c.clock
                .automatic
                .store(false, std::sync::atomic::Ordering::Relaxed);
            let old = c.context.live.borrow().devices["one"]
                .admitted
                .clone()
                .unwrap();
            let two = function(&c.context.registry, "two", "main");
            peer.property("two", 2, true);
            until(|| two.snapshot().power == Power::On).await;
            let urn = "urn:miot-spec-v2:device:light:0000A001:xiaomi-ceil04:2:0000C802";
            let mut instance = ceil04_instance();
            instance["type"] = json!(urn);
            set_temperature_range(&mut instance, 2700, 6500);
            {
                let mut cloud = c.cloud.lock().unwrap();
                cloud.instance_types.insert("one".into(), urn.into());
                cloud.catalog_instances.insert(urn.into(), instance);
            }
            c.context.request_sync();
            until(|| old.life.is_closed() && cloud_active(&c, "one")).await;
            let one = function(&c.context.registry, "one", "main");
            peer.online("one", false);
            peer.property("two", 2, false);
            until(|| two.snapshot().power == Power::Off).await;
            assert!(
                !c.context.live.borrow().devices["one"]
                    .cloud_access
                    .as_ref()
                    .unwrap()
                    .online,
                "the replacement must receive offline events on the existing cloud connection"
            );
            assert!(matches!(
                one.snapshot().availability,
                Availability::Unavailable(_)
            ));
            peer.online("one", true);
            peer.property("two", 2, true);
            until(|| two.snapshot().power == Power::On).await;
            until(|| one.snapshot().availability == Availability::Available).await;
            assert_eq!(peer.count("device/one/state/#"), 2);
            assert_eq!(peer.state.borrow().connects.len(), 1);
        })
        .await;
    }));
}

#[test]
fn a_failed_description_update_preserves_cloud_online_notifications() {
    let (rig, listener, peer) = with_cloud_peer(&["one", "two"]);
    block_on(rig.run(|c| async move {
        future::or(peer.run(&listener), async {
            until(|| cloud_active(&c, "one") && cloud_active(&c, "two")).await;
            let one = function(&c.context.registry, "one", "main");
            let two = function(&c.context.registry, "two", "main");
            until(|| one.snapshot().power == Power::Off && two.snapshot().power == Power::Off)
                .await;
            c.clock
                .automatic
                .store(false, std::sync::atomic::Ordering::Relaxed);
            let committed = c.store.devices().load_configuration().unwrap();
            let urn = "urn:miot-spec-v2:device:outlet:0000A002:urn-update:2";
            {
                let mut cloud = c.cloud.lock().unwrap();
                cloud.instance_types.insert("one".into(), urn.into());
                cloud
                    .catalog_instances
                    .insert(urn.into(), json!({"type": urn, "services": [{}]}));
            }
            c.context.request_sync();
            until(|| c.context.live.borrow().devices["one"].blocked.is_some()).await;
            // The second device provides a shared-wire processing barrier.
            peer.property("two", 2, true);
            until(|| two.snapshot().power == Power::On).await;
            peer.online("one", false);
            peer.property("two", 2, false);
            until(|| two.snapshot().power == Power::Off).await;
            assert!(
                !c.context.live.borrow().devices["one"]
                    .cloud_access
                    .as_ref()
                    .unwrap()
                    .online,
                "a failed new description must not retire the old online subscription"
            );
            assert!(matches!(
                one.snapshot().availability,
                Availability::Unavailable(_)
            ));
            peer.online("one", true);
            peer.property("two", 2, true);
            until(|| two.snapshot().power == Power::On).await;
            until(|| one.snapshot().availability == Availability::Available).await;
            one.execute(Command::On).await.unwrap();
            assert_eq!(c.cloud.lock().unwrap().property_writes.len(), 1);
            assert_eq!(c.store.devices().load_configuration().unwrap(), committed);
        })
        .await;
    }));
}

#[test]
fn invalid_or_unavailable_new_urn_keeps_the_committed_light_controllable() {
    let rig = light_rig();
    block_on(rig.run(|c| async move {
        let key = DeviceKey::new("xiaomi", "one");
        ready(&c).await;
        let app_before = c.publication.current();
        let committed = app_before.configuration.device(&key).cloned().unwrap();
        let physical = c.context.registry.get(&key).unwrap();
        let matter = PublishedLightModel::new(&c.publication, &c.store);
        let attributes = matter.read(committed.endpoint).await;
        let status = crate::terminal::AuthStatus::new(
            c.context.status.borrow().authentication.clone(),
            "/bin/migate".into(),
            "/data".into(),
        );
        status.update_adapter(c.context.diagnostics());
        let terminal = crate::terminal::bridge::Context::committed(c.publication.clone(), &status);
        let before = terminal
            .handle_line("status one", 0)
            .await
            .unwrap()
            .unwrap();
        let old_capabilities = before
            .lines()
            .find(|line| line.trim_start().starts_with("Capabilities:"))
            .unwrap()
            .to_string();
        for (urn, http_status, diagnostic) in [
            (
                "urn:miot-spec-v2:device:light:0000A001:urn-update:1",
                200,
                "invalid MIoT description: type",
            ),
            (
                "urn:miot-spec-v2:device:light:0000A001:urn-update:2",
                500,
                "500",
            ),
        ] {
            {
                let mut cloud = c.cloud.lock().unwrap();
                cloud.instance_types.insert("one".into(), urn.into());
                cloud.instance_status.insert(urn.into(), http_status);
                cloud
                    .catalog_instances
                    .insert(urn.into(), json!({"type": urn, "services": [{}]}));
            }
            c.context.request_sync();
            until(|| {
                c.cloud
                    .lock()
                    .unwrap()
                    .instance_reads
                    .get(urn)
                    .is_some_and(|reads| *reads >= 1)
            })
            .await;
            until(|| {
                c.context.live.borrow().devices["one"]
                    .blocked
                    .as_deref()
                    .is_some_and(|reason| reason.contains(diagnostic))
            })
            .await;
            status.update_adapter(c.context.diagnostics());
            let after = terminal
                .handle_line("status one", 0)
                .await
                .unwrap()
                .unwrap();
            assert!(
                after.contains(&format!(
                    "Configuration: root version {}.",
                    app_before.configuration.root_version
                )),
                "{after}"
            );
            assert!(after.contains(&old_capabilities), "{after}");
            let saved = c.store.devices().load_configuration().unwrap();
            assert_eq!(saved, app_before.configuration);
            let served = matter.read(committed.endpoint).await;
            assert_eq!(
                (
                    served.minimum_mireds,
                    served.maximum_mireds,
                    served.configuration_version
                ),
                (
                    attributes.minimum_mireds,
                    attributes.maximum_mireds,
                    attributes.configuration_version
                ),
            );
            assert!(physical.same_instance(&c.context.registry.get(&key).unwrap()));
            let writes = c.cloud.lock().unwrap().numeric_writes.len();
            assert_eq!(
                terminal.handle_line("off one", 0).await.unwrap().unwrap(),
                "one: command accepted"
            );
            until(|| c.cloud.lock().unwrap().numeric_writes.len() > writes).await;
        }
    }));
}

fn ceil04_instance() -> Value {
    serde_json::from_str(include_str!(
        "../../../../tests/fixtures/miot/xiaomi-light-ceil04.json"
    ))
    .unwrap()
}

fn set_temperature_range(instance: &mut Value, min: u64, max: u64) {
    let mut found = false;
    for service in instance["services"].as_array_mut().unwrap() {
        let Some(properties) = service.get_mut("properties").and_then(Value::as_array_mut) else {
            continue;
        };
        for property in properties {
            if property["type"]
                .as_str()
                .is_some_and(|kind| kind.contains(":color-temperature:"))
            {
                property["value-range"] = json!([min, max, 1]);
                found = true;
            }
        }
    }
    assert!(found, "the ceil04 fixture has a color-temperature property");
}

#[test]
fn new_urn_commit_failure_keeps_old_views_then_applies_and_restores_the_range() {
    let rig = light_rig();
    block_on(rig.run(|c| async move {
        let key = DeviceKey::new("xiaomi", "one");
        ready(&c).await;

        c.publication
            .rename(&key, "Saved ceiling")
            .expect("the committed light can be renamed");
        c.store
            .matter()
            .put(rs_matter::persist::BASIC_INFO_KEY, &[0x11, 0x22])
            .unwrap();
        let committed = c
            .publication
            .current()
            .configuration
            .device(&key)
            .cloned()
            .expect("restored light has a committed configuration");
        let unique_id = committed.unique_id.clone();
        let endpoint = committed.endpoint;
        assert_eq!(committed.label, "Saved ceiling");
        let root_before = c.publication.current().configuration.root_version;
        let old_matter = PublishedLightModel::new(&c.publication, &c.store);
        let old_attributes = old_matter.read(endpoint).await;
        let db = rusqlite::Connection::open(c.store.path()).unwrap();
        db.execute_batch(
            "CREATE TRIGGER fail_urn_commit BEFORE UPDATE ON bridge_configuration
             BEGIN SELECT RAISE(ABORT, 'injected URN commit failure'); END;",
        )
        .unwrap();

        let new_urn = "urn:miot-spec-v2:device:light:0000A001:xiaomi-ceil04-v2:1:0000C802";
        let mut instance = ceil04_instance();
        instance["type"] = json!(new_urn);
        set_temperature_range(&mut instance, 2700, 6500);
        {
            let mut cloud = c.cloud.lock().unwrap();
            cloud.instance_types.insert("one".into(), new_urn.into());
            cloud.catalog_instances.insert(new_urn.into(), instance);
        }
        c.context.request_sync();
        until(|| {
            c.cloud
                .lock()
                .unwrap()
                .instance_reads
                .get(new_urn)
                .is_some_and(|reads| *reads >= 1)
        })
        .await;
        until(|| {
            let live = c.context.live.borrow();
            assert!(
                live.devices["one"].blocked.is_none(),
                "unexpected description failure: {:?}",
                live.devices["one"].blocked
            );
            c.publication.failure().is_some()
        })
        .await;
        assert_eq!(old_matter.read(endpoint).await, old_attributes);
        assert_eq!(
            c.publication.current().configuration.root_version,
            root_before
        );
        assert_eq!(
            c.store.devices().load_configuration().unwrap().device(&key),
            Some(&committed)
        );
        let status = crate::terminal::AuthStatus::new(
            c.context.status.borrow().authentication.clone(),
            "/bin/migate".into(),
            "/data".into(),
        );
        status.update_adapter(c.context.diagnostics());
        let terminal = crate::terminal::bridge::Context::committed(c.publication.clone(), &status);
        let status_line = terminal
            .handle_line("status one", 0)
            .await
            .unwrap()
            .unwrap();
        assert!(!status_line.contains("temperature 2700–6500 K"));
        assert_eq!(
            terminal.handle_line("off one", 0).await.unwrap().unwrap(),
            "one: command accepted"
        );
        db.execute_batch("DROP TRIGGER fail_urn_commit").unwrap();
        c.context.request_sync();
        until(|| {
            c.publication
                .current()
                .configuration
                .device(&key)
                .and_then(|device| device.native.as_ref())
                .is_some_and(|native| native.instance_type == new_urn)
        })
        .await;
        {
            let mut cloud = c.cloud.lock().unwrap();
            cloud.raw_values.insert(("one".into(), 2, 1), json!(true));
            cloud.raw_values.insert(("one".into(), 2, 3), json!(6000));
        }
        c.context.refresh_source("one");
        until(|| {
            c.context.registry.get(&key).is_some_and(|physical| {
                physical.functions()[0]
                    .description()
                    .temperature_interval()
                    .is_some_and(|interval| interval.max() == 6500)
                    && physical.functions()[0]
                        .snapshot()
                        .lighting
                        .and_then(|lighting| lighting.temperature_kelvin)
                        == Some(6000)
            })
        })
        .await;

        let status = crate::terminal::AuthStatus::new(
            c.context.status.borrow().authentication.clone(),
            "/bin/migate".into(),
            "/data".into(),
        );
        status.update_adapter(c.context.diagnostics());
        let terminal = crate::terminal::bridge::Context::committed(c.publication.clone(), &status);
        let report = terminal
            .handle_line("status one", 0)
            .await
            .unwrap()
            .unwrap();
        assert!(
            report.contains("temperature 2700–6500 K"),
            "terminal status does not show the widened range:\n{report}"
        );
        assert!(
            report.contains("temperature: 6000 K"),
            "terminal status does not show real feedback inside the widened range:\n{report}"
        );

        let saved = c
            .store
            .devices()
            .load_configuration()
            .unwrap()
            .device(&key)
            .cloned()
            .unwrap();
        assert_eq!(saved.unique_id, unique_id);
        assert_eq!(saved.endpoint, endpoint);
        assert_eq!(saved.label, "Saved ceiling");
        assert_eq!(
            saved.configuration_version,
            committed.configuration_version + 1
        );
        assert_eq!(
            c.publication.current().configuration.root_version,
            root_before + 1
        );
        let new_matter = PublishedLightModel::new(&c.publication, &c.store);
        let updated = new_matter.read(endpoint).await;
        assert_eq!((updated.minimum_mireds, updated.maximum_mireds), (154, 370));
        assert_eq!(updated.current_mireds, Some(167));
        assert_eq!(updated.configuration_version, saved.configuration_version);
        assert_eq!(saved.native.as_ref().unwrap().model, "xiaomi.light.ceil04");
        assert_eq!(
            saved.native.as_ref().unwrap().device.functions()[0]
                .lighting()
                .unwrap()
                .temperature()
                .unwrap()
                .range()
                .max(),
            6500
        );
        assert_eq!(
            c.store
                .matter()
                .get(rs_matter::persist::BASIC_INFO_KEY)
                .unwrap(),
            Some(vec![0x11, 0x22]),
            "the bridge protocol blob must survive the directory URN update"
        );
        let (_events, receiver) = flume::unbounded();
        let (discovery, _driver) = Discovery::with_events(receiver);
        let (restored, _) =
            XiaomiAdapter::new(c.store.clone(), c.context.cloud.clone(), discovery).unwrap();
        let app =
            Rc::new(crate::app::App::open(restored.source_handle(), c.store.clone()).unwrap());
        let restored_matter = PublishedLightModel::new(&app, &c.store);
        let attributes = restored_matter.read(endpoint).await;
        assert_eq!(
            (attributes.minimum_mireds, attributes.maximum_mireds),
            (154, 370)
        );
        assert_eq!(
            attributes.configuration_version,
            saved.configuration_version
        );
        assert_eq!(app.current().configuration.device(&key), Some(&saved));
    }));
}

#[test]
fn metadata_only_type_change_reuses_the_instance_without_refetching() {
    let rig = light_rig();
    block_on(rig.run(|c| async move {
        let key = DeviceKey::new("xiaomi", "one");
        ready(&c).await;

        let before = c.publication.current();
        let root_version = before.configuration.root_version;
        let committed = before
            .configuration
            .device(&key)
            .cloned()
            .expect("restored light has a committed configuration");
        let configuration_version = committed.configuration_version;
        let physical = c.context.registry.get(&key).unwrap();
        let native_runtime = c.context.live.borrow().devices["one"]
            .admitted
            .clone()
            .unwrap();
        let matter = PublishedLightModel::new(&c.publication, &c.store);
        let attributes = matter.read(committed.endpoint).await;
        let model = committed.native.as_ref().unwrap().model.clone();

        // The control description is byte-identical except for the top-level
        // type URN, so only configuration metadata may change.
        let new_urn = "urn:miot-spec-v2:device:light:0000A001:xiaomi-ceil04:2:0000C802";
        let mut instance = ceil04_instance();
        instance["type"] = json!(new_urn);
        {
            let mut cloud = c.cloud.lock().unwrap();
            cloud.instance_types.insert("one".into(), new_urn.into());
            cloud.catalog_instances.insert(new_urn.into(), instance);
        }
        c.context.request_sync();
        until(|| {
            c.cloud
                .lock()
                .unwrap()
                .instance_reads
                .get(new_urn)
                .is_some_and(|reads| *reads >= 1)
        })
        .await;
        until(|| {
            c.publication
                .current()
                .configuration
                .device(&key)
                .and_then(|device| device.native.as_ref())
                .is_some_and(|native| native.instance_type == new_urn)
        })
        .await;

        let reads = c.cloud.lock().unwrap().instance_reads[new_urn];
        assert_eq!(
            reads, 1,
            "the first directory read must fetch the new instance exactly once"
        );
        let current = c.context.registry.get(&key).unwrap();
        assert!(
            physical.same_instance(&current),
            "a metadata-only URN change must reuse the served physical instance"
        );
        let saved = c
            .store
            .devices()
            .load_configuration()
            .unwrap()
            .device(&key)
            .cloned()
            .unwrap();
        assert_eq!(saved.unique_id, committed.unique_id);
        assert_eq!(saved.endpoint, committed.endpoint);
        assert_eq!(saved.label, committed.label);
        assert_eq!(saved.configuration_version, configuration_version);
        assert_eq!(
            c.publication.current().configuration.root_version,
            root_version
        );
        assert_eq!(saved.native.as_ref().unwrap().model, model);
        assert_eq!(saved.native.as_ref().unwrap().instance_type, new_urn);

        assert!(Rc::ptr_eq(
            &native_runtime,
            c.context.live.borrow().devices["one"]
                .admitted
                .as_ref()
                .unwrap()
        ));
        assert_eq!(matter.read(committed.endpoint).await, attributes);
        assert_eq!(
            c.publication.current().entries[0].native.instance_type,
            new_urn
        );
        let catalogs = c.cloud.lock().unwrap().catalogs;
        let mut synced = c.context.configuration_changed.listen();
        c.context.request_sync();
        loop {
            synced.await;
            if c.cloud.lock().unwrap().catalogs > catalogs {
                break;
            }
            synced = c.context.configuration_changed.listen();
        }
        assert_eq!(
            c.cloud.lock().unwrap().instance_reads[new_urn],
            reads,
            "a complete unchanged directory must reuse its effective native description"
        );
        assert_eq!(
            c.publication
                .current()
                .configuration
                .device(&key)
                .and_then(|device| device.native.as_ref())
                .map(|native| native.instance_type.as_str()),
            Some(new_urn)
        );
    }));
}

#[test]
fn a_new_directory_urn_supersedes_a_held_description_load() {
    let rig = light_rig();
    block_on(rig.run(|c| async move {
        let key = DeviceKey::new("xiaomi", "one");
        until(|| c.context.cloud_candidate("one", true).is_ok()).await;
        let root = c.publication.current().configuration.root_version;
        let old_urn = "urn:miot-spec-v2:device:light:0000A001:xiaomi-ceil04:2:0000C802";
        let new_urn = "urn:miot-spec-v2:device:light:0000A001:xiaomi-ceil04:3:0000C802";
        let (release, held) = flume::unbounded();
        {
            let mut cloud = c.cloud.lock().unwrap();
            for (urn, max) in [(old_urn, 6500), (new_urn, 7000)] {
                let mut instance = ceil04_instance();
                instance["type"] = json!(urn);
                set_temperature_range(&mut instance, 2700, max);
                cloud.catalog_instances.insert(urn.into(), instance);
            }
            cloud.instance_gates.insert(old_urn.into(), held);
            cloud.instance_types.insert("one".into(), old_urn.into());
        }
        c.context.request_sync();
        until(|| c.cloud.lock().unwrap().instance_reads.get(old_urn) == Some(&1)).await;
        c.cloud
            .lock()
            .unwrap()
            .instance_types
            .insert("one".into(), new_urn.into());
        c.context.request_sync();
        until(|| {
            c.publication
                .current()
                .configuration
                .device(&key)
                .and_then(|device| device.native.as_ref())
                .is_some_and(|native| native.instance_type == new_urn)
        })
        .await;
        release.send(()).unwrap();
        let catalogs = c.cloud.lock().unwrap().catalogs;
        let mut synced = c.context.configuration_changed.listen();
        c.context.request_sync();
        loop {
            synced.await;
            if c.cloud.lock().unwrap().catalogs > catalogs {
                break;
            }
            synced = c.context.configuration_changed.listen();
        }
        let current = c.publication.current();
        let device = current.configuration.device(&key).unwrap();
        assert_eq!(device.native.as_ref().unwrap().instance_type, new_urn);
        assert_eq!(current.configuration.root_version, root + 1);
        let matter = PublishedLightModel::new(&c.publication, &c.store);
        assert_eq!(matter.read(device.endpoint).await.minimum_mireds, 143);
        assert_eq!(c.cloud.lock().unwrap().instance_reads[new_urn], 1);
    }));
}
