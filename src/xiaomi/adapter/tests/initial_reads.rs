use super::*;
use crate::xiaomi::capability::PropertyBinding;

const POWER: PropertyBinding = PropertyBinding { siid: 2, piid: 1 };

fn work(context: &super::super::state::Context, did: &str) -> Rc<super::super::work::DeviceWork> {
    context.live.borrow().devices[did].admitted.clone().unwrap()
}

#[test]
fn initial_notification_or_failure_wakes_only_the_initial_waiter() {
    // None represents a failed read; both kinds of notification are valid.
    for retained in [Some(false), Some(true), None] {
        let rig = Rig::new(true, &["one"]);
        let configuration = rig.store.devices().load_configuration().unwrap();
        let handle = rig.source.source_handle();
        let device = work(&rig.source.context, "one");
        block_on(async {
            let mut initial = Box::pin(handle.listen_initial_reads());
            let mut refresh = Box::pin(rig.source.context.refresh_changed.listen());
            assert!(!handle.initial_reads_ready(&configuration));
            assert!(poll_once(initial.as_mut()).await.is_none());
            assert!(poll_once(refresh.as_mut()).await.is_none());

            let expected = if let Some(retained) = retained {
                device.notify_value_at(POWER, &json!("invalid"), None, retained, false, 1);
                assert!(!handle.initial_reads_ready(&configuration));
                assert!(poll_once(initial.as_mut()).await.is_none());
                device.notify_value_at(POWER, &json!(true), None, retained, false, 1);
                Power::On
            } else {
                device
                    .refresh
                    .borrow_mut()
                    .fail_initial_for_test("outlet:2", POWER);
                Power::Unknown
            };
            assert!(poll_once(initial.as_mut()).await.is_some());
            assert!(poll_once(refresh.as_mut()).await.is_none());
            assert_eq!(
                device.physical().functions()[0].snapshot().power(),
                Some(expected)
            );
            assert!(handle.initial_reads_ready(&configuration));
        });
    }
}

#[test]
fn retained_air_conditioner_values_satisfy_only_their_real_bindings() {
    let instance: Value = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/miot/lumi-mcn04.json"
    ))
    .unwrap();
    let rig = Rig::custom_lights(&[("one", "generic.ac.model", instance)]);
    let configuration = rig.store.devices().load_configuration().unwrap();
    let source = rig.source.source_handle();
    let device = work(&rig.source.context, "one");
    let function = device
        .native
        .device()
        .functions()
        .iter()
        .find(|f| f.air_conditioner().is_some())
        .unwrap();
    let values = [
        (PropertyBinding { siid: 3, piid: 1 }, json!(true)),
        (PropertyBinding { siid: 3, piid: 2 }, json!(0)),
        (PropertyBinding { siid: 3, piid: 4 }, json!(24.0)),
        (PropertyBinding { siid: 4, piid: 2 }, json!(1)),
        (PropertyBinding { siid: 4, piid: 4 }, json!(false)),
    ];
    for (index, (binding, value)) in values.iter().enumerate() {
        let decoded =
            super::super::work::DeviceWork::decode_air_conditioner(function, *binding, value)
                .unwrap();
        device.observe_air_conditioner(
            function,
            *binding,
            decoded,
            Observation {
                source: PathKind::Local,
                source_time: None,
                received_at: 1,
            },
            Origin::Cache,
        );
        assert_eq!(
            source.initial_reads_ready(&configuration),
            index + 1 == values.len()
        );
    }
}

#[test]
fn notification_can_complete_the_initial_read_while_a_cloud_request_is_in_flight() {
    let rig = Rig::new(true, &["one"]);
    let (release, gate) = flume::bounded(1);
    {
        let mut cloud = rig.cloud.lock().unwrap();
        cloud.online.insert("one".into());
        cloud.read_gates.insert(("one".into(), 2, 1), gate);
    }
    block_on(rig.run(|c| async move {
        until(|| !c.cloud.lock().unwrap().property_reads.is_empty()).await;
        let source = super::super::SourceHandle(c.context.clone());
        let configuration = c.store.devices().load_configuration().unwrap();
        let device = work(&c.context, "one");
        assert!(!source.initial_reads_ready(&configuration));
        device.notify_value_at(POWER, &json!(true), None, false, false, 1);
        assert!(source.initial_reads_ready(&configuration));
        assert!(device.refresh.borrow().contains("outlet:2", POWER));
        release.send(()).unwrap();
        until(|| device.refresh.borrow().snapshot().inflight.is_empty()).await;
        assert_eq!(
            device.physical().functions()[0].snapshot().power(),
            Some(Power::On)
        );
    }));
}

