use crate::xiaomi::legacy_miio::decode_shared_feedback;
use serde_json::json;

#[test]
fn shared_feedback_uses_only_the_two_low_digits_and_keeps_field_isolation() {
    let valid = decode_shared_feedback(&json!(["heat", 31, 26.5])).unwrap();
    assert_eq!(valid.native_mode.value.as_deref(), Some("heat"));
    assert_eq!(valid.fan_speed.blow.level, Some(2));
    assert_eq!(valid.fan_speed.ventilation.level, Some(1));
    assert_eq!(valid.temperature.value, Some(26.5));

    // Leading zero is omitted from the integer representation.
    let leading_zero = decode_shared_feedback(&json!(["heat", 3, 26])).unwrap();
    assert_eq!(leading_zero.fan_speed.blow.level, None);
    assert!(
        leading_zero
            .fan_speed
            .blow
            .reason
            .as_deref()
            .is_some_and(|reason| reason.contains('0'))
    );
    assert_eq!(leading_zero.fan_speed.ventilation.level, Some(2));

    // Higher digits are not part of the two fan levels and must not invalidate
    // either low digit.
    for (raw, blow, ventilation) in [
        (131, Some(2), Some(1)),
        (113, Some(1), Some(2)),
        (331, Some(2), Some(1)),
        (100, None, None),
    ] {
        let reading = decode_shared_feedback(&json!(["heat", raw, 26])).unwrap();
        assert_eq!(reading.fan_speed.blow.level, blow, "{raw}");
        assert_eq!(reading.fan_speed.ventilation.level, ventilation, "{raw}");
        if raw == 100 {
            assert!(reading.fan_speed.blow.reason.is_some(), "{raw}");
            assert!(reading.fan_speed.ventilation.reason.is_some(), "{raw}");
        } else {
            assert!(reading.fan_speed.blow.reason.is_none(), "{raw}");
            assert!(reading.fan_speed.ventilation.reason.is_none(), "{raw}");
        }
    }

    // An invalid ventilation code does not erase the valid blow digit.
    let partial = decode_shared_feedback(&json!(["heat", 32, 26])).unwrap();
    assert_eq!(partial.fan_speed.blow.level, Some(2));
    assert_eq!(partial.fan_speed.ventilation.level, None);
    assert!(partial.fan_speed.ventilation.reason.is_some());

    // Invalid mode or temperature only affects its own field.
    let mixed = decode_shared_feedback(&json!([7, 13, "warm"])).unwrap();
    assert_eq!(mixed.native_mode.value, None);
    assert!(mixed.native_mode.reason.is_some());
    assert_eq!(mixed.temperature.value, None);
    assert!(mixed.temperature.reason.is_some());
    assert_eq!(mixed.fan_speed.blow.level, Some(1));
    assert_eq!(mixed.fan_speed.ventilation.level, Some(2));

    for shape in [
        json!([]),
        json!(["heat", 31]),
        json!(["heat", 31, 26, 1]),
        json!({}),
    ] {
        assert!(decode_shared_feedback(&shape).is_err(), "{shape}");
    }
    for invalid in [json!(["heat", -1, 26]), json!(["heat", 2.5, 26])] {
        let reading = decode_shared_feedback(&invalid).unwrap();
        assert_eq!(reading.fan_speed.blow.level, None, "{invalid}");
        assert_eq!(reading.fan_speed.ventilation.level, None, "{invalid}");
        assert!(reading.fan_speed.blow.reason.is_some(), "{invalid}");
    }
}
