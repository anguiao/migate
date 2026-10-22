use super::*;
use crate::device::NumericRange;
use crate::xiaomi::capability::PropertyBinding;

const POWER: PropertyBinding = PropertyBinding { siid: 2, piid: 1 };
const BRIGHTNESS: PropertyBinding = PropertyBinding { siid: 2, piid: 2 };
const TEMPERATURE: PropertyBinding = PropertyBinding { siid: 2, piid: 3 };

fn device_work(c: &Control, did: &str) -> Rc<super::super::work::DeviceWork> {
    c.context.live.borrow().devices[did]
        .admitted
        .as_ref()
        .unwrap()
        .clone()
}

async fn release_read(peer: &Peer, did: &str, siid: u32, value: Value) {
    let index = peer
        .state
        .borrow()
        .held
        .iter()
        .position(|(publish, _)| {
            let payload = decode_mips(&publish.payload).unwrap().payload;
            publish.topic.rsplit('/').next() == Some("get")
                && payload["did"] == did
                && payload["siid"] == siid
        })
        .unwrap();
    let (publish, _) = peer.state.borrow_mut().held.remove(index);
    let client = peer.state.borrow().client_id.clone();
    peer.send(response(&client, &publish, value));
}

#[test]
fn a_native_replacement_clears_its_previous_subscription_failure() {
    block_on(Rig::light("xiaomi.light.ceil04").run(|c| async move {
        c.announce();
        let peer = Rc::new(Peer::default());
        peer.value("one", POWER, json!(false));
        peer.value("one", BRIGHTNESS, json!(50));
        peer.value("one", TEMPERATURE, json!(2700));
        peer.state
            .borrow_mut()
            .sub_reject
            .insert(("one".into(), 2, 3));
        let control = &c;
        c.peer(peer, |peer| async move {
            until(|| {
                control
                    .context
                    .status
                    .borrow()
                    .pending
                    .iter()
                    .any(|pending| pending.did == "one" && pending.reason.contains("subscribe 2.3"))
            })
            .await;
            peer.state.borrow_mut().sub_reject.clear();
            let before = device_work(control, "one");
            let mut replacement = before.native.clone();
            replacement = crate::xiaomi::capability::NativeDescription::new(
                replacement.model().to_owned(),
                replacement.instance_type().to_owned(),
                super::support::device_with_temperature_grid(
                    replacement.device(),
                    NumericRange::new(3000, 5500, 1).unwrap(),
                ),
            )
            .unwrap();
            control
                .context
                .live
                .borrow_mut()
                .devices
                .get_mut("one")
                .unwrap()
                .latest = Some(replacement);
            control.context.reconcile("one").unwrap();
            until(|| {
                let current = device_work(control, "one");
                !Rc::ptr_eq(&before, &current)
            })
            .await;
            assert!(
                control
                    .context
                    .status
                    .borrow()
                    .pending
                    .iter()
                    .all(|pending| {
                        pending.did != "one" || !pending.reason.contains("subscribe 2.3")
                    })
            );
            let current = device_work(control, "one");
            until(|| current.test_local_subscribed_len() == current.bindings().len()).await;
        })
        .await;
    }));
}

