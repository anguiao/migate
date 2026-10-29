//! Source parsers use static, bounded function labels for Matter FixedLabel.

use crate::{device::DeviceKey, xiaomi::capability::parse_device_for_model};

#[test]
fn parsed_source_function_labels_are_bounded() {
    for (model, data) in [
        (
            "cuco.plug.v3",
            include_str!("../../../../tests/fixtures/miot/outlet.json"),
        ),
        (
            "cuco.plug.cp7pd",
            include_str!("../../../../tests/fixtures/miot/cuco-cp7pd.json"),
        ),
        (
            "xiaomi.light.ceil04",
            include_str!("../../../../tests/fixtures/miot/xiaomi-light-ceil04.json"),
        ),
    ] {
        let instance: serde_json::Value = serde_json::from_str(data).unwrap();
        let device =
            parse_device_for_model(DeviceKey::new("xiaomi", "fixture"), model, &instance).unwrap();
        for function in device.functions() {
            assert!(
                function.label().len() <= 32,
                "source function labels are static and bounded: {}",
                function.label()
            );
        }
    }
}
