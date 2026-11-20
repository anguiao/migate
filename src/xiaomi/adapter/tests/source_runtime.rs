use super::*;
use crate::device::TemperatureInterval;
use crate::xiaomi::capability::PropertyBinding;

#[test]
fn readmission_commits_its_own_description_after_the_held_batch_is_ready() {
    let rig = Rig::new(true, &["one", "two"]);
    let context = &rig.source.context;
    let app = Rc::new(crate::app::App::open(
        rig.source.source_handle(),
        rig.store.clone(),
    ));
    let key = DeviceKey::new("xiaomi", "one");
    let next_description = |did: &str| {
        let mut raw = context.directory.borrow().devices[did]
            .latest_description
            .as_ref()
            .unwrap()
            .raw
            .clone();
        raw.instance_type = "urn:miot-spec-v2:device:outlet:0000A002:example:3".into();
        raw.instance["type"] = json!(raw.instance_type);
        raw.recognize(false).unwrap()
    };
    let one = next_description("one");
    let two = next_description("two");
    let infos = [&one, &two]
        .map(|description| {
            let raw = &description.raw;
            (
                raw.key.id.clone(),
                crate::xiaomi::mijia::CloudDeviceInfo {
                    did: raw.key.id.clone(),
                    model: raw.model.clone(),
                    instance_type: raw.instance_type.clone(),
                    online: true,
                    lan: crate::xiaomi::mijia::LanCredentials::NotIp,
                },
            )
        })
        .into_iter()
        .collect();
    let db = rusqlite::Connection::open(rig.store.path()).unwrap();
    db.execute_batch(
        "CREATE TRIGGER block_withdrawal BEFORE UPDATE ON bridge_configuration
        BEGIN SELECT RAISE(ABORT, 'injected'); END;",
    )
    .unwrap();
    context.remove("one").unwrap();
    let withdrawal = app.plan().unwrap().unwrap();
    assert!(app.apply(app.prepare(withdrawal)).is_err());
    context.directory.borrow_mut().confirm_members(
        &["one".to_owned(), "two".to_owned()].into_iter().collect(),
        infos,
        None,
    );
    context.pending_description("one", "new admission awaiting description".into());
    let status = crate::terminal::AuthStatus::new(
        context.diagnostics().auth,
        "/bin/migate".into(),
        "/data".into(),
    );
    let terminal = crate::terminal::bridge::Context::committed(app.clone(), &status);
    let detail = block_on(terminal.handle_line("status one"))
        .unwrap()
        .unwrap();
    assert!(
        detail.lines().count() == 1 && detail.contains("new admission awaiting description"),
        "{detail}"
    );
    let list = block_on(terminal.handle_line("list")).unwrap().unwrap();
    assert_eq!(
        list.lines()
            .filter(|line| line.starts_with("one ("))
            .collect::<Vec<_>>(),
        [detail.as_str()]
    );

    // The previous configuration is still on disk through both stages of the new admission.
    for description in [None, Some(one.clone())] {
        if let Some(description) = description {
            context
                .directory
                .borrow_mut()
                .accept_description("one", description);
        }
        let plan = app.plan().unwrap().unwrap();
        assert!(plan.clear_revocations.is_empty());
        assert!(plan.configuration.device(&key).unwrap().native.is_none());
        assert!(app.apply(app.prepare(plan)).is_err());
        assert!(context.directory.borrow().devices["one"].work.is_none());
    }

    db.execute_batch("DROP TRIGGER block_withdrawal").unwrap();
    context
        .directory
        .borrow_mut()
        .accept_description("two", two);
    let plan = app.plan().unwrap().unwrap();
    assert_eq!(plan.clear_revocations, [key]);
    app.apply(app.prepare(plan)).unwrap();
    assert_eq!(
        context.directory.borrow().devices["one"]
            .work
            .as_ref()
            .unwrap()
            .native
            .instance_type(),
        one.raw.instance_type
    );
    assert!(rig.store.devices().load_revocations().unwrap().is_empty());
    for command in ["list", "status one"] {
        let text = block_on(terminal.handle_line(command)).unwrap().unwrap();
        assert!(
            text.lines()
                .any(|line| line.starts_with("one (") && !line.contains("pending (")),
            "{text}"
        );
        assert!(!text.contains("source retired"), "{text}");
    }
}

