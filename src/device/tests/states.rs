use super::*;
use crate::device::{StateInput, StateSnapshot};

#[test]
fn state_functions_have_no_commands_and_preserve_valid_observations_on_disconnect() {
    for kind in [
        FunctionKind::Contact,
        FunctionKind::Motion,
        FunctionKind::Occupancy,
    ] {
        let (physical, receivers) = PhysicalDevice::new(
            PhysicalDeviceDescription::new(
                DeviceKey::new("xiaomi", "sensor"),
                "Sensor",
                vec![FunctionDescription {
                    id: "state".into(),
                    label: "State".into(),
                    kind,
                }],
            )
            .unwrap(),
        );
        assert!(receivers.is_empty());
        let function = &physical.functions()[0];
        assert_eq!(
            function.snapshot().state_sensor().unwrap(),
            StateSnapshot {
                active: None,
                input: None,
                source_time: None,
                received_at: None
            }
        );
        function
            .observe_state(
                Some(true),
                StateInput::Property(serde_json::json!(0)),
                Some(10),
                20,
            )
            .unwrap();
        let known = function.snapshot().state_sensor().unwrap();
        physical.set_availability(Availability::Unavailable("source disconnected".into()));
        assert_eq!(function.snapshot().state_sensor().unwrap(), known);
        function
            .observe_state(None, StateInput::Property(serde_json::json!(2)), None, 21)
            .unwrap();
        let unknown = function.snapshot().state_sensor().unwrap();
        assert_eq!(unknown.active, None);
        assert_eq!(unknown.source_time, None);
        assert_eq!(unknown.received_at, Some(21));
        assert_eq!(
            unknown.input,
            Some(StateInput::Property(serde_json::json!(2)))
        );
        function.observe(false);
        assert_eq!(function.snapshot().state_sensor().unwrap(), unknown);
        assert_eq!(
            futures_lite::future::block_on(function.execute(Command::On)),
            Err(CommandError::Unsupported)
        );
    }
}
