use super::*;
use crate::device::{
    AirConditionerCommand, AirConditionerFanSpeed as Speed, AirConditionerMode as Mode,
    AirConditionerRequest, AirConditionerStep as Step, AirConditionerTarget, AirConditionerValue,
    DeviceRequest, TemperatureGrid,
};

fn air_conditioner() -> (PhysicalDevice, CommandReceiver) {
    let description = PhysicalDeviceDescription::new(
        DeviceKey::new("test", "air-conditioner-commands"),
        "Air conditioner",
        vec![FunctionDescription {
            id: "air-conditioner:3".into(),
            label: "Air conditioner".into(),
            kind: FunctionKind::AirConditioner {
                temperature: TemperatureGrid::new(16, 30, 1).unwrap(),
                vertical_swing: true,
            },
        }],
    )
    .unwrap();
    let (device, receiver) = new_device(description);
    device.set_availability(Availability::Available);
    (device, receiver)
}

async fn next_air_conditioner(receiver: &mut CommandReceiver) -> AirConditionerRequest {
    match receiver.next_operation().await.unwrap() {
        DeviceRequest::AirConditioner(request) => request,
        _ => panic!("expected air-conditioner request"),
    }
}

fn planning_mode(function: &crate::device::Function) -> Option<Mode> {
    let state = function.0.state.lock().unwrap();
    let runtime = state.air_conditioner().unwrap();
    runtime.planning_mode.or(runtime.observed_mode)
}

#[test]
fn combination_validates_before_sending_and_acceptance_does_not_change_observation() {
    block_on(async {
        let (device, mut receiver) = air_conditioner();
        let function = &device.functions()[0];
        function.observe_air_conditioner(AirConditionerValue::Power(false), Some(1), 1);
        function.observe_air_conditioner(AirConditionerValue::Mode(Mode::Cool), Some(1), 1);
        let original = function.snapshot();
        let invalid = function
            .begin_air_conditioner(AirConditionerCommand::Combined(AirConditionerTarget {
                mode: Some(Mode::Heat),
                temperature: Some(31),
                speed: Some(Speed::Low),
                swing: Some(true),
            }))
            .unwrap();
        let request = next_air_conditioner(&mut receiver).await;
        assert_eq!(request.plan(), Err(CommandError::Constraint));
        request.reject(CommandError::Constraint);
        assert_eq!(invalid.accepted().await, Err(CommandError::Constraint));

        let action = function
            .begin_air_conditioner(AirConditionerCommand::Combined(AirConditionerTarget {
                mode: Some(Mode::Heat),
                temperature: Some(24),
                speed: Some(Speed::Low),
                swing: Some(true),
            }))
            .unwrap();
        let request = next_air_conditioner(&mut receiver).await;
        let steps = request.plan().unwrap();
        assert_eq!(
            steps,
            vec![
                Step::Power(true),
                Step::Mode(Mode::Heat),
                Step::Temperature(24),
                Step::Speed(Speed::Low),
                Step::Swing(true)
            ]
        );
        for step in steps {
            request.mark_sent(step).unwrap();
            request.step_accepted(step).unwrap();
        }
        request.accept().unwrap();
        action.accepted().await.unwrap();
        assert_eq!(function.snapshot(), original);
    });
}

#[test]
fn accepted_mode_is_only_a_command_context_until_new_feedback_arrives() {
    block_on(async {
        let (device, mut receiver) = air_conditioner();
        let function = &device.functions()[0];
        function.observe_air_conditioner(AirConditionerValue::Power(true), Some(1), 1);
        function.observe_air_conditioner(AirConditionerValue::Mode(Mode::Cool), Some(1), 1);
        let action = function
            .begin_air_conditioner(AirConditionerCommand::Mode(Mode::Heat))
            .unwrap();
        let request = next_air_conditioner(&mut receiver).await;
        assert_eq!(request.plan().unwrap(), vec![Step::Mode(Mode::Heat)]);
        request.mark_sent(Step::Mode(Mode::Heat)).unwrap();
        request.step_accepted(Step::Mode(Mode::Heat)).unwrap();
        request.accept().unwrap();
        action.accepted().await.unwrap();
        assert_eq!(
            function.snapshot().air_conditioner().unwrap().mode.value,
            Some(Mode::Cool)
        );
        assert_eq!(planning_mode(function), Some(Mode::Heat));
        function.observe_air_conditioner(AirConditionerValue::Mode(Mode::Dry), Some(2), 2);
        assert_eq!(planning_mode(function), Some(Mode::Dry));
    });
}

