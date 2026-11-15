use super::*;

fn instance(model: &str) -> Value {
    let raw = match model {
        "zimi.switch.dhkg01" => include_str!("../../../../tests/fixtures/miot/zimi-dhkg01.json"),
        "zimi.switch.dhkg02" => include_str!("../../../../tests/fixtures/miot/zimi-dhkg02.json"),
        "zimi.switch.dhkg05" => include_str!("../../../../tests/fixtures/miot/zimi-dhkg05.json"),
        "xiaomi.switch.w3" => include_str!("../../../../tests/fixtures/miot/xiaomi-w3.json"),
        "xiaomi.controller.86v1" => {
            include_str!("../../../../tests/fixtures/miot/xiaomi-86v1.json")
        }
        _ => unreachable!(),
    };
    serde_json::from_str(raw).unwrap()
}

#[test]
fn standard_wall_instances_recognize_only_independent_load_relays() {
    for (model, siids) in [
        ("zimi.switch.dhkg01", &[2][..]),
        ("zimi.switch.dhkg02", &[2, 3][..]),
        ("zimi.switch.dhkg05", &[2, 3, 4][..]),
        ("xiaomi.switch.w3", &[2, 3, 4][..]),
        ("xiaomi.controller.86v1", &[10, 11, 12][..]),
    ] {
        let parsed = super::super::parse_device_for_model(key(), model, &instance(model))
            .unwrap_or_else(|error| panic!("{model}: {error}"));
        assert_eq!(
            parsed
                .functions()
                .iter()
                .map(|f| f.id())
                .collect::<Vec<_>>(),
            siids
                .iter()
                .map(|siid| format!("relay:{siid}"))
                .collect::<Vec<_>>(),
            "{model}"
        );
        for (function, siid) in parsed.functions().iter().zip(siids) {
            assert_eq!(
                function.power(),
                Some(PropertyBinding {
                    siid: *siid,
                    piid: 1
                })
            );
            assert_eq!(function.kind(), FunctionKind::Relay);
            assert_eq!(
                function.relay_mode(),
                Some((
                    PropertyBinding {
                        siid: *siid,
                        piid: 2
                    },
                    true
                ))
            );
        }
    }
}

#[test]
fn relay_requires_a_readable_unambiguous_wired_wireless_mode() {
    for alteration in 0..6 {
        let mut raw = instance("zimi.switch.dhkg05");
        let middle = raw["services"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|s| s["iid"] == 3)
            .unwrap();
        let mode = &mut middle["properties"][1];
        match alteration {
            0 => mode["access"] = json!(["write", "notify"]),
            1 => mode["format"] = json!("bool"),
            2 => mode["value-list"][0]["description"] = json!("single click"),
            3 => mode["value-list"][1]["value"] = json!(2),
            4 => mode["value-list"][0]["value"] = json!(1),
            5 => {
                mode["type"] = json!("urn:miot-spec-v2:property:click-mode:00000008:zimi-dhkg05:1")
            }
            _ => unreachable!(),
        }
        let parsed =
            super::super::parse_device_for_model(key(), "zimi.switch.dhkg05", &raw).unwrap();
        assert_eq!(
            parsed
                .functions()
                .iter()
                .map(|f| f.id())
                .collect::<Vec<_>>(),
            ["relay:2", "relay:4"]
        );
        assert!(
            parsed
                .recognition()
                .diagnostics()
                .iter()
                .any(|d| d.target() == "relay:3")
        );
    }
    let mut raw = instance("zimi.switch.dhkg05");
    let middle = raw["services"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|s| s["iid"] == 3)
        .unwrap();
    middle["properties"][1]["access"] = json!(["read"]);
    let parsed = super::super::parse_device_for_model(key(), "zimi.switch.dhkg05", &raw).unwrap();
    assert_eq!(
        parsed.functions()[1].relay_mode(),
        Some((PropertyBinding { siid: 3, piid: 2 }, false))
    );
}

#[test]
fn malformed_relay_does_not_remove_other_services() {
    let mut raw = instance("zimi.switch.dhkg05");
    let services = raw["services"].as_array_mut().unwrap();
    let middle = services.iter_mut().find(|s| s["iid"] == 3).unwrap();
    middle["properties"][0]["access"] = json!(["read", "notify"]);
    let parsed = super::super::parse_device_for_model(key(), "zimi.switch.dhkg05", &raw).unwrap();
    assert_eq!(
        parsed
            .functions()
            .iter()
            .map(|f| f.id())
            .collect::<Vec<_>>(),
        ["relay:2", "relay:4"]
    );
    assert!(
        parsed
            .recognition()
            .diagnostics()
            .iter()
            .any(|d| d.target() == "relay:3")
    );
}
