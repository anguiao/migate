use super::*;
use crate::device::{
    AirConditionerCommand as Command, AirConditionerFanSpeed as Speed, AirConditionerMode as Mode,
    AirConditionerTarget, AirConditionerValue, CommandError, TemperatureGrid,
    air_conditioner::AirConditionerConstraints,
};
use crate::xiaomi::adapter::PathKind;

fn rig() -> Rig {
    let description = serde_json::from_str(include_str!(
        "../../../../../tests/fixtures/miot/lumi-mcn04.json"
    ))
    .unwrap();
    Rig::custom_lights(&[("one", "generic.ac.model", description)])
}

#[test]
fn unsent_step_is_revalidated_after_a_range_change_at_the_transport_boundary() {
    block_on(rig().run(|c| async move {
        let wire = Rc::new(Wire::default());
        let (_, driver) = local(&c, wire.clone());
        let function = function(&c.context.registry, "one", "air-conditioner:3");
        function.observe_air_conditioner(AirConditionerValue::Mode(Mode::Cool), Some(1), 1);
        future::or(
            async {
                driver.run(|_| {}).await.unwrap();
                panic!("driver stopped");
            },
            async {
                let action = function
                    .begin_air_conditioner(Command::Temperature(26))
                    .unwrap();
                until(|| wire.polls.get() > 0).await;
                function.set_air_conditioner_constraints(
                    AirConditionerConstraints::new(
                        TemperatureGrid::new(16, 30, 1).unwrap(),
                        [(Mode::Cool, vec![24], vec![Speed::Low])],
                    )
                    .unwrap(),
                );
                wire.allow(usize::MAX);
                assert_eq!(action.accepted().await, Err(CommandError::InvalidInState));
                assert!(wire.written.borrow().is_empty());
            },
        )
        .await;
    }));
}

#[test]
fn feedback_after_mode_receipt_restricts_the_next_unsent_step() {
    block_on(rig().run(|c| async move {
        let wire = Rc::new(Wire::default());
        wire.allow(usize::MAX);
        let (_, driver) = local(&c, wire.clone());
        let function = function(&c.context.registry, "one", "air-conditioner:3");
        function.observe_air_conditioner(AirConditionerValue::Power(true), Some(1), 1);
        function.observe_air_conditioner(AirConditionerValue::Mode(Mode::Cool), Some(1), 1);
        function.set_air_conditioner_constraints(
            AirConditionerConstraints::new(
                TemperatureGrid::new(16, 30, 1).unwrap(),
                [
                    (Mode::Cool, vec![], vec![Speed::Low]),
                    (Mode::Heat, vec![24], vec![Speed::High]),
                    (Mode::Auto, vec![], vec![Speed::Auto]),
                ],
            )
            .unwrap(),
        );
        future::or(
            async {
                driver.run(|_| {}).await.unwrap();
                panic!("driver stopped");
            },
            async {
                let action = function
                    .begin_air_conditioner(Command::Combined(AirConditionerTarget {
                        mode: Some(Mode::Heat),
                        temperature: Some(24),
                        speed: None,
                        swing: None,
                    }))
                    .unwrap();
                until(|| {
                    let mut frames = crate::xiaomi::mqtt::Frames::default();
                    frames.push(&wire.written.borrow()).unwrap();
                    frames.next_packet().unwrap().is_some()
                })
                .await;
                wire.allow(0);
                let first_bytes = wire.written.borrow().len();
                let polls = wire.polls.get();
                wire.reply(json!({"result":[{"did":"one","siid":3,"piid":2,"code":0}]}));
                until(|| wire.polls.get() > polls).await;
                assert_eq!(
                    function.snapshot().air_conditioner().unwrap().mode.value,
                    Some(Mode::Cool)
                );
                function.observe_air_conditioner(AirConditionerValue::Mode(Mode::Auto), Some(2), 2);
                wire.allow(usize::MAX);
                assert_eq!(action.accepted().await, Err(CommandError::InvalidInState));
                assert_eq!(wire.written.borrow().len(), first_bytes);
            },
        )
        .await;
    }));
}

#[test]
fn route_switch_reselects_only_an_unsent_air_conditioner_step() {
    for sent in [false, true] {
        let rig = rig();
        rig.cloud.lock().unwrap().online.insert("one".into());
        block_on(rig.run(|c| async move {
            until(|| c.context.cloud_scope_valid.get()).await;
            let wire = Rc::new(Wire::default());
            if sent {
                wire.allow(1);
            }
            let (handle, driver) = local(&c, wire.clone());
            let function = function(&c.context.registry, "one", "air-conditioner:3");
            function.observe_air_conditioner(AirConditionerValue::Mode(Mode::Cool), Some(1), 1);
            future::or(
                async {
                    driver.run(|_| {}).await.unwrap();
                    panic!("driver stopped");
                },
                async {
                    let action = function
                        .begin_air_conditioner(Command::Speed(Speed::Auto))
                        .unwrap();
                    if sent {
                        until(|| wire.written.borrow().len() == 1).await;
                    } else {
                        until(|| wire.polls.get() > 0).await;
                    }
                    c.context.local_outcome(&handle, &response_timeout());
                    wire.allow(usize::MAX);
                    if sent {
                        until(|| wire.written.borrow().len() > 1).await;
                        wire.reply(json!({"result":[{"did":"one","siid":3,"piid":1,"code":0}]}));
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
                    assert_eq!(
                        actual,
                        if sent {
                            vec![(json!(4), json!(2), json!(0))]
                        } else {
                            vec![
                                (json!(3), json!(1), json!(true)),
                                (json!(4), json!(2), json!(0)),
                            ]
                        }
                    );
                    if !sent {
                        assert!(wire.written.borrow().is_empty());
                    }
                    drop(cloud);
                    let last = c.context.diagnostics().devices[0].commands[0]
                        .last
                        .clone()
                        .unwrap();
                    assert_eq!(
                        last.properties
                            .iter()
                            .map(|property| property.path.unwrap())
                            .collect::<Vec<_>>(),
                        [
                            if sent {
                                PathKind::Local
                            } else {
                                PathKind::Cloud
                            },
                            PathKind::Cloud
                        ]
                    );
                },
            )
            .await;
        }));
    }
}

#[test]
fn possibly_sent_failed_air_conditioner_step_is_not_replayed_or_followed() {
    let rig = rig();
    rig.cloud.lock().unwrap().online.insert("one".into());
    block_on(rig.run(|c| async move {
        until(|| c.context.cloud_scope_valid.get()).await;
        let wire = Rc::new(Wire::default());
        wire.allow(1);
        let (handle, driver) = local(&c, wire.clone());
        let function = function(&c.context.registry, "one", "air-conditioner:3");
        function.observe_air_conditioner(AirConditionerValue::Mode(Mode::Cool), Some(1), 1);
        future::or(
            async {
                driver.run(|_| {}).await.unwrap();
                panic!("driver stopped");
            },
            async {
                let action = function
                    .begin_air_conditioner(Command::Speed(Speed::Low))
                    .unwrap();
                until(|| wire.written.borrow().len() == 1).await;
                c.context.local_outcome(&handle, &response_timeout());
                wire.allow(usize::MAX);
                until(|| wire.written.borrow().len() > 1).await;
                wire.reply(json!({"result":[{"did":"one","siid":3,"piid":1,"code":-704}]}));
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
                assert_eq!(last.properties.len(), 1);
                assert_eq!(last.properties[0].path, Some(PathKind::Local));
            },
        )
        .await;
    }));
}
