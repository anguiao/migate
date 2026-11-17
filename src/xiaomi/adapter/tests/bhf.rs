use super::*;
use crate::device::{FanCommand, HeaterCommand, HeaterMode, TemperatureSourceState};
use crate::xiaomi::capability::PropertyBinding;
use serde_json::json;

mod control;
mod readiness;
mod shared_feedback;

const DID: &str = "one";
const BLOW: &str = "fan:3:2";
const VENTILATION: &str = "fan:3:4";
const HEATER: &str = "heater:3";
const BLOW_POWER: PropertyBinding = PropertyBinding { siid: 3, piid: 2 };
const VENTILATION_POWER: PropertyBinding = PropertyBinding { siid: 3, piid: 4 };
const HEATING: PropertyBinding = PropertyBinding { siid: 3, piid: 3 };
const TARGET: PropertyBinding = PropertyBinding { siid: 3, piid: 8 };
const HEAT_LEVEL: PropertyBinding = PropertyBinding { siid: 3, piid: 5 };
const MODE: PropertyBinding = PropertyBinding { siid: 3, piid: 1 };

fn v13() -> Value {
    serde_json::from_str(include_str!(
        "../../../../tests/fixtures/miot/yeelink-bhf-light-v13.json"
    ))
    .unwrap()
}

fn rig() -> (Rig, Async<TcpListener>, Rc<CloudPeer>) {
    let rig = Rig::custom_lights(&[(DID, "yeelink.bhf_light.v13", v13())]);
    let (rig, listener, cloud) = with_cloud_rig(rig);
    {
        let mut state = rig.cloud.lock().unwrap();
        state
            .rpc_results
            .insert("get_prop".into(), json!(["heat", 31, 26.5]));
        for (binding, value) in [
            (PropertyBinding { siid: 2, piid: 1 }, json!(false)),
            (PropertyBinding { siid: 2, piid: 2 }, json!(0)),
            (PropertyBinding { siid: 2, piid: 3 }, json!(50)),
            (BLOW_POWER, json!(true)),
            (VENTILATION_POWER, json!(true)),
            (HEATING, json!(false)),
            (TARGET, json!(25)),
            (HEAT_LEVEL, json!(1)),
            (MODE, json!(0)),
        ] {
            state
                .raw_values
                .insert((DID.into(), binding.siid, binding.piid), value);
        }
    }
    (rig, listener, cloud)
}

fn function(c: &Control, id: &str) -> crate::device::Function {
    super::support::function(&c.context, DID, id)
}

fn work(c: &Control) -> Rc<crate::xiaomi::adapter::work::DeviceWork> {
    c.context.directory.borrow().devices[DID]
        .work
        .as_ref()
        .unwrap()
        .clone()
}

async fn eventually(mut predicate: impl FnMut() -> bool) {
    let ok = future::or(
        async {
            async_io::Timer::after(Duration::from_secs(10)).await;
            false
        },
        async {
            until(&mut predicate).await;
            true
        },
    )
    .await;
    assert!(ok, "timed out waiting for device behavior");
}

fn standard_peer() -> Rc<Peer> {
    let peer = Rc::new(Peer::default());
    for (binding, value) in [
        (PropertyBinding { siid: 2, piid: 1 }, json!(false)),
        (PropertyBinding { siid: 2, piid: 2 }, json!(0)),
        (PropertyBinding { siid: 2, piid: 3 }, json!(50)),
        (BLOW_POWER, json!(true)),
        (VENTILATION_POWER, json!(true)),
        (HEATING, json!(false)),
        (TARGET, json!(25)),
        (HEAT_LEVEL, json!(1)),
        (MODE, json!(0)),
    ] {
        peer.value(DID, binding, value);
    }
    peer
}

fn has_shared_demand(c: &Control) -> bool {
    let device = work(c);
    let refresh = device.refresh.borrow();
    let snapshot = refresh.snapshot();
    snapshot
        .pending
        .iter()
        .any(|read| read.input == crate::xiaomi::capability::ReadInput::SharedFeedback)
        || snapshot
            .inflight
            .iter()
            .any(|(_, input)| *input == crate::xiaomi::capability::ReadInput::SharedFeedback)
}

async fn quiesce(c: &Control) {
    eventually(|| !c.cloud.lock().unwrap().rpc_reads.is_empty()).await;
    c.clock
        .automatic
        .store(false, std::sync::atomic::Ordering::Relaxed);
    async_io::Timer::after(Duration::from_millis(50)).await;
    let device = work(c);
    eventually(|| device.refresh.borrow().snapshot().inflight.is_empty()).await;
}

async fn flush_all_polls(c: &Control) {
    for _ in 0..3 {
        c.clock.advance(Duration::from_secs(16));
        for _ in 0..12 {
            async_io::Timer::after(Duration::from_millis(60)).await;
            let device = work(c);
            let idle = {
                let refresh = device.refresh.borrow();
                let snapshot = refresh.snapshot();
                snapshot.pending.is_empty() && snapshot.inflight.is_empty()
            };
            if idle {
                return;
            }
            c.clock.advance(Duration::from_secs(1));
        }
    }
    panic!("periodic reads did not settle");
}

fn hub_reads(peer: &Peer, from: usize) -> Vec<(u32, u32)> {
    peer.state.borrow().seen[from..]
        .iter()
        .filter(|(operation, _)| operation == "get")
        .map(|(_, value)| {
            (
                value["siid"].as_u64().unwrap() as u32,
                value["piid"].as_u64().unwrap() as u32,
            )
        })
        .collect()
}

fn related_read(binding: PropertyBinding) -> (u32, u32) {
    (binding.siid, binding.piid)
}
