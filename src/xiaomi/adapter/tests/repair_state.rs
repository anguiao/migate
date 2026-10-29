use super::{
    support::{repair_ready, repair_rig},
    *,
};
use crate::device::{LightMode, Rgb};

#[test]
fn representative_cannot_mix_a_new_source_mode_with_an_old_rgb_cache() {
    let rig = repair_rig(1, false);
    block_on(rig.run(|c| async move {
        repair_ready(&c).await;
        until(|| {
            ["member-a", "member-b"].iter().all(|did| {
                function(&c.context.registry, did, "light:2")
                    .snapshot()
                    .lighting()
                    .is_some_and(|state| state.mode.is_some() && state.color.is_some())
            })
        })
        .await;
        let light = function(&c.context.registry, "group-1", "light:2");
        let work = c.context.live.borrow().devices["member-a"]
            .admitted
            .clone()
            .unwrap();
        let color = crate::xiaomi::capability::PropertyBinding { siid: 4, piid: 1 };
        let mode = crate::xiaomi::capability::PropertyBinding { siid: 4, piid: 2 };
        work.notify_value(color, &json!(0x00ff00), false, false);
        c.context.refresh_group_states("member-a");
        assert_eq!(
            light.snapshot().lighting().unwrap().color,
            Rgb::from_packed(0x00ff00)
        );

        // Only a mode has arrived from the new source authority. Its missing
        // RGB cannot be filled with the previous authority's cached value.
        work.test_source_changed();
        work.notify_value(mode, &json!(0), false, false);
        c.context.refresh_group_states("member-a");
        let state = light.snapshot().lighting().unwrap();
        assert_eq!(state.mode, Some(LightMode::Temperature));
        assert_eq!(state.color, None);

        // A readable unknown-mode A and a valid but OFF B are in the same
        // fallback tier, so stable member order selects A and keeps it unknown.
        work.test_source_changed();
        work.notify_value(
            crate::xiaomi::capability::PropertyBinding { siid: 2, piid: 1 },
            &json!(true),
            false,
            false,
        );
        c.context.refresh_group_states("member-a");
        let state = light.snapshot().lighting().unwrap();
        assert_eq!(state.mode, None);
        assert_eq!(state.color, None);
    }));
}

#[test]
fn physical_repair_preserves_native_power_brightness_and_temperature() {
    let rig = repair_rig(0, true);
    rig.cloud.lock().unwrap().catalog_groups[0]["extra"]["member_list"]["is_physical"] =
        json!(true);
    let listener = Async::<TcpListener>::bind(([127, 0, 0, 1], 0)).unwrap();
    rig.source.context.notification_connector.set(
        super::super::notifications::NotificationConnector::Plaintext(
            listener.get_ref().local_addr().unwrap(),
        ),
    );
    let peer = Rc::new(CloudPeer::default());
    block_on(rig.run(|c| async move {
        future::or(peer.run(&listener), async {
            repair_ready(&c).await;
            until(|| {
                cloud_active(&c, "group-1")
                    && cloud_active(&c, "member-a")
                    && cloud_active(&c, "member-b")
            })
            .await;
            let group = function(&c.context.registry, "group-1", "light:2");
            for (did, siid, piid, value) in [
                ("group-1", 2, 1, json!(false)),
                ("group-1", 2, 2, json!(13)),
                ("group-1", 2, 3, json!(4000)),
                ("member-a", 2, 1, json!(true)),
                ("member-a", 2, 2, json!(80)),
                ("member-a", 2, 3, json!(3000)),
                ("member-a", 4, 1, json!(0x00ff00)),
                ("member-a", 4, 2, json!(1)),
            ] {
                c.cloud
                    .lock()
                    .unwrap()
                    .raw_values
                    .insert((did.into(), siid, piid), value.clone());
                peer.send(Packet::Publish(Publish::new(
                    format!("device/{did}/up/properties_changed/{siid}/{piid}"),
                    QoS::AtMostOnce,
                    json!({"params":{"did":did,"siid":siid,"piid":piid,"value":value}}).to_string(),
                )));
            }
            until(|| {
                group.snapshot().lighting().is_some_and(|state| {
                    state.color == Rgb::from_packed(0x00ff00) && state.mode == Some(LightMode::Rgb)
                })
            })
            .await;
            let snapshot = group.snapshot();
            let color = snapshot.lighting().unwrap();
            assert_eq!(snapshot.power(), Some(Power::Off));
            assert_eq!(color.brightness_percent, Some(13.0));
            assert_eq!(color.temperature_kelvin, Some(4000));
        })
        .await;
    }));
}
