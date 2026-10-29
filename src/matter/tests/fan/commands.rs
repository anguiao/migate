use super::*;
use crate::device::{
    Availability, CommandError, CommandReceiver, DeviceRequest, FanCommand, FanRequest, FanStep,
};
use rs_matter::{dm::InvokeReplyInstance, tlv::TLVElement, utils::storage::WriteBuf};

macro_rules! setup {
    ($swing:expr, $device:ident, $receiver:ident, $handler:ident, $im:ident) => {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let identity = store.load_identity().unwrap();
        let ($device, mut receivers) = PhysicalDevice::new(
            PhysicalDeviceDescription::new(
                DeviceKey::new("xiaomi", "fan-command"),
                "Fan",
                vec![FunctionDescription {
                    id: "main".into(),
                    label: "Fan".into(),
                    kind: FunctionKind::Fan {
                        horizontal_swing: $swing,
                    },
                }],
            )
            .unwrap(),
        );
        let mut $receiver = receivers.remove(0);
        $device.set_availability(Availability::Available);
        let $handler = model_handler(vec![single_entry(&$device)]);
        let info = basic_info(&identity);
        let matter = Matter::new(&info, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
        let buffers: MatterBuffers = MatterBuffers::new();
        let state: InteractionModelState = InteractionModelState::new();
        let crypto = default_crypto(rand::rng(), DAC_PRIVKEY);
        let kv = matter.kv(StoreAdapter::new(store.matter()));
        let $im = InteractionModel::new(&matter, &crypto, &buffers, &$handler, &kv, &state);
    };
}

async fn next_fan(receiver: &mut CommandReceiver) -> FanRequest {
    let DeviceRequest::Fan(request) = receiver.next_operation().await.unwrap() else {
        panic!("expected fan request")
    };
    request
}

#[test]
fn writes_dispatch_fan_commands_and_wait_for_all_receipts() {
    setup!(true, device, receiver, handler, im);
    let function = &device.functions()[0];
    function.observe_fan(FanValue::Power(true), Some(1), 1);
    function.observe_fan(FanValue::Speed(40), Some(1), 1);
    function.observe_fan(FanValue::Mode(1), Some(1), 1);
    function.observe_fan(FanValue::Swing(false), Some(1), 1);
    block_on(async {
        for (attribute, raw, command) in [
            (2, 1, FanCommand::Speed(1)),
            (2, 40, FanCommand::Speed(40)),
            (5, 100, FanCommand::Speed(100)),
            (5, 0, FanCommand::Speed(0)),
            (0, 0, FanCommand::Power(false)),
            (0, 1, FanCommand::Speed(33)),
            (0, 2, FanCommand::Speed(66)),
            (0, 3, FanCommand::Speed(100)),
            (0, 4, FanCommand::Speed(100)),
            (0, 6, FanCommand::Speed(100)),
            (8, 0, FanCommand::Swing(false)),
            (8, 1, FanCommand::Swing(true)),
        ] {
            let bytes = [0x04, raw];
            let mut ctx = Context::new(&im, 0x202, attribute);
            ctx.data = TLVElement::new(&bytes);
            let mut write = pin!(handler.write(&ctx));
            assert!(
                poll_once(write.as_mut()).await.is_none(),
                "attribute {attribute}, value {raw}"
            );
            let request = next_fan(&mut receiver).await;
            assert_eq!(request.command, command);
            match command {
                FanCommand::Speed(speed @ 1..=100) => {
                    if request.needs_power_on() {
                        request.mark_sent(FanStep::Power(true)).unwrap();
                        request.step_accepted().unwrap();
                        assert!(poll_once(write.as_mut()).await.is_none());
                    }
                    request.mark_sent(FanStep::Mode).unwrap();
                    request.step_accepted().unwrap();
                    assert!(poll_once(write.as_mut()).await.is_none());
                    request.mark_sent(FanStep::Speed(speed)).unwrap();
                    request.step_accepted().unwrap();
                }
                FanCommand::Power(power) => {
                    request.mark_sent(FanStep::Power(power)).unwrap();
                    request.step_accepted().unwrap();
                }
                FanCommand::Speed(0) => {
                    request.mark_sent(FanStep::Power(false)).unwrap();
                    request.step_accepted().unwrap();
                }
                FanCommand::Speed(_) => unreachable!(),
                FanCommand::Swing(swing) => {
                    request.mark_sent(FanStep::Swing(swing)).unwrap();
                    request.step_accepted().unwrap();
                }
            }
            assert!(poll_once(write.as_mut()).await.is_none());
            request.accept().unwrap();
            write.await.unwrap();
        }
        let observed = function.snapshot().fan().unwrap();
        assert_eq!(observed.power.value, Some(true));
        assert_eq!(observed.speed.value, Some(40));
        assert_eq!(observed.mode.value, Some(1));
        assert_eq!(observed.swing.value, Some(false));
    });
}

#[test]
fn invalid_writes_are_rejected_without_queueing_or_changing_observations() {
    setup!(true, device, receiver, handler, im);
    let function = &device.functions()[0];
    function.observe_fan(FanValue::Power(true), Some(1), 1);
    function.observe_fan(FanValue::Speed(40), Some(1), 1);
    function.observe_fan(FanValue::Mode(1), Some(1), 1);
    function.observe_fan(FanValue::Swing(false), Some(1), 1);
    let before = function.snapshot();
    block_on(async {
        for (attribute, bytes, error) in [
            (2, &[0x14][..], ErrorCode::InvalidInState),
            (5, &[0x14][..], ErrorCode::InvalidInState),
            (2, &[0x04, 101][..], ErrorCode::ConstraintError),
            (5, &[0x04, 255][..], ErrorCode::ConstraintError),
            (0, &[0x04, 5][..], ErrorCode::ConstraintError),
            (0, &[0x04, 7][..], ErrorCode::ConstraintError),
            (0, &[0x04, 255][..], ErrorCode::ConstraintError),
            (8, &[0x04, 2][..], ErrorCode::ConstraintError),
            (8, &[0x04, 4][..], ErrorCode::ConstraintError),
            (8, &[0x04, 0x80][..], ErrorCode::ConstraintError),
            (8, &[0x04, 3][..], ErrorCode::ConstraintError),
        ] {
            let mut ctx = Context::new(&im, 0x202, attribute);
            ctx.data = TLVElement::new(bytes);
            assert_eq!(
                handler.write(&ctx).await.unwrap_err().code(),
                error,
                "attribute {attribute}, bytes {bytes:?}"
            );
            assert!(poll_once(receiver.next_operation()).await.is_none());
        }
        assert_eq!(function.snapshot(), before);
    });
}

#[test]
fn write_failures_and_timeouts_are_returned_to_the_caller() {
    setup!(true, _device, receiver, handler, im);
    block_on(async {
        for (failure, expected) in [
            (CommandError::Timeout, ErrorCode::RxTimeout),
            (CommandError::Rejected("failed".into()), ErrorCode::Failure),
        ] {
            let bytes = [0x04, 42];
            let mut ctx = Context::new(&im, 0x202, 2);
            ctx.data = TLVElement::new(&bytes);
            let mut write = pin!(handler.write(&ctx));
            assert!(poll_once(write.as_mut()).await.is_none());
            let request = next_fan(&mut receiver).await;
            request.mark_sent(FanStep::Power(true)).unwrap();
            request.reject(failure);
            assert_eq!(write.await.unwrap_err().code(), expected);
        }
    });
}

#[test]
fn on_off_commands_remain_independent_and_toggle_requires_observation() {
    setup!(true, device, receiver, handler, im);
    let function = &device.functions()[0];
    block_on(async {
        let mut ctx = Context::new(&im, 6, 0);
        ctx.command.cmd_id = 2;
        let mut out = [0; 128];
        assert_eq!(
            handler
                .invoke(
                    &ctx,
                    InvokeReplyInstance::new(&ctx.command, WriteBuf::new(&mut out))
                )
                .await
                .unwrap_err()
                .code(),
            ErrorCode::Failure
        );
        assert!(poll_once(receiver.next_operation()).await.is_none());
        function.observe_fan(FanValue::Power(true), Some(1), 1);
        for (id, target) in [(0, false), (1, true), (2, false)] {
            ctx.command.cmd_id = id;
            let mut out = [0; 128];
            let mut invoke = pin!(handler.invoke(
                &ctx,
                InvokeReplyInstance::new(&ctx.command, WriteBuf::new(&mut out))
            ));
            assert!(poll_once(invoke.as_mut()).await.is_none());
            let request = next_fan(&mut receiver).await;
            assert_eq!(request.command, FanCommand::Power(target));
            request.mark_sent(FanStep::Power(target)).unwrap();
            request.step_accepted().unwrap();
            request.accept().unwrap();
            invoke.await.unwrap();
        }
        assert_eq!(function.snapshot().fan().unwrap().power.value, Some(true));
    });
}
