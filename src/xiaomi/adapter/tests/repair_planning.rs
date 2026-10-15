use super::{
    repair::{confirmed_for, group_record, ls2307_instance, repair_ready, repair_rig},
    *,
};
use crate::device::{Capability, LightColorTarget, LightDimension, LightTarget, NumericRange, Rgb};

#[test]
fn native_rgb_group_uses_member_temperature_interval_without_repair() {
    let rig = repair_rig(0, true);
    let mut native = ls2307_instance();
    native["services"][1]["properties"][2]["value-range"] = json!([2000, 8000, 1]);
    {
        let mut cloud = rig.cloud.lock().unwrap();
        let mut group = group_record("group-record", "group-1", &["member-a", "member-b"], false);
        group["model"] = json!("devcea.light.ls2307");
        group["spec_type"] = native["type"].clone();
        cloud.catalog_groups = vec![group];
        cloud
            .catalog_instances
            .insert(native["type"].as_str().unwrap().into(), native);
    }
    block_on(rig.run(|c| async move {
        repair_ready(&c).await;
        let light = function(&c.context.registry, "group-1", "main");
        assert!(light.description().has_capability(Capability::ColorRgb));
        assert!(c.context.group_repair("group-1").is_none());
        let native = c.publication.current();
        let cached = &native
            .source
            .entries
            .iter()
            .find(|entry| entry.physical.key().id == "group-1")
            .unwrap()
            .cached;
        let bindings = cached.device.functions[0].lighting.as_ref().unwrap();
        assert!(bindings.color.is_some(), "RGB must stay genuinely native");
        assert_eq!(bindings.temperature.as_ref().unwrap().range.min, 2000);
        assert_eq!(bindings.temperature.as_ref().unwrap().range.max, 8000);
        assert_eq!(
            light.description().temperature_range(),
            Some(NumericRange {
                min: 2700,
                max: 6500,
                step: 1,
            })
        );
        let action = light
            .begin_light_action(
                LightDimension::Color,
                LightTarget {
                    color: Some(LightColorTarget::Rgb(Rgb::from_packed(0xff0000).unwrap())),
                    ..LightTarget::default()
                },
                false,
            )
            .unwrap();
        action.accepted().await.unwrap();
        let gate = c.context.live.borrow().devices["group-1"]
            .work
            .as_ref()
            .unwrap()
            .light_write
            .clone()
            .unwrap();
        let mode = bindings.mode.unwrap();
        assert!(
            gate.is_feedback(mode.property, &json!(mode.rgb_value)),
            "the native group's required mode receipt must own its short late echo"
        );
        assert!(!gate.is_feedback(mode.property, &json!(mode.temperature_value)));
        c.clock.advance(Duration::from_secs(4));
        assert!(!gate.is_feedback(mode.property, &json!(mode.rgb_value)));
    }));
}

#[test]
fn failed_preparation_reuses_only_the_latest_member_temperature_shape() {
    let rig = repair_rig(0, true);
    block_on(rig.run(|c| async move {
        repair_ready(&c).await;
        let key = DeviceKey::new("xiaomi", "group-1");
        let before = c.store.devices().load_published(&key).unwrap().unwrap();
        let set_range = |min, max| {
            let mut instance = ls2307_instance();
            instance["services"][1]["properties"][2]["value-range"] = json!([min, max, 1]);
            let cached = confirmed_for(&c, "member-a", "devcea.light.ls2307", &instance);
            c.context
                .live
                .borrow_mut()
                .devices
                .get_mut("member-a")
                .unwrap()
                .native_confirmed = Some(cached);
        };
        set_range(3000, 6400);
        let prepared = c.publication.prepare().unwrap().unwrap();
        let db = rusqlite::Connection::open(c.store.path()).unwrap();
        db.execute_batch(
            "CREATE TRIGGER fail_config BEFORE INSERT ON device_cache
             BEGIN SELECT RAISE(ABORT, 'injected'); END;",
        )
        .unwrap();
        assert!(c.publication.apply(prepared).is_err());
        assert_eq!(
            c.store.devices().load_published(&key).unwrap().unwrap(),
            before,
            "failed complete save must preserve the old configuration"
        );

        // A newer genuine member input arrives before retrying the failed save.
        set_range(3200, 6200);
        let latest = c.publication.prepare().unwrap().unwrap();
        db.execute_batch("DROP TRIGGER fail_config").unwrap();
        c.publication.stop_actions();
        c.publication.apply(latest).unwrap();
        let light = function(&c.context.registry, "group-1", "main");
        assert_eq!(
            light.description().temperature_range(),
            Some(NumericRange {
                min: 3200,
                max: 6200,
                step: 1,
            }),
            "a prepared group cannot retain an obsolete member interval"
        );
        let after = c.store.devices().load_published(&key).unwrap().unwrap();
        assert_eq!(after.endpoint, before.endpoint);
        assert_eq!(after.unique_id, before.unique_id);
        assert_eq!(
            after.configuration_version,
            before.configuration_version + 1
        );
    }));
}
