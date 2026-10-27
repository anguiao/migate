use super::*;

fn local_active(c: &Control, did: &str) -> bool {
    c.context.live.borrow().devices[did]
        .admitted
        .as_ref()
        .is_some_and(|work| work.test_observer_is_local())
}

fn startup_paths(members: &[&str]) -> Vec<String> {
    members
        .iter()
        .flat_map(|did| {
            [
                format!("device/{did}/up/properties_changed/2/1"),
                format!("device/{did}/state/#"),
            ]
        })
        .collect()
}

async fn release_startup_subscriptions(peer: &CloudPeer, count: usize) {
    let mut acknowledged = 0;
    while acknowledged < count {
        until(|| !peer.state.borrow().held.is_empty()).await;
        let held = std::mem::take(&mut peer.state.borrow_mut().held);
        assert!(peer.state.borrow().peak_held <= 16);
        acknowledged += held.len();
        for subscribe in held {
            peer.send(Packet::SubAck(SubAck::new(
                subscribe.pkid,
                vec![SubscribeReasonCode::QoS2],
            )));
        }
    }
    assert_eq!(acknowledged, count);
}

mod authorization;
mod observations;
mod recovery;
mod selection;
mod startup;
