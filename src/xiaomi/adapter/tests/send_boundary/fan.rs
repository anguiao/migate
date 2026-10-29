use super::*;
use crate::device::{CommandError, FanCommand, FanValue};
use crate::xiaomi::adapter::PathKind;

fn fan_rig() -> Rig {
    let description = serde_json::from_str(include_str!(
        "../../../../../tests/fixtures/miot/dmaker-p5c.json"
    ))
    .unwrap();
    Rig::custom_lights(&[("one", "dmaker.fan.p5c", description)])
}

#[test]
fn route_change_reselects_only_an_unsent_step_and_records_each_path() {
    for sent in [false, true] {
        let rig = fan_rig();
        rig.cloud.lock().unwrap().online.insert("one".into());
        block_on(rig.run(|c| async move {
            until(|| c.context.cloud_scope_valid.get()).await;
            let wire = Rc::new(Wire::default());
            if sent {
                wire.allow(1);
            }
            let (handle, driver) = local(&c, wire.clone());
            let fan = function(&c.context.registry, "one", "main");
            future::or(
                async {
                    driver.run(|_| {}).await.unwrap();
                    panic!("driver stopped");
                },
                async {
                    let action = fan.begin_fan(FanCommand::Speed(52)).unwrap();
                    if sent {
                        until(|| wire.written.borrow().len() == 1).await;
                    } else {
                        until(|| wire.polls.get() > 0).await;
                    }
                    c.context.local_outcome(&handle, &response_timeout());
                    wire.allow(usize::MAX);
                    if sent {
                        until(|| wire.written.borrow().len() > 1).await;
                        wire.reply(json!({"result":[{"did":"one","siid":2,"piid":1,"code":0}]}));
                    }
                    action.accepted().await.unwrap();
                    let cloud = c.cloud.lock().unwrap();
                    let actual = cloud
                        .numeric_writes
                        .iter()
                        .map(|(items, _)| {
                            (
                                items[0]["siid"].clone(),
                                items[0]["piid"].clone(),
                                items[0]["value"].clone(),
                            )
                        })
                        .collect::<Vec<_>>();
                    if sent {
                        assert_eq!(
                            actual,
                            vec![
                                (json!(2), json!(3), json!(0)),
                                (json!(8), json!(1), json!(52))
                            ]
                        );
                    } else {
                        assert!(wire.written.borrow().is_empty());
                        assert_eq!(
                            actual,
                            vec![
                                (json!(2), json!(1), json!(true)),
                                (json!(2), json!(3), json!(0)),
                                (json!(8), json!(1), json!(52))
                            ]
                        );
                    }
                    drop(cloud);
                    let last = c.context.diagnostics().devices[0].commands[0]
                        .last
                        .clone()
                        .unwrap();
                    assert_eq!(
                        last.properties
                            .iter()
                            .map(|property| (
                                property.binding.siid,
                                property.binding.piid,
                                property.value.clone(),
                                property.path.unwrap(),
                            ))
                            .collect::<Vec<_>>(),
                        vec![
                            (
                                2,
                                1,
                                json!(true),
                                if sent {
                                    PathKind::Local
                                } else {
                                    PathKind::Cloud
                                }
                            ),
                            (2, 3, json!(0), PathKind::Cloud),
                            (8, 1, json!(52), PathKind::Cloud),
                        ],
                    );
                },
            )
            .await;
        }));
    }
}

#[test]
fn possibly_sent_rejected_step_is_never_replayed_or_followed() {
    let rig = fan_rig();
    rig.cloud.lock().unwrap().online.insert("one".into());
    block_on(rig.run(|c| async move {
        until(|| c.context.cloud_scope_valid.get()).await;
        let wire = Rc::new(Wire::default());
        wire.allow(1);
        let (handle, driver) = local(&c, wire.clone());
        let fan = function(&c.context.registry, "one", "main");
        future::or(
            async {
                driver.run(|_| {}).await.unwrap();
                panic!("driver stopped");
            },
            async {
                let action = fan.begin_fan(FanCommand::Speed(52)).unwrap();
                until(|| wire.written.borrow().len() == 1).await;
                c.context.local_outcome(&handle, &response_timeout());
                wire.allow(usize::MAX);
                until(|| wire.written.borrow().len() > 1).await;
                wire.reply(json!({"result":[{"did":"one","siid":2,"piid":1,"code":-704}]}));
                assert!(matches!(
                    action.accepted().await,
                    Err(CommandError::Rejected(_))
                ));
                for _ in 0..30 {
                    future::yield_now().await;
                }
                assert!(c.cloud.lock().unwrap().numeric_writes.is_empty());
                let last = c.context.diagnostics().devices[0].commands[0]
                    .last
                    .clone()
                    .unwrap();
                assert_eq!(
                    last.properties
                        .iter()
                        .map(|property| property.path.unwrap())
                        .collect::<Vec<_>>(),
                    vec![PathKind::Local]
                );
                assert_eq!(last.properties.len(), 1);
            },
        )
        .await;
    }));
}

#[test]
fn unsent_speed_uses_the_latest_power_observation_at_the_send_boundary() {
    for latest in [false, true] {
        block_on(fan_rig().run(|c| async move {
            let wire = Rc::new(Wire::default());
            let (_, driver) = local(&c, wire.clone());
            let fan = function(&c.context.registry, "one", "main");
            fan.observe_fan(FanValue::Power(!latest), Some(1), 1);
            future::or(
                async {
                    driver.run(|_| {}).await.unwrap();
                    panic!("driver stopped");
                },
                async {
                    let _action = fan.begin_fan(FanCommand::Speed(52)).unwrap();
                    until(|| wire.polls.get() > 0).await;
                    fan.observe_fan(FanValue::Power(latest), Some(2), 2);
                    wire.allow(usize::MAX);
                    until(|| {
                        let mut frames = crate::xiaomi::mqtt::Frames::default();
                        frames.push(&wire.written.borrow()).unwrap();
                        frames.next_packet().unwrap().is_some()
                    })
                    .await;
                    let mut frames = crate::xiaomi::mqtt::Frames::default();
                    frames.push(&wire.written.borrow()).unwrap();
                    let Packet::Publish(publish) = frames.next_packet().unwrap().unwrap() else {
                        panic!()
                    };
                    let payload = decode_mips(&publish.payload).unwrap().payload;
                    let expected = if latest {
                        json!([{"did":"one","siid":2,"piid":3,"value":0}])
                    } else {
                        json!([{"did":"one","siid":2,"piid":1,"value":true}])
                    };
                    assert_eq!(payload["rpc"]["params"], expected);
                },
            )
            .await;
        }));
    }
}
