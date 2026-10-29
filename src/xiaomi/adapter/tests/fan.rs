use super::*;
use crate::{device::FanSnapshot, xiaomi::capability::PropertyBinding};

const POWER: PropertyBinding = PropertyBinding { siid: 2, piid: 1 };
const MODE: PropertyBinding = PropertyBinding { siid: 2, piid: 3 };
const SWING: PropertyBinding = PropertyBinding { siid: 2, piid: 4 };
const SPEED: PropertyBinding = PropertyBinding { siid: 8, piid: 1 };
fn rig() -> Rig {
    let instance = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/miot/dmaker-p5c.json"
    ))
    .unwrap();
    Rig::custom_lights(&[("one", "dmaker.fan.p5c", instance)])
}
fn fan(c: &Control) -> FanSnapshot {
    function(&c.context.registry, "one", "fan:2")
        .snapshot()
        .fan()
        .unwrap()
}
fn work(c: &Control) -> Rc<super::work::DeviceWork> {
    c.context.live.borrow().devices["one"]
        .admitted
        .as_ref()
        .unwrap()
        .clone()
}
fn peer() -> Rc<Peer> {
    let peer = Rc::new(Peer::default());
    for (binding, value) in [
        (POWER, json!(true)),
        (MODE, json!(1)),
        (SPEED, json!(40)),
        (SWING, json!(false)),
    ] {
        peer.value("one", binding, value);
    }
    peer
}
mod commands;
mod failures;
mod freshness;
mod lifecycle;

fn cloud_rig() -> (Rig, Async<TcpListener>, Rc<CloudPeer>) {
    use crate::xiaomi::adapter::notifications::NotificationConnector;
    let rig = rig();
    let listener = Async::<TcpListener>::bind(([127, 0, 0, 1], 0)).unwrap();
    rig.source
        .context
        .notification_connector
        .set(NotificationConnector::Plaintext(
            listener.get_ref().local_addr().unwrap(),
        ));
    {
        let mut cloud = rig.cloud.lock().unwrap();
        cloud.online.insert("one".into());
        for (binding, value) in [
            (POWER, json!(false)),
            (MODE, json!(1)),
            (SPEED, json!(40)),
            (SWING, json!(false)),
        ] {
            cloud
                .raw_values
                .insert(("one".into(), binding.siid, binding.piid), value);
        }
    }
    (rig, listener, Rc::new(CloudPeer::default()))
}
#[test]
fn fan_reads_and_subscribes_each_independent_native_property() {
    block_on(rig().run(|c| async move {
        c.announce();
        let control = &c;
        c.peer(peer(), |peer| async move {
            until(|| {
                let value = fan(control);
                value.power.value == Some(true)
                    && work(control).fan_mode_observation("fan:2").value == Some(1)
                    && value.speed.value == Some(40)
                    && value.swing.value == Some(false)
                    && work(control).function_observer("fan:2").is_some()
            })
            .await;
            let subscriptions = work(control).subscription_status("fan:2");
            for binding in [POWER, MODE, SPEED, SWING] {
                assert!(
                    subscriptions
                        .iter()
                        .any(|item| item.source == PathKind::Local
                            && item.binding
                                == super::model::SubscriptionBinding::Property(binding)
                            && item.ready)
                );
            }
            assert_eq!(
                subscriptions
                    .iter()
                    .filter(|item| item.source == PathKind::Local)
                    .count(),
                4
            );
            let function = function(&control.context.registry, "one", "fan:2");
            let revision = function.snapshot().revision;
            peer.notify_value_at("one", SPEED, json!(40), Some(700), false);
            until(|| fan(control).speed.source_time == Some(700)).await;
            assert_eq!(function.snapshot().revision, revision);
            peer.notify_value_at("one", MODE, json!(0), Some(701), false);
            until(|| work(control).fan_mode_observation("fan:2").value == Some(0)).await;
            assert_eq!(fan(control).speed.value, Some(40));
            assert!(
                !peer
                    .state
                    .borrow()
                    .seen
                    .iter()
                    .any(|(op, _)| op == "rpcReq")
            );
        })
        .await;
    }));
}
#[test]
fn fan_notification_wins_over_an_older_read() {
    block_on(rig().run(|c| async move {
        c.announce();
        let control = &c;
        c.peer(peer(), |peer| async move {
            until(|| {
                fan(control).speed.value == Some(40)
                    && work(control).function_observer("fan:2").is_some()
            })
            .await;
            peer.state
                .borrow_mut()
                .read_timeout
                .insert(("one".into(), 8, 1));
            work(control).refresh_property("fan:2", SPEED);
            until(|| {
                peer.state.borrow().held.iter().any(|(publish, _)| {
                    let payload = decode_mips(&publish.payload).unwrap().payload;
                    payload["siid"] == 8 && payload["piid"] == 1
                })
            })
            .await;
            peer.notify_value_at("one", SPEED, json!(64), Some(401), false);
            until(|| fan(control).speed.value == Some(64)).await;
            peer.release_property("one", SPEED, json!({"value": 20, "updateTime": 1}))
                .await;
            until(|| !work(control).refresh.borrow().contains("fan:2", SPEED)).await;
            assert_eq!(fan(control).speed.value, Some(64));
        })
        .await;
    }));
}

