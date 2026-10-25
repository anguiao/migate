//! Read-only numeric sensor observations through the real source runtime.

use super::*;
use crate::device::{Availability, MeasurementSnapshot};
use crate::xiaomi::capability::PropertyBinding;

const TEMPERATURE: PropertyBinding = PropertyBinding {
    siid: 3,
    piid: 1001,
};

const HUMIDITY: PropertyBinding = PropertyBinding {
    siid: 3,
    piid: 1008,
};

fn sensor_peer(temperature: f64, humidity: f64) -> Rc<Peer> {
    let peer = Rc::new(Peer::default());
    peer.value("sensor", TEMPERATURE, json!(temperature));
    peer.value("sensor", HUMIDITY, json!(humidity));
    peer
}

fn measurement(context: &super::super::state::Context, did: &str, id: &str) -> MeasurementSnapshot {
    super::support::function(&context.registry, did, id)
        .snapshot()
        .measurement()
        .expect("sensor function")
}

fn work(context: &super::super::state::Context, did: &str) -> Rc<super::super::work::DeviceWork> {
    context.live.borrow().devices[did]
        .admitted
        .as_ref()
        .unwrap()
        .clone()
}

async fn wait_cloud_values(c: &Control, did: &str) {
    for _ in 0..500 {
        if measurement(&c.context, did, "temperature").value == Some(18.0)
            && measurement(&c.context, did, "humidity").value == Some(38.0)
        {
            return;
        }
        async_io::Timer::after(Duration::from_millis(10)).await;
    }
    panic!("cloud sensor values were not read for {did}");
}

const BATTERY: PropertyBinding = PropertyBinding {
    siid: 2,
    piid: 1003,
};

fn battery(
    context: &super::super::state::Context,
    did: &str,
) -> Option<crate::device::BatterySnapshot> {
    context
        .registry
        .get(&DeviceKey::new("xiaomi", did))
        .unwrap()
        .battery_snapshot()
}

fn battery_status(
    context: &super::super::state::Context,
    did: &str,
) -> super::super::model::DeviceStatus {
    context
        .status
        .borrow()
        .devices
        .iter()
        .find(|device| device.did == did)
        .cloned()
        .unwrap()
}

mod access;
mod battery;
mod measurements;
mod sources;
