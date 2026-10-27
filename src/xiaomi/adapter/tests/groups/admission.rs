use super::*;

#[test]
fn native_group_description_restarts_after_its_loading_token_is_retired() {
    let rig = Rig::new(true, &["one"]);
    rig.clock
        .automatic
        .store(false, std::sync::atomic::Ordering::Relaxed);
    let instance = group_instance();
    let urn = instance["type"].as_str().unwrap().to_string();
    let (release, gate) = flume::bounded(1);
    {
        let mut cloud = rig.cloud.lock().unwrap();
        cloud.catalog_groups = vec![group_record("record", "group-1", &["one"], false)];
        cloud.catalog_instances.insert(urn.clone(), instance);
        cloud.instance_gates.insert(urn.clone(), gate);
    }
    block_on(rig.run(|c| async move {
        until(|| c.cloud.lock().unwrap().instance_reads.get(&urn) == Some(&1)).await;
        let loading = c.context.live.borrow().devices["group-1"]
            .loading
            .clone()
            .unwrap()
            .life;
        c.cloud.lock().unwrap().instance_gates.remove(&urn);
        let mut credentials = c.context.live.borrow().credentials.clone().unwrap();
        credentials.tokens.access_token = "replacement-access".into();
        c.store.xiaomi().replace(&credentials).unwrap();
        c.context.adopt_credentials(credentials);
        future::or(
            async {
                until(|| {
                    c.context.live.borrow().devices["group-1"]
                        .loading
                        .as_ref()
                        .is_none_or(|current| !Rc::ptr_eq(&current.life, &loading))
                })
                .await;
            },
            async {
                async_io::Timer::after(Duration::from_secs(2)).await;
                panic!("retired group loading still owns its slot");
            },
        )
        .await;
        release.send(()).unwrap();
        c.context.request_sync();
        until(|| {
            c.context
                .registry
                .get(&DeviceKey::new("xiaomi", "group-1"))
                .is_some()
        })
        .await;
        assert!(c.cloud.lock().unwrap().instance_reads[&urn] >= 2);
    }));
}

#[test]
fn native_group_cloud_permission_recovers_only_after_fresh_confirmation() {
    let rig = super::super::support::repair_rig(1, true);
    block_on(rig.run(|c| async move {
        super::super::support::repair_ready(&c).await;
        c.clock
            .automatic
            .store(false, std::sync::atomic::Ordering::Relaxed);
        let light = function(&c.context.registry, "group-1", "main");
        c.cloud.lock().unwrap().property_code = -704_012_904;
        assert!(light.execute(Command::Off).await.is_err());
        assert!(matches!(
            light.snapshot().availability,
            Availability::Unavailable(_)
        ));

        // A failed membership read retains the group but cannot restore access.
        {
            let mut cloud = c.cloud.lock().unwrap();
            cloud.property_code = 0;
            cloud.catalog_groups[0]["extra"]["member_list"]["status"] = json!("invalid");
        }
        c.context.request_sync();
        until(|| c.context.relations.borrow().snapshot()[0].error.is_some()).await;
        assert!(matches!(
            light.execute(Command::Off).await,
            Err(crate::device::CommandError::Unavailable(_))
        ));
        assert_eq!(c.cloud.lock().unwrap().numeric_writes.len(), 1);

        c.cloud.lock().unwrap().catalog_groups[0]["extra"]["member_list"]["status"] = json!("1");
        c.context.request_sync();
        until(|| c.context.relations.borrow().snapshot()[0].error.is_none()).await;
        assert_eq!(light.snapshot().availability, Availability::Available);
        light.execute(Command::Off).await.unwrap();
        assert_eq!(c.cloud.lock().unwrap().numeric_writes.len(), 2);
    }));
}