#[test]
fn fan_recovery_reads_all_fields_and_rejects_retired_sources() {
    block_on(rig().run(|c| async move {
        c.announce();
        let old_source = Rc::new(RefCell::new(None));
        let captured = old_source.clone();
        let control = &c;
        c.peer(peer(), |_| async move {
            until(|| {
                fan(control).power.value == Some(true)
                    && work(control).fan_mode_observation("fan:2").value == Some(1)
                    && fan(control).speed.value == Some(40)
                    && fan(control).swing.value == Some(false)
                    && work(control).function_observer("fan:2").is_some()
            })
            .await;
            *captured.borrow_mut() = work(control).function_observer("fan:2");
        })
        .await;
        until(|| c.context.live.borrow().session.is_none()).await;
        let previous = fan(&c);
        assert_eq!(previous.speed.value, Some(40));
        let replacement = peer();
        for (binding, value) in [
            (POWER, json!(false)),
            (MODE, json!(0)),
            (SPEED, json!(72)),
            (SWING, json!(true)),
        ] {
            replacement.value("one", binding, value);
        }
        let control = &c;
        c.peer(replacement, |peer| async move {
            until(|| {
                fan(control).power.value == Some(false)
                    && work(control).fan_mode_observation("fan:2").value == Some(0)
                    && fan(control).speed.value == Some(72)
                    && fan(control).swing.value == Some(true)
                    && work(control).function_observer("fan:2").is_some()
            })
            .await;
            let observed = fan(control);
            control.context.property_value_event(
                old_source.borrow().as_ref().unwrap(),
                "one",
                SPEED,
                &json!(99),
                Some(999),
                false,
            );
            assert_eq!(fan(control), observed);
            work(control).retire();
            peer.notify_value_at("one", SPEED, json!(98), Some(1000), false);
            for _ in 0..50 {
                future::yield_now().await;
            }
            assert_eq!(fan(control), observed);
            assert!(
                !peer
                    .state
                    .borrow()
                    .seen
                    .iter()
                    .any(|(op, _)| op == "rpcReq")
            );
        })
        .await;
    }));
}

#[test]
fn fan_pushless_device_keeps_local_reads_and_uses_cloud_notifications() {
    use crate::xiaomi::adapter::notifications::{NotificationConnector, NotificationSource};
    let rig = rig();
    let listener = Async::<TcpListener>::bind(([127, 0, 0, 1], 0)).unwrap();
    rig.source
        .context
        .notification_connector
        .set(NotificationConnector::Plaintext(
            listener.get_ref().local_addr().unwrap(),
        ));
    {
        let mut http = rig.cloud.lock().unwrap();
        http.online.insert("one".into());
        for (binding, value) in [
            (POWER, json!(false)),
            (MODE, json!(0)),
            (SPEED, json!(75)),
            (SWING, json!(true)),
        ] {
            http.raw_values
                .insert(("one".into(), binding.siid, binding.piid), value);
        }
    }
    let cloud = Rc::new(CloudPeer::default());
    block_on(rig.run(|c| async move {
        future::or(cloud.run(&listener), async {
            c.announce();
            let peer = peer();
            peer.state.borrow_mut().pushless.insert("one".into());
            let control = &c;
            let cloud = &cloud;
            c.peer(peer, |peer| async move {
                until(|| {
                    matches!(
                        work(control).function_observer("fan:2"),
                        Some(NotificationSource::Cloud(_))
                    ) && fan(control).speed.value == Some(40)
                })
                .await;
                let diagnostic = control.context.diagnostics();
                let status = diagnostic
                    .devices
                    .iter()
                    .find(|device| device.did == "one")
                    .unwrap();
                assert_eq!(status.functions[0].selected_read, Some(PathKind::Local));
                assert_eq!(
                    status.functions[0].notification_source,
                    Some(PathKind::Cloud)
                );
                cloud.property_at("one", 8, 1, json!(62));
                until(|| fan(control).speed.value == Some(62)).await;
                peer.notify_value_at("one", SPEED, json!(99), Some(900), false);
                cloud.property_at("one", 2, 3, json!(0));
                until(|| work(control).fan_mode_observation("fan:2").value == Some(0)).await;
                assert_eq!(fan(control).speed.value, Some(62));
                assert_eq!(
                    work(control).property_source("fan:2", SPEED),
                    Some(PathKind::Cloud)
                );
            })
            .await;
        })
        .await;
    }));
}

