use super::*;

#[test]
fn failed_revocation_persistence_stops_the_source_after_closing_control() {
    let rig = Rig::new(false, &["one"]);
    let instance = group_instance();
    let urn = instance["type"].as_str().unwrap().to_string();
    {
        let mut cloud = rig.cloud.lock().unwrap();
        // Keep the group DID in the selected home so withdrawal reaches the
        // group-sync retirement path instead of the ordinary member removal.
        cloud.members.push("group-1".into());
        cloud.catalog_groups = vec![group_record("record-1", "group-1", &["one"], false)];
        cloud.catalog_instances.insert(urn, instance);
    }
    let store = rig.store.clone();
    let context = rig.source.context.clone();
    let result = block_on(rig.run_result(|c| async move {
        c.announce();
        let group = DeviceKey::new("xiaomi", "group-1");
        until(|| c.publication.current().published.get(&group).is_some()).await;
        let physical = c.publication.current().published.get(&group).unwrap();
        let db = rusqlite::Connection::open(c.store.path()).unwrap();
        db.execute_batch(
            "CREATE TRIGGER fail_revocation BEFORE INSERT ON publication_revocation
             BEGIN SELECT RAISE(ABORT, 'injected revocation failure'); END;",
        )
        .unwrap();
        c.cloud.lock().unwrap().catalog_groups[0]["extra"]["member_list"]["member_ship"] =
            json!({});
        c.context.request_sync();
        until(|| {
            matches!(
                physical.availability(),
                crate::device::Availability::Unavailable(_)
            )
        })
        .await;
        async_io::Timer::after(Duration::from_millis(100)).await;
    }));
    let error = result.expect_err("a revocation persistence failure must stop the source");
    assert!(error.to_string().contains("record revocation"), "{error}");
    assert!(
        context
            .registry
            .get(&DeviceKey::new("xiaomi", "group-1"))
            .is_none()
            || context
                .live
                .borrow()
                .devices
                .get("group-1")
                .and_then(|slot| slot.admitted.as_ref())
                .is_none_or(|work| work.life.is_closed()),
        "native authority must remain closed after the failed revocation"
    );
    assert!(
        store.devices().load_revocations().unwrap().is_empty(),
        "a failed revocation write must not be reported as saved"
    );
}