/// A group whose native control description is unchanged must keep its
/// installed runtime when only the served logical description (member-derived
/// capability interval) changes. Rebuilding native subscriptions here is the
/// whole-replace behavior this slice removes.
#[test]
fn logical_only_group_change_reuses_the_native_runtime() {
    let rig = repair_rig(0, true);
    block_on(rig.run(|c| async move {
        repair_ready(&c).await;
        let group = function(&c.context, "group-1", "light:2");
        assert_eq!(
            group.description().temperature_interval(),
            Some(TemperatureInterval::new(2700, 6500).unwrap())
        );
        let runtime_before = c.context.directory.borrow().devices["group-1"]
            .work
            .clone()
            .expect("the admitted group has an installed native runtime");

        // A valid member range update changes the group's derived logical
        // interval while the group's own native control description stays
        // byte-for-byte identical.
        let mut narrowed = ls2307_instance();
        narrowed["services"][1]["properties"][2]["value-range"] = json!([3000, 6400, 1]);
        let confirmed = confirmed_for(&c, "member-a", "devcea.light.ls2307", &narrowed);
        {
            let mut live = c.context.directory.borrow_mut();
            let slot = live.devices.get_mut("member-a").unwrap();
            slot.latest_description = Some(confirmed.into());
        }
        c.context.reconcile("member-a").unwrap();
        until(|| {
            function(&c.context, "group-1", "light:2")
                .description()
                .temperature_interval()
                == Some(TemperatureInterval::new(3000, 6400).unwrap())
        })
        .await;

        let runtime_after = c.context.directory.borrow().devices["group-1"]
            .work
            .clone()
            .expect("the group still has an installed native runtime");
        assert!(
            Rc::ptr_eq(&runtime_before, &runtime_after),
            "a logical-only group change must reuse the installed native runtime, not rebuild it"
        );
        assert!(
            !runtime_before.life.is_closed(),
            "the reused native runtime authority must stay open"
        );
    }));
}

/// The same logical-only change over a real physical-group wire: the native
/// runtime and its subscriptions stay, the new sink receives native
/// notifications, and the retired logical instance cannot accept commands.
#[test]
fn logical_only_group_change_keeps_subscriptions_and_projects_to_the_new_sink() {
    let mut rig = repair_rig(0, true);
    let instance = group_instance();
    let urn = instance["type"].as_str().unwrap().to_string();
    {
        let mut cloud = rig.cloud.lock().unwrap();
        cloud.catalog_groups[0]["extra"]["member_list"]["is_physical"] = json!(true);
    }
    let record = crate::xiaomi::capability::NativeDescription::new(
        "mijia.light.group3".into(),
        urn,
        crate::xiaomi::capability::parse_group(
            DeviceKey::new("xiaomi", "group-1"),
            "mijia.light.group3",
            &instance,
        )
        .unwrap(),
    )
    .unwrap();
    rig.data.push(record);
    block_on(rig.run(|c| async move {
        c.announce();
        let peer = Rc::new(Peer::default());
        for did in ["group-1", "member-a", "member-b"] {
            for (siid, piid, value) in [(2, 1, json!(true)), (2, 2, json!(50)), (2, 3, json!(2700))]
            {
                peer.value(did, PropertyBinding { siid, piid }, value);
            }
        }
        for did in ["member-a", "member-b"] {
            peer.value(did, MEMBER_COLOR, json!(0x112233));
            peer.value(did, MEMBER_MODE, json!(0));
        }
        let control = &c;
        c.peer(peer, |peer| async move {
            until(|| {
                control
                    .context
                    .directory
                    .borrow()
                    .devices
                    .get("group-1")
                    .is_some_and(|slot| slot.work.is_some())
            })
            .await;
            until(|| {
                matches!(
                    control.context.control_path("group-1"),
                    Some(super::super::routing::ControlPath::Local(_))
                )
            })
            .await;
            let runtime = control.context.directory.borrow().devices["group-1"]
                .work
                .clone()
                .expect("the physical group has a native runtime");
            until(|| {
                runtime
                    .bindings()
                    .iter()
                    .all(|binding| runtime.test_local_subscribed_contains(*binding))
            })
            .await;
            assert_eq!(
                control.context.active_notification("group-1"),
                Some(super::super::model::PathKind::Local)
            );

            let old = function(&control.context, "group-1", "light:2");
            assert_eq!(
                old.description().temperature_interval(),
                Some(TemperatureInterval::new(2700, 6500).unwrap())
            );
            let subscribes = peer.count("subscribe", "group-1", 2);
            let writes = peer.count("rpcReq", "group-1", 2);

            let mut narrowed = ls2307_instance();
            narrowed["services"][1]["properties"][2]["value-range"] = json!([3000, 6400, 1]);
            let confirmed = confirmed_for(control, "member-a", "devcea.light.ls2307", &narrowed);
            {
                let mut live = control.context.directory.borrow_mut();
                let slot = live.devices.get_mut("member-a").unwrap();
                slot.latest_description = Some(confirmed.into());
            }
            control.context.reconcile("member-a").unwrap();
            until(|| {
                function(&control.context, "group-1", "light:2")
                    .description()
                    .temperature_interval()
                    == Some(TemperatureInterval::new(3000, 6400).unwrap())
            })
            .await;

            let current = control.context.directory.borrow().devices["group-1"]
                .work
                .clone()
                .expect("the native runtime survives a logical replacement");
            assert!(
                Rc::ptr_eq(&runtime, &current),
                "a logical-only group change must reuse the native runtime"
            );
            async_io::Timer::after(Duration::from_millis(20)).await;
            assert_eq!(
                peer.count("subscribe", "group-1", 2),
                subscribes,
                "a logical-only group change must not rebuild native subscriptions"
            );

            // The retired logical instance cannot accept a new command or
            // reach the wire after the sink swap.
            assert!(old.execute(Command::On).await.is_err());
            async_io::Timer::after(Duration::from_millis(20)).await;
            assert_eq!(peer.count("rpcReq", "group-1", 2), writes);

            // Native notifications now project into the new logical sink.
            let new = function(&control.context, "group-1", "light:2");
            peer.notify_value(
                "group-1",
                PropertyBinding { siid: 2, piid: 1 },
                json!(false),
                false,
            );
            until(|| new.snapshot().power() == Some(Power::Off)).await;
        })
        .await;
    }));
}

