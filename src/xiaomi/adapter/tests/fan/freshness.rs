use super::*;
use crate::device::FanCommand;

fn writes(peer: &Peer) -> Vec<Value> {
    peer.state
        .borrow()
        .seen
        .iter()
        .filter(|(op, value)| op == "rpcReq" && value["did"] == "one")
        .map(|(_, value)| value["rpc"]["params"][0].clone())
        .collect()
}

#[test]
fn read_started_before_off_cannot_clear_the_on_write_barrier() {
    block_on(rig().run(|c| async move {
        c.announce();
        let control = &c;
        c.peer(peer(), |peer| async move {
            until(|| {
                fan(control).power.value == Some(true)
                    && work(control).function_observer("fan:2").is_some()
                    && !work(control).refresh.borrow().contains("fan:2", POWER)
            })
            .await;
            peer.state
                .borrow_mut()
                .read_timeout
                .insert(("one".into(), POWER.siid, POWER.piid));
            work(control).refresh_property("fan:2", POWER);
            until(|| {
                peer.state.borrow().held.iter().any(|(publish, _)| {
                    let payload = decode_mips(&publish.payload).unwrap().payload;
                    payload["siid"] == POWER.siid && payload["piid"] == POWER.piid
                })
            })
            .await;
            let function = function(&control.context.registry, "one", "fan:2");
            function
                .begin_fan(FanCommand::Power(false))
                .unwrap()
                .accepted()
                .await
                .unwrap();
            peer.release_property("one", POWER, json!({"value": true, "updateTime": 1}))
                .await;
            until(|| !work(control).refresh.borrow().contains("fan:2", POWER)).await;
            peer.state.borrow_mut().hold_writes = true;
            let action = function.begin_fan(FanCommand::Speed(60)).unwrap();
            until(|| writes(&peer).len() == 2).await;
            assert_eq!(writes(&peer)[1]["siid"], POWER.siid);
            assert_eq!(writes(&peer)[1]["piid"], POWER.piid);
            assert_eq!(writes(&peer)[1]["value"], true);
            drop(action);
        })
        .await;
    }));
}

#[test]
fn cached_on_notification_cannot_clear_the_on_write_barrier() {
    block_on(rig().run(|c| async move {
        c.announce();
        let control = &c;
        c.peer(peer(), |peer| async move {
            until(|| {
                fan(control).power.value == Some(true)
                    && work(control).function_observer("fan:2").is_some()
            })
            .await;
            let function = function(&control.context.registry, "one", "fan:2");
            function
                .begin_fan(FanCommand::Power(false))
                .unwrap()
                .accepted()
                .await
                .unwrap();
            let before = fan(control).power;
            let source = work(control).function_observer("fan:2").unwrap();
            control.context.property_value_event(
                &source,
                "one",
                POWER,
                &json!(true),
                Some(901),
                true,
            );
            assert_eq!(fan(control).power, before);
            peer.state.borrow_mut().hold_writes = true;
            let action = function.begin_fan(FanCommand::Speed(60)).unwrap();
            until(|| writes(&peer).len() == 2).await;
            assert_eq!(writes(&peer)[1]["siid"], POWER.siid);
            assert_eq!(writes(&peer)[1]["piid"], POWER.piid);
            assert_eq!(writes(&peer)[1]["value"], true);
            drop(action);
        })
        .await;
    }));
}

#[test]
fn retained_fan_snapshot_does_not_retire_an_inflight_read() {
    block_on(rig().run(|c| async move {
        c.announce();
        let control = &c;
        c.peer(peer(), |peer| async move {
            until(|| {
                fan(control).power.value == Some(true)
                    && work(control).function_observer("fan:2").is_some()
                    && !work(control).refresh.borrow().contains("fan:2", POWER)
            })
            .await;
            peer.state
                .borrow_mut()
                .read_timeout
                .insert(("one".into(), POWER.siid, POWER.piid));
            work(control).refresh_property("fan:2", POWER);
            until(|| {
                peer.state.borrow().held.iter().any(|(publish, _)| {
                    let payload = decode_mips(&publish.payload).unwrap().payload;
                    payload["siid"] == POWER.siid && payload["piid"] == POWER.piid
                })
            })
            .await;
            let previous = fan(control).power;
            let source = work(control).function_observer("fan:2").unwrap();
            control.context.property_value_event(
                &source,
                "one",
                POWER,
                &json!(false),
                Some(900),
                true,
            );
            assert_eq!(fan(control).power, previous);
            peer.release_property("one", POWER, json!({"value": false, "updateTime": 901}))
                .await;
            until(|| fan(control).power.value == Some(false)).await;
            assert_eq!(fan(control).power.source_time, Some(901));
        })
        .await;
    }));
}
