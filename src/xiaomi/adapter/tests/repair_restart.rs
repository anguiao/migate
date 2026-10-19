use super::{
    support::{confirmed_for, repair_ready, repair_rig},
    *,
};
use crate::{
    app::App,
    device::{LightTarget, Rgb},
};

async fn apply_restored(publication: &App) -> Result<(), AdapterError> {
    loop {
        let changed = publication.listen();
        if let Some(plan) = publication.plan()? {
            if plan.rebuild {
                publication.stop_actions();
            }
            publication.apply(publication.prepare(plan))?;
        }
        changed.await;
    }
}

#[test]
fn fresh_adapter_restores_rgb_plan_and_controls_real_member_bindings() {
    let rig = repair_rig(1, true);
    block_on(rig.run(|c| async move {
        repair_ready(&c).await;
        until(|| c.context.group_repair("group-1").is_some()).await;
        let before = c.store.devices().load_configuration().unwrap();
        let key = DeviceKey::new("xiaomi", "group-1");
        let original = c.context.registry.get(&key).unwrap();
        let (restored, ready) = XiaomiAdapter::build(
            c.store.clone(),
            c.context.cloud.clone(),
            c.discovery.clone(),
            c.clock.clone(),
        )
        .unwrap();
        ready.wait().await.unwrap();
        let registry = restored.context.registry.clone();
        let group = registry.get(&key).unwrap();
        assert!(!original.same_instance(&group));
        let light = group.function("main").unwrap();
        assert!(light.description().supports_rgb());
        let initial = light.snapshot();
        assert_eq!(initial.power, Power::Unknown);
        let color = initial.lighting.unwrap();
        assert_eq!(color.color, None);
        assert_eq!(color.mode, None);
        assert_eq!(color.temperature_kelvin, None);

        let context = restored.context.clone();
        let publication = Rc::new(App::open(restored.source_handle(), c.store.clone()).unwrap());
        assert!(context.group_repair("group-1").is_some());
        let subscription_key = rs_matter::persist::PERSISTENT_SUBSCRIPTIONS_START;
        c.store.matter().put(subscription_key, &[1, 2, 3]).unwrap();
        assert!(
            publication.plan().unwrap().is_none(),
            "the restored complete configuration is already installed"
        );
        assert_eq!(
            c.store.matter().get(subscription_key).unwrap(),
            Some(vec![1, 2, 3]),
            "restoring an unchanged configuration must retain saved subscriptions"
        );
        assert!(context.group_repair("group-1").is_some());
        let after = c.store.devices().load_configuration().unwrap();
        assert_eq!(after.root_version, before.root_version);
        assert_eq!(after.devices, before.devices);
        for native in after
            .devices
            .iter()
            .filter_map(|device| device.native.as_ref())
        {
            if native.device().key() == &key {
                assert!(
                    native.device().functions()[0]
                        .lighting()
                        .unwrap()
                        .color()
                        .is_none()
                );
            }
        }
        c.cloud.lock().unwrap().numeric_writes.clear();

        // Drive the new adapter and the same production configuration applier
        // against the existing mock cloud; no old runtime state is reused.
        let applying = apply_restored(&publication);
        let testing = async {
            until(|| group.availability() == Availability::Available).await;
            until(|| {
                ["member-a", "member-b"]
                    .iter()
                    .all(|did| function(&registry, did, "main").snapshot().power == Power::On)
            })
            .await;
            let target = Rgb {
                red: 19,
                green: 122,
                blue: 233,
            };
            light
                .begin_light_action(LightTarget::rgb(None, target), true)
                .unwrap()
                .accepted()
                .await
                .unwrap();
            until(|| {
                let cloud = c.cloud.lock().unwrap();
                ["member-a", "member-b"].iter().all(|did| {
                    cloud
                        .numeric_writes
                        .iter()
                        .flat_map(|(items, _)| items)
                        .any(|item| {
                            item["did"] == *did
                                && item["siid"] == 4
                                && item["piid"] == 1
                                && item["value"] == target.packed()
                        })
                })
            })
            .await;
            let saved = c.store.devices().load_configuration().unwrap();
            assert_eq!(saved.root_version, before.root_version);
            assert_eq!(saved.devices, before.devices);
            Ok(())
        };
        future::or(restored.run(|_| {}), future::or(applying, testing))
            .await
            .unwrap();
    }));
}

#[test]
fn fresh_adapter_restores_changed_member_profile_and_downgraded_group() {
    let rig = repair_rig(0, true);
    block_on(rig.run(|c| async move {
        repair_ready(&c).await;
        until(|| c.context.group_repair("group-1").is_some()).await;
        let instance: Value = serde_json::from_str(include_str!(
            "../../../../tests/fixtures/miot/yeelink-light-light3.json"
        ))
        .unwrap();
        let native = confirmed_for(&c, "member-a", "yeelink.light.light3", &instance);
        {
            let mut live = c.context.live.borrow_mut();
            let member = live.devices.get_mut("member-a").unwrap();
            member.latest = Some(native.clone());
            member.latest = Some(native);
        }
        c.context.reconcile("member-a").unwrap();
        c.context.stage();
        let key = DeviceKey::new("xiaomi", "group-1");
        until(|| {
            c.publication
                .current()
                .published
                .get(&key)
                .is_some_and(|group| !group.functions()[0].description().supports_rgb())
        })
        .await;
        let before = c.store.devices().load_configuration().unwrap();
        let (restored, ready) = XiaomiAdapter::build(
            c.store.clone(),
            c.context.cloud.clone(),
            c.discovery.clone(),
            c.clock.clone(),
        )
        .unwrap();
        ready.wait().await.unwrap();
        let registry = restored.context.registry.clone();
        let member = registry.get(&DeviceKey::new("xiaomi", "member-a")).unwrap();
        assert!(
            !member.functions()[0].description().supports_rgb(),
            "the committed native profile is restored without a stale logical mirror"
        );
        assert!(
            restored.context.live.borrow().devices["member-a"]
                .blocked
                .is_none(),
            "the valid committed native description is not blocked on restart"
        );
        assert_eq!(member.functions()[0].snapshot().power, Power::Unknown);

        let publication = App::open(restored.source_handle(), c.store.clone()).unwrap();
        if let Some(plan) = publication.plan().unwrap() {
            assert!(
                !plan.rebuild,
                "restart must not replace the committed member or widen the group range"
            );
            publication.apply(publication.prepare(plan)).unwrap();
        }
        assert!(restored.context.group_repair("group-1").is_none());
        let after = c.store.devices().load_configuration().unwrap();
        assert_eq!(after.root_version, before.root_version);
        assert_eq!(after.devices, before.devices);
        assert_eq!(after.groups, before.groups);
    }));
}