#[test]
fn first_cloud_read_failure_unblocks_without_waiting_for_its_retry() {
    let rig = Rig::new(true, &["one"]);
    rig.clock
        .automatic
        .store(false, std::sync::atomic::Ordering::Relaxed);
    {
        let mut cloud = rig.cloud.lock().unwrap();
        cloud.online.insert("one".into());
        cloud.read_status = 503;
    }
    block_on(rig.run(|c| async move {
        let handle = c.context.clone();
        let source = super::super::SourceHandle(handle);
        let configuration = c.store.devices().load_configuration().unwrap();
        assert!(!source.initial_reads_ready(&configuration));
        until(|| c.context.cloud_candidate("one", true).is_ok()).await;
        c.clock.advance(Duration::from_millis(201));
        let device = work(&c.context, "one");
        until(|| {
            device
                .refresh
                .borrow()
                .property_problem("outlet:2", POWER)
                .is_some()
        })
        .await;
        assert!(source.initial_reads_ready(&configuration));
        assert!(device.refresh.borrow().contains("outlet:2", POWER));
        assert_eq!(
            device.physical().functions()[0].snapshot().power(),
            Some(Power::Unknown)
        );
        assert!(!c.cloud.lock().unwrap().property_reads.is_empty());
    }));
}

#[test]
fn cloud_offline_result_counts_before_it_retires_the_read_path() {
    let rig = Rig::new(true, &["one"]);
    rig.clock
        .automatic
        .store(false, std::sync::atomic::Ordering::Relaxed);
    {
        let mut cloud = rig.cloud.lock().unwrap();
        cloud.online.insert("one".into());
        cloud.read_codes.insert(("one".into(), 2, 1), -704_042_011);
    }
    block_on(rig.run(|c| async move {
        let source = super::super::SourceHandle(c.context.clone());
        let configuration = c.store.devices().load_configuration().unwrap();
        assert!(!source.initial_reads_ready(&configuration));
        until(|| c.context.cloud_candidate("one", true).is_ok()).await;
        c.clock.advance(Duration::from_millis(201));
        until(|| source.initial_reads_ready(&configuration)).await;
        assert!(
            !c.context.live.borrow().devices["one"]
                .cloud_access
                .as_ref()
                .unwrap()
                .online
        );
        assert_eq!(
            work(&c.context, "one").physical().functions()[0]
                .snapshot()
                .power(),
            Some(Power::Unknown)
        );
    }));
}

#[test]
fn native_and_logical_replacements_need_their_own_initial_observations() {
    let rig = Rig::new(true, &["one"]);
    let configuration = rig.store.devices().load_configuration().unwrap();
    let handle = rig.source.source_handle();
    let old = work(&rig.source.context, "one");
    old.notify_value_at(POWER, &json!(true), None, false, false, 1);
    assert!(handle.initial_reads_ready(&configuration));
    let (physical, _) = crate::device::PhysicalDevice::new(old.native.physical_description("one"));
    let replacement = Rc::new(super::super::work::DeviceWork::new(
        old.native.clone(),
        Rc::new(super::super::state::LogicalSink::new(physical)),
        rig.source.context.refresh_changed.clone(),
        rig.source.context.initial_reads_changed.clone(),
        None,
    ));
    old.retire();
    rig.source
        .context
        .live
        .borrow_mut()
        .devices
        .get_mut("one")
        .unwrap()
        .admitted = Some(replacement.clone());
    assert!(!handle.initial_reads_ready(&configuration));
    old.notify_value_at(POWER, &json!(false), None, false, false, 2);
    assert!(!handle.initial_reads_ready(&configuration));
    replacement.notify_value_at(POWER, &json!(false), None, false, false, 3);
    assert!(handle.initial_reads_ready(&configuration));
    let (physical, _) =
        crate::device::PhysicalDevice::new(replacement.native.physical_description("one"));
    replacement.replace_sink(Rc::new(super::super::state::LogicalSink::new(physical)));
    assert!(!handle.initial_reads_ready(&configuration));
    replacement.notify_value_at(POWER, &json!(true), None, false, false, 4);
    assert!(handle.initial_reads_ready(&configuration));
    assert_eq!(
        replacement.physical().functions()[0].snapshot().power(),
        Some(Power::On)
    );
}

