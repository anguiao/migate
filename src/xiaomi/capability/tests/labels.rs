//! Source parsers use static, bounded function labels for Matter FixedLabel.

use crate::{device::DeviceKey, xiaomi::capability::parse_device};

#[test]
fn parsed_source_function_labels_are_bounded() {
    for data in [
        include_str!("../../../../tests/fixtures/miot/outlet.json"),
        include_str!("../../../../tests/fixtures/miot/cuco-cp7pd.json"),
        include_str!("../../../../tests/fixtures/miot/xiaomi-light-ceil04.json"),
    ] {
        let instance: serde_json::Value = serde_json::from_str(data).unwrap();
        let device = parse_device(DeviceKey::new("xiaomi", "fixture"), &instance).unwrap();
        for function in device.functions() {
            assert!(
                function.label().len() <= 32,
                "source function labels are static and bounded: {}",
                function.label()
            );
        }
    }
}
