use super::*;
use crate::device::{
    DeviceRequest, Function, FunctionState, HeaterCommand, HeaterMode, HeaterRequest,
    HeaterSnapshot, HeaterStep, HeaterValue, TemperatureGrid, TemperatureSourceState,
};

fn heater() -> (PhysicalDevice, CommandReceiver) {
    let description = PhysicalDeviceDescription::new(
        DeviceKey::new("xiaomi", "heater-commands"),
        "Bath heater",
        vec![FunctionDescription {
            id: "heater:3".into(),
            label: "暖风".into(),
            kind: FunctionKind::Heater {
                temperature: TemperatureGrid::new(25, 45, 1).unwrap(),
            },
        }],
    )
    .unwrap();
    let (device, receiver) = new_device(description);
    device.set_availability(Availability::Available);
    (device, receiver)
}

async fn next_heater(receiver: &mut CommandReceiver) -> HeaterRequest {
    match receiver.next_operation().await.unwrap() {
        DeviceRequest::Heater(request) => request,
        _ => panic!("expected heater request"),
    }
}

fn snapshot(function: &Function) -> HeaterSnapshot {
    match function.snapshot().state {
        FunctionState::Heater { heater } => heater,
        _ => panic!("expected heater state"),
    }
}

fn observe(function: &Function, power: bool, temperature: i16) {
    function.observe_heater(HeaterValue::Power(power), Some(1), 1);
    function.observe_heater(HeaterValue::Temperature(temperature), Some(1), 1);
}

#[test]
fn out_of_range_targets_leave_the_valid_temperature_command_queued() {
    block_on(async {
        let (device, mut receiver) = heater();
        let function = &device.functions()[0];
        observe(function, false, 25);
        let original = function.snapshot();

        let temperature = function
            .begin_heater(HeaterCommand::Temperature(30))
            .unwrap();
        for value in [24, 46] {
            assert!(matches!(
                function.begin_heater(HeaterCommand::Temperature(value)),
                Err(CommandError::Constraint)
            ));
        }
        let request = next_heater(&mut receiver).await;
        assert_eq!(request.step(), HeaterStep::Temperature(30));
        request.mark_sent().unwrap();
        request.step_accepted().unwrap();
        request.accept().unwrap();
        temperature.accepted().await.unwrap();
        assert_eq!(function.snapshot(), original);

        observe(function, false, 30);
        let heater = snapshot(function);
        assert_eq!(
            (heater.power.value, heater.temperature.value),
            (Some(false), Some(30))
        );
    });
}

#[test]
fn temperature_and_mode_commands_write_only_their_declared_native_fields() {
    block_on(async {
        let (device, mut receiver) = heater();
        let function = &device.functions()[0];
        observe(function, false, 25);
        let original = function.snapshot();
        for (command, step) in [
            (HeaterCommand::Temperature(25), HeaterStep::Temperature(25)),
            (HeaterCommand::Temperature(45), HeaterStep::Temperature(45)),
            (
                HeaterCommand::Mode(HeaterMode::Heat),
                HeaterStep::Power(true),
            ),
            (
                HeaterCommand::Mode(HeaterMode::Off),
                HeaterStep::Power(false),
            ),
            (HeaterCommand::Power(true), HeaterStep::Power(true)),
            (HeaterCommand::Power(false), HeaterStep::Power(false)),
        ] {
            let action = function.begin_heater(command).unwrap();
            let request = next_heater(&mut receiver).await;
            assert_eq!(request.step(), step);
            request.mark_sent().unwrap();
            request.step_accepted().unwrap();
            request.accept().unwrap();
            action.accepted().await.unwrap();
            assert_eq!(function.snapshot(), original);
        }
    });
}