#[test]
fn native_group_publication_preserves_complete_configuration_on_storage_failure() {
    let rig = Rig::new(false, &["one"]);
    let instance: Value = serde_json::from_str(include_str!(
        "../../../../../tests/fixtures/miot/group3-instance.json"
    ))
    .unwrap();
    {
        let mut cloud = rig.cloud.lock().unwrap();
        cloud.members.push("member-new".into());
        cloud
            .catalog_instances
            .insert(instance["type"].as_str().unwrap().into(), instance);
        cloud.catalog_groups = vec![group_record("record-1", "group-1", &["one"], false)];
    }
    block_on(rig.run(|c| async move {
        c.announce();
        until(|| c.context.registry.get(&DeviceKey::new("xiaomi", "group-1")).is_some()).await;
        let key = DeviceKey::new("xiaomi", "group-1");
        let committed = c.store.devices().load_configuration().unwrap().device(&key).cloned()
            .expect("native admission must use the complete configuration commit");
        let _scope = c.context.live.borrow().scope.clone().unwrap();
        let old = c.store.devices().load_configuration().unwrap();
        assert!(
            old.devices
                .iter()
                .any(|device| device.key == key && device.native.is_some())
        );
        assert_eq!(old.groups[0].members, ["one"]);
        let member = DeviceKey::new("xiaomi", "one");
        assert!(!old.derive().published.contains(&member), "a member of a published group must be hidden");
        assert!(c.publication.current().published.get(&member).is_none());

        let db = rusqlite::Connection::open(c.store.path()).unwrap();
        db.execute_batch("CREATE TRIGGER fail_configuration BEFORE UPDATE ON bridge_configuration WHEN NEW.json LIKE '%member-new%' BEGIN SELECT RAISE(ABORT, 'injected'); END;").unwrap();
        // A newly confirmed member changes the candidate relation; the serving
        // source, identity and repair inputs must remain on the old full view.
        {
            let mut cloud = c.cloud.lock().unwrap();
            cloud.catalog_groups = vec![group_record("record-1", "group-1", &["one", "member-new"], false)];
        }
        c.context.request_sync();
        until(|| c.context.relations.borrow().snapshot().iter().any(|group| group.membership.members.iter().any(|member| member == "member-new"))).await;
        until(|| c.publication.failure().is_some()).await;
        let status = crate::terminal::AuthStatus::new(
            c.context.status.borrow().authentication.clone(),
            "/bin/migate".into(),
            "/data".into(),
        );
        let terminal =
            crate::terminal::bridge::Context::committed(c.publication.clone(), &status);
        let report = terminal.handle_line("status", 0).await.unwrap().unwrap();
        assert!(report.contains("pending application"));
        assert!(report.contains("Configuration apply failed:"));
        assert_eq!(
            c.context.graph.borrow().rules["group-1"]
                .members
                .iter()
                .cloned()
                .collect::<Vec<_>>(),
            ["one"]
        );
        let saved = c.store.devices().load_configuration().unwrap();
        assert_eq!(saved.root_version, old.root_version);
        assert_eq!(saved.groups[0].members, old.groups[0].members);
        assert_eq!(c.store.devices().load_configuration().unwrap().device(&key).cloned().unwrap().unique_id, committed.unique_id);
    }));
}

#[test]
fn first_binding_applies_the_initial_catalog_without_a_cached_scope() {
    let rig = Rig::new(false, &["one"]);
    rig.cloud.lock().unwrap().catalog_groups =
        vec![group_record("group-record", "group-1", &["one"], false)];
    block_on(rig.run(|control| async move {
        control.announce();
        until(|| control.cloud.lock().unwrap().homes > 0).await;
        until(|| {
            control
                .context
                .relations
                .borrow()
                .snapshot()
                .iter()
                .any(|group| {
                    group.active
                        && group
                            .membership
                            .members
                            .iter()
                            .any(|member| member == "one")
                })
        })
        .await;
        assert!(control.context.live.borrow().scope.is_some());
        assert_eq!(control.cloud.lock().unwrap().catalogs, 1);
    }));
}

