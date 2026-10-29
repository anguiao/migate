use super::*;
use crate::device::{CommandError, FanCommand};
use futures_lite::future::poll_once;

fn writes(peer: &Peer) -> Vec<(Value, Value, Value)> {
    peer.state
        .borrow()
        .seen
        .iter()
        .filter(|(op, value)| op == "rpcReq" && value["did"] == "one")
        .map(|(_, value)| {
            let items = value["rpc"]["params"].as_array().unwrap();
            assert_eq!(items.len(), 1);
            (
                items[0]["siid"].clone(),
                items[0]["piid"].clone(),
                items[0]["value"].clone(),
            )
        })
        .collect()
}

async fn accept(peer: &Peer, binding: PropertyBinding) {
    peer.release_property(
        "one",
        binding,
        json!({"result":[{"did":"one","siid":binding.siid,"piid":binding.piid,"code":0}]}),
    )
    .await;
}

#[test]
fn local_speed_waits_for_each_receipt_and_keeps_observations_unchanged() {
    block_on(rig().run(|c| async move {
        c.announce();
        let peer = peer();
        peer.value("one", POWER, json!(false));
        let control = &c;
        c.peer(peer, |peer| async move {
            until(|| {
                fan(control).power.value == Some(false)
                    && fan(control).speed.value == Some(40)
                    && fan(control).mode.value == Some(1)
                    && fan(control).swing.value == Some(false)
                    && [POWER, MODE, SPEED, SWING]
                        .iter()
                        .all(|binding| !work(control).refresh.borrow().contains("main", *binding))
            })
            .await;
            let function = function(&control.context.registry, "one", "main");
            let before = function.snapshot();
            peer.state.borrow_mut().hold_writes = true;
            let action = function.begin_fan(FanCommand::Speed(40)).unwrap();
            until(|| writes(&peer).len() == 1).await;
            assert_eq!(writes(&peer), vec![(json!(2), json!(1), json!(true))]);
            assert!(poll_once(action.accepted()).await.is_none());
            accept(&peer, POWER).await;
            until(|| writes(&peer).len() == 2).await;
            assert_eq!(writes(&peer)[1], (json!(2), json!(3), json!(0)));
            assert!(poll_once(action.accepted()).await.is_none());
            accept(&peer, MODE).await;
            until(|| writes(&peer).len() == 3).await;
            assert_eq!(writes(&peer)[2], (json!(8), json!(1), json!(40)));
            assert!(poll_once(action.accepted()).await.is_none());
            accept(&peer, SPEED).await;
            action.accepted().await.unwrap();
            assert_eq!(function.snapshot(), before);
        })
        .await;
    }));
}

#[test]
fn off_waits_for_sent_step_and_new_speed_reopens_after_stale_on() {
    block_on(rig().run(|c| async move {
        c.announce();
        let control = &c;
        c.peer(peer(), |peer| async move {
            until(|| fan(control).power.value == Some(true) && fan(control).speed.value == Some(40) && fan(control).swing.value == Some(false) && work(control).function_observer("main").is_some()).await;
            let function = function(&control.context.registry, "one", "main");
            let before = function.snapshot();
            peer.state.borrow_mut().hold_writes = true;
            let old = function.begin_fan(FanCommand::Speed(40)).unwrap();
            until(|| writes(&peer).len() == 1).await;
            assert_eq!(writes(&peer)[0], (json!(2), json!(3), json!(0)));
            let pending = function.begin_fan(FanCommand::Speed(50)).unwrap();
            let swing = function.begin_fan(FanCommand::Swing(true)).unwrap();
            let replaced = function.begin_fan(FanCommand::Speed(60)).unwrap();
            let latest = function.begin_fan(FanCommand::Speed(70)).unwrap();
            assert_eq!(replaced.accepted().await, Err(CommandError::Superseded));
            let off = function.begin_fan(FanCommand::Power(false)).unwrap();
            assert_eq!(pending.accepted().await, Err(CommandError::Superseded));
            assert_eq!(latest.accepted().await, Err(CommandError::Superseded));
            assert_eq!(old.accepted().await, Err(CommandError::Superseded));
            let after = function.begin_fan(FanCommand::Speed(80)).unwrap();
            for _ in 0..30 { future::yield_now().await; }
            assert_eq!(writes(&peer).len(), 1);
            let status = crate::terminal::AuthStatus::new(control.context.status.borrow().authentication.clone(), "/bin/migate".into(), "/data".into());
            status.update_adapter(control.context.diagnostics());
            let terminal = crate::terminal::bridge::Context::committed(control.publication.clone(), &status);
            let text = terminal.handle_line("status one", 0).await.unwrap().unwrap();
            assert!(text.contains("queued fan commands: swing on, power off, speed 80%."), "{text}");
            accept(&peer, MODE).await;
            until(|| writes(&peer).len() == 2).await;
            assert_eq!(writes(&peer)[1], (json!(2), json!(4), json!(true)));
            accept(&peer, SWING).await;
            swing.accepted().await.unwrap();
            until(|| writes(&peer).len() == 3).await;
            assert_eq!(writes(&peer)[2], (json!(2), json!(1), json!(false)));
            accept(&peer, POWER).await;
            off.accepted().await.unwrap();
            until(|| writes(&peer).len() == 4).await;
            assert_eq!(writes(&peer)[3], (json!(2), json!(1), json!(true)));
            accept(&peer, POWER).await;
            until(|| writes(&peer).len() == 5).await;
            accept(&peer, MODE).await;
            until(|| writes(&peer).len() == 6).await;
            accept(&peer, SPEED).await;
            after.accepted().await.unwrap();
            assert_eq!(function.snapshot(), before);
            status.update_adapter(control.context.diagnostics());
            let text = terminal.handle_line("status one", 0).await.unwrap().unwrap();
            assert!(text.contains("target speed 80%; properties 2.1=true (local), 2.3=0 (local), 8.1=80 (local); actual path local; accepted."), "{text}");
        }).await;
    }));
}