#[test]
fn a_supported_range_change_keeps_identity_filters_old_reads_and_uses_the_new_instance() {
    let rig = Rig::light("xiaomi.light.ceil04");
    block_on(rig.run(|c| async move {
        c.publication
            .rename(&DeviceKey::new("xiaomi", "one"), "Controller saved name")
            .unwrap();
        c.announce();
        let peer = Rc::new(Peer::default());
        peer.value("one", POWER, json!(false));
        peer.value("one", BRIGHTNESS, json!(50));
        peer.value("one", TEMPERATURE, json!(2700));
        peer.state
            .borrow_mut()
            .sub_timeout
            .insert(("one".into(), 2, 3));
        let control = &c;
        c.peer(peer, |peer| async move {
            until(|| peer.count("get", "one", 2) >= 1 && peer.count("subscribe", "one", 2) >= 1)
                .await;
            let key = DeviceKey::new("xiaomi", "one");
            let before = control
                .store
                .devices()
                .load_configuration()
                .unwrap()
                .device(&key)
                .cloned()
                .unwrap();
            assert_eq!(before.label, "Controller saved name");
            let before_physical = control.context.registry.get(&key).unwrap();
            // Hold one old-instance temperature read; its late value stays
            // inside the widened range, so only instance filtering can reject it.
            peer.state.borrow_mut().hold_reads = true;
            device_work(control, "one").refresh_property("main", TEMPERATURE);
            until(|| !peer.state.borrow().held.is_empty()).await;
            let mut changed = device_work(control, "one").native.clone();
            changed = crate::xiaomi::capability::NativeDescription::new(
                changed.model().to_owned(),
                changed.instance_type().to_owned(),
                super::support::device_with_temperature_grid(
                    changed.device(),
                    NumericRange::new(2700, 6500, 1).unwrap(),
                ),
            )
            .unwrap();
            peer.value("one", TEMPERATURE, json!(6000));
            control
                .context
                .live
                .borrow_mut()
                .devices
                .get_mut("one")
                .unwrap()
                .latest = Some(changed);
            control.context.reconcile("one").unwrap();
            until(|| {
                control
                    .context
                    .registry
                    .get(&key)
                    .is_some_and(|now| !before_physical.same_instance(&now))
            })
            .await;
            let old = std::mem::take(&mut peer.state.borrow_mut().held_subs);
            for sub in old {
                peer.send(Packet::SubAck(SubAck::new(
                    sub.pkid,
                    vec![SubscribeReasonCode::QoS2],
                )));
            }
            peer.state.borrow_mut().hold_reads = false;
            let current = function(&control.context.registry, "one", "main");
            device_work(control, "one").refresh();
            until(|| current.snapshot().lighting().unwrap().temperature_kelvin == Some(6000)).await;
            // The old held read (2700 K) belongs to the retired instance and
            // must not overwrite the new instance's actual 6000 K.
            release_read(&peer, "one", 2, json!({"value":2700})).await;
            for _ in 0..20 {
                future::yield_now().await;
            }
            let target = crate::device::LightTarget::temperature(None, 5000.0).unwrap();
            current
                .begin_light_action(target, false)
                .unwrap()
                .accepted()
                .await
                .unwrap();
            assert_eq!(peer.count("rpcReq", "one", 2), 1);
            assert_eq!(
                current.snapshot().lighting().unwrap().temperature_kelvin,
                Some(6000)
            );
            let after = control
                .store
                .devices()
                .load_configuration()
                .unwrap()
                .device(&key)
                .cloned()
                .unwrap();
            assert_eq!(after.unique_id, before.unique_id);
            assert_eq!(after.endpoint, before.endpoint);
            assert_eq!(after.label, "Controller saved name");
            assert_eq!(
                after.native.as_ref().unwrap().device().functions()[0]
                    .lighting()
                    .unwrap()
                    .temperature()
                    .unwrap()
                    .range()
                    .max(),
                6500
            );
        })
        .await;
    }));
}

#[test]
fn endpoint_exhaustion_is_pending_and_does_not_stop_cached_devices() {
    let rig = Rig::new(true, &["one"]);
    rig.cloud.lock().unwrap().members = vec!["one".into(), "multi".into()];
    let mut configuration = rig.store.devices().load_configuration().unwrap();
    let historical = super::support::device_with_key(rig.data[0].device(), "history");
    configuration
        .devices
        .push(crate::storage::tests::fixtures::identity(
            &historical.physical_description("History"),
            65533,
        ));
    rig.store
        .devices()
        .commit_configuration(&configuration, &[])
        .unwrap();
    block_on(rig.run(|c| async move {
        c.announce();
        let peer = Rc::new(Peer::default());
        let control = &c;
        c.peer(peer, |peer| async move {
            initialized(control, &peer, &["one"]).await;
            until(|| {
                control
                    .context
                    .status
                    .borrow()
                    .pending
                    .iter()
                    .any(|pending| pending.did == "multi" && pending.reason.contains("exhausted"))
            })
            .await;
            assert_eq!(control.context.registry.list().len(), 1);
            assert!(
                control
                    .store
                    .devices()
                    .load_configuration()
                    .unwrap()
                    .device(&DeviceKey::new("xiaomi", "multi"))
                    .cloned()
                    .is_none()
            );
            function(&control.context.registry, "one", "main")
                .execute(Command::On)
                .await
                .unwrap();
        })
        .await;
    }));
}

#[test]
fn publication_database_failure_keeps_the_complete_cached_configuration() {
    let rig = Rig::new(true, &["one"]);
    let store = rig.store.clone();
    rig.cloud.lock().unwrap().members = vec!["one".into(), "multi".into()];
    rusqlite::Connection::open(store.path()).unwrap().execute_batch("CREATE TRIGGER fail_multi BEFORE UPDATE ON bridge_configuration BEGIN SELECT RAISE(ABORT,'publication failed'); END;").unwrap();
    block_on(rig.run(|c| async move {
        c.announce();
        let control = &c;
        c.peer(Rc::new(Peer::default()), |peer| async move {
            initialized(control, &peer, &["one"]).await;
            until(|| control.publication.failure().is_some()).await;
            assert_eq!(control.context.registry.list().len(), 1);
            assert!(
                control
                    .store
                    .devices()
                    .load_configuration()
                    .unwrap()
                    .device(&DeviceKey::new("xiaomi", "multi"))
                    .cloned()
                    .is_none()
            );
            assert!(
                control
                    .store
                    .devices()
                    .load_configuration()
                    .unwrap()
                    .device(&DeviceKey::new("xiaomi", "multi"))
                    .and_then(|device| device.native.as_ref())
                    .is_none()
            );
            function(&control.context.registry, "one", "main")
                .execute(Command::On)
                .await
                .unwrap();
        })
        .await;
    }));
}
