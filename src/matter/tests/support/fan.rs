use crate::device::{
    CommandReceiver, DeviceKey, FanValue, Function, FunctionDescription, FunctionKind,
    PhysicalDevice, PhysicalDeviceDescription,
};

pub(in crate::matter::tests) fn fan_device(
    did: &str,
    swing: bool,
) -> (PhysicalDevice, CommandReceiver) {
    let (device, mut receivers) = PhysicalDevice::new(
        PhysicalDeviceDescription::new(
            DeviceKey::new("xiaomi", did),
            "Fan",
            vec![FunctionDescription {
                id: "main".into(),
                label: "Fan".into(),
                kind: FunctionKind::Fan {
                    horizontal_swing: swing,
                },
            }],
        )
        .unwrap(),
    );
    (device, receivers.remove(0))
}

pub(in crate::matter::tests) fn observe_fan_state(
    function: &Function,
    power: bool,
    speed: u8,
    mode: u32,
    swing: bool,
) {
    for value in [
        FanValue::Power(power),
        FanValue::Speed(speed),
        FanValue::Mode(mode),
        FanValue::Swing(swing),
    ] {
        function.observe_fan(value, Some(1), 1);
    }
}

// Keep the borrowed protocol resources in the caller's scope.
macro_rules! fan_model {
    ($device:expr, $handler:ident, $im:ident) => {
        use rs_matter::{
            MATTER_PORT, Matter,
            crypto::default_crypto,
            dm::devices::test::{DAC_PRIVKEY, TEST_DEV_ATT, TEST_DEV_COMM},
            im::{InteractionModel, InteractionModelState},
            transport::exchange::MatterBuffers,
        };
        use $crate::{matter::storage::StoreAdapter, storage::Store};
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let identity = store.load_identity().unwrap();
        let info = $crate::matter::model::basic_info(&identity);
        let matter = Matter::new(&info, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
        let buffers: MatterBuffers = MatterBuffers::new();
        let state: InteractionModelState = InteractionModelState::new();
        let crypto = default_crypto(rand::rng(), DAC_PRIVKEY);
        let kv = matter.kv(StoreAdapter::new(store.matter()));
        let $handler = $crate::matter::tests::support::model_handler(vec![
            $crate::matter::tests::support::single_entry($device),
        ]);
        let $im = InteractionModel::new(&matter, &crypto, &buffers, &$handler, &kv, &state);
    };
}
pub(in crate::matter::tests) use fan_model;
