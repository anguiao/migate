use super::*;
use crate::device::{DeviceRequest, FanCommand, FanRequest, FanStep, FanValue};

fn fan(swing: bool) -> (PhysicalDevice, CommandReceiver) {
    let description = PhysicalDeviceDescription::new(
        DeviceKey::new("xiaomi", "fan-commands"),
        "Fan",
        vec![FunctionDescription {
            id: "main".into(),
            label: "Fan".into(),
            kind: FunctionKind::Fan {
                horizontal_swing: swing,
            },
        }],
    )
    .unwrap();
    let (device, receiver) = new_device(description);
    device.set_availability(Availability::Available);
    (device, receiver)
}

async fn next_fan(receiver: &mut CommandReceiver) -> FanRequest {
    match receiver.next_operation().await.unwrap() {
        DeviceRequest::Fan(request) => request,
        _ => panic!("expected fan request"),
    }
}

#[test]
fn fan_merges_only_adjacent_unsent_speeds_and_off_interrupts_sent_speed() {
    block_on(async {
        let (device, mut receiver) = fan(true);
        let function = &device.functions()[0];
        let first = function.begin_fan(FanCommand::Speed(20)).unwrap();
        let unsent = next_fan(&mut receiver).await;
        let second = function.begin_fan(FanCommand::Speed(30)).unwrap();
        assert_eq!(first.accepted().await, Err(CommandError::Superseded));
        assert_eq!(
            unsent.mark_sent(FanStep::Mode),
            Err(CommandError::Superseded)
        );
        drop(unsent);
        let request = next_fan(&mut receiver).await;
        request.mark_sent(FanStep::Power(true)).unwrap();
        let third = function.begin_fan(FanCommand::Speed(40)).unwrap();
        let swing = function.begin_fan(FanCommand::Swing(true)).unwrap();
        let fourth = function.begin_fan(FanCommand::Speed(50)).unwrap();
        let fifth = function.begin_fan(FanCommand::Speed(60)).unwrap();
        assert_eq!(fourth.accepted().await, Err(CommandError::Superseded));
        let off = function.begin_fan(FanCommand::Speed(0)).unwrap();
        assert_eq!(third.accepted().await, Err(CommandError::Superseded));
        assert_eq!(fifth.accepted().await, Err(CommandError::Superseded));
        assert_eq!(second.accepted().await, Err(CommandError::Superseded));
        let after = function.begin_fan(FanCommand::Speed(70)).unwrap();
        assert!(poll_once(receiver.next_operation()).await.is_none());
        assert_eq!(request.step_accepted(), Err(CommandError::Superseded));
        drop(request);
        let request = next_fan(&mut receiver).await;
        assert_eq!(request.command, FanCommand::Swing(true));
        request.mark_sent(FanStep::Swing(true)).unwrap();
        request.step_accepted().unwrap();
        request.accept().unwrap();
        swing.accepted().await.unwrap();
        let request = next_fan(&mut receiver).await;
        assert_eq!(request.command, FanCommand::Speed(0));
        request.mark_sent(FanStep::Power(false)).unwrap();
        request.step_accepted().unwrap();
        request.accept().unwrap();
        off.accepted().await.unwrap();
        let request = next_fan(&mut receiver).await;
        assert_eq!(request.command, FanCommand::Speed(70));
        request.mark_sent(FanStep::Mode).unwrap();
        request.step_accepted().unwrap();
        request.accept().unwrap();
        after.accepted().await.unwrap();
    });
}

#[test]
fn fan_uses_only_observed_power_and_invalidates_cached_on_after_off_is_sent() {
    block_on(async {
        let (device, mut receiver) = fan(true);
        let function = &device.functions()[0];
        function.observe_fan(FanValue::Power(true), Some(1), 1);
        function.observe_fan(FanValue::Mode(1), Some(1), 1);
        function.observe_fan(FanValue::Speed(40), Some(1), 1);
        let before = function.snapshot();
        let off = function.begin_fan(FanCommand::Power(false)).unwrap();
        let request = next_fan(&mut receiver).await;
        request.mark_sent(FanStep::Power(false)).unwrap();
        request.step_accepted().unwrap();
        request.accept().unwrap();
        off.accepted().await.unwrap();
        assert_eq!(function.snapshot(), before);
        let speed = function.begin_fan(FanCommand::Speed(40)).unwrap();
        let request = next_fan(&mut receiver).await;
        assert!(request.needs_power_on());
        request.mark_sent(FanStep::Power(true)).unwrap();
        request.step_accepted().unwrap();
        request.mark_sent(FanStep::Mode).unwrap();
        request.step_accepted().unwrap();
        request.mark_sent(FanStep::Speed(40)).unwrap();
        request.step_accepted().unwrap();
        request.accept().unwrap();
        speed.accepted().await.unwrap();
        assert_eq!(function.snapshot(), before);
        function.observe_fan(FanValue::Power(true), Some(2), 2);
        let action = function.begin_fan(FanCommand::Speed(30)).unwrap();
        let request = next_fan(&mut receiver).await;
        assert!(!request.needs_power_on());
        request.reject(CommandError::Cancelled);
        assert_eq!(action.accepted().await, Err(CommandError::Cancelled));
    });
}

#[test]
fn fan_validates_commands_and_toggle_requires_observed_power() {
    block_on(async {
        let (device, mut receiver) = fan(false);
        let function = &device.functions()[0];
        assert_eq!(
            function.execute(Command::Toggle).await,
            Err(CommandError::UnknownState)
        );
        assert!(matches!(
            function.begin_fan(FanCommand::Speed(101)),
            Err(CommandError::Rejected(_))
        ));
        assert!(matches!(
            function.begin_fan(FanCommand::Swing(true)),
            Err(CommandError::Unsupported)
        ));
        function.observe_fan(FanValue::Power(false), Some(1), 1);
        let action = function.execute(Command::Toggle);
        futures_lite::pin!(action);
        assert!(poll_once(action.as_mut()).await.is_none());
        let request = next_fan(&mut receiver).await;
        assert_eq!(request.command, FanCommand::Power(true));
        request.mark_sent(FanStep::Power(true)).unwrap();
        request.step_accepted().unwrap();
        request.accept().unwrap();
        action.await.unwrap();
        assert_eq!(function.snapshot().fan().unwrap().power.value, Some(false));
    });
}

#[test]
fn dropping_an_execute_future_withdraws_its_queued_power_command() {
    block_on(async {
        let (device, mut receiver) = fan(true);
        let function = &device.functions()[0];
        let first = function.begin_fan(FanCommand::Swing(true)).unwrap();
        let request = next_fan(&mut receiver).await;
        request.mark_sent(FanStep::Swing(true)).unwrap();
        let mut queued = Box::pin(function.execute(Command::On));
        assert!(poll_once(queued.as_mut()).await.is_none());
        drop(queued);
        request.step_accepted().unwrap();
        request.accept().unwrap();
        first.accepted().await.unwrap();
        assert!(poll_once(receiver.next_operation()).await.is_none());
    });
}

#[test]
fn cached_on_alone_does_not_prove_the_fan_is_currently_on() {
    block_on(async {
        let (device, mut receiver) = fan(true);
        let function = &device.functions()[0];
        function.observe_cached_fan(FanValue::Power(true), Some(1), 1);
        let action = function.begin_fan(FanCommand::Speed(30)).unwrap();
        let request = next_fan(&mut receiver).await;
        assert!(request.needs_power_on());
        request.reject(CommandError::Cancelled);
        assert_eq!(action.accepted().await, Err(CommandError::Cancelled));
    });
}
