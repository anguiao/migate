use super::*;

#[test]
fn terminal_controls_hidden_member_without_publishing_or_redirecting_it() {
    let rig = repair_rig(0, true);
    block_on(rig.run(|c| async move {
        repair_ready(&c).await;
        let member = DeviceKey::new("xiaomi", "member-a");
        let current = c.publication.current();
        assert!(!current.derived.published.contains(&member));
        assert!(current.derived.visible_functions(&member).is_empty());
        let before = current.configuration.clone();
        let status = crate::terminal::AuthStatus::new(
            c.context.status.borrow().authentication.clone(),
            "/bin/migate".into(),
            "/data".into(),
        );
        let terminal = crate::terminal::bridge::Context::committed(c.publication.clone(), &status);
        let light = function(&c.context.registry, "member-a", "light:2");
        for command in ["list", "status member-a"] {
            let report = terminal.handle_line(command, 0).await.unwrap().unwrap();
            assert!(report.contains("Matter: hidden or reserved;"), "{report}");
            assert!(
                report.contains(&format!("light:2 ({}): on", light.description().label)),
                "{report}"
            );
            assert!(report.contains("Brightness: 50%"), "{report}");
        }
        c.cloud.lock().unwrap().numeric_writes.clear();
        for command in ["off member-a light:2", "on member-a light:2"] {
            assert_eq!(
                terminal.handle_line(command, 0).await.unwrap().unwrap(),
                "member-a: command accepted"
            );
        }
        let writes = c
            .cloud
            .lock()
            .unwrap()
            .numeric_writes
            .iter()
            .flat_map(|(items, _)| items.clone())
            .collect::<Vec<_>>();
        assert_eq!(writes.len(), 2);
        for (write, target) in writes.iter().zip([false, true]) {
            assert_eq!(write["did"], "member-a");
            assert_eq!(write["siid"], 2);
            assert_eq!(write["piid"], 1);
            assert_eq!(write["value"], target);
        }
        let after = c.publication.current();
        assert_eq!(after.configuration, before);
        assert!(!after.derived.published.contains(&member));
        assert!(after.derived.visible_functions(&member).is_empty());
    }));
}

#[test]
fn virtual_group_native_writes_refresh_actual_member_values_and_mode() {
    let rig = super::super::support::repair_rig(1, true);
    block_on(rig.run(|c| async move {
        super::super::support::repair_ready(&c).await;
        let light = function(&c.context.registry, "group-1", "light:2");
        // Native control acceptance does not manufacture actual state. Reads
        // must use the members, including mode when a CT command selects white.
        for (dimension, target, values) in [
            (None, None, vec![(2, 1, json!(false))]),
            (
                Some(crate::device::LightDimension::Brightness),
                Some(
                    crate::device::LightTarget::brightness(None, 70.0).expect("valid test target"),
                ),
                vec![(2, 2, json!(60))],
            ),
            (
                Some(crate::device::LightDimension::Color),
                Some(
                    crate::device::LightTarget::temperature(None, 4000.0)
                        .expect("valid test target"),
                ),
                vec![(2, 3, json!(3800)), (4, 2, json!(0))],
            ),
        ] {
            until(|| {
                c.context
                    .live
                    .borrow()
                    .devices
                    .values()
                    .filter_map(|slot| slot.admitted.as_ref())
                    .all(|work| {
                        work.refresh.borrow().snapshot().pending.is_empty()
                            && work.refresh.borrow().snapshot().inflight.is_empty()
                    })
            })
            .await;
            {
                let mut cloud = c.cloud.lock().unwrap();
                cloud.property_reads.clear();
                for did in ["member-a", "member-b"] {
                    for (siid, piid, value) in &values {
                        cloud
                            .raw_values
                            .insert((did.into(), *siid, *piid), value.clone());
                    }
                }
            }
            if let Some(target) = target {
                light
                    .begin_light_action(target, true)
                    .unwrap()
                    .accepted()
                    .await
                    .unwrap();
            } else {
                light.execute(Command::Off).await.unwrap();
            }
            until(|| {
                let cloud = c.cloud.lock().unwrap();
                ["member-a", "member-b"].iter().all(|did| {
                    values.iter().all(|(siid, piid, _)| {
                        cloud
                            .property_reads
                            .contains(&(did.to_string(), *siid, *piid))
                    })
                })
            })
            .await;
            assert!(
                c.cloud
                    .lock()
                    .unwrap()
                    .property_reads
                    .iter()
                    .all(|(did, _, _)| did != "group-1")
            );
            until(|| match dimension {
                None => light.snapshot().power() == Some(Power::Off),
                Some(crate::device::LightDimension::Brightness) => {
                    light.snapshot().lighting().unwrap().brightness_percent == Some(60.0)
                }
                Some(crate::device::LightDimension::Color) => {
                    let state = light.snapshot().lighting().unwrap();
                    state.temperature_kelvin == Some(3800)
                        && state.mode == Some(crate::device::LightMode::Temperature)
                }
            })
            .await;
        }
        assert!(
            c.cloud
                .lock()
                .unwrap()
                .numeric_writes
                .iter()
                .all(|(items, _)| items.iter().all(|item| item["did"] == "group-1"))
        );
    }));
}
