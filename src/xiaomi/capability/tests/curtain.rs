use crate::{
    device::{Command, CommandError, CurtainMotion, DeviceKey, FunctionKind, PhysicalDevice},
    xiaomi::capability::{NumericFormat, PropertyBinding, parse_device_for_model},
};
use serde_json::{Value, json};

fn instance() -> Value {
    serde_json::from_str(include_str!(
        "../../../../tests/fixtures/miot/xiaomi-acn010.json"
    ))
    .unwrap()
}

fn parse(instance: &Value) -> Result<super::super::ParsedDevice, super::super::CapabilityError> {
    parse_device_for_model(
        DeviceKey::new("xiaomi", "curtain"),
        "xiaomi.curtain.acn010",
        instance,
    )
}

#[test]
fn curtain_public_description_creates_one_control_without_power() {
    let parsed = parse(&instance()).unwrap();
    assert_eq!(parsed.functions().len(), 1);
    let function = &parsed.functions()[0];
    assert_eq!(function.id(), "curtain:2");
    assert!(function.kind().is_control());
    assert_eq!(function.kind(), FunctionKind::Curtain);
    assert_eq!(function.power(), None);
    let curtain = function.curtain().unwrap();
    assert_eq!(
        curtain.motor().property(),
        PropertyBinding { siid: 2, piid: 1 }
    );
    assert_eq!(curtain.motor().format(), NumericFormat::Uint8);
    assert_eq!(curtain.motor().pause(), 2);
    assert_eq!(
        curtain.status().property(),
        PropertyBinding { siid: 2, piid: 2 }
    );
    assert_eq!(
        curtain.status().read_value(&json!(0)).unwrap(),
        CurtainMotion::Opening
    );
    assert_eq!(
        curtain.status().read_value(&json!(1)).unwrap(),
        CurtainMotion::Closing
    );
    assert_eq!(
        curtain.status().read_value(&json!(2)).unwrap(),
        CurtainMotion::Stopped
    );
    assert!(curtain.status().read_value(&json!(3)).is_err());
    for (position, piid) in [(curtain.current(), 3), (curtain.target(), 4)] {
        assert_eq!(position.property(), PropertyBinding { siid: 2, piid });
        assert_eq!(position.format(), NumericFormat::Uint8);
        assert_eq!(
            (
                position.range().min(),
                position.range().max(),
                position.range().step()
            ),
            (0, 100, 1)
        );
        assert_eq!(position.read_raw(&json!(37)).unwrap(), 37);
        assert!(position.read_raw(&json!(101)).is_err());
        assert_eq!(position.quantize(37.6), 38);
    }
    let (device, _) = PhysicalDevice::new(parsed.physical_description("Curtain"));
    let function = &device.functions()[0];
    let snapshot = function.snapshot();
    assert_eq!(snapshot.power(), None);
    let curtain = snapshot.curtain().unwrap();
    assert_eq!(curtain.current_position.value, None);
    assert_eq!(curtain.target_position.value, None);
    assert_eq!(curtain.status.value, None);
    assert_eq!(
        futures_lite::future::block_on(function.execute(Command::On)),
        Err(CommandError::Unsupported)
    );
}

#[test]
fn curtain_requires_each_property_permissions_and_semantics() {
    let original = instance();
    for index in 0..4 {
        let mut missing = original.clone();
        missing["services"][1]["properties"]
            .as_array_mut()
            .unwrap()
            .remove(index);
        assert!(parse(&missing).is_err(), "missing property {index}");
    }
    for index in 0..4 {
        let mut invalid = original.clone();
        invalid["services"][1]["properties"][index]["access"] = json!([]);
        assert!(parse(&invalid).is_err(), "missing access {index}");
    }
    let mut missing_pause = original.clone();
    missing_pause["services"][1]["properties"][0]["value-list"][2]["description"] = json!("Other");
    assert!(parse(&missing_pause).is_err());
    let mut missing_status = original.clone();
    missing_status["services"][1]["properties"][1]["value-list"][0]["description"] = json!("Other");
    assert!(parse(&missing_status).is_err());
    let mut invalid_range = original.clone();
    invalid_range["services"][1]["properties"][3]["value-range"] = json!([1, 100, 1]);
    assert!(parse(&invalid_range).is_err());
    let mut ambiguous_pause = original;
    ambiguous_pause["services"][1]["properties"][0]["value-list"][3]["value"] = json!(2);
    assert!(parse(&ambiguous_pause).is_err());
}

#[test]
fn position_grid_quantizes_writes_without_rounding_reports() {
    let mut description = instance();
    description["services"][1]["properties"][3]["value-range"] = json!([0, 100, 5]);
    let parsed = parse(&description).unwrap();
    let target = parsed.functions()[0].curtain().unwrap().target();
    assert_eq!(target.read_raw(&json!(37)).unwrap(), 37);
    assert_eq!(target.quantize(37.6), 40);
    assert_eq!(target.quantize(0.0), 0);
    assert_eq!(target.quantize(100.0), 100);
}
