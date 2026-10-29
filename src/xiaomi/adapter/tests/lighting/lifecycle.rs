use super::*;

#[test]
fn stopping_the_source_clears_light_state_but_not_outlet_cache() {
    block_on(Rig::light("xiaomi.light.ceil04").run(|c| async move {
        c.announce();
        let peer = peer_values(&c);
        let control = &c;
        c.peer(peer, |peer| async move {
            let light = ready_light(control).await;
            assert!(
                light
                    .snapshot()
                    .lighting()
                    .unwrap()
                    .brightness_percent
                    .is_some()
            );
            control.context.stop();
            let snapshot = light.snapshot();
            assert!(matches!(
                snapshot.availability,
                Availability::Unavailable(_)
            ));
            assert_eq!(snapshot.power(), Some(Power::Unknown));
            let lighting = snapshot.lighting().unwrap();
            assert_eq!(lighting.brightness_percent, None);
            assert_eq!(lighting.temperature_kelvin, None);
            assert_eq!(lighting.color, None);
            assert_eq!(lighting.mode, Some(LightMode::Temperature));
            let _ = peer;
        })
        .await;
    }));
}

#[test]
fn narrower_supported_range_replaces_the_logical_light_while_keeping_identity() {
    block_on(Rig::light("xiaomi.light.ceil04").run(|c| async move {
        c.announce();
        let peer = peer_values(&c);
        let control = &c;
        c.peer(peer, |peer| async move {
            let key = DeviceKey::new("xiaomi", "one");
            let light = ready_light(control).await;
            assert_eq!(
                light.description().temperature_interval().unwrap().max(),
                5700
            );
            let before = control
                .store
                .devices()
                .load_configuration()
                .unwrap()
                .device(&key)
                .cloned()
                .unwrap();
            let original_physical = control.context.registry.get(&key).unwrap();
            let original = control.context.live.borrow().devices["one"]
                .admitted
                .as_ref()
                .unwrap()
                .native
                .clone();
            let mut narrower = original.clone();
            narrower = crate::xiaomi::capability::NativeDescription::new(
                narrower.model().to_owned(),
                narrower.instance_type().to_owned(),
                super::super::support::device_with_temperature_grid(
                    narrower.device(),
                    crate::device::NumericRange::new(2700, 5600, 1).unwrap(),
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
                .candidate
                .as_mut()
                .unwrap()
                .native = Some(narrower);
            control.context.reconcile("one").unwrap();

            // Wait for the actual instance replacement, not the still-serving
            // old available instance.
            let replaced = ready_replacement_light(control).await;
            assert_eq!(
                replaced.description().temperature_interval().unwrap().max(),
                5600
            );
            let physical = control.context.registry.get(&key).unwrap();
            assert_eq!(physical.availability(), Availability::Available);
            assert!(!physical.same_instance(&original_physical));
            let saved = control
                .store
                .devices()
                .load_configuration()
                .unwrap()
                .device(&key)
                .cloned()
                .unwrap();
            assert_eq!(saved.unique_id, before.unique_id);
            assert_eq!(saved.endpoint, before.endpoint);
            assert_eq!(saved.label, before.label);
            assert_eq!(
                saved.configuration_version,
                before.configuration_version + 1
            );
            assert_eq!(
                saved.native.as_ref().unwrap().device().functions()[0]
                    .lighting()
                    .unwrap()
                    .temperature()
                    .unwrap()
                    .range()
                    .max(),
                5600
            );
            let _ = peer;
        })
        .await;
    }));
}

#[test]
fn binding_change_retires_old_feedback_authority() {
    block_on(Rig::light("xiaomi.light.ceil04").run(|c| async move {
        c.announce();
        let peer = peer_values(&c);
        let control = &c;
        c.peer(peer, |peer| async move {
            let old = ready_light(control).await;
            freeze_light_clock(control);
            peer.state.borrow_mut().hold_writes = true;
            let action = old.begin_light_action(brightness(60.0), false).unwrap();
            until(|| peer.count("rpcReq", "one", 0) == 1).await;
            let mut changed = control.context.live.borrow().devices["one"]
                .admitted
                .as_ref()
                .unwrap()
                .native
                .clone();
            changed = crate::xiaomi::capability::NativeDescription::new(
                changed.model().to_owned(),
                changed.instance_type().to_owned(),
                super::super::support::device_with_brightness_format(
                    changed.device(),
                    crate::xiaomi::capability::NumericFormat::Uint16,
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
                .candidate
                .as_mut()
                .unwrap()
                .native = Some(changed);
            control.context.reconcile("one").unwrap();
            let new = ready_replacement_light(control).await;
            assert_eq!(
                new.description().temperature_interval().unwrap().max(),
                5700
            );
            assert!(!old.light_action_current(action.token()));
            let next = new.begin_light_action(brightness(70.0), false).unwrap();
            async_io::Timer::after(std::time::Duration::from_millis(30)).await;
            assert_eq!(peer.count("rpcReq", "one", 0), 1);
            assert!(poll_once(next.accepted()).await.is_none());
            assert!(poll_once(action.accepted()).await.is_none());
            // The old receipt lands after the binding authority was replaced;
            // it must not re-enter the new accepted history.
            peer.release_property("one", BRIGHTNESS, accepted_items(&[(BRIGHTNESS, 0)]))
                .await;
            action.accepted().await.unwrap();
            until(|| peer.count("rpcReq", "one", 0) == 2).await;
            peer.release_property("one", BRIGHTNESS, accepted_items(&[(BRIGHTNESS, 0)]))
                .await;
            next.accepted().await.unwrap();
            assert_eq!(
                new.snapshot().lighting().unwrap().brightness_percent,
                Some(1.0)
            );
            peer.notify_value("one", BRIGHTNESS, json!(60), false);
            until(|| !new.light_action_current(next.token())).await;
            assert_eq!(
                next.ended().await.unwrap_err(),
                crate::device::CommandError::Superseded
            );
            assert_eq!(
                new.snapshot().lighting().unwrap().brightness_percent,
                Some(60.0)
            );
            assert_eq!(peer.count("rpcReq", "one", 0), 2);
        })
        .await;
    }));
}

#[test]
fn retired_work_replacement_retires_old_feedback_authority() {
    block_on(Rig::light("xiaomi.light.ceil04").run(|c| async move {
        c.announce();
        let peer = peer_values(&c);
        let control = &c;
        c.peer(peer, |peer| async move {
            let old = ready_light(control).await;
            freeze_light_clock(control);
            let first = old.begin_light_action(brightness(60.0), true).unwrap();
            first.accepted().await.unwrap();
            let accepted_at = control.clock.now();
            first.ended().await.unwrap();
            drop(first);
            assert_eq!(peer.count("rpcReq", "one", 2), 1);
            assert_eq!(
                old.snapshot().lighting().unwrap().brightness_percent,
                Some(1.0)
            );
            drop(old);

            let old_work = control.context.live.borrow().devices["one"]
                .admitted
                .clone()
                .unwrap();
            let retired_work = Rc::downgrade(&old_work);
            let mut changed = old_work.native.clone();
            changed = crate::xiaomi::capability::NativeDescription::new(
                changed.model().to_owned(),
                changed.instance_type().to_owned(),
                super::super::support::device_with_temperature_grid(
                    changed.device(),
                    crate::device::NumericRange::new(3000, 5500, 1).unwrap(),
                ),
            )
            .unwrap();
            drop(old_work);
            peer.value("one", TEMPERATURE, json!(4000));
            control
                .context
                .live
                .borrow_mut()
                .devices
                .get_mut("one")
                .unwrap()
                .candidate
                .as_mut()
                .unwrap()
                .native = Some(changed);
            control.context.reconcile("one").unwrap();
            let new = ready_replacement_light(control).await;
            assert_eq!(
                new.description().temperature_interval().unwrap().max(),
                5500
            );
            assert!(retired_work.upgrade().is_none());
            let next = new.begin_light_action(brightness(70.0), false).unwrap();
            next.accepted().await.unwrap();
            assert_eq!(peer.count("rpcReq", "one", 2), 2);
            assert_eq!(
                new.snapshot().lighting().unwrap().brightness_percent,
                Some(1.0)
            );
            // The replaced binding authority retires the old accepted
            // history, so a later old-value report is an external operation.
            control
                .clock
                .advance(accepted_at + Duration::from_millis(2999) - control.clock.now());
            peer.notify_value("one", BRIGHTNESS, json!(60), false);
            until(|| !new.light_action_current(next.token())).await;
            assert_eq!(
                next.ended().await.unwrap_err(),
                crate::device::CommandError::Superseded
            );
            assert_eq!(
                new.snapshot().lighting().unwrap().brightness_percent,
                Some(60.0)
            );
            assert_eq!(peer.count("rpcReq", "one", 2), 2);
        })
        .await;
    }));
}