#[test]
fn off_cancels_previous_settings_waits_for_sent_step_and_allows_a_new_intent() {
    block_on(async {
        let (device, mut receiver) = air_conditioner();
        let function = &device.functions()[0];
        function.observe_air_conditioner(AirConditionerValue::Power(true), Some(1), 1);
        function.observe_air_conditioner(AirConditionerValue::Mode(Mode::Cool), Some(1), 1);
        let setting = function
            .begin_air_conditioner(AirConditionerCommand::Speed(Speed::Auto))
            .unwrap();
        let request = next_air_conditioner(&mut receiver).await;
        request.mark_sent(Step::Speed(Speed::Auto)).unwrap();
        let off = function
            .begin_air_conditioner(AirConditionerCommand::Power(false))
            .unwrap();
        let next = function
            .begin_air_conditioner(AirConditionerCommand::Speed(Speed::High))
            .unwrap();
        assert_eq!(setting.accepted().await, Err(CommandError::Superseded));
        assert!(poll_once(receiver.next_operation()).await.is_none());
        assert_eq!(
            request.step_accepted(Step::Speed(Speed::Auto)),
            Err(CommandError::Superseded)
        );
        drop(request);
        let request = next_air_conditioner(&mut receiver).await;
        assert_eq!(request.plan().unwrap(), vec![Step::Power(false)]);
        request.mark_sent(Step::Power(false)).unwrap();
        request.step_accepted(Step::Power(false)).unwrap();
        request.accept().unwrap();
        off.accepted().await.unwrap();
        let request = next_air_conditioner(&mut receiver).await;
        assert_eq!(
            request.plan().unwrap(),
            vec![Step::Power(true), Step::Speed(Speed::High)]
        );
        request.reject(CommandError::Cancelled);
        assert_eq!(next.accepted().await, Err(CommandError::Cancelled));
    });
}

#[test]
fn runtime_range_and_source_lifecycle_control_each_execution() {
    use crate::device::air_conditioner::AirConditionerConstraints;
    block_on(async {
        let (device, mut receiver) = air_conditioner();
        let function = &device.functions()[0];
        function.observe_air_conditioner(AirConditionerValue::Power(true), Some(1), 1);
        function.observe_air_conditioner(AirConditionerValue::Mode(Mode::Auto), Some(1), 1);
        function.set_air_conditioner_constraints(Some(
            AirConditionerConstraints::new(
                TemperatureGrid::new(16, 30, 1).unwrap(),
                [
                    (Mode::Auto, vec![], vec![Speed::Auto, Speed::Low]),
                    (Mode::Heat, vec![24], vec![Speed::Low]),
                ],
            )
            .unwrap(),
        ));
        let action = function
            .begin_air_conditioner(AirConditionerCommand::Temperature(24))
            .unwrap();
        let request = next_air_conditioner(&mut receiver).await;
        assert_eq!(request.plan(), Err(CommandError::InvalidInState));
        request.reject(CommandError::InvalidInState);
        assert_eq!(action.accepted().await, Err(CommandError::InvalidInState));
        let action = function
            .begin_air_conditioner(AirConditionerCommand::Temperature(31))
            .unwrap();
        let request = next_air_conditioner(&mut receiver).await;
        assert_eq!(request.plan(), Err(CommandError::Constraint));
        request.reject(CommandError::Constraint);
        assert_eq!(action.accepted().await, Err(CommandError::Constraint));
        function.air_conditioner_source_changed();
        let action = function
            .begin_air_conditioner(AirConditionerCommand::Temperature(24))
            .unwrap();
        let request = next_air_conditioner(&mut receiver).await;
        assert_eq!(request.plan(), Err(CommandError::UnknownState));
        request.reject(CommandError::UnknownState);
        assert_eq!(action.accepted().await, Err(CommandError::UnknownState));
    });
}

#[test]
fn restrictions_are_checked_again_at_the_actual_send_boundary() {
    use crate::device::air_conditioner::AirConditionerConstraints;
    block_on(async {
        let (device, mut receiver) = air_conditioner();
        let function = &device.functions()[0];
        function.observe_air_conditioner(AirConditionerValue::Power(true), Some(1), 1);
        function.observe_air_conditioner(AirConditionerValue::Mode(Mode::Cool), Some(1), 1);
        let action = function
            .begin_air_conditioner(AirConditionerCommand::Speed(Speed::High))
            .unwrap();
        let request = next_air_conditioner(&mut receiver).await;
        assert_eq!(request.plan().unwrap(), [Step::Speed(Speed::High)]);
        function.set_air_conditioner_constraints(Some(
            AirConditionerConstraints::new(
                TemperatureGrid::new(16, 30, 1).unwrap(),
                [(Mode::Cool, vec![24], vec![Speed::Low])],
            )
            .unwrap(),
        ));
        assert_eq!(
            request.mark_sent(Step::Speed(Speed::High)),
            Err(CommandError::InvalidInState)
        );
        request.reject(CommandError::InvalidInState);
        assert_eq!(action.accepted().await, Err(CommandError::InvalidInState));
    });
}