#[test]
fn accepted_commands_never_infer_state_and_off_keeps_a_following_temperature() {
    block_on(async {
        let (device, mut receiver) = heater();
        let function = &device.functions()[0];
        observe(function, true, 25);
        let temperature = function
            .begin_heater(HeaterCommand::Temperature(28))
            .unwrap();
        let pending = next_heater(&mut receiver).await;
        assert_eq!(pending.step(), HeaterStep::Temperature(28));
        let off = function
            .begin_heater(HeaterCommand::Mode(HeaterMode::Off))
            .unwrap();
        assert_eq!(temperature.accepted().await, Err(CommandError::Superseded));
        drop(pending);
        let request = next_heater(&mut receiver).await;
        assert_eq!(request.step(), HeaterStep::Power(false));
        request.mark_sent().unwrap();
        request.step_accepted().unwrap();
        request.accept().unwrap();
        off.accepted().await.unwrap();
        assert_eq!(snapshot(function).power.value, Some(true));

        let later = function
            .begin_heater(HeaterCommand::Temperature(30))
            .unwrap();
        let request = next_heater(&mut receiver).await;
        assert_eq!(request.step(), HeaterStep::Temperature(30));
        request.reject(CommandError::Cancelled);
        assert_eq!(later.accepted().await, Err(CommandError::Cancelled));
        assert_eq!(snapshot(function).power.value, Some(true));
    });
}

#[test]
fn built_in_temperature_source_is_independent_of_power_and_keeps_diagnostics() {
    let (device, _receiver) = heater();
    let function = &device.functions()[0];
    let builtin = |function: &Function| {
        let heater = snapshot(function);
        (heater.temperature_source, heater.current_temperature.value)
    };
    assert_eq!(
        builtin(function),
        (TemperatureSourceState::Unavailable, None)
    );
    assert_eq!(
        function.snapshot().temperature_source(),
        Some(TemperatureSourceState::Unavailable)
    );
    function.set_heater_temperature_source_available(true);
    assert_eq!(
        builtin(function),
        (TemperatureSourceState::AwaitingReading, None)
    );
    function
        .observe_heater_temperature(22.4, Some(10), 11)
        .unwrap();
    let TemperatureSourceState::Available(reading) = builtin(function).0 else {
        panic!("valid reading must be available")
    };
    assert_eq!(
        (reading.value, reading.source_time, reading.received_at),
        (Some(22.4), Some(10), Some(11))
    );
    function.set_heater_temperature_source_available(false);
    assert_eq!(
        builtin(function),
        (TemperatureSourceState::Unavailable, Some(22.4))
    );
    function.set_heater_temperature_source_available(true);
    assert_eq!(
        builtin(function),
        (TemperatureSourceState::AwaitingReading, Some(22.4))
    );
    function
        .observe_heater_temperature(24.0, Some(20), 21)
        .unwrap();
    let TemperatureSourceState::Available(reading) = builtin(function).0 else {
        panic!("recovered reading must be available")
    };
    assert_eq!(reading.value, Some(24.0));
    assert_eq!(snapshot(function).power.value, None);
    assert_eq!(snapshot(function).temperature.value, None);
}
#[test]
fn heater_commands_reject_other_function_kinds_without_queueing() {
    block_on(async {
        for (label, kind) in [
            ("outlet", FunctionKind::Outlet),
            (
                "air conditioner",
                FunctionKind::AirConditioner {
                    temperature: TemperatureGrid::new(16, 30, 1).unwrap(),
                    vertical_swing: false,
                },
            ),
        ] {
            let description = PhysicalDeviceDescription::new(
                DeviceKey::new("xiaomi", label),
                label,
                vec![FunctionDescription {
                    id: "main".into(),
                    label: label.into(),
                    kind,
                }],
            )
            .unwrap();
            let (device, mut receiver) = new_device(description);
            device.set_availability(Availability::Available);
            let function = &device.functions()[0];
            let result = function.begin_heater(HeaterCommand::Power(true));
            assert!(matches!(result, Err(CommandError::Unsupported)), "{label}");
            assert!(
                poll_once(receiver.next_operation()).await.is_none(),
                "{label}"
            );
        }
    });
}
