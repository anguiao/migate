use super::*;
use std::{
    future::Future,
    path::PathBuf,
    sync::atomic::{AtomicBool, Ordering},
    task::{Wake, Waker},
};

// The action ends after the first configuration plan. A revocation arriving
// at that boundary must be included in the commit before Matter starts again.
struct LateRevocation {
    directory: PathBuf,
    recorded: AtomicBool,
}

impl Wake for LateRevocation {
    fn wake(self: Arc<Self>) {
        Store::open(&self.directory)
            .unwrap()
            .devices()
            .record_revocations(&[crate::configuration::Revocation {
                key: DeviceKey::new("xiaomi", "group-1"),
                reason: "access changed during shutdown".into(),
            }])
            .unwrap();
        self.recorded.store(true, Ordering::Relaxed);
    }
}

#[test]
fn configured_runtime_rebuilds_only_for_configuration_and_keeps_committed_state() {
    let rig = Rig::unrestored_with_member_light(&["one", "two"])
        .with_stall_timeout(Duration::from_secs(15));
    let instance: Value = serde_json::from_str(include_str!(
        "../../../../../tests/fixtures/miot/group3-instance.json"
    ))
    .unwrap();
    let urn = instance["type"].as_str().unwrap().to_string();
    {
        let mut cloud = rig.cloud.lock().unwrap();
        cloud.catalog_instances.insert(urn, instance);
        cloud.catalog_groups = vec![group_record("record-1", "group-1", &["one"], false)];
        cloud.online.insert("two".into());
        for (piid, value) in [(1, json!(false)), (2, json!(50)), (3, json!(3000))] {
            cloud.raw_values.insert(("group-1".into(), 2, piid), value);
        }
    }
    block_on(rig.run_result_without_publication(|c| async move {
        c.announce();
        until(|| {
            c.context
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
        let port = free_udp_port();
        let store = c.store.clone();
        let identity = store.load_identity().unwrap();
        let report = crate::xiaomi::auth::AuthReport::for_test(
            crate::xiaomi::auth::AuthenticationState::NotSignedIn,
            None,
            crate::xiaomi::auth::CertificateUpdate::NotNeeded,
            0,
        );
        let status = crate::terminal::AuthStatus::new(report, "/bin/migate".into(), "/data".into());
        let handle = crate::xiaomi::adapter::SourceHandle(c.context.clone());
        let options = crate::app::BridgeOptions {
            store: &store,
            identity: &identity,
            status: &status,
            port,
        };
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let mut terminal_input =
            Async::new(std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap())
                .unwrap();
        let input =
            futures_lite::io::BufReader::new(Async::new(listener.accept().unwrap().0).unwrap());
        let (reply, replies) = flume::unbounded();
        let output = TerminalOutput {
            bytes: Vec::new(),
            replies: reply,
        };
        let runner_starts = Cell::new(0);
        let late = Arc::new(LateRevocation {
            directory: store.path().parent().unwrap().to_owned(),
            recorded: AtomicBool::new(false),
        });
        let runtime = crate::app::run_xiaomi_bridge(
            options,
            async move { Ok(handle) },
            future::pending::<Result<(), crate::RuntimeError>>(),
            input,
            output,
            future::pending::<Result<(), crate::RuntimeError>>(),
            |event| {
                if matches!(event, crate::matter::PairingEvent::Opened { .. }) {
                    if late.recorded.load(Ordering::Relaxed) {
                        assert!(
                            store.devices().load_revocations().unwrap().is_empty(),
                            "the new runner started before the late revocation was reconciled"
                        );
                    }
                    runner_starts.set(runner_starts.get() + 1);
                }
                Ok(())
            },
        );
        let assertions = async {
            let group = DeviceKey::new("xiaomi", "group-1");
            let two = DeviceKey::new("xiaomi", "two");
            until(|| {
                store
                    .devices()
                    .load_configuration()
                    .unwrap()
                    .device(&group)
                    .cloned()
                    .is_some()
            })
            .await;
            until(|| {
                store
                    .devices()
                    .load_configuration()
                    .unwrap()
                    .device(&two)
                    .cloned()
                    .is_some()
            })
            .await;
            until(|| runtime_socket_live(port)).await;
            let version = || store.devices().load_configuration().unwrap().root_version;
            let root_before = version();
            let group_before = store
                .devices()
                .load_configuration()
                .unwrap()
                .device(&group)
                .cloned()
                .unwrap();
            let two_before = c.context.registry.get(&two).unwrap();
            let device_ids = |report: &str| {
                report
                    .lines()
                    .filter(|line| !line.starts_with(' ') && line.contains("): "))
                    .filter_map(|line| line.split_once(" ("))
                    .map(|(did, _)| did.to_string())
                    .collect::<BTreeSet<_>>()
            };
            let report = terminal_command(&mut terminal_input, &replies, "status").await;
            assert!(
                report.contains(&format!("Configuration: root version {root_before}.")),
                "{report}"
            );
            assert!(
                report.contains("Native group (virtual): members one."),
                "{report}"
            );
            let list = terminal_command(&mut terminal_input, &replies, "list").await;
            assert_eq!(
                device_ids(&list),
                BTreeSet::from(["group-1".into(), "one".into(), "two".into()])
            );
            assert!(list.contains("one (name one):"), "{list}");
            assert!(list.contains("Matter: hidden or reserved;"), "{list}");

            let initial_starts = runner_starts.get();

            // Phase 1: a pure relation change rebuilds but must not bump any
            // configuration version. The unresolved member has no source device.
            c.cloud.lock().unwrap().members.push("member-a".into());
            c.cloud.lock().unwrap().catalog_groups = vec![group_record(
                "record-1",
                "group-1",
                &["one", "member-a"],
                false,
            )];
            c.context.request_sync();
            until(|| {
                store
                    .devices()
                    .load_configuration()
                    .unwrap()
                    .groups
                    .first()
                    .is_some_and(|relation| relation.members.iter().any(|m| m == "member-a"))
            })
            .await;
            until(|| runtime_socket_live(port)).await;
            until(|| runner_starts.get() > initial_starts).await;
            assert_eq!(version(), root_before);
            let report = terminal_command(&mut terminal_input, &replies, "status").await;
            assert!(
                report.contains(&format!("Configuration: root version {root_before}.")),
                "{report}"
            );
            assert!(
                report.contains("Native group (virtual): members member-a, one."),
                "{report}"
            );
            assert_eq!(
                device_ids(&report),
                BTreeSet::from(["group-1".into(), "one".into(), "two".into()])
            );
            let group_after_relation = store
                .devices()
                .load_configuration()
                .unwrap()
                .device(&group)
                .cloned()
                .unwrap();
            assert_eq!(
                group_after_relation.configuration_version,
                group_before.configuration_version
            );
            // The unchanged outlet keeps serving through the rebuild.
            let two_after_relation = c.context.registry.get(&two).unwrap();
            assert!(two_before.same_instance(&two_after_relation));
            assert_eq!(two_before.label(), two_after_relation.label());

            // A real active group light action is cancelled when the runner is
            // dropped for the next rebuild.
            let (id, _) = {
                let live = c.context.directory.borrow();
                let record = live.devices["group-1"].work.as_ref().unwrap();
                let cached = record.native.clone();
                let light = cached
                    .device()
                    .functions()
                    .iter()
                    .find(|function| function.lighting().is_some())
                    .unwrap();
                (light.description().id.clone(), light.power().unwrap())
            };
            let light = function(&c.context.registry, "group-1", &id);
            let action = light
                .begin_light_action(
                    crate::device::LightTarget::brightness(Some(true), 70.0)
                        .expect("valid test target"),
                    false,
                )
                .unwrap();
            action.accepted().await.unwrap();
            let token = action.token();
            assert!(light.light_action_current(token));
            let mut changes = light.subscribe_light_actions().unwrap();
            let mut stopped = std::pin::pin!(changes.changed());
            let waker = Waker::from(late.clone());
            assert!(
                stopped
                    .as_mut()
                    .poll(&mut TaskContext::from_waker(&waker))
                    .is_pending()
            );

            // Phase 2: the relation returns to its confirmed members while the
            // confirmed native range changes the published fixed shape.
            {
                let mut live = c.context.directory.borrow_mut();
                let slot = live.devices.get_mut("group-1").unwrap();
                let candidate = slot.latest_description.as_mut().unwrap();
                let native = candidate.native.as_ref().unwrap();
                let grid = crate::device::NumericRange::new(2700, 6500, 1).unwrap();
                candidate.native = Some(
                    crate::xiaomi::capability::NativeDescription::new(
                        native.model().into(),
                        native.instance_type().into(),
                        super::super::support::device_with_temperature_grid(native.device(), grid),
                    )
                    .unwrap(),
                );
                slot.confirmed = true;
            }
            c.cloud.lock().unwrap().catalog_groups =
                vec![group_record("record-1", "group-1", &["one"], false)];
            c.context.request_sync();
            until(|| {
                let saved = store.devices().load_configuration().unwrap();
                saved.root_version > root_before
                    && saved
                        .groups
                        .iter()
                        .any(|relation| relation.members == ["one"])
            })
            .await;
            assert!(
                !light.light_action_current(token),
                "the rebuild cancelled the active light action"
            );
            until(|| runtime_socket_live(port)).await;
            assert!(late.recorded.load(Ordering::Relaxed));
            assert_eq!(version(), root_before + 1);
            let group_after_bounds = store
                .devices()
                .load_configuration()
                .unwrap()
                .device(&group)
                .cloned()
                .unwrap();
            assert_eq!(
                group_after_bounds.configuration_version,
                group_before.configuration_version + 1
            );
            assert_eq!(group_after_bounds.unique_id, group_before.unique_id);
            assert_eq!(group_after_bounds.endpoint, group_before.endpoint);
            let root_settled = version();
            let report = terminal_command(&mut terminal_input, &replies, "status group-1").await;
            assert!(
                report.contains(&format!("Configuration: root version {root_settled}.")),
                "{report}"
            );
            assert!(
                report.contains(&format!(
                    "configuration version {}.",
                    group_after_bounds.configuration_version
                )),
                "{report}"
            );
            assert!(
                report.contains("Native group (virtual): members one."),
                "{report}"
            );
            assert!(
                report.contains("Effective temperature range: 2700–6500 K."),
                "{report}"
            );
            assert!(!report.contains("Configuration apply failed:"), "{report}");
            let committed_list = terminal_command(&mut terminal_input, &replies, "list").await;

            // The unchanged outlet still accepts real control after the
            // rebuild: an On/Off round trip reaches the source as property
            // writes for did "two".
            c.cloud.lock().unwrap().property_writes.clear();
            assert_eq!(
                terminal_command(&mut terminal_input, &replies, "on two outlet:2").await,
                "two: command accepted\n"
            );
            until(|| {
                c.cloud
                    .lock()
                    .unwrap()
                    .property_writes
                    .iter()
                    .any(|(did, _, _, value, _)| did == "two" && *value)
            })
            .await;
            assert_eq!(
                terminal_command(&mut terminal_input, &replies, "off two outlet:2").await,
                "two: command accepted\n"
            );
            until(|| {
                c.cloud
                    .lock()
                    .unwrap()
                    .property_writes
                    .iter()
                    .any(|(did, _, _, value, _)| did == "two" && !*value)
            })
            .await;

            // Ordinary state changes do not rebuild or touch the socket.
            let starts = runner_starts.get();
            light
                .observe_light(crate::device::LightPatch {
                    power: Some(true),
                    ..Default::default()
                })
                .unwrap();
            c.context.changed.notify(usize::MAX);
            async_io::Timer::after(Duration::from_millis(20)).await;
            assert_eq!(runner_starts.get(), starts);
            assert_eq!(version(), root_settled);
            assert!(runtime_socket_live(port));

            // A failed save keeps the whole committed configuration and only
            // records the retryable failure.
            let db = rusqlite::Connection::open(store.path()).unwrap();
            db.execute_batch(
                "CREATE TRIGGER fail_identity BEFORE UPDATE ON bridge_configuration
                 BEGIN SELECT RAISE(ABORT, 'injected'); END;",
            )
            .unwrap();
            let before_failure = runner_starts.get();
            c.cloud.lock().unwrap().catalog_groups = vec![group_record(
                "record-1",
                "group-1",
                &["one", "member-a"],
                false,
            )];
            c.context.request_sync();
            until(|| runner_starts.get() > before_failure).await;
            until(|| runtime_socket_live(port)).await;
            assert_eq!(version(), root_settled);
            assert_eq!(
                c.context.graph.borrow().rules["group-1"]
                    .members
                    .iter()
                    .map(|member| member.device.id.as_str())
                    .collect::<Vec<_>>(),
                ["one"]
            );
            let group_after_failure = store
                .devices()
                .load_configuration()
                .unwrap()
                .device(&group)
                .cloned()
                .unwrap();
            assert_eq!(
                group_after_failure.configuration_version,
                group_after_bounds.configuration_version
            );
            assert_eq!(group_after_failure.unique_id, group_before.unique_id);
            assert_eq!(group_after_failure.endpoint, group_before.endpoint);
            let report = terminal_command(&mut terminal_input, &replies, "status group-1").await;
            assert!(
                report.contains(&format!(
                    "Configuration: root version {root_settled}; pending application."
                )),
                "{report}"
            );
            assert!(report.contains("Configuration apply failed:"), "{report}");
            assert!(
                report.contains(&format!(
                    "configuration version {}.",
                    group_after_bounds.configuration_version
                )),
                "{report}"
            );
            assert!(
                report.contains("Native group (virtual): members one."),
                "{report}"
            );
            assert!(!report.contains("members member-a, one."), "{report}");
            assert!(
                report.contains("Effective temperature range: 2700–6500 K."),
                "{report}"
            );
            let list = terminal_command(&mut terminal_input, &replies, "list").await;
            assert_eq!(device_ids(&list), device_ids(&committed_list));
            assert!(
                list.contains("Native group (virtual): members one."),
                "{list}"
            );
            Ok::<(), crate::RuntimeError>(())
        };
        future::or(runtime, assertions).await.unwrap();
    }))
    .unwrap();
}

#[test]
fn failed_source_wins_before_configured_startup_commits() {
    let rig = Rig::new(false, &["one"]);
    block_on(rig.run_result_without_publication(|c| async move {
        c.announce();
        until(|| {
            let live = c.context.directory.borrow();
            live.scope.is_some()
                && live.devices.get("one").is_some_and(|slot| {
                    slot.latest_description
                        .as_ref()
                        .is_some_and(|source| source.native.is_some())
                })
        })
        .await;
        let port = free_udp_port();
        let store = c.store.clone();
        let identity = store.load_identity().unwrap();
        let report = crate::xiaomi::auth::AuthReport::for_test(
            crate::xiaomi::auth::AuthenticationState::NotSignedIn,
            None,
            crate::xiaomi::auth::CertificateUpdate::NotNeeded,
            0,
        );
        let status = crate::terminal::AuthStatus::new(report, "/bin/migate".into(), "/data".into());
        let handle = crate::xiaomi::adapter::SourceHandle(c.context.clone());
        let before = store.devices().load_configuration().unwrap();
        let before_revocations = store.devices().load_revocations().unwrap();
        let paired = Rc::new(Cell::new(false));
        let paired_flag = paired.clone();
        let error = crate::app::run_xiaomi_bridge(
            crate::app::BridgeOptions {
                store: &store,
                identity: &identity,
                status: &status,
                port,
            },
            async move { Ok(handle) },
            async { Err("source failed before startup".into()) },
            &b""[..],
            Vec::new(),
            future::pending::<Result<(), crate::RuntimeError>>(),
            move |_| {
                paired_flag.set(true);
                Ok(())
            },
        )
        .await
        .unwrap_err();
        assert_eq!(error.to_string(), "source failed before startup");
        assert!(!paired.get(), "a failed source must not start pairing");
        let after = store.devices().load_configuration().unwrap();
        assert_eq!(after.root_version, before.root_version);
        assert_eq!(after.devices, before.devices);
        assert_eq!(
            store.devices().load_revocations().unwrap(),
            before_revocations
        );
        assert!(
            store
                .devices()
                .load_configuration()
                .unwrap()
                .device(&DeviceKey::new("xiaomi", "one"))
                .cloned()
                .is_none(),
            "a failed source must not publish any device"
        );
    }))
    .unwrap();
}
