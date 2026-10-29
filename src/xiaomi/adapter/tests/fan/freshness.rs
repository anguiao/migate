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
                    && work(control).function_observer("main").is_some()
                    && !work(control).refresh.borrow().contains("main", POWER)
            })
            .await;
            peer.state
                .borrow_mut()
                .read_timeout
                .insert(("one".into(), POWER.siid, POWER.piid));
            work(control).refresh_property("main", POWER);
            until(|| {
                peer.state.borrow().held.iter().any(|(publish, _)| {
                    let payload = decode_mips(&publish.payload).unwrap().payload;
                    payload["siid"] == POWER.siid && payload["piid"] == POWER.piid
                })
            })
            .await;
            let function = function(&control.context.registry, "one", "main");
            function
                .begin_fan(FanCommand::Power(false))
                .unwrap()
                .accepted()
                .await
                .unwrap();
            peer.release_property("one", POWER, json!({"value": true, "updateTime": 1}))
                .await;
            until(|| !work(control).refresh.borrow().contains("main", POWER)).await;
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
                    && work(control).function_observer("main").is_some()
            })
            .await;
            let function = function(&control.context.registry, "one", "main");
            function
                .begin_fan(FanCommand::Power(false))
                .unwrap()
                .accepted()
                .await
                .unwrap();
            peer.notify_value_at("one", POWER, json!(true), Some(901), true);
            until(|| fan(control).power.source_time == Some(901)).await;
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