#[test]
fn new_mode_feedback_before_or_after_acceptance_controls_later_steps() {
    use crate::device::air_conditioner::AirConditionerConstraints;
    for feedback_before_receipt in [true, false] {
        for temperature in [true, false] {
            block_on(async {
                let (device, mut receiver) = air_conditioner();
                let function = &device.functions()[0];
                function.observe_air_conditioner(AirConditionerValue::Power(true), Some(1), 1);
                function.observe_air_conditioner(AirConditionerValue::Mode(Mode::Cool), Some(1), 1);
                function.set_air_conditioner_constraints(Some(
                    AirConditionerConstraints::new(
                        TemperatureGrid::new(16, 30, 1).unwrap(),
                        [
                            (Mode::Cool, vec![], vec![Speed::Low]),
                            (Mode::Heat, vec![24], vec![Speed::High]),
                            (Mode::Auto, vec![], vec![Speed::Auto, Speed::Low]),
                        ],
                    )
                    .unwrap(),
                ));
                let action = function
                    .begin_air_conditioner(AirConditionerCommand::Combined(AirConditionerTarget {
                        mode: Some(Mode::Heat),
                        temperature: temperature.then_some(24),
                        speed: (!temperature).then_some(Speed::High),
                        swing: None,
                    }))
                    .unwrap();
                let request = next_air_conditioner(&mut receiver).await;
                assert!(request.plan().is_ok());
                request.mark_sent(Step::Mode(Mode::Heat)).unwrap();
                if feedback_before_receipt {
                    function.observe_air_conditioner(
                        AirConditionerValue::Mode(Mode::Auto),
                        Some(2),
                        2,
                    );
                }
                request.step_accepted(Step::Mode(Mode::Heat)).unwrap();
                if !feedback_before_receipt {
                    function.observe_air_conditioner(
                        AirConditionerValue::Mode(Mode::Auto),
                        Some(2),
                        2,
                    );
                }
                let step = if temperature {
                    Step::Temperature(24)
                } else {
                    Step::Speed(Speed::High)
                };
                assert_eq!(
                    request.mark_sent(step),
                    Err(CommandError::InvalidInState),
                    "feedback_before_receipt={feedback_before_receipt}, temperature={temperature}"
                );
                request.reject(CommandError::InvalidInState);
                assert_eq!(action.accepted().await, Err(CommandError::InvalidInState));
                assert_eq!(
                    function.snapshot().air_conditioner().unwrap().mode.value,
                    Some(Mode::Auto)
                );
            });
        }
    }
}

#[test]
fn later_steps_validate_only_their_own_remaining_parameters() {
    use crate::device::air_conditioner::AirConditionerConstraints;
    block_on(async {
        let (device, mut receiver) = air_conditioner();
        let function = &device.functions()[0];
        function.observe_air_conditioner(AirConditionerValue::Power(true), Some(1), 1);
        function.observe_air_conditioner(AirConditionerValue::Mode(Mode::Cool), Some(1), 1);
        let constraints = |temperatures| {
            AirConditionerConstraints::new(
                TemperatureGrid::new(16, 30, 1).unwrap(),
                [
                    (Mode::Cool, vec![], vec![Speed::Low]),
                    (Mode::Heat, temperatures, vec![Speed::High]),
                ],
            )
            .unwrap()
        };
        function.set_air_conditioner_constraints(Some(constraints(vec![24])));
        let action = function
            .begin_air_conditioner(AirConditionerCommand::Combined(AirConditionerTarget {
                mode: Some(Mode::Heat),
                temperature: Some(24),
                speed: Some(Speed::High),
                swing: None,
            }))
            .unwrap();
        let request = next_air_conditioner(&mut receiver).await;
        assert!(request.plan().is_ok());
        for step in [Step::Mode(Mode::Heat), Step::Temperature(24)] {
            request.mark_sent(step).unwrap();
            request.step_accepted(step).unwrap();
        }
        assert_eq!(
            function.snapshot().air_conditioner().unwrap().mode.value,
            Some(Mode::Cool)
        );
        function.set_air_conditioner_constraints(Some(constraints(vec![])));
        request.mark_sent(Step::Speed(Speed::High)).unwrap();
        request.step_accepted(Step::Speed(Speed::High)).unwrap();
        request.accept().unwrap();
        action.accepted().await.unwrap();
    });
}

