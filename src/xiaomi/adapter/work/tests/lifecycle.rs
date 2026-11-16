use super::super::observation::FanObservation;
use super::super::*;
use crate::{
    device::{FanValue, Power, RelayMode},
    xiaomi::capability::parse_device_for_model,
};
use serde_json::json;

fn device(model: &str, fixture: &str) -> DeviceWork {
    let instance: serde_json::Value = serde_json::from_str(fixture).unwrap();
    let native = NativeDescription::new(
        model.into(),
        instance["type"].as_str().unwrap().into(),
        parse_device_for_model(DeviceKey::new("xiaomi", "one"), model, &instance).unwrap(),
    )
    .unwrap();
    let (physical, _) = PhysicalDevice::new(native.physical_description("Device"));
    DeviceWork::new(
        native,
        Rc::new(LogicalSink::new(physical)),
        Rc::new(Event::new()),
        Rc::new(Event::new()),
        Rc::new(Event::new()),
        None,
    )
}

fn replace_logical(work: &DeviceWork) -> Rc<LogicalSink> {
    let (physical, _) = PhysicalDevice::new(work.native.physical_description("Replacement"));
    work.replace_sink(Rc::new(LogicalSink::new(physical)))
}

fn bath_heater() -> DeviceWork {
    device(
        "yeelink.bhf_light.v13",
        include_str!("../../../../../tests/fixtures/miot/yeelink-bhf-light-v13.json"),
    )
}

#[test]
fn logical_replacement_requests_fresh_fan_power_and_shared_feedback() {
    let work = bath_heater();
    let id = "fan:3:2";
    let function = work
        .native
        .device()
        .functions()
        .iter()
        .find(|f| f.id() == id)
        .unwrap();
    let power = function.fan().unwrap().power();
    work.observe_fan(
        function,
        power,
        FanObservation::Value(FanValue::Power(true)),
        Observation {
            source: PathKind::Local,
            source_time: None,
            received_at: 1,
        },
        Origin::Read,
    );
    assert!(work.refresh.borrow().initial_complete(id, power));
    let previous_power = work.current_sink().shared_fan_power(id);
    replace_logical(&work).close();
    let refresh = work.refresh.borrow();
    assert!(!refresh.initial_complete(id, power));
    assert!(
        refresh.contains(id, power),
        "the replacement needs its own power baseline"
    );
    assert!(refresh.contains_input(SHARED_FEEDBACK_CHANNEL, ReadInput::SharedFeedback));
    assert!(!Rc::ptr_eq(
        &previous_power,
        &work.current_sink().shared_fan_power(id)
    ));
}

#[test]
fn logical_replacement_retires_preceding_native_reads() {
    let work = bath_heater();
    let id = "fan:3:2";
    let power = PropertyBinding { siid: 3, piid: 2 };
    let stamp = work.observations.read_stamp_input(id, power.into());
    replace_logical(&work).close();
    assert!(!work.observations.read_is_current(&work, id, power, &stamp));
}

#[test]
fn logical_replacement_keeps_observed_relay_state_and_queues_its_baseline() {
    let work = device(
        "zimi.switch.dhkg05",
        include_str!("../../../../../tests/fixtures/miot/zimi-dhkg05.json"),
    );
    let id = "relay:2";
    let function = work
        .native
        .device()
        .functions()
        .iter()
        .find(|f| f.id() == id)
        .unwrap();
    let mode = function.relay_mode().unwrap().0;
    work.observe_relay_power_notification(id, true, PathKind::Cloud);
    work.observe_relay_mode(
        function,
        mode,
        &json!(1),
        Observation {
            source: PathKind::Local,
            source_time: None,
            received_at: 1,
        },
        Origin::Notification,
    )
    .unwrap();
    replace_logical(&work).close();
    let snapshot = work.physical().function(id).unwrap().snapshot();
    assert_eq!(snapshot.power(), Some(Power::On));
    assert_eq!(snapshot.relay_mode(), Some(RelayMode::Wireless));
    assert_eq!(work.observation_source(id), Some(PathKind::Cloud));
    assert_eq!(work.property_source(id, mode), Some(PathKind::Local));
    assert!(work.refresh.borrow().contains(id, mode));
}

#[test]
fn stale_shared_reads_queueing_and_waiting_for_feedback_are_independent() {
    let work = bath_heater();
    let input = ReadInput::SharedFeedback;
    let stamp = work
        .observations
        .read_stamp_input(SHARED_FEEDBACK_CHANNEL, input);
    work.invalidate_shared_read_for_function("fan:3:2");
    assert!(matches!(
        work.observe_shared_feedback(
            &json!(["heat", 31, 26.5]),
            &stamp,
            Observation {
                source: PathKind::Cloud,
                source_time: None,
                received_at: 1
            },
        ),
        super::super::super::refresh::ReadResult::Retired
    ));
    assert!(!work.shared_feedback().pending);
    assert!(
        !work
            .refresh
            .borrow()
            .contains_input(SHARED_FEEDBACK_CHANNEL, input)
    );

    work.refresh_shared_feedback();
    assert!(
        work.refresh
            .borrow()
            .contains_input(SHARED_FEEDBACK_CHANNEL, input)
    );
    assert!(!work.shared_feedback().pending);

    work.refresh_related_inputs(RelatedScope::All);
    assert!(work.shared_feedback().pending);
}