#[test]
fn revoked_native_group_stays_reserved_and_startup_recovery_skips_it() {
    let rig = Rig::new(false, &["one"]);
    let instance: Value = serde_json::from_str(include_str!(
        "../../../../../tests/fixtures/miot/group3-instance.json"
    ))
    .unwrap();
    let urn = instance["type"].as_str().unwrap().to_string();
    {
        let mut cloud = rig.cloud.lock().unwrap();
        cloud.catalog_instances.insert(urn, instance);
        cloud.catalog_groups = vec![group_record("record-1", "group-1", &["one"], false)];
    }
    block_on(rig.run(|c| async move {
        c.announce();
        let store = c.store.clone();
        let group = DeviceKey::new("xiaomi", "group-1");
        let one = DeviceKey::new("xiaomi", "one");
        until(|| {
            c.context.relations.borrow().snapshot().iter().any(|group| {
                group.active
                    && group
                        .membership
                        .members
                        .iter()
                        .any(|member| member == "one")
            })
        })
        .await;
        until(|| {
            c.store
                .devices()
                .load_configuration()
                .unwrap()
                .device(&group)
                .cloned()
                .is_some()
        })
        .await;
        until(|| c.publication.current().published.get(&group).is_some()).await;
        let group_before = store
            .devices()
            .load_configuration()
            .unwrap()
            .device(&group)
            .cloned()
            .unwrap();
        let group_physical = c.publication.current().published.get(&group).unwrap();

        // Recovery requires the hidden member's own description to be saved;
        // its admission may lag the group's native publication.
        until(|| {
            store
                .devices()
                .load_configuration()
                .unwrap()
                .devices
                .iter()
                .any(|record| record.key == one)
        })
        .await;

        // The real complete-catalog deletion closes the retained authority and
        // writes its tombstone while the identity update keeps failing.
        let db = rusqlite::Connection::open(store.path()).unwrap();
        db.execute_batch(
            "CREATE TRIGGER fail_identity BEFORE UPDATE ON bridge_configuration
             BEGIN SELECT RAISE(ABORT, 'injected'); END;",
        )
        .unwrap();
        c.cloud.lock().unwrap().catalog_groups = vec![];
        c.context.request_sync();
        until(|| {
            store
                .devices()
                .load_revocations()
                .unwrap()
                .iter()
                .any(|revocation| revocation.key == group)
        })
        .await;
        assert!(matches!(
            group_physical.availability(),
            crate::device::Availability::Unavailable(_)
        ));
        assert!(
            group_physical.functions()[0]
                .execute(crate::device::Command::On)
                .await
                .is_err()
        );
        let revocations = store.devices().load_revocations().unwrap();
        assert!(revocations.iter().any(|revocation| {
            revocation.key == group && revocation.reason == "native group withdrawn"
        }));
        assert!(
            store
                .devices()
                .load_configuration()
                .unwrap()
                .devices
                .iter()
                .any(|record| record.key == group && record.native.is_some())
        );

        // A failed startup save serves the committed view with revocation applied.
        let identity = store.load_identity().unwrap();
        let report = crate::xiaomi::auth::AuthReport::for_test(
            crate::xiaomi::auth::AuthenticationState::NotSignedIn,
            None,
            crate::xiaomi::auth::CertificateUpdate::NotNeeded,
            0,
        );
        let status = crate::terminal::AuthStatus::new(report, "/bin/migate".into(), "/data".into());
        let startup_port = free_udp_port();
        let failing = XiaomiAdapter::build(
            store.clone(),
            c.context.cloud.clone(),
            c.discovery.clone(),
            c.context.clock.clone(),
        )
        .unwrap()
        .0;
        let failing_handle = crate::xiaomi::adapter::SourceHandle(failing.context.clone());
        let recovered_view = crate::app::App::open(failing_handle.clone(), store.clone()).unwrap();
        assert!(recovered_view.current().published.get(&group).is_none());
        assert!(recovered_view.current().published.get(&one).is_some());
        assert!(!failing.context.graph.borrow().rules.contains_key("group-1"));
        let started = Rc::new(event_listener::Event::new());
        let listening = started.listen();
        let ready_signal = started.clone();
        crate::app::run_xiaomi_bridge(
            crate::app::BridgeOptions {
                store: &store,
                identity: &identity,
                status: &status,
                port: startup_port,
            },
            async move { Ok(failing_handle) },
            future::pending::<Result<(), crate::RuntimeError>>(),
            &b""[..],
            Vec::new(),
            async move {
                listening.await;
                Ok(())
            },
            move |_| {
                ready_signal.notify(usize::MAX);
                Ok(())
            },
        )
        .await
        .unwrap();
        assert!(!runtime_socket_live(startup_port));

        // Once the save can succeed, a fresh restart applies the bootstrap
        // revocation overlay without reviving the group; the hidden member
        // returns with unknown state and the identity stays reserved. The
        // whole recovery stays synchronous so the running applier cannot
        // interleave.
        let db = rusqlite::Connection::open(store.path()).unwrap();
        db.execute_batch("DROP TRIGGER fail_identity;").unwrap();
        let restored = XiaomiAdapter::build(
            store.clone(),
            c.context.cloud.clone(),
            c.discovery.clone(),
            c.context.clock.clone(),
        )
        .unwrap()
        .0;
        {
            let live = restored.context.live.borrow();
            assert!(
                live.devices
                    .get("group-1")
                    .is_none_or(|slot| slot.admitted.is_none()),
                "bootstrap overlay must not admit a revoked group"
            );
            assert!(
                live.devices
                    .get("one")
                    .is_some_and(|slot| slot.admitted.is_some()),
                "hidden member is restored"
            );
        }
        assert!(
            !restored.context.is_group("group-1"),
            "a revoked group has no executable rule after recovery"
        );
        assert!(
            !restored
                .context
                .graph
                .borrow()
                .rules
                .contains_key("group-1"),
            "the recovery graph applies the persisted revocation"
        );
        let member = restored.context.registry.get(&one).unwrap();
        assert_eq!(
            member.functions()[0].snapshot().power(),
            Some(Power::Unknown)
        );
        let reopened = crate::app::App::open(
            crate::xiaomi::adapter::SourceHandle(restored.context.clone()),
            store.clone(),
        )
        .unwrap();
        if let Some(plan) = reopened.plan().unwrap() {
            let prepared = reopened.prepare(plan);
            reopened.apply(prepared).unwrap();
        }
        assert!(reopened.current().published.get(&group).is_none());
        let recovered = store.devices().load_configuration().unwrap();
        let reserved = recovered
            .devices
            .iter()
            .find(|record| record.key == group)
            .unwrap();
        assert!(reserved.native.is_none());
        assert_eq!(reserved.unique_id, group_before.unique_id);
        assert_eq!(reserved.endpoint, group_before.endpoint);
        assert_eq!(
            reserved.configuration_version,
            group_before.configuration_version + 1
        );
    }));
}