#[test]
fn retiring_a_waiting_source_wakes_the_initial_read_waiter() {
    let rig = Rig::new(true, &["one"]);
    let configuration = rig.store.devices().load_configuration().unwrap();
    let handle = rig.source.source_handle();
    let device = work(&rig.source.context, "one");
    block_on(async {
        let mut listener = Box::pin(handle.listen_initial_reads());
        assert!(!handle.initial_reads_ready(&configuration));
        assert!(poll_once(listener.as_mut()).await.is_none());
        device.retire();
        assert!(poll_once(listener.as_mut()).await.is_some());
        assert!(handle.initial_reads_ready(&configuration));
    });
}

#[test]
fn shared_battery_is_required_once_alongside_published_sensor_functions() {
    let rig = Rig::sensors(&["sensor"]);
    let configuration = rig.store.devices().load_configuration().unwrap();
    let handle = rig.source.source_handle();
    let device = work(&rig.source.context, "sensor");
    let temperature = PropertyBinding {
        siid: 3,
        piid: 1001,
    };
    let humidity = PropertyBinding {
        siid: 3,
        piid: 1008,
    };
    let battery = PropertyBinding {
        siid: 2,
        piid: 1003,
    };
    device.notify_value_at(temperature, &json!(22.0), None, false, false, 1);
    device.notify_value_at(humidity, &json!(55.0), None, false, false, 1);
    assert!(!handle.initial_reads_ready(&configuration));
    device.notify_value_at(battery, &json!(72), None, true, false, 1);
    assert_eq!(
        device.physical().battery_snapshot().unwrap().value,
        Some(72.0)
    );
    assert!(handle.initial_reads_ready(&configuration));
}

#[test]
fn group_waits_for_its_actual_member_sources_without_unrelated_hidden_brightness() {
    for physical in [false, true] {
        let rig = repair_rig(1, true);
        rig.cloud.lock().unwrap().catalog_groups[0]["extra"]["member_list"]["is_physical"] =
            json!(physical);
        block_on(rig.run(|c| async move {
            until(|| {
                c.context
                    .graph
                    .borrow()
                    .rules
                    .get("group-1")
                    .is_some_and(|rule| rule.is_physical == physical && rule.members.len() == 2)
            })
            .await;
            let configuration = c.store.devices().load_configuration().unwrap();
            let (restored, _) = XiaomiAdapter::build(
                c.store.clone(),
                c.context.cloud.clone(),
                c.discovery.clone(),
                c.clock.clone(),
            )
            .unwrap();
            let source = restored.source_handle();
            assert!(!source.initial_reads_ready(&configuration));
            if physical {
                let group = work(&restored.context, "group-1");
                for (binding, value) in [
                    (POWER, json!(true)),
                    (PropertyBinding { siid: 2, piid: 2 }, json!(50)),
                    (PropertyBinding { siid: 2, piid: 3 }, json!(2700)),
                ] {
                    group.notify_value_at(binding, &value, None, false, false, 1);
                }
            }
            for did in ["member-a", "member-b"] {
                let member = work(&restored.context, did);
                for (binding, value) in [
                    (POWER, json!(true)),
                    (PropertyBinding { siid: 2, piid: 3 }, json!(2700)),
                    (MEMBER_COLOR, json!(0x112233)),
                    (MEMBER_MODE, json!(1)),
                ] {
                    member.notify_value_at(binding, &value, None, false, false, 1);
                }
            }
            if !physical {
                assert!(!source.initial_reads_ready(&configuration));
                for did in ["member-a", "member-b"] {
                    work(&restored.context, did).notify_value_at(
                        PropertyBinding { siid: 2, piid: 2 },
                        &json!(50),
                        None,
                        false,
                        false,
                        1,
                    );
                }
            }
            assert!(source.initial_reads_ready(&configuration));
        }));
    }
}
