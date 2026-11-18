use super::*;

#[test]
fn readmitted_group_waits_for_its_new_native_description() {
    let rig = super::super::support::repair_rig(1, true);
    block_on(rig.run(|c| async move {
        super::super::support::repair_ready(&c).await;
        let key = DeviceKey::new("xiaomi", "group-1");
        let db = rusqlite::Connection::open(c.store.path()).unwrap();
        db.execute_batch(
            "CREATE TRIGGER block_withdrawal BEFORE UPDATE ON bridge_configuration
            BEGIN SELECT RAISE(ABORT, 'injected'); END;",
        )
        .unwrap();
        let mut group = c.cloud.lock().unwrap().catalog_groups.pop().unwrap();
        c.context.request_sync();
        until(|| {
            !c.context.directory.borrow().devices.contains_key("group-1")
                && c.publication.failure().is_some()
        })
        .await;

        let urn = "urn:miot-spec-v2:device:light:0000A001:mijia-group3:4:0000C802";
        let mut instance = group_instance();
        instance["type"] = json!(urn);
        group["spec_type"] = json!(urn);
        let (release, gate) = flume::bounded(1);
        {
            let mut cloud = c.cloud.lock().unwrap();
            cloud.catalog_groups.push(group);
            cloud.catalog_instances.insert(urn.into(), instance);
            cloud.instance_gates.insert(urn.into(), gate);
        }
        c.context.request_sync();
        until(|| c.cloud.lock().unwrap().instance_reads.contains_key(urn)).await;
        db.execute_batch("DROP TRIGGER block_withdrawal").unwrap();
        let plan = c.publication.plan().unwrap().unwrap();
        assert!(plan.clear_revocations.is_empty());
        c.publication.apply(c.publication.prepare(plan)).unwrap();
        assert!(c.context.physical(&key).is_none());
        assert!(
            c.store
                .devices()
                .load_revocations()
                .unwrap()
                .iter()
                .any(|record| record.key == key)
        );

        release.send(()).unwrap();
        until(|| c.context.physical(&key).is_some()).await;
        assert_eq!(
            c.context.directory.borrow().devices["group-1"]
                .work
                .as_ref()
                .unwrap()
                .native
                .instance_type(),
            urn
        );
        assert!(c.store.devices().load_revocations().unwrap().is_empty());
    }));
}

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
        let loading = c.context.directory.borrow().devices["group-1"]
            .loading
            .clone()
            .unwrap()
            .life;
        c.cloud.lock().unwrap().instance_gates.remove(&urn);
        let mut credentials = c.context.credentials.borrow().record().cloned().unwrap();
        credentials.ha.authorization.tokens.access_token = "replacement-access".into();
        c.store.xiaomi().fixture_replace(&credentials).unwrap();
        c.context.adopt_credentials(credentials);
        future::or(
            async {
                until(|| {
                    c.context.directory.borrow().devices["group-1"]
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
                .physical(&DeviceKey::new("xiaomi", "group-1"))
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
        let light = function(&c.context, "group-1", "light:2");
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
        until(|| {
            c.context.directory.borrow().confirmed_groups()[0]
                .error
                .is_some()
        })
        .await;
        assert!(matches!(
            light.execute(Command::Off).await,
            Err(crate::device::CommandError::Unavailable(_))
        ));
        assert_eq!(c.cloud.lock().unwrap().numeric_writes.len(), 1);

        c.cloud.lock().unwrap().catalog_groups[0]["extra"]["member_list"]["status"] = json!("1");
        c.context.request_sync();
        until(|| {
            c.context.directory.borrow().confirmed_groups()[0]
                .error
                .is_none()
        })
        .await;
        assert_eq!(light.snapshot().availability, Availability::Available);
        light.execute(Command::Off).await.unwrap();
        assert_eq!(c.cloud.lock().unwrap().numeric_writes.len(), 2);
    }));
}

