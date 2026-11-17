use super::*;
use crate::device::{PhysicalDevice, TemperatureSourceState};
use crate::xiaomi::adapter::state::LogicalSink;

#[test]
fn logical_replacement_reestablishes_an_available_shared_temperature_source() {
    let instance = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/miot/yeelink-bhf-light-v13.json"
    ))
    .unwrap();
    let (rig, listener, peer) = with_cloud_rig(Rig::custom_lights(&[(
        "one",
        "yeelink.bhf_light.v13",
        instance,
    )]));
    rig.cloud
        .lock()
        .unwrap()
        .rpc_results
        .insert("get_prop".into(), json!(["heat", 31, 26.5]));
    block_on(rig.run(|c| async move {
        future::or(peer.run(&listener), async {
            let work = c.context.directory.borrow().devices["one"]
                .work
                .clone()
                .unwrap();
            until(|| work.shared_feedback().source.is_some()).await;
            let (physical, _) =
                PhysicalDevice::new(work.native.physical_description("Replacement"));
            work.replace_sink(Rc::new(LogicalSink::new(physical)))
                .close();
            work.reconcile_state_reads(&c.context.device_routes(&work.key.id));
            let heater = work.physical().function("heater:3").unwrap();
            assert_eq!(
                heater.snapshot().heater().unwrap().temperature_source,
                TemperatureSourceState::AwaitingReading
            );
        })
        .await;
    }));
}
