//! Shared property-value boundary, including supplied update times.

use crate::xiaomi::property::{update_time, update_time_map};
use serde_json::json;

#[test]
fn update_times_are_explicit_opaque_values() {
    let valid = json!({"value": 21.5, "updateTime": 1_700_000_000});
    assert_eq!(update_time(&valid), Some(1_700_000_000));
    assert_eq!(
        update_time_map(valid.as_object().unwrap()),
        Some(1_700_000_000)
    );

    // A bare scalar is not an attribute update time.
    assert_eq!(update_time(&json!(1_700_000_000)), None);
    assert_eq!(update_time(&json!({"value": 21.5})), None);

    // Source time is an opaque diagnostic i64; terminal rendering must not
    // assume RFC 3339 representability.
    for preserved in [i64::MAX, i64::MIN, 253_402_300_800] {
        let value = json!({"value": 21.5, "updateTime": preserved});
        assert_eq!(update_time(&value), Some(preserved), "{preserved}");
        assert_eq!(
            update_time_map(value.as_object().unwrap()),
            Some(preserved),
            "{preserved}"
        );
    }
}