#[test]
fn fan_partial_baseline_and_terminal_show_per_property_diagnostics() {
    block_on(rig().run(|c| async move {
        c.announce();
        let peer = peer();
        peer.state
            .borrow_mut()
            .read_timeout
            .insert(("one".into(), 8, 1));
        let control = &c;
        c.peer(peer, |peer| async move {
            until(|| {
                fan(control).power.value == Some(true)
                    && work(control).fan_mode_observation("fan:2").value == Some(1)
                    && peer.state.borrow().held.iter().any(|(publish, _)| {
                        let payload = decode_mips(&publish.payload).unwrap().payload;
                        payload["siid"] == 8 && payload["piid"] == 1
                    })
            })
            .await;
            assert_eq!(fan(control).speed.value, None);
            assert_eq!(fan(control).swing.value, None);
            let status = crate::terminal::AuthStatus::new(
                control.context.status.borrow().authentication.clone(),
                "/bin/migate".into(),
                "/data".into(),
            );
            status.update_adapter(control.context.diagnostics());
            let result =
                crate::terminal::bridge::Context::committed(control.publication.clone(), &status)
                    .handle_line("status one", 0)
                    .await
                    .unwrap()
                    .unwrap();
            for expected in [
                "Power: on",
                "Native mode: 1",
                "Speed: unknown",
                "Horizontal swing: unknown",
                "8.1",
                "Category: fan",
                "fan:2: core",
                "dmaker.fan.straight-wind",
            ] {
                assert!(result.contains(expected), "missing {expected}: {result}");
            }
            peer.release_property("one", SPEED, json!({"value": 40, "updateTime": 30}))
                .await;
            until(|| {
                fan(control).speed.value == Some(40) && fan(control).swing.value == Some(false)
            })
            .await;
        })
        .await;
    }));
}

#[test]
fn fan_failed_property_read_keeps_last_value_and_reports_its_own_problem() {
    block_on(rig().run(|c| async move {
        c.announce();
        let control = &c;
        c.peer(peer(), |peer| async move {
            until(|| {
                fan(control).speed.value == Some(40)
                    && work(control).function_observer("fan:2").is_some()
                    && !work(control).refresh.borrow().contains("fan:2", SPEED)
            })
            .await;
            peer.notify_value_at("one", SPEED, json!(101), Some(801), false);
            peer.state
                .borrow_mut()
                .read_fail
                .insert(("one".into(), 8, 1));
            work(control).refresh_property("fan:2", SPEED);
            work(control)
                .refresh
                .borrow_mut()
                .exhaust_pending("fan:2", SPEED);
            until(|| {
                work(control)
                    .refresh
                    .borrow()
                    .property_problem("fan:2", SPEED)
                    .is_some()
            })
            .await;
            assert_eq!(fan(control).speed.value, Some(40));
            assert_ne!(fan(control).speed.source_time, Some(801));
            assert!(
                work(control)
                    .refresh
                    .borrow()
                    .property_problem("fan:2", MODE)
                    .is_none()
            );
            let status = crate::terminal::AuthStatus::new(
                control.context.status.borrow().authentication.clone(),
                "/bin/migate".into(),
                "/data".into(),
            );
            status.update_adapter(control.context.diagnostics());
            for command in ["list", "status one"] {
                let text = crate::terminal::bridge::Context::committed(
                    control.publication.clone(),
                    &status,
                )
                .handle_line(command, 0)
                .await
                .unwrap()
                .unwrap();
                let line = text
                    .lines()
                    .find(|line| line.contains("Speed: level 40/100"))
                    .unwrap();
                for expected in [
                    "property 8.1",
                    "observation source local",
                    "read: one read 8.1",
                    "local subscription ready",
                ] {
                    assert!(line.contains(expected), "missing {expected}: {line}");
                }
            }
            peer.state.borrow_mut().read_fail.clear();
            work(control).refresh_property("fan:2", SPEED);
            until(|| {
                work(control)
                    .refresh
                    .borrow()
                    .property_problem("fan:2", SPEED)
                    .is_none()
            })
            .await;
            assert_eq!(fan(control).speed.value, Some(40));
        })
        .await;
    }));
}