/// Re-admit the same native description after a real source revocation.
async fn revoke_and_readmit(c: &Control, id: &str) -> crate::device::Function {
    let before = c
        .context
        .physical(&DeviceKey::new("xiaomi", "one"))
        .unwrap();
    let native = c.context.directory.borrow().devices["one"]
        .work
        .as_ref()
        .unwrap()
        .native
        .clone();
    let cloud_access = c.context.directory.borrow().devices["one"]
        .cloud_access
        .clone();
    c.context.revoke("one", "test removal").unwrap();
    {
        let mut live = c.context.directory.borrow_mut();
        let slot = live.devices.get_mut("one").unwrap();
        slot.latest_description.as_mut().unwrap().native = Some(native);
        slot.confirmed = true;
        slot.cloud_access = cloud_access;
    }
    c.context.reconcile("one").unwrap();
    until(|| {
        c.context
            .physical(&DeviceKey::new("xiaomi", "one"))
            .is_some_and(|now| !before.same_instance(&now))
    })
    .await;
    let new = function(&c.context, "one", id);
    until(|| new.snapshot().power() == Some(Power::Off)).await;
    new
}

#[test]
fn a_sent_outlet_receipt_survives_revocation_readmission_without_touching_the_new_sink() {
    let rig = Rig::new(true, &["one"]);
    {
        let mut cloud = rig.cloud.lock().unwrap();
        cloud.online.insert("one".into());
        cloud.raw_values.insert(("one".into(), 2, 1), json!(false));
    }
    block_on(rig.run(|c| async move {
        until(|| c.cloud.lock().unwrap().homes > 0).await;
        for code in [0, -704] {
            let old = function(&c.context, "one", "outlet:2");
            until(|| old.snapshot().power() == Some(Power::Off)).await;
            let (release, gate) = flume::bounded(1);
            let writes = c.cloud.lock().unwrap().property_writes.len();
            {
                let mut cloud = c.cloud.lock().unwrap();
                cloud.property_code = code;
                cloud.property_gate = Some(gate);
            }
            let mut command = Box::pin(old.execute(Command::On));
            assert!(poll_once(command.as_mut()).await.is_none());
            until(|| c.cloud.lock().unwrap().property_writes.len() == writes + 1).await;
            let mut queued = Box::pin(old.execute(Command::Off));
            assert!(poll_once(queued.as_mut()).await.is_none());
            let new = revoke_and_readmit(&c, "outlet:2").await;
            assert!(
                poll_once(command.as_mut()).await.is_none(),
                "a sent write must wait for its original receipt after retirement"
            );
            assert!(matches!(
                queued.await,
                Err(crate::device::CommandError::Unavailable(_))
            ));
            c.cloud.lock().unwrap().property_gate = None;
            release.send(()).unwrap();
            let expected = if code == 0 {
                Ok(())
            } else {
                Err(crate::device::CommandError::Rejected(format!(
                    "cloud code {code}"
                )))
            };
            assert_eq!(command.await.map(|_| ()), expected);
            let status = c
                .context
                .commands
                .snapshot("one", ["outlet:2".to_owned()].into_iter());
            let last = status[0].last.as_ref().unwrap();
            assert_eq!(last.actual_path, Some(PathKind::Cloud));
            assert_eq!(last.result, Some(expected));
            assert!(status[0].inflight.is_none());
            async_io::Timer::after(Duration::from_millis(20)).await;
            assert_eq!(old.snapshot().power(), Some(Power::Off));
            assert_eq!(new.snapshot().power(), Some(Power::Off));
            assert_eq!(c.cloud.lock().unwrap().property_writes.len(), writes + 1);
        }
    }));
}

