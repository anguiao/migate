use super::*;
pub(super) use crate::device::{Command, DeviceRegistry};
use crate::xiaomi::capability::NativeDescription;
pub(super) use crate::xiaomi::{
    central::{decode_mips, encode_mips, test_support::Broker},
    discovery::SERVICE_TYPE,
};
pub(super) use crate::{
    configuration::Scope,
    device::{Availability, DeviceKey, Power},
    storage::Store,
    xiaomi::{
        capability::tests::parse_device,
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

mod cloud_peer;
mod groups;
mod peer;
mod rig;

pub(super) use cloud_peer::{
    CloudBroker, CloudPeer, cloud_active, with_cloud_peer, with_cloud_rig, with_cloud_sensors,
};
pub(super) use groups::{
    MEMBER_COLOR, MEMBER_MODE, confirmed_for, group_instance, group_record, ls2307_instance,
    repair_ready, repair_rig,
};
pub(super) use peer::{Peer, peer_run, response, topic_key};
pub(super) use rig::{Control, Rig};

/// Test-only outage/removal simulation: close the installed runtime authority
/// and retire its attachments without going through description handling.
pub(super) fn force_unavailable(context: &super::super::state::Context, did: &str, reason: &str) {
    let reason = reason.to_owned();
    if let Some(slot) = context.live.borrow_mut().devices.get_mut(did) {
        slot.blocked = Some(reason.clone());
    }
    if let Some(work) = context
        .live
        .borrow()
        .devices
        .get(did)
        .and_then(|slot| slot.admitted.clone())
    {
        work.physical()
            .set_availability(Availability::Unavailable(reason.clone()));
    }
    context.retire_work(did);
    context.changed.notify(usize::MAX);
    context.refresh_group_states(did);
    context.problem(did, Some(reason));
}

pub(super) fn sensor_native(did: &str) -> NativeDescription {
    let instance: Value = serde_json::from_str(include_str!(
        "../../../../../tests/fixtures/miot/miaomiaoce-t6.json"
    ))
    .unwrap();
    NativeDescription::new(
        "miaomiaoce.sensor_ht.t6".into(),
        instance["type"].as_str().unwrap().into(),
        parse_device(DeviceKey::new("xiaomi", did), &instance).unwrap(),
    )
    .unwrap()
}

/// A restored sensor shape without a declared battery capability. This is the
/// persisted capability surface an old schema-8 record exposes.
pub(super) fn sensor_native_without_battery(did: &str) -> NativeDescription {
    let instance: Value = serde_json::from_str(include_str!(
        "../../../../../tests/fixtures/miot/miaomiaoce-t6.json"
    ))
    .unwrap();
    let parsed = parse_device(DeviceKey::new("xiaomi", did), &instance).unwrap();
    let device = crate::xiaomi::capability::ParsedDevice::new_with_battery_diagnostic(
        parsed.key().clone(),
        parsed.functions().to_vec(),
        parsed.refresh_trigger(),
        None,
        Some("battery service is missing".into()),
    )
    .unwrap();
    NativeDescription::new(
        "miaomiaoce.sensor_ht.t6".into(),
        instance["type"].as_str().unwrap().into(),
        device,
    )
    .unwrap()
}

pub(super) fn cached(_uid: &str, did: &str, multi: bool) -> NativeDescription {
    let instance: Value = serde_json::from_str(if multi {
        include_str!("../../../../../tests/fixtures/miot/cuco-cp7pd.json")
    } else {
        include_str!("../../../../../tests/fixtures/miot/outlet.json")
    })
    .unwrap();
    NativeDescription::new(
        if multi {
            "cuco.plug.cp7pd"
        } else {
            "cuco.plug.v3"
        }
        .into(),
        instance["type"].as_str().unwrap().into(),
        parse_device(DeviceKey::new("xiaomi", did), &instance).unwrap(),
    )
    .unwrap()
}

pub(super) fn scope(uid: &str) -> Scope {
    Scope {
        uid: uid.into(),
        home_id: "local".into(),
    }
}
/// Commit a current-store configuration with identities and native facts.
pub(super) fn commit_natives(store: &Store, uid: &str, natives: &[NativeDescription]) {
    use crate::configuration::{CommittedConfiguration, CommittedDevice, CommittedFunction};
    let mut configuration = CommittedConfiguration {
        scope: Some(scope(uid)),
        root_version: 1,
        devices: Vec::new(),
        groups: Vec::new(),
    };
    let mut endpoint = 2u16;
    for native in natives {
        let id = native.device().key().id.clone();
        let functions = native
            .device()
            .functions()
            .iter()
            .enumerate()
            .map(|(index, function)| CommittedFunction {
                id: function.id().to_owned(),
                endpoint: endpoint + 1 + index as u16,
            })
            .collect();
        configuration.devices.push(CommittedDevice {
            key: native.device().key().clone(),
            unique_id: crate::storage::tests::fixtures::unique(&id),
            endpoint,
            label: format!("name {id}"),
            configuration_version: 1,
            functions,
            native: Some(native.clone()),
        });
        endpoint += native.device().functions().len() as u16 + 1;
    }
    configuration.devices.sort_by(|a, b| a.key.cmp(&b.key));
    store
        .devices()
        .commit_configuration(&configuration, &[])
        .unwrap();
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
                            .all(|function| function.snapshot().power() != Some(Power::Unknown))
                        && c.context
                            .live
                            .borrow()
                            .devices
                            .get(*did)
                            .and_then(|slot| slot.admitted.as_ref())
                            .is_some_and(|work| {
                                // Every function owns one or more native
                                // property bindings, not one per channel.
                                work.test_local_subscribed_len() == work.bindings().len()
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

/// Rebuild a valid native description around an explicit function list.
pub(super) fn device_with_functions(
    device: &crate::xiaomi::capability::ParsedDevice,
    functions: Vec<crate::xiaomi::capability::ParsedFunction>,
) -> crate::xiaomi::capability::ParsedDevice {
    crate::xiaomi::capability::ParsedDevice::new(
        device.key().clone(),
        functions,
        device.refresh_trigger(),
    )
    .unwrap()
}

pub(super) fn device_with_key(
    device: &crate::xiaomi::capability::ParsedDevice,
    id: &str,
) -> crate::xiaomi::capability::ParsedDevice {
    crate::xiaomi::capability::ParsedDevice::new(
        crate::device::DeviceKey::new(device.key().source.clone(), id),
        device.functions().to_vec(),
        device.refresh_trigger(),
    )
    .unwrap()
}

/// Rebuild a light description with a different native temperature grid.
pub(super) fn device_with_temperature_grid(
    device: &crate::xiaomi::capability::ParsedDevice,
    grid: crate::device::NumericRange,
) -> crate::xiaomi::capability::ParsedDevice {
    use crate::xiaomi::capability::{
        BrightnessBinding, ColorBinding, LightBindings, ModeBinding, ParsedFunction,
        TemperatureBinding,
    };
    let function = &device.functions()[0];
    let light = function.lighting().unwrap();
    let brightness = light.brightness().map(|binding| {
        BrightnessBinding::new(binding.property(), binding.format(), binding.range()).unwrap()
    });
    let temperature = TemperatureBinding::new(
        light.temperature().unwrap().property(),
        light.temperature().unwrap().format(),
        grid,
    )
    .unwrap();
    let color = light
        .color()
        .map(|binding| ColorBinding::new(binding.property(), binding.format()).unwrap());
    let mode = light.mode().map(|binding| {
        ModeBinding::new(
            binding.property(),
            binding.format(),
            binding.temperature_value(),
            binding.rgb_value(),
        )
        .unwrap()
    });
    let lighting = LightBindings::new(brightness, Some(temperature), color, mode).unwrap();
    let function = ParsedFunction::light(
        function.id(),
        function.label(),
        function.power().expect("light power"),
        lighting,
    );
    device_with_functions(device, vec![function])
}

pub(super) fn device_with_brightness_format(
    device: &crate::xiaomi::capability::ParsedDevice,
    format: crate::xiaomi::capability::NumericFormat,
) -> crate::xiaomi::capability::ParsedDevice {
    use crate::xiaomi::capability::{
        BrightnessBinding, ColorBinding, LightBindings, ModeBinding, ParsedFunction,
        TemperatureBinding,
    };
    let function = &device.functions()[0];
    let light = function.lighting().unwrap();
    let brightness = light.brightness().map(|binding| {
        BrightnessBinding::new(binding.property(), format, binding.range()).unwrap()
    });
    let temperature = light.temperature().map(|binding| {
        TemperatureBinding::new(binding.property(), binding.format(), binding.range()).unwrap()
    });
    let color = light
        .color()
        .map(|binding| ColorBinding::new(binding.property(), binding.format()).unwrap());
    let mode = light.mode().map(|binding| {
        ModeBinding::new(
            binding.property(),
            binding.format(),
            binding.temperature_value(),
            binding.rgb_value(),
        )
        .unwrap()
    });
    let lighting = LightBindings::new(brightness, temperature, color, mode).unwrap();
    let function = ParsedFunction::light(
        function.id(),
        function.label(),
        function.power().expect("light power"),
        lighting,
    );
    device_with_functions(device, vec![function])
}

/// Test shorthand for source-boundary observations without a supplied update
/// time. Production callers always use the explicit `_at` APIs.
pub(super) trait DeviceWorkTestExt {
    fn notify_value(
        &self,
        binding: crate::xiaomi::capability::PropertyBinding,
        value: &Value,
        cached_report: bool,
        takeover: bool,
    ) -> bool;
    fn observe_read(
        &self,
        id: &str,
        binding: crate::xiaomi::capability::PropertyBinding,
        stamp: &crate::xiaomi::adapter::observations::ReadStamp,
        value: &Value,
    ) -> Result<(), crate::xiaomi::capability::CapabilityError>;
}
impl DeviceWorkTestExt for super::super::work::DeviceWork {
    fn notify_value(
        &self,
        binding: crate::xiaomi::capability::PropertyBinding,
        value: &Value,
        cached_report: bool,
        takeover: bool,
    ) -> bool {
        self.notify_value_at(binding, value, None, cached_report, takeover, 0)
    }
    fn observe_read(
        &self,
        id: &str,
        binding: crate::xiaomi::capability::PropertyBinding,
        stamp: &crate::xiaomi::adapter::observations::ReadStamp,
        value: &Value,
    ) -> Result<(), crate::xiaomi::capability::CapabilityError> {
        self.observe_read_at(
            id,
            binding,
            stamp,
            value,
            Observation {
                source: PathKind::Local,
                source_time: None,
                received_at: 0,
            },
        )
    }
}
