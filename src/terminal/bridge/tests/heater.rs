use super::*;
use crate::device::{
    DeviceRequest, FunctionKind, HeaterCommand, HeaterMode, HeaterStep, HeaterValue,
    TemperatureGrid,
};
use futures_lite::future::{block_on, zip};

fn heater() -> (
    tempfile::TempDir,
    Store,
    AuthStatus,
    DeviceRegistry,
    PhysicalDevice,
    CommandReceiver,
) {
    let (directory, store, status, registry, _) = fixture();
    let (device, mut receivers) = PhysicalDevice::new(
        PhysicalDeviceDescription::new(
            DeviceKey::new("xiaomi", "bath"),
            "公卫浴霸",
            vec![FunctionDescription {
                id: "heater:3".into(),
                label: "暖风".into(),
                kind: FunctionKind::Heater {
                    temperature: TemperatureGrid::new(25, 45, 1).unwrap(),
                },
            }],
        )
        .unwrap(),
    );
    device.set_availability(Availability::Available);
    add_device(&store, &device.description());
    registry.insert(device.clone());
    (
        directory,
        store,
        status,
        registry,
        device,
        receivers.remove(0),
    )
}

async fn next_heater(receiver: &mut CommandReceiver) -> crate::device::HeaterRequest {
    let DeviceRequest::Heater(request) = receiver.next_operation().await.unwrap() else {
        panic!("expected heater request")
    };
    request
}

fn settle(request: crate::device::HeaterRequest) {
    request.mark_sent().unwrap();
    request.step_accepted().unwrap();
    request.accept().unwrap();
}

#[test]
fn status_shows_heating_feedback_and_built_in_temperature() {
    block_on(async {
        let (_directory, store, status, registry, device, _receiver) = heater();
        let function = device.function("heater:3").unwrap();
        function.observe_heater(HeaterValue::Power(false), Some(1), 1);
        function.observe_heater(HeaterValue::Temperature(25), Some(1), 1);
        function.set_heater_temperature_source_available(true);
        function
            .observe_heater_temperature(22.5, Some(2), 3)
            .unwrap();

        let output = handle_line(&registry, &store.devices(), &status, "status bath")
            .await
            .unwrap()
            .unwrap();
        assert!(output.contains("heater:3 (暖风)"), "{output}");
        assert!(output.contains("Power: off"), "{output}");
        assert!(output.contains("Mode: Off"), "{output}");
        assert!(output.contains("Target temperature: 25 °C"), "{output}");
        assert!(output.contains("Current temperature: 22.50 °C"), "{output}");

        function.observe_heater(HeaterValue::Power(true), Some(4), 4);
        function
            .observe_heater_temperature(23.0, Some(4), 4)
            .unwrap();
        let output = handle_line(&registry, &store.devices(), &status, "status bath")
            .await
            .unwrap()
            .unwrap();
        assert!(output.contains("Power: on"), "{output}");
        assert!(output.contains("Mode: Heat"), "{output}");
        assert!(output.contains("Current temperature: 23.00 °C"), "{output}");
    });
}

#[test]
fn mode_and_temperature_commands_use_the_heating_command_boundary() {
    block_on(async {
        let (_directory, store, status, registry, device, mut receiver) = heater();
        let function = device.function("heater:3").unwrap();
        let view = support::view(&registry, &store.devices()).unwrap();
        function.observe_heater(HeaterValue::Power(false), Some(1), 1);
        function.observe_heater(HeaterValue::Temperature(25), Some(1), 1);

        let (result, ()) = zip(
            view.handle_words(&["temperature", "bath", "heater:3", "30"], &status, || {
                panic!("accepted temperature control must not sample diagnostics")
            }),
            async {
                let request = next_heater(&mut receiver).await;
                assert_eq!(request.command, HeaterCommand::Temperature(30));
                assert_eq!(request.step(), HeaterStep::Temperature(30));
                settle(request);
            },
        )
        .await;
        assert_eq!(result.unwrap().unwrap(), "bath: command accepted");
        assert_eq!(
            function.snapshot().heater().unwrap().temperature.value,
            Some(25)
        );

        let (result, ()) = zip(
            view.handle_words(&["mode", "bath", "heater:3", "heat"], &status, || {
                panic!("accepted mode control must not sample diagnostics")
            }),
            async {
                let request = next_heater(&mut receiver).await;
                assert_eq!(request.command, HeaterCommand::Mode(HeaterMode::Heat));
                assert_eq!(request.step(), HeaterStep::Power(true));
                settle(request);
            },
        )
        .await;
        assert_eq!(result.unwrap().unwrap(), "bath: command accepted");
        assert_eq!(
            function.snapshot().heater().unwrap().power.value,
            Some(false)
        );

        let (result, ()) = zip(
            view.handle_words(&["mode", "bath", "heater:3", "off"], &status, || {
                panic!("accepted mode control must not sample diagnostics")
            }),
            async {
                let request = next_heater(&mut receiver).await;
                assert_eq!(request.command, HeaterCommand::Mode(HeaterMode::Off));
                assert_eq!(request.step(), HeaterStep::Power(false));
                settle(request);
            },
        )
        .await;
        assert_eq!(result.unwrap().unwrap(), "bath: command accepted");

        for (line, power) in [("on bath heater:3", true), ("off bath heater:3", false)] {
            let (result, ()) = zip(
                handle_line(&registry, &store.devices(), &status, line),
                async {
                    let request = next_heater(&mut receiver).await;
                    assert_eq!(request.command, HeaterCommand::Power(power));
                    assert_eq!(request.step(), HeaterStep::Power(power));
                    settle(request);
                },
            )
            .await;
            assert_eq!(result.unwrap().unwrap(), "bath: command accepted");
        }
    });
}

#[test]
fn invalid_temperature_and_non_heating_function_do_not_send_commands() {
    block_on(async {
        let (_directory, store, status, registry, device, mut receiver) = heater();
        let function = device.function("heater:3").unwrap();
        function.observe_heater(HeaterValue::Power(false), Some(1), 1);
        function.observe_heater(HeaterValue::Temperature(25), Some(1), 1);

        let invalid = handle_line(
            &registry,
            &store.devices(),
            &status,
            "temperature bath heater:3 warm",
        )
        .await
        .unwrap()
        .unwrap();
        assert!(invalid.contains("whole number"), "{invalid}");
        assert!(poll_once(receiver.next_operation()).await.is_none());

        let reply = handle_line(
            &registry,
            &store.devices(),
            &status,
            "temperature bath heater:3 24",
        )
        .await
        .unwrap()
        .unwrap();
        assert!(
            reply.contains("outside the supported constraint"),
            "{reply}"
        );
        assert!(poll_once(receiver.next_operation()).await.is_none());

        let (outlet, _) = PhysicalDevice::new(
            PhysicalDeviceDescription::new(
                DeviceKey::new("xiaomi", "outlet"),
                "Outlet",
                vec![FunctionDescription {
                    id: "outlet:2".into(),
                    label: "主电源".into(),
                    kind: FunctionKind::Outlet,
                }],
            )
            .unwrap(),
        );
        add_device(&store, &outlet.description());
        registry.insert(outlet);
        let unsupported = handle_line(
            &registry,
            &store.devices(),
            &status,
            "mode outlet outlet:2 heat",
        )
        .await
        .unwrap()
        .unwrap();
        assert!(
            unsupported.contains("does not support heating mode"),
            "{unsupported}"
        );
    });
}
