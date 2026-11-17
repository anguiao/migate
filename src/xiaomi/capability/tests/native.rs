use crate::{
    device::DeviceKey,
    xiaomi::capability::{
        NativeDescription, ParsedDevice, ParsedFunction, tests::source::SourceFixture,
    },
};

fn generic_heater() -> SourceFixture {
    SourceFixture::new(
        "heater",
        "vendor.heater",
        serde_json::from_str(include_str!(
            "../../../../tests/fixtures/miot/yeelink-bhf-light-v13.json"
        ))
        .unwrap(),
    )
}

#[test]
fn function_label_changes_reuse_the_complete_native_bindings() {
    let source = generic_heater();
    let heater = source
        .device()
        .functions()
        .iter()
        .find_map(|function| function.heater())
        .unwrap();
    let description = |label| {
        NativeDescription::new(
            source.model().into(),
            source.instance_type().into(),
            ParsedDevice::new(
                DeviceKey::new("xiaomi", "heater"),
                vec![ParsedFunction::heater_control(label, heater.clone())],
                None,
            )
            .unwrap(),
        )
        .unwrap()
    };
    let original = description("Bathroom");
    let renamed = description("Guest bath");
    assert_ne!(original, renamed);
    assert!(original.same_control_as(&renamed));
    assert!(renamed.same_control_as(&original));
}
