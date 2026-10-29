use super::*;
use crate::device::{Availability, DeviceRequest, FanCommand, FanStep};
use rs_matter::dm::AttrChangeNotifier;

#[test]
fn notifications_and_versions_follow_each_actual_projection() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    let identity = store.load_identity().unwrap();
    let (device, mut receivers) = PhysicalDevice::new(
        PhysicalDeviceDescription::new(
            DeviceKey::new("xiaomi", "fan-reports"),
            "Fan",
            vec![FunctionDescription {
                id: "main".into(),
                label: "Fan".into(),
                kind: FunctionKind::Fan {
                    horizontal_swing: true,
                },
            }],
        )
        .unwrap(),
    );
    device.set_availability(Availability::Available);
    let function = &device.functions()[0];
    function.observe_fan(FanValue::Power(true), Some(1), 1);
    function.observe_fan(FanValue::Speed(40), Some(1), 1);
    function.observe_fan(FanValue::Mode(0), Some(1), 1);
    function.observe_fan(FanValue::Swing(false), Some(1), 1);
    let handler = model_handler(vec![single_entry(&device)]);
    let info = basic_info(&identity);
    let matter = Matter::new(&info, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
    let buffers: MatterBuffers = MatterBuffers::new();
    let state: InteractionModelState = InteractionModelState::new();
    let crypto = default_crypto(rand::rng(), DAC_PRIVKEY);
    let kv = matter.kv(StoreAdapter::new(store.matter()));
    let im = InteractionModel::new(&matter, &crypto, &buffers, &handler, &kv, &state);
    let ctx = Context::new(&im, 0x202, 0);
    let power_ctx = Context::new(&im, 6, 0);
    let speed_ctx = Context::new(&im, 0x202, 3);
    block_on(async {
        let version = async |ctx| report_dataver(&read_bytes(&handler, ctx).await.unwrap());
        let on_initial = version(&power_ctx).await;
        let fan_initial = version(&speed_ctx).await;
        im.notify_attr_changed(2, 0x202, 8);
        assert_eq!(version(&speed_ctx).await, fan_initial);
        let mut run = pin!(handler.run(&ctx));
        assert!(poll_once(run.as_mut()).await.is_none());
        assert!(ctx.changes.borrow().is_empty());

        function.observe_fan(FanValue::Speed(41), Some(2), 2);
        assert!(poll_once(run.as_mut()).await.is_none());
        assert_eq!(
            ctx.changes.replace(Vec::new()),
            [(2, 0x202, 2), (2, 0x202, 3), (2, 0x202, 5), (2, 0x202, 6)]
        );
        assert_eq!(version(&power_ctx).await, on_initial);
        let fan_speed = version(&speed_ctx).await;
        assert_ne!(fan_speed, fan_initial);

        function.observe_fan(FanValue::Speed(41), Some(3), 3);
        function.observe_fan(FanValue::Mode(1), Some(3), 3);
        assert!(poll_once(run.as_mut()).await.is_none());
        assert!(ctx.changes.borrow().is_empty());
        assert_eq!(version(&power_ctx).await, on_initial);
        assert_eq!(version(&speed_ctx).await, fan_speed);

        let action = function.begin_fan(FanCommand::Swing(true)).unwrap();
        let DeviceRequest::Fan(request) = receivers[0].next_operation().await.unwrap() else {
            panic!("expected fan request")
        };
        request.mark_sent(FanStep::Swing(true)).unwrap();
        request.step_accepted().unwrap();
        request.accept().unwrap();
        action.accepted().await.unwrap();
        assert!(poll_once(run.as_mut()).await.is_none());
        assert!(ctx.changes.borrow().is_empty());
        assert_eq!(version(&speed_ctx).await, fan_speed);

        function.observe_fan(FanValue::Swing(true), Some(4), 4);
        assert!(poll_once(run.as_mut()).await.is_none());
        assert_eq!(ctx.changes.replace(Vec::new()), [(2, 0x202, 8)]);
        assert_eq!(version(&power_ctx).await, on_initial);
        let fan_swing = version(&speed_ctx).await;
        assert_ne!(fan_swing, fan_speed);

        function.observe_fan(FanValue::Speed(67), Some(5), 5);
        assert!(poll_once(run.as_mut()).await.is_none());
        assert_eq!(
            ctx.changes.replace(Vec::new()),
            [
                (2, 0x202, 0),
                (2, 0x202, 2),
                (2, 0x202, 3),
                (2, 0x202, 5),
                (2, 0x202, 6)
            ]
        );
        assert_eq!(version(&power_ctx).await, on_initial);
        let fan_high = version(&speed_ctx).await;
        assert_ne!(fan_high, fan_swing);

        function.observe_fan(FanValue::Power(false), Some(6), 6);
        assert!(poll_once(run.as_mut()).await.is_none());
        let mut changed = ctx.changes.replace(Vec::new());
        changed.sort_unstable();
        assert_eq!(
            changed,
            [
                (2, 6, 0),
                (2, 0x202, 0),
                (2, 0x202, 2),
                (2, 0x202, 3),
                (2, 0x202, 5),
                (2, 0x202, 6)
            ]
        );
        let on_off = version(&power_ctx).await;
        let fan_off = version(&speed_ctx).await;
        assert_ne!(on_off, on_initial);
        assert_ne!(fan_off, fan_high);
        function.observe_fan(FanValue::Speed(80), Some(7), 7);
        function.observe_fan(FanValue::Mode(0), Some(7), 7);
        assert!(poll_once(run.as_mut()).await.is_none());
        assert!(ctx.changes.borrow().is_empty());
        assert_eq!(version(&power_ctx).await, on_off);
        assert_eq!(version(&speed_ctx).await, fan_off);

        function.observe_fan(FanValue::Power(true), Some(8), 8);
        assert!(poll_once(run.as_mut()).await.is_none());
        let mut changed = ctx.changes.replace(Vec::new());
        changed.sort_unstable();
        assert_eq!(
            changed,
            [
                (2, 6, 0),
                (2, 0x202, 0),
                (2, 0x202, 2),
                (2, 0x202, 3),
                (2, 0x202, 5),
                (2, 0x202, 6)
            ]
        );
        assert_ne!(version(&power_ctx).await, on_off);
        assert_ne!(version(&speed_ctx).await, fan_off);
        assert_eq!(
            report_value(&read_bytes(&handler, &speed_ctx).await.unwrap())
                .u8()
                .unwrap(),
            80
        );
    });
}
