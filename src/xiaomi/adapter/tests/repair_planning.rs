use super::{
    support::{confirmed_for, group_record, ls2307_instance, repair_ready, repair_rig},
    *,
};
use crate::device::{LightTarget, Rgb, TemperatureInterval};

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
        let light = function(&c.context.registry, "group-1", "light:2");
        assert!(light.description().supports_rgb());
        assert!(c.context.group_repair("group-1").is_none());
        let native = c.publication.current();
        let cached = &native
            .entries
            .iter()
            .find(|entry| entry.physical.key().id == "group-1")
            .unwrap()
            .native;
        let bindings = cached.device().functions()[0].lighting().unwrap();
        assert!(bindings.color().is_some(), "RGB must stay genuinely native");
        assert_eq!(bindings.temperature().as_ref().unwrap().range().min(), 2000);
        assert_eq!(bindings.temperature().as_ref().unwrap().range().max(), 8000);
        assert_eq!(
            light.description().temperature_interval(),
            Some(TemperatureInterval::new(2700, 6500).unwrap())
        );
        c.cloud.lock().unwrap().property_reads.clear();
        let action = light
            .begin_light_action(
                LightTarget::rgb(None, Rgb::from_packed(0xff0000).unwrap()),
                false,
            )
            .unwrap();
        action.accepted().await.unwrap();
        until(|| {
            let cloud = c.cloud.lock().unwrap();
            ["member-a", "member-b"].iter().all(|did| {
                [1, 2].iter().all(|piid| {
                    cloud
                        .property_reads
                        .iter()
                        .any(|(read_did, siid, read_piid)| {
                            read_did == did && *siid == 4 && read_piid == piid
                        })
                })
            })
        })
        .await;
        assert_eq!(
            light.snapshot().lighting().unwrap().color,
            Rgb::from_packed(0x112233),
            "native RGB acceptance cannot replace actual member feedback"
        );
        let writes = c.cloud.lock().unwrap();
        assert!(
            writes
                .property_reads
                .iter()
                .all(|(did, _, _)| did != "group-1")
        );
        let items: Vec<_> = writes
            .numeric_writes
            .iter()
            .flat_map(|(items, _)| items)
            .collect();
        assert!(items.iter().all(|item| item["did"] == "group-1"));
        assert!(
            items
                .iter()
                .any(|item| item["siid"] == 4 && item["piid"] == 1 && item["value"] == 0xff0000),
            "native RGB must use its own binding"
        );
    }));
}

#[test]
fn failed_preparation_reuses_only_the_latest_member_temperature_shape() {
    let rig = repair_rig(0, true);
    block_on(rig.run(|c| async move {
        repair_ready(&c).await;
        let key = DeviceKey::new("xiaomi", "group-1");
        let before = c
            .store
            .devices()
            .load_configuration()
            .unwrap()
            .device(&key)
            .cloned()
            .unwrap();
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
                .candidate = Some(cached.into());
        };
        set_range(3000, 6400);
        let db = rusqlite::Connection::open(c.store.path()).unwrap();
        let subscription_key = rs_matter::persist::PERSISTENT_SUBSCRIPTIONS_START;
        c.store.matter().put(subscription_key, &[1, 2, 3]).unwrap();
        db.execute_batch(
            "CREATE TRIGGER fail_subscription_delete BEFORE DELETE ON blobs
             BEGIN SELECT RAISE(ABORT, 'injected'); END;",
        )
        .unwrap();
        let prepared = c
            .publication
            .prepare(c.publication.plan().unwrap().unwrap());
        assert!(c.publication.apply(prepared).is_err());
        assert!(c.publication.failure().is_some());
        assert!(c.store.matter().contains(subscription_key).unwrap());
        assert_eq!(
            c.store
                .devices()
                .load_configuration()
                .unwrap()
                .device(&key)
                .cloned()
                .unwrap(),
            before
        );
        db.execute_batch("DROP TRIGGER fail_subscription_delete")
            .unwrap();

        db.execute_batch(
            "CREATE TRIGGER fail_config BEFORE UPDATE ON bridge_configuration
             BEGIN SELECT RAISE(ABORT, 'injected'); END;",
        )
        .unwrap();
        let prepared = c
            .publication
            .prepare(c.publication.plan().unwrap().unwrap());
        assert!(c.publication.apply(prepared).is_err());
        assert!(c.store.matter().contains(subscription_key).unwrap());
        assert_eq!(
            c.store
                .devices()
                .load_configuration()
                .unwrap()
                .device(&key)
                .cloned()
                .unwrap(),
            before,
            "failed complete save must preserve the old configuration"
        );

        // A newer genuine member input arrives before retrying the failed save.
        set_range(3200, 6200);
        let latest = c
            .publication
            .prepare(c.publication.plan().unwrap().unwrap());
        db.execute_batch("DROP TRIGGER fail_config").unwrap();
        c.publication.stop_actions();
        c.publication.apply(latest).unwrap();
        let light = function(&c.context.registry, "group-1", "light:2");
        assert_eq!(
            light.description().temperature_interval(),
            Some(TemperatureInterval::new(3200, 6200).unwrap()),
            "a prepared group cannot retain an obsolete member interval"
        );
        let after = c
            .store
            .devices()
            .load_configuration()
            .unwrap()
            .device(&key)
            .cloned()
            .unwrap();
        assert_eq!(after.endpoint, before.endpoint);
        assert_eq!(after.unique_id, before.unique_id);
        assert_eq!(
            after.configuration_version,
            before.configuration_version + 1
        );
    }));
}