#[test]
fn restart_restores_group_and_hidden_member_identity_and_effective_status() {
    let rig = Rig::unrestored_with_member_light(&["one"]);
    let instance: Value = serde_json::from_str(include_str!(
        "../../../../../tests/fixtures/miot/group3-instance.json"
    ))
    .unwrap();
    let urn = instance["type"].as_str().unwrap().to_string();
    {
        let mut cloud = rig.cloud.lock().unwrap();
        cloud.catalog_instances.insert(urn, instance);
        cloud.catalog_groups = vec![group_record("record-1", "group-1", &["one"], false)];
    }
    block_on(rig.run(|c| async move {
        c.announce();
        let store = c.store.clone();
        let group = DeviceKey::new("xiaomi", "group-1");
        let one = DeviceKey::new("xiaomi", "one");
        until(|| {
            c.store
                .devices()
                .load_configuration()
                .unwrap()
                .device(&group)
                .cloned()
                .is_some()
        })
        .await;
        until(|| c.publication.current().published.get(&group).is_some()).await;
        let group_before = store
            .devices()
            .load_configuration()
            .unwrap()
            .device(&group)
            .cloned()
            .unwrap();
        // The hidden member keeps a reserved identity as soon as its own
        // description is admitted (it may lag the group confirmation).
        until(|| {
            store
                .devices()
                .load_configuration()
                .unwrap()
                .devices
                .iter()
                .any(|record| record.key == one)
        })
        .await;
        let member_before = store
            .devices()
            .load_configuration()
            .unwrap()
            .devices
            .into_iter()
            .find(|record| record.key == one)
            .expect("hidden member keeps its reserved identity");
        assert!(member_before.native.is_some());
        let root_before = store.devices().load_configuration().unwrap().root_version;
        assert!(c.publication.current().published.get(&one).is_none());
        assert!(
            c.publication
                .current()
                .derived
                .groups
                .values()
                .any(|relation| {
                    relation.group_did == "group-1"
                        && relation.members.iter().any(|member| member == "one")
                })
        );

        // Terminal renders the effective hidden member and its membership.
        let status = crate::terminal::AuthStatus::new(
            crate::xiaomi::auth::AuthReport::for_test(
                crate::xiaomi::auth::AuthenticationState::NotSignedIn,
                None,
                crate::xiaomi::auth::CertificateUpdate::NotNeeded,
                0,
            ),
            "/bin/migate".into(),
            "/data".into(),
        );
        let terminal = crate::terminal::bridge::Context::committed(c.publication.clone(), &status);
        let list = terminal.handle_line("list", 0).await.unwrap().unwrap();
        let help = terminal.handle_line("help", 0).await.unwrap().unwrap();
        assert!(help.contains(&format!(
            "Configuration: root version {}.",
            c.publication.current().configuration.root_version
        )));
        // The effective registry keeps every real source ref; a hidden member
        // is a normal row marked hidden with its confirmed memberships.
        assert!(list.contains("one (name one):"), "{list}");
        assert!(list.contains("Matter: hidden or reserved;"), "{list}");
        assert!(
            list.contains("Group group-1 (virtual): published."),
            "{list}"
        );
        let detail = terminal
            .handle_line("status one", 0)
            .await
            .unwrap()
            .unwrap();
        assert!(
            detail.contains("Group group-1 (virtual): published."),
            "{detail}"
        );

        // Withdraw the confirmed relation (record present, empty membership);
        // the group is reserved and the member becomes an ordinary device.
        c.cloud.lock().unwrap().catalog_groups =
            vec![group_record("record-1", "group-1", &[], false)];
        c.context.request_sync();
        until(|| {
            store
                .devices()
                .load_configuration()
                .unwrap()
                .device(&group)
                .is_some_and(|record| record.native.is_none())
        })
        .await;
        let root_after = store.devices().load_configuration().unwrap().root_version;
        assert!(root_after > root_before);
        let saved = store.devices().load_configuration().unwrap();
        let reserved = saved
            .devices
            .iter()
            .find(|record| record.key == group)
            .unwrap();
        assert!(reserved.native.is_none());
        assert_eq!(reserved.unique_id, group_before.unique_id);
        assert_eq!(reserved.endpoint, group_before.endpoint);
        assert_eq!(
            reserved.configuration_version,
            group_before.configuration_version + 1
        );
        until(|| c.publication.current().published.get(&one).is_some()).await;
        let list = terminal.handle_line("list", 0).await.unwrap().unwrap();
        assert!(
            list.contains("group-1 (native group): not published (confirmed empty relation)."),
            "{list}"
        );

        // A fresh restart restores the withdrawn relation, the published member
        // with unknown state and the reserved group identity.
        let restored = XiaomiAdapter::build(
            store.clone(),
            c.context.cloud.clone(),
            c.discovery.clone(),
            c.context.clock.clone(),
        )
        .unwrap()
        .0;
        {
            let live = restored.context.live.borrow();
            assert!(
                live.devices
                    .get("group-1")
                    .is_none_or(|slot| slot.admitted.is_none()),
                "a withdrawn group is not admitted"
            );
            assert!(
                live.devices
                    .get("one")
                    .is_some_and(|slot| slot.admitted.is_some()),
                "the former hidden member is restored"
            );
        }
        let member = restored.context.registry.get(&one).unwrap();
        assert_eq!(
            member.functions()[0].snapshot().power(),
            Some(Power::Unknown)
        );
        let opened = crate::app::App::open(
            crate::xiaomi::adapter::SourceHandle(restored.context.clone()),
            store.clone(),
        )
        .unwrap();
        // Install the bootstrap relation overlay through the normal boundary.
        if let Some(plan) = opened.plan().unwrap() {
            let prepared = opened.prepare(plan);
            opened.apply(prepared).unwrap();
        }
        assert_eq!(opened.current().configuration.root_version, root_after);
        assert!(opened.current().published.get(&group).is_none());
        assert!(opened.current().published.get(&one).is_some());
        let recovered = store.devices().load_configuration().unwrap();
        let reserved = recovered
            .devices
            .iter()
            .find(|record| record.key == group)
            .unwrap();
        assert!(reserved.native.is_none());
        assert_eq!(reserved.unique_id, group_before.unique_id);
        assert_eq!(reserved.endpoint, group_before.endpoint);
        assert_eq!(
            reserved.configuration_version,
            group_before.configuration_version + 1
        );
        let member_record = recovered
            .devices
            .iter()
            .find(|record| record.key == one)
            .unwrap();
        assert_eq!(member_record.unique_id, member_before.unique_id);
        assert_eq!(member_record.endpoint, member_before.endpoint);
        let status = crate::terminal::AuthStatus::new(
            crate::xiaomi::auth::AuthReport::for_test(
                crate::xiaomi::auth::AuthenticationState::NotSignedIn,
                None,
                crate::xiaomi::auth::CertificateUpdate::NotNeeded,
                0,
            ),
            "/bin/migate".into(),
            "/data".into(),
        );
        let terminal = crate::terminal::bridge::Context::committed(Rc::new(opened), &status);
        let list = terminal.handle_line("list", 0).await.unwrap().unwrap();
        assert!(
            list.contains("group-1 (native group): not published (native group withdrawn)."),
            "{list}"
        );
    }));
}