#[test]
fn external_notification_does_not_interrupt_started_speed() {
    block_on(rig().run(|c| async move {
        c.announce();
        let control = &c;
        c.peer(peer(), |peer| async move {
            until(|| {
                fan(control).power.value == Some(true)
                    && fan(control).speed.value == Some(40)
                    && work(control).function_observer("main").is_some()
            })
            .await;
            let function = function(&control.context.registry, "one", "main");
            peer.state.borrow_mut().hold_writes = true;
            let action = function.begin_fan(FanCommand::Speed(55)).unwrap();
            until(|| writes(&peer).len() == 1).await;
            peer.notify_value_at("one", POWER, json!(false), Some(900), false);
            until(|| fan(control).power.value == Some(false)).await;
            assert!(poll_once(action.accepted()).await.is_none());
            accept(&peer, MODE).await;
            until(|| writes(&peer).len() == 2).await;
            assert_eq!(writes(&peer)[1], (json!(8), json!(1), json!(55)));
            accept(&peer, SPEED).await;
            action.accepted().await.unwrap();
            assert_eq!(fan(control).power.value, Some(false));
            assert_eq!(fan(control).speed.value, Some(40));
        })
        .await;
    }));
}

#[test]
fn terminal_power_zero_speed_and_swing_write_only_their_own_properties() {
    block_on(rig().run(|c| async move {
        c.announce();
        let control = &c;
        c.peer(peer(), |peer| async move {
            until(|| {
                fan(control).power.value == Some(true)
                    && fan(control).speed.value == Some(40)
                    && fan(control).mode.value == Some(1)
                    && fan(control).swing.value == Some(false)
            })
            .await;
            let function = function(&control.context.registry, "one", "main");
            let before = function.snapshot().fan();
            let status = crate::terminal::AuthStatus::new(
                control.context.status.borrow().authentication.clone(),
                "/bin/migate".into(),
                "/data".into(),
            );
            let terminal =
                crate::terminal::bridge::Context::committed(control.publication.clone(), &status);
            assert_eq!(
                terminal.handle_line("off one", 0).await.unwrap().unwrap(),
                "one: command accepted"
            );
            assert_eq!(
                terminal.handle_line("on one", 0).await.unwrap().unwrap(),
                "one: command accepted"
            );
            function
                .begin_fan(FanCommand::Swing(true))
                .unwrap()
                .accepted()
                .await
                .unwrap();
            function
                .begin_fan(FanCommand::Speed(0))
                .unwrap()
                .accepted()
                .await
                .unwrap();
            assert_eq!(
                writes(&peer),
                vec![
                    (json!(2), json!(1), json!(false)),
                    (json!(2), json!(1), json!(true)),
                    (json!(2), json!(4), json!(true)),
                    (json!(2), json!(1), json!(false)),
                ]
            );
            assert_eq!(function.snapshot().fan(), before);
        })
        .await;
    }));
}

#[test]
fn off_waits_for_the_old_step_timeout_and_ignores_its_late_receipt() {
    block_on(rig().run(|c| async move {
        c.announce();
        let control = &c;
        c.peer(peer(), |peer| async move {
            until(|| {
                fan(control).power.value == Some(true)
                    && fan(control).speed.value == Some(40)
                    && work(control).function_observer("main").is_some()
            })
            .await;
            peer.state.borrow_mut().hold_writes = true;
            let function = function(&control.context.registry, "one", "main");
            let old = function.begin_fan(FanCommand::Speed(55)).unwrap();
            until(|| writes(&peer).len() == 1).await;
            let off = function.begin_fan(FanCommand::Power(false)).unwrap();
            assert_eq!(old.accepted().await, Err(CommandError::Superseded));
            for _ in 0..30 {
                future::yield_now().await;
            }
            assert_eq!(writes(&peer).len(), 1);
            until(|| writes(&peer).len() == 2).await;
            assert_eq!(writes(&peer)[1], (json!(2), json!(1), json!(false)));
            accept(&peer, MODE).await;
            for _ in 0..30 {
                future::yield_now().await;
            }
            assert_eq!(writes(&peer).len(), 2);
            accept(&peer, POWER).await;
            off.accepted().await.unwrap();
        })
        .await;
    }));
}