#[test]
fn a_sent_local_power_receipt_settles_after_revocation_and_readmission() {
    for relay in [false, true] {
        let (rig, id) = if relay {
            (
                Rig::profile_light(
                    "zimi.switch.dhkg05",
                    serde_json::from_str(include_str!(
                        "../../../../tests/fixtures/miot/zimi-dhkg05.json"
                    ))
                    .unwrap(),
                ),
                "relay:2",
            )
        } else {
            (Rig::new(true, &["one"]), "outlet:2")
        };
        block_on(rig.run(|c| async move {
            c.announce();
            let control = &c;
            let peer = Rc::new(Peer::default());
            if relay {
                for siid in [2, 3, 4] {
                    peer.value("one", PropertyBinding { siid, piid: 2 }, json!(0));
                }
            }
            c.peer(peer, |peer| async move {
                initialized(control, &peer, &["one"]).await;
                peer.state.borrow_mut().hold_writes = true;
                for code in [0, -704] {
                    let old = function(&control.context, "one", id);
                    let writes = peer.count("rpcReq", "one", 2);
                    let mut command = Box::pin(old.execute(Command::On));
                    assert!(poll_once(command.as_mut()).await.is_none());
                    until(|| peer.count("rpcReq", "one", 2) == writes + 1).await;
                    let mut queued = Box::pin(old.execute(Command::Off));
                    assert!(poll_once(queued.as_mut()).await.is_none());
                    let new = revoke_and_readmit(control, id).await;
                    assert!(
                        poll_once(command.as_mut()).await.is_none(),
                        "the gateway must retain a sent write until its own receipt"
                    );
                    assert!(matches!(
                        queued.await,
                        Err(crate::device::CommandError::Unavailable(_))
                    ));
                    peer.release("one", 2, accepted("one", 2, code)).await;
                    let expected = if code == 0 {
                        Ok(())
                    } else {
                        Err(crate::device::CommandError::Rejected(format!(
                            "gateway code {code}"
                        )))
                    };
                    assert_eq!(command.await.map(|_| ()), expected);
                    let status = control
                        .context
                        .commands
                        .snapshot("one", [id.to_owned()].into_iter());
                    let last = status[0].last.as_ref().unwrap();
                    assert_eq!(last.actual_path, Some(PathKind::Local));
                    assert_eq!(last.result, Some(expected));
                    assert!(status[0].inflight.is_none());
                    assert_eq!(old.snapshot().power(), Some(Power::Off));
                    assert_eq!(new.snapshot().power(), Some(Power::Off));
                    assert_eq!(peer.count("rpcReq", "one", 2), writes + 1);
                }
            })
            .await;
        }));
    }
}