#[test]
fn individual_commands_only_plan_their_declared_fields() {
    block_on(async {
        let (device, mut receiver) = air_conditioner();
        let function = &device.functions()[0];
        function.observe_air_conditioner(AirConditionerValue::Power(false), Some(1), 1);
        function.observe_air_conditioner(AirConditionerValue::Mode(Mode::Cool), Some(1), 1);
        for (command, steps) in [
            (AirConditionerCommand::Power(true), vec![Step::Power(true)]),
            (
                AirConditionerCommand::Power(false),
                vec![Step::Power(false)],
            ),
            (
                AirConditionerCommand::Mode(Mode::Heat),
                vec![Step::Power(true), Step::Mode(Mode::Heat)],
            ),
            (
                AirConditionerCommand::Temperature(25),
                vec![Step::Temperature(25)],
            ),
            (
                AirConditionerCommand::Speed(Speed::Auto),
                vec![Step::Power(true), Step::Speed(Speed::Auto)],
            ),
            (AirConditionerCommand::Swing(true), vec![Step::Swing(true)]),
        ] {
            let action = function.begin_air_conditioner(command).unwrap();
            let request = next_air_conditioner(&mut receiver).await;
            assert_eq!(request.plan().unwrap(), steps);
            request.reject(CommandError::Cancelled);
            assert_eq!(action.accepted().await, Err(CommandError::Cancelled));
        }
    });
}

#[test]
fn fresh_feedback_wins_over_a_later_receipt_and_mode_failure_discards_temporary_context() {
    block_on(async {
        let (device, mut receiver) = air_conditioner();
        let function = &device.functions()[0];
        function.observe_air_conditioner(AirConditionerValue::Power(true), Some(1), 1);
        function.observe_air_conditioner(AirConditionerValue::Mode(Mode::Cool), Some(1), 1);
        let action = function
            .begin_air_conditioner(AirConditionerCommand::Mode(Mode::Heat))
            .unwrap();
        let request = next_air_conditioner(&mut receiver).await;
        request.mark_sent(Step::Mode(Mode::Heat)).unwrap();
        function.observe_air_conditioner(AirConditionerValue::Mode(Mode::Dry), Some(2), 2);
        request.step_accepted(Step::Mode(Mode::Heat)).unwrap();
        request.accept().unwrap();
        action.accepted().await.unwrap();
        assert_eq!(planning_mode(function), Some(Mode::Dry));

        let action = function
            .begin_air_conditioner(AirConditionerCommand::Mode(Mode::Heat))
            .unwrap();
        let request = next_air_conditioner(&mut receiver).await;
        request.mark_sent(Step::Mode(Mode::Heat)).unwrap();
        request.step_accepted(Step::Mode(Mode::Heat)).unwrap();
        request.accept().unwrap();
        action.accepted().await.unwrap();
        assert_eq!(planning_mode(function), Some(Mode::Heat));
        let failed = function
            .begin_air_conditioner(AirConditionerCommand::Mode(Mode::Fan))
            .unwrap();
        let request = next_air_conditioner(&mut receiver).await;
        request.mark_sent(Step::Mode(Mode::Fan)).unwrap();
        request.reject(CommandError::Timeout);
        assert_eq!(failed.accepted().await, Err(CommandError::Timeout));
        assert_eq!(planning_mode(function), Some(Mode::Dry));
    });
}

#[test]
fn adjacent_unsent_parameters_merge_but_mode_and_power_are_barriers() {
    block_on(async {
        let (device, mut receiver) = air_conditioner();
        let function = &device.functions()[0];
        function.observe_air_conditioner(AirConditionerValue::Power(true), Some(1), 1);
        function.observe_air_conditioner(AirConditionerValue::Mode(Mode::Cool), Some(1), 1);
        let first = function
            .begin_air_conditioner(AirConditionerCommand::Temperature(20))
            .unwrap();
        let latest = function
            .begin_air_conditioner(AirConditionerCommand::Temperature(21))
            .unwrap();
        assert_eq!(first.accepted().await, Err(CommandError::Superseded));
        let barrier = function
            .begin_air_conditioner(AirConditionerCommand::Mode(Mode::Heat))
            .unwrap();
        let next = function
            .begin_air_conditioner(AirConditionerCommand::Temperature(22))
            .unwrap();
        let request = next_air_conditioner(&mut receiver).await;
        assert_eq!(request.command, AirConditionerCommand::Temperature(21));
        request.mark_sent(Step::Temperature(21)).unwrap();
        request.step_accepted(Step::Temperature(21)).unwrap();
        request.accept().unwrap();
        latest.accepted().await.unwrap();
        let request = next_air_conditioner(&mut receiver).await;
        assert_eq!(request.command, AirConditionerCommand::Mode(Mode::Heat));
        request.mark_sent(Step::Mode(Mode::Heat)).unwrap();
        request.step_accepted(Step::Mode(Mode::Heat)).unwrap();
        request.accept().unwrap();
        barrier.accepted().await.unwrap();
        let request = next_air_conditioner(&mut receiver).await;
        assert_eq!(request.command, AirConditionerCommand::Temperature(22));
        request.reject(CommandError::Cancelled);
        assert_eq!(next.accepted().await, Err(CommandError::Cancelled));
    });
}