#[test]
fn native_group_publication_preserves_complete_configuration_on_storage_failure() {
    let rig = Rig::unrestored_with_member_light(&["one"]);
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
        until(|| c.context.physical(&DeviceKey::new("xiaomi", "group-1")).is_some()
            && c.context.graph.borrow().rules.get("group-1").is_some_and(|rule|
                rule.members.iter().any(|member| member.device.id == "one"))).await;
        let key = DeviceKey::new("xiaomi", "group-1");
        let committed = c.store.devices().load_configuration().unwrap().device(&key).cloned()
            .expect("native admission must use the complete configuration commit");
        let _scope = c.context.directory.borrow().scope.clone().unwrap();
        let old = c.store.devices().load_configuration().unwrap();
        assert!(
            old.devices
                .iter()
                .any(|device| device.key == key && device.native.is_some())
        );
        assert_eq!(old.groups[0].members, ["one"]);
        let member = DeviceKey::new("xiaomi", "one");
        assert!(!old.derive().published.contains(&member), "a member of a published group must be hidden");
        assert!(c.publication.current().published_device(&member).is_none());

        let db = rusqlite::Connection::open(c.store.path()).unwrap();
        db.execute_batch("CREATE TRIGGER fail_configuration BEFORE UPDATE ON bridge_configuration WHEN NEW.json LIKE '%member-new%' BEGIN SELECT RAISE(ABORT, 'injected'); END;").unwrap();
        // A newly confirmed member changes the candidate relation; the serving
        // source, identity and repair inputs must remain on the old full view.
        {
            let mut cloud = c.cloud.lock().unwrap();
            cloud.catalog_groups = vec![group_record("record-1", "group-1", &["one", "member-new"], false)];
        }
        c.context.request_sync();
        until(|| c.context.directory.borrow().confirmed_groups().iter().any(|group| group.membership.members.iter().any(|member| member == "member-new"))).await;
        until(|| c.publication.failure().is_some()).await;
        let terminal = c.terminal();
        let report = terminal.handle_line("status").await.unwrap().unwrap();
        assert!(report.contains("pending application"));
        assert!(report.contains("Configuration apply failed:"));
        assert_eq!(
            c.context.graph.borrow().rules["group-1"]
                .members
                .iter()
                .map(|member| member.device.id.as_str())
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
fn new_group_does_not_wait_for_a_held_member_description() {
    let rig = Rig::new(false, &["one"]);
    let instance = super::super::support::group_instance();
    let member_type = rig
        .data
        .iter()
        .find(|record| record.device().key().id == "one")
        .unwrap()
        .instance_type()
        .to_owned();
    let (release, gate) = flume::bounded::<()>(1);
    {
        let mut cloud = rig.cloud.lock().unwrap();
        cloud.catalog_groups = vec![group_record("group-record", "group-1", &["one"], false)];
        cloud
            .catalog_instances
            .insert(instance["type"].as_str().unwrap().into(), instance);
        cloud.instance_gates.insert(member_type.clone(), gate);
    }
    block_on(rig.run(|c| async move {
        c.announce();
        until(|| {
            c.cloud
                .lock()
                .unwrap()
                .instance_reads
                .get(&member_type)
                .copied()
                .unwrap_or(0)
                > 0
        })
        .await;
        until(|| {
            c.context
                .physical(&DeviceKey::new("xiaomi", "group-1"))
                .is_some()
        })
        .await;
        assert!(
            c.context.directory.borrow().devices["one"]
                .loading
                .is_some()
        );
        release.send(()).unwrap();
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
                .directory
                .borrow()
                .confirmed_groups()
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
        assert!(control.context.directory.borrow().scope.is_some());
        assert_eq!(control.cloud.lock().unwrap().catalogs, 1);
    }));
}

fn group_with_undescribed_member() -> Rig {
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
    rig
}

#[test]
fn missing_description_member_group_is_controllable_with_unknown_state() {
    let rig = group_with_undescribed_member();
    block_on(rig.run(|c| async move {
        c.announce();
        until(|| {
            c.context
                .physical(&DeviceKey::new("xiaomi", "group-1"))
                .is_some()
        })
        .await;
        {
            let live = c.context.directory.borrow();
            let slot = &live.devices["group-1"];
            let record = slot.work.as_ref().unwrap();
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
            assert!(live.devices["member-a"].work.is_none());
            // Native group commands use cloud access; its state needs known members.
            assert!(
                live.devices["group-1"]
                    .work
                    .as_ref()
                    .is_none_or(|work| !work.test_local_attached())
            );
        }
        assert!(matches!(
            c.context.control_path("group-1"),
            Some(super::super::routing::ControlPath::Cloud(_))
        ));
        let first = c.context.directory.borrow().devices["group-1"]
            .work
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
    }));
}

#[test]
fn repeated_and_failed_directories_keep_the_admitted_native_group() {
    let rig = group_with_undescribed_member();
    block_on(rig.run(|c| async move {
        c.announce();
        until(|| {
            c.context
                .physical(&DeviceKey::new("xiaomi", "group-1"))
                .is_some()
        })
        .await;
        let first = c.context.directory.borrow().devices["group-1"]
            .work
            .clone()
            .unwrap();
        for (status, failed) in [(503, true), (200, false)] {
            c.cloud.lock().unwrap().catalog_status = status;
            c.context.request_sync();
            until(|| c.context.diagnostics().catalog_problem.is_some() == failed).await;
            let live = c.context.directory.borrow();
            let slot = &live.devices["group-1"];
            assert!(Rc::ptr_eq(slot.work.as_ref().unwrap(), &first));
            assert!(slot.cloud_access.is_some());
        }
    }));
}
