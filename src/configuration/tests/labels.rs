use super::super::default_device_label;

#[test]
fn default_labels_are_bounded_utf8_and_include_model_and_device_id() {
    assert_eq!(
        default_device_label("cuco.plug.v3", "571329355"),
        "cuco.plug.v3 (571329355)"
    );
    for (model, id) in [
        ("插座设备".repeat(20), "设备标识".repeat(20)),
        ("x".repeat(100), "1".repeat(128)),
    ] {
        let label = default_device_label(&model, &id);
        assert!(label.len() <= 32);
        assert!(label.ends_with(')'));
        assert!(label.contains('('));
    }
}
