use super::support::{Context, entry, model_handler, read_bytes, report_value};
use super::*;
use crate::device::FanValue;
use futures_lite::future::block_on;
use serde_json::Value;

const MODEL: &str = "yeelink.bhf_light.v13";

fn v13() -> Value {
    serde_json::from_str(include_str!(
        "../../../tests/fixtures/miot/yeelink-bhf-light-v13.json"
    ))
    .unwrap()
}

#[test]
fn two_independent_fan_endpoints_keep_speed_max_two_and_off_low_high() {
    use crate::{matter::storage::StoreAdapter, storage::Store};
    use rs_matter::{
        MATTER_PORT, Matter,
        crypto::default_crypto,
        dm::devices::test::{DAC_PRIVKEY, TEST_DEV_ATT, TEST_DEV_COMM},
        im::{InteractionModel, InteractionModelState},
        transport::exchange::MatterBuffers,
    };

    let source = crate::xiaomi::capability::tests::source::SourceFixture::new("bath", MODEL, v13());
    let (physical, _) = PhysicalDevice::new(source.native.device().physical_description("Bath"));
    for (id, speed) in [("fan:3:2", 2), ("fan:3:4", 1)] {
        let function = physical.function(id).unwrap();
        function.observe_fan(FanValue::Power(true), Some(1), 1);
        function.observe_fan(FanValue::Speed(speed), Some(1), 1);
    }
    let frozen = entry(&physical, 10, &[("fan:3:2", 11), ("fan:3:4", 12)]);

    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    let identity = store.load_identity().unwrap();
    let info = crate::matter::model::basic_info(&identity);
    let matter = Matter::new(&info, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
    let buffers: MatterBuffers = MatterBuffers::new();
    let state: InteractionModelState = InteractionModelState::new();
    let crypto = default_crypto(rand::rng(), DAC_PRIVKEY);
    let kv = matter.kv(StoreAdapter::new(store.matter()));
    let handler = model_handler(vec![frozen]);
    let im = InteractionModel::new(&matter, &crypto, &buffers, &handler, &kv, &state);

    physical
        .function("fan:3:2")
        .unwrap()
        .observe_fan(FanValue::Power(false), Some(2), 2);
    for (endpoint, expected_mode) in [(11, 1), (12, 1)] {
        block_on(async {
            let speed_max = Context::at(&im, endpoint, 0x202, 4);
            assert_eq!(
                report_value(&read_bytes(&handler, &speed_max).await.unwrap())
                    .u8()
                    .unwrap(),
                2
            );
            let sequence = Context::at(&im, endpoint, 0x202, 1);
            assert_eq!(
                report_value(&read_bytes(&handler, &sequence).await.unwrap())
                    .u8()
                    .unwrap(),
                expected_mode
            );
            let setting = Context::at(&im, endpoint, 0x202, 2);
            let percent = report_value(&read_bytes(&handler, &setting).await.unwrap())
                .u8()
                .unwrap();
            if endpoint == 11 {
                // Closed is 0% even though the remembered high level remains.
                assert_eq!(percent, 0);
                assert_eq!(
                    physical
                        .function("fan:3:2")
                        .unwrap()
                        .snapshot()
                        .fan()
                        .unwrap()
                        .speed
                        .value,
                    Some(2)
                );
            } else {
                assert_eq!(percent, 50);
            }
        });
    }
}