#[test]
fn missing_description_member_group_is_controllable_with_unknown_state() {
    let rig = Rig::new(false, &["one"]);
    let instance: Value = serde_json::from_str(include_str!(
        "../../../../../tests/fixtures/miot/group3-instance.json"
    ))
    .unwrap();
    let instance_type = instance["type"].as_str().unwrap().to_string();
    {
        let mut cloud = rig.cloud.lock().unwrap();
        // A home member that never receives its own description record.
        cloud.members.push("member-a".into());
        cloud.catalog_groups = vec![group_record(
            "group-record",
            "group-1",
            &["member-a"],
            false,
        )];
        cloud.catalog_instances.insert(instance_type, instance);
    }
    block_on(rig.run(|c| async move {
        c.announce();
        until(|| c.cloud.lock().unwrap().homes > 0).await;
        until(|| {
            c.context
                .live
                .borrow()
                .devices
                .get("group-1")
                .is_some_and(|slot| slot.admitted.is_some())
        })
        .await;
        {
            let live = c.context.live.borrow();
            let slot = &live.devices["group-1"];
            let record = slot.admitted.as_ref().unwrap();
            let cached = record.native.clone();
            assert_eq!(cached.model(), "mijia.light.group3");
            let light = cached
                .device()
                .functions()
                .iter()
                .find(|function| function.lighting().is_some())
                .unwrap();
            assert!(
                light
                    .description()
                    .capabilities()
                    .iter()
                    .any(|capability| matches!(capability, crate::device::Capability::Brightness))
            );
            assert!(light.description().capabilities().iter().any(|capability| {
                matches!(capability, crate::device::Capability::ColorTemperature(_))
            }));
            // The member has no description and no source of its own.
            assert!(live.devices["member-a"].admitted.is_none());
            // Native group commands use cloud access; its state needs known members.
            assert!(
                live.devices["group-1"]
                    .admitted
                    .as_ref()
                    .is_none_or(|work| !work.test_local_attached())
            );
        }
        assert!(matches!(
            c.context.control_path("group-1"),
            Some(super::super::routing::ControlPath::Cloud(_))
        ));
        let first = c.context.live.borrow().devices["group-1"]
            .admitted
            .clone()
            .unwrap();
        assert_eq!(first.physical().label(), "Native group");
        let light = first.physical().functions()[0].clone();
        assert_eq!(light.snapshot().power(), Some(Power::Unknown));
        let state = light.snapshot().lighting().unwrap();
        assert_eq!(state.brightness_percent, None);
        assert_eq!(state.temperature_kelvin, None);
        assert_eq!(state.color, None);
        assert_eq!(state.mode, None);
        light.execute(Command::On).await.unwrap();
        assert_eq!(light.snapshot().power(), Some(Power::Unknown));
        assert!(
            c.cloud
                .lock()
                .unwrap()
                .property_reads
                .iter()
                .all(|(did, _, _)| did != "group-1")
        );
        // A repeated complete directory must preserve the native instance.
        c.context.request_sync();
        until(|| c.cloud.lock().unwrap().catalogs >= 2).await;
        {
            let live = c.context.live.borrow();
            let slot = &live.devices["group-1"];
            assert!(std::rc::Rc::ptr_eq(slot.admitted.as_ref().unwrap(), &first));
            assert!(slot.cloud_access.is_some());
        }
        // A failed account catalog keeps confirmed groups and their access.
        c.cloud.lock().unwrap().catalog_status = 503;
        c.context.request_sync();
        until(|| c.cloud.lock().unwrap().catalogs >= 3).await;
        {
            let live = c.context.live.borrow();
            let slot = &live.devices["group-1"];
            assert!(std::rc::Rc::ptr_eq(slot.admitted.as_ref().unwrap(), &first));
            assert!(slot.cloud_access.is_some());
        }
    }));
}
