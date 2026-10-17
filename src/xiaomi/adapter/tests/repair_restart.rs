use super::{
    repair::{confirmed_for, repair_ready, repair_rig},
    *,
};
use crate::{
    device::{Capability, LightColorTarget, LightDimension, LightTarget, Rgb},
    publication::Publication,
};

async fn apply_restored(publication: &Publication) -> Result<(), AdapterError> {
    loop {
        let changed = publication.listen();
        if let Some(prepared) = publication.prepare()? {
            if prepared.rebuild {
                publication.stop_actions();
            }
            publication.apply(prepared)?;
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
        let scope = c.context.live.borrow().scope.clone().unwrap();
        let before = c.store.devices().load_publication(Some(&scope)).unwrap();
        let key = DeviceKey::new("xiaomi", "group-1");
        let original = c.context.registry.get(&key).unwrap();
        let (restored, ready) = XiaomiAdapter::build(
            c.store.clone(),
            c.context.cloud.clone(),
            c.discovery.clone(),
            c.clock.clone(),
        )
        .unwrap();
        let registry = ready.wait().await.unwrap();
        let group = registry.get(&key).unwrap();
        assert!(!original.same_instance(&group));
        let light = group.function("main").unwrap();
        assert!(light.description().has_capability(Capability::ColorRgb));
        let initial = light.snapshot();
        assert_eq!(initial.power, Power::Unknown);
        let color = initial.lighting.unwrap();
        assert_eq!(color.color, None);
        assert_eq!(color.mode, None);
        assert_eq!(color.temperature_kelvin, None);

        let context = restored.context.clone();
        let publication =
            Rc::new(Publication::open(restored.source_handle(), c.store.clone()).unwrap());
        assert!(context.group_repair("group-1").is_none());
        let prepared = publication
            .prepare()
            .unwrap()
            .expect("restore must install the reconstructed plan");
        assert!(
            !prepared.rebuild,
            "restoring the same fixed configuration needs no rebuild"
        );
        publication.apply(prepared).unwrap();
        assert!(context.group_repair("group-1").is_some());
        let after = c.store.devices().load_publication(Some(&scope)).unwrap();
        assert_eq!(after.root_version, before.root_version);
        assert_eq!(after.records, before.records);
        for native in &after.cached {
            if native.device.key == key {
                assert!(
                    native.device.functions[0]
                        .lighting
                        .as_ref()
                        .unwrap()
                        .color
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
                .begin_light_action(
                    LightDimension::Color,
                    LightTarget {
                        color: Some(LightColorTarget::Rgb(target)),
                        ..Default::default()
                    },
                    true,
                )
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
            let saved = c.store.devices().load_publication(Some(&scope)).unwrap();
            assert_eq!(saved.root_version, before.root_version);
            assert_eq!(saved.records, before.records);
            Ok(())
        };
        future::or(restored.run(|_| {}), future::or(applying, testing))
            .await
            .unwrap();
    }));
}

#[test]
fn fresh_adapter_keeps_downgraded_native_input_and_static_member_unavailable() {
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
            member.native_confirmed = Some(native.clone());
            member.candidate = Some((native, None));
        }
        c.context.reconcile("member-a").unwrap();
        c.context.stage();
        let key = DeviceKey::new("xiaomi", "group-1");
        until(|| {
            c.publication
                .current()
                .published
                .get(&key)
                .is_some_and(|group| {
                    !group.functions()[0]
                        .description()
                        .has_capability(Capability::ColorRgb)
                })
        })
        .await;
        let scope = c.context.live.borrow().scope.clone().unwrap();
        let before = c.store.devices().load_publication(Some(&scope)).unwrap();
        let (restored, ready) = XiaomiAdapter::build(
            c.store.clone(),
            c.context.cloud.clone(),
            c.discovery.clone(),
            c.clock.clone(),
        )
        .unwrap();
        let registry = ready.wait().await.unwrap();
        let member = registry.get(&DeviceKey::new("xiaomi", "member-a")).unwrap();
        assert!(
            member.functions()[0]
                .description()
                .has_capability(Capability::ColorRgb),
            "the reserved independent description stays static"
        );
        assert!(
            restored.context.live.borrow().devices["member-a"]
                .blocked
                .is_some(),
            "bootstrap must preserve static incompatibility from genuine Native vs logical data"
        );
        assert!(member.functions()[0].execute(Command::On).await.is_err());

        let publication = Publication::open(restored.source_handle(), c.store.clone()).unwrap();
        if let Some(prepared) = publication.prepare().unwrap() {
            assert!(
                !prepared.rebuild,
                "restart must not migrate the static member or widen the group range"
            );
            publication.apply(prepared).unwrap();
        }
        assert!(restored.context.group_repair("group-1").is_none());
        let after = c.store.devices().load_publication(Some(&scope)).unwrap();
        assert_eq!(after.root_version, before.root_version);
        assert_eq!(after.records, before.records);
        assert_eq!(after.cached, before.cached);
    }));
}
