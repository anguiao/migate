use super::*;
use crate::device::TemperatureInterval;
use crate::xiaomi::capability::PropertyBinding;

/// A group whose native control description is unchanged must keep its
/// installed runtime when only the served logical description (member-derived
/// capability interval) changes. Rebuilding native subscriptions here is the
/// whole-replace behavior this slice removes.
#[test]
fn logical_only_group_change_reuses_the_native_runtime() {
    let rig = repair_rig(0, true);
    block_on(rig.run(|c| async move {
        repair_ready(&c).await;
        let group = function(&c.context.registry, "group-1", "main");
        assert_eq!(
            group.description().temperature_interval(),
            Some(TemperatureInterval::new(2700, 6500).unwrap())
        );
        let runtime_before = c.context.live.borrow().devices["group-1"]
            .admitted
            .clone()
            .expect("the admitted group has an installed native runtime");

        // A valid member range update changes the group's derived logical
        // interval while the group's own native control description stays
        // byte-for-byte identical.
        let mut narrowed = ls2307_instance();
        narrowed["services"][1]["properties"][2]["value-range"] = json!([3000, 6400, 1]);
        let confirmed = confirmed_for(&c, "member-a", "devcea.light.ls2307", &narrowed);
        {
            let mut live = c.context.live.borrow_mut();
            let slot = live.devices.get_mut("member-a").unwrap();
            slot.native_confirmed = Some(confirmed.clone());
            slot.candidate = Some((confirmed, None));
        }
        c.context.reconcile("member-a").unwrap();
        until(|| {
            function(&c.context.registry, "group-1", "main")
                .description()
                .temperature_interval()
                == Some(TemperatureInterval::new(3000, 6400).unwrap())
        })
        .await;

        let runtime_after = c.context.live.borrow().devices["group-1"]
            .admitted
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
    let mut record = cached("account", "group-1", false);
    record.model = "mijia.light.group3".into();
    record.instance_type = urn;
    record.device = crate::xiaomi::capability::parse_group(
        DeviceKey::new("xiaomi", "group-1"),
        &record.model,
        &instance,
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
                    .live
                    .borrow()
                    .devices
                    .get("group-1")
                    .is_some_and(|slot| slot.admitted.is_some())
            })
            .await;
            until(|| {
                matches!(
                    control.context.control_path("group-1"),
                    Some(super::super::routing::ControlPath::Local(_))
                )
            })
            .await;
            let runtime = control.context.live.borrow().devices["group-1"]
                .admitted
                .clone()
                .expect("the physical group has a native runtime");
            until(|| {
                let subscribed = runtime.subscribed.borrow();
                runtime
                    .bindings()
                    .iter()
                    .all(|binding| subscribed.contains(&(binding.siid, binding.piid)))
            })
            .await;
            assert_eq!(
                control.context.active_notification("group-1"),
                Some(super::super::model::PathKind::Local)
            );

            let old = function(&control.context.registry, "group-1", "main");
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
                let mut live = control.context.live.borrow_mut();
                let slot = live.devices.get_mut("member-a").unwrap();
                slot.native_confirmed = Some(confirmed.clone());
                slot.candidate = Some((confirmed, None));
            }
            control.context.reconcile("member-a").unwrap();
            until(|| {
                function(&control.context.registry, "group-1", "main")
                    .description()
                    .temperature_interval()
                    == Some(TemperatureInterval::new(3000, 6400).unwrap())
            })
            .await;

            let current = control.context.live.borrow().devices["group-1"]
                .admitted
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
            let new = function(&control.context.registry, "group-1", "main");
            peer.notify_value(
                "group-1",
                PropertyBinding { siid: 2, piid: 1 },
                json!(false),
                false,
            );
            until(|| new.snapshot().power == Power::Off).await;
        })
        .await;
    }));
}

/// A sent outlet cloud write is a raw receipt owned outside the command
/// consumer. Same-DID revocation re-admission keeps the native control
/// description unchanged; the retired command settles early, but the raw
/// receipt still completes and never writes back into the new sink.
#[test]
fn a_sent_outlet_receipt_survives_revocation_readmission_without_touching_the_new_sink() {
    let rig = Rig::new(true, &["one"]);
    {
        let mut cloud = rig.cloud.lock().unwrap();
        cloud.online.insert("one".into());
        cloud.raw_values.insert(("one".into(), 2, 1), json!(false));
    }
    let (release, gate) = flume::bounded(1);
    block_on(rig.run(|c| async move {
        until(|| c.cloud.lock().unwrap().homes > 0).await;
        let old = function(&c.context.registry, "one", "main");
        until(|| old.snapshot().power == Power::Off).await;

        c.cloud.lock().unwrap().property_gate = Some(gate);
        let mut command = Box::pin(old.execute(Command::On));
        assert!(poll_once(command.as_mut()).await.is_none());
        until(|| c.cloud.lock().unwrap().property_writes.len() == 1).await;

        // Same-DID revocation re-admission: the native description and its
        // binding stay byte-for-byte identical.
        let native = c.context.live.borrow().devices["one"]
            .admitted
            .as_ref()
            .unwrap()
            .native
            .clone();
        let access = c.context.live.borrow().devices["one"].access.clone();
        let cloud_access = c.context.live.borrow().devices["one"].cloud_access.clone();
        c.context.revoke("one", "test removal").unwrap();
        assert_eq!(command.await, Err(crate::device::CommandError::Cancelled));
        {
            let mut live = c.context.live.borrow_mut();
            let slot = live.devices.get_mut("one").unwrap();
            slot.candidate = Some((native, None));
            slot.confirmed = true;
            slot.access = access;
            slot.cloud_access = cloud_access;
        }
        c.context.reconcile("one").unwrap();
        until(|| c.context.live.borrow().devices["one"].candidate.is_none()).await;
        let new = function(&c.context.registry, "one", "main");
        assert!(new.snapshot().power != Power::On);

        // The original HTTP future is still in flight after the consumer and
        // sink ended; releasing it must complete and settle that operation.
        let inflight = || {
            c.context
                .commands
                .snapshot("one", ["main".to_owned()].into_iter())[0]
                .inflight
                .is_some()
        };
        assert!(
            inflight(),
            "the raw receipt must stay in flight after readmission"
        );
        assert_eq!(c.cloud.lock().unwrap().property_writes.len(), 1);
        c.cloud.lock().unwrap().property_gate = None;
        release.send(()).unwrap();
        until(|| !inflight()).await;
        let last = &c
            .context
            .commands
            .snapshot("one", ["main".to_owned()].into_iter())[0]
            .last;
        assert_eq!(
            last.as_ref().and_then(|status| status.result.clone()),
            Some(Err(crate::device::CommandError::Cancelled)),
            "the late receipt settles the retired operation"
        );

        // The late acceptance belongs to the retired sink; the new logical
        // instance keeps the actual state and sends no second write.
        async_io::Timer::after(Duration::from_millis(20)).await;
        assert_eq!(new.snapshot().power, Power::Off);
        assert_eq!(c.cloud.lock().unwrap().property_writes.len(), 1);
    }));
}
