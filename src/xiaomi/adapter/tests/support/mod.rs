use super::*;
pub(super) use crate::device::{Command, DeviceRegistry};
pub(super) use crate::xiaomi::{
    central::{decode_mips, encode_mips, test_support::Broker},
    discovery::SERVICE_TYPE,
};
pub(super) use crate::{
    device::{Availability, DeviceKey, Power},
    storage::{CachedPhysicalDevice, CachedScope, Store},
    xiaomi::{
        capability::parse_device,
        central::test_support::TlsFixture,
        cloud::{CloudClient, group_id},
        discovery::Discovery,
        test_support::{MockResponse, dynamic_mock_server},
    },
};
pub(super) use async_io::Async;
pub(super) use base64::{Engine as _, engine::general_purpose::STANDARD};
pub(super) use event_listener::Event;
pub(super) use futures_lite::future;
pub(super) use futures_lite::future::{block_on, poll_once};
pub(super) use futures_rustls::TlsAcceptor;
pub(super) use mqttbytes::{QoS, v5::*};
pub(super) use serde_json::Value;
pub(super) use serde_json::json;
pub(super) use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
    net::{SocketAddr, TcpListener},
    rc::Rc,
};
pub(super) use std::{
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
    time::Duration,
};

mod peer;
mod rig;

pub(super) use peer::{Peer, peer_run, response, topic_key};
pub(super) use rig::{Control, Rig};

pub(super) fn cached(uid: &str, did: &str, multi: bool) -> CachedPhysicalDevice {
    let instance: Value = serde_json::from_str(if multi {
        include_str!("../../../../../tests/fixtures/miot/cuco-cp7pd.json")
    } else {
        include_str!("../../../../../tests/fixtures/miot/outlet.json")
    })
    .unwrap();
    CachedPhysicalDevice {
        uid: uid.into(),
        home_id: "local".into(),
        group_id: group_id(uid, "local"),
        model: if multi {
            "cuco.plug.cp7pd"
        } else {
            "cuco.plug.v3"
        }
        .into(),
        instance_type: instance["type"].as_str().unwrap().into(),
        device: parse_device(DeviceKey::new("xiaomi", did), &instance).unwrap(),
    }
}
pub(super) fn scope(uid: &str) -> CachedScope {
    CachedScope {
        uid: uid.into(),
        home_id: "local".into(),
        group_id: group_id(uid, "local"),
    }
}
pub(super) struct TestClock {
    pub(super) unix_start: i64,
    pub(super) elapsed: Mutex<Duration>,
    pub(super) deadlines: Mutex<Vec<Duration>>,
    pub(super) changed: Event,
    pub(super) automatic: std::sync::atomic::AtomicBool,
}
impl Default for TestClock {
    fn default() -> Self {
        Self {
            unix_start: time::OffsetDateTime::now_utc().unix_timestamp(),
            elapsed: Default::default(),
            deadlines: Default::default(),
            changed: Event::new(),
            automatic: Default::default(),
        }
    }
}
impl TestClock {
    pub(super) fn wall(&self) -> i64 {
        self.unix_start + self.now().as_secs() as i64
    }
    pub(super) fn advance(&self, by: Duration) {
        *self.elapsed.lock().unwrap() += by;
        self.changed.notify(usize::MAX);
    }
}
impl Clock for TestClock {
    fn now(&self) -> Duration {
        *self.elapsed.lock().unwrap()
    }
    fn unix_now(&self) -> i64 {
        self.wall()
    }
    fn wait_until(&self, deadline: Duration) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        self.deadlines.lock().unwrap().push(deadline);
        Box::pin(async move {
            loop {
                let listener = self.changed.listen();
                if self.now() >= deadline {
                    return;
                }
                listener.await;
            }
        })
    }
}

pub(super) fn announcement(
    address: SocketAddr,
    uid: &str,
    name: &str,
    hub: u64,
) -> mdns_sd::ServiceEvent {
    let mut profile = [0; 23];
    profile[1..9].copy_from_slice(&hub.to_be_bytes());
    profile[9..17].copy_from_slice(
        &u64::from_str_radix(&group_id(uid, "local"), 16)
            .unwrap()
            .to_le_bytes(),
    );
    profile[20] = 0x10;
    profile[22] = 2;
    let profile = STANDARD.encode(profile);
    mdns_sd::ServiceEvent::ServiceResolved(Box::new(
        mdns_sd::ServiceInfo::new(
            SERVICE_TYPE,
            name,
            "isolated.local.",
            address.ip().to_string(),
            address.port(),
            &[("profile", profile.as_str())][..],
        )
        .unwrap()
        .as_resolved_service(),
    ))
}
pub(super) async fn until(mut predicate: impl FnMut() -> bool) {
    while !predicate() {
        future::yield_now().await;
    }
}
pub(super) fn function(registry: &DeviceRegistry, did: &str, id: &str) -> crate::device::Function {
    registry
        .get(&DeviceKey::new("xiaomi", did))
        .unwrap()
        .function(id)
        .unwrap()
}
pub(super) async fn initialized(c: &Control, peer: &Peer, dids: &[&str]) {
    until(|| {
        dids.iter().all(|did| {
            c.context
                .registry
                .get(&DeviceKey::new("xiaomi", *did))
                .is_some_and(|physical| {
                    physical.availability() == Availability::Available
                        && physical
                            .functions()
                            .iter()
                            .all(|function| function.snapshot().power != Power::Unknown)
                        && c.context
                            .live
                            .borrow()
                            .session
                            .as_ref()
                            .and_then(|session| session.devices.get(*did))
                            .is_some_and(|work| {
                                work.subscribed.borrow().len()
                                    == physical.functions().len()
                                        + usize::from(
                                            work.record
                                                .cached
                                                .borrow()
                                                .device
                                                .refresh_trigger
                                                .is_some(),
                                        )
                            })
                })
        }) && peer
            .state
            .borrow()
            .seen
            .iter()
            .filter(|(op, _)| op == "getDevList")
            .count()
            >= 1
    })
    .await;
}
pub(super) async fn advance_to(clock: &TestClock, deadline: Duration) {
    until(|| clock.deadlines.lock().unwrap().contains(&deadline)).await;
    clock.advance(deadline - clock.now());
}
pub(super) fn accepted(did: &str, siid: u32, code: i32) -> Value {
    json!({"result":[{"did":did,"siid":siid,"piid":1,"code":code}]})
}
