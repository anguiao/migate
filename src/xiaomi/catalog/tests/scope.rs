use super::*;

#[test]
fn scope_exclusion_precedes_auxiliary_services_and_access_is_enforced() {
    let auxiliary = json!([service(
        2,
        "light",
        json!([property(1, "on", "bool", &["read", "write"]),])
    )]);
    assert!(
        compile_spec("yeelink.light.nl1", &spec("night-light", auxiliary.clone()))
            .unwrap()
            .features
            .is_empty()
    );
    assert!(
        compile_spec("vendor.camera.new", &spec("camera", auxiliary))
            .unwrap()
            .features
            .is_empty()
    );
    let read_only = spec(
        "light",
        json!([service(
            2,
            "light",
            json!([property(1, "on", "bool", &["read", "notify"]),])
        )]),
    );
    assert!(
        compile_spec("vendor.light.readonly", &read_only)
            .unwrap()
            .features
            .is_empty()
    );
}

#[test]
fn indicator_and_protection_switches_do_not_create_loads() {
    let document = spec(
        "outlet",
        json!([
            service(
                2,
                "switch",
                json!([property(1, "on", "bool", &["read", "write"])])
            ),
            service(
                4,
                "indicator-light",
                json!([property(1, "on", "bool", &["read", "write"])])
            ),
            service(
                7,
                "power-protect",
                json!([property(1, "on", "bool", &["read", "write"])])
            ),
        ]),
    );
    let compiled = compile_spec("vendor.plug.new", &document).unwrap();
    assert_eq!(compiled.features.len(), 1);
    assert_eq!(compiled.features[0].definition.service_instance, 2);
}

#[test]
fn all_approved_models_compile_the_expected_core_features() {
    let expectations = [
        ("yeelink.light.light3", 1),
        ("yeelink.light.ml9", 1),
        ("yeelink.light.spot2", 1),
        ("xiaomi.light.ceil04", 1),
        ("xiaomi.light.bar2", 1),
        ("devcea.light.ls2307", 1),
        ("lemesh.light.wy0c15", 1),
        ("mijia.light.group3", 1),
        ("xiaomi.switch.w3", 3),
        ("zimi.switch.dhkg01", 1),
        ("zimi.switch.dhkg02", 2),
        ("zimi.switch.dhkg05", 3),
        ("xiaomi.controller.86v1", 3),
        ("cuco.plug.cp7pd", 4),
        ("cuco.plug.v3", 1),
        ("qmi.plug.psv3", 1),
        ("zimi.plug.zncz01", 1),
        ("lumi.acpartner.mcn02", 1),
        ("lumi.acpartner.mcn04", 1),
        ("xiaomi.curtain.acn010", 1),
        ("dmaker.fan.p5c", 1),
        ("miaomiaoce.sensor_ht.t2", 2),
        ("miaomiaoce.sensor_ht.t6", 2),
        ("miaomiaoce.sensor_ht.t8", 2),
        ("miaomiaoce.sensor_ht.t9", 2),
        ("xiaomi.sensor_ht.mini", 2),
        ("xiaomi.motion.pir1", 2),
        ("izq.sensor_occupy.trio", 2),
        ("linp.sensor_occupy.hb01", 2),
        ("xiaomi.sensor_occupy.03", 2),
        ("xiaomi.sensor_occupy.p1", 2),
        ("isa.magnet.dw2hl", 1),
        ("linp.magnet.m1", 1),
        ("xiaomi.vacuum.c104", 1),
        ("yeelink.bhf_light.v13", 4),
    ];
    assert_eq!(expectations.len(), 35);
    for (model, count) in expectations {
        let compiled = compile_spec(model, &public_spec(model)).unwrap();
        assert_eq!(compiled.features.len(), count, "{model}");
    }
    for model in [
        "yeelink.light.light3",
        "yeelink.light.ml9",
        "yeelink.light.spot2",
        "xiaomi.light.ceil04",
        "xiaomi.light.bar2",
        "devcea.light.ls2307",
        "lemesh.light.wy0c15",
        "mijia.light.group3",
    ] {
        assert!(
            compile_spec(model, &public_spec(model))
                .unwrap()
                .features
                .iter()
                .all(|feature| feature.definition.role == FeatureRole::Light),
            "{model}"
        );
    }
    for model in [
        "xiaomi.switch.w3",
        "zimi.switch.dhkg01",
        "zimi.switch.dhkg02",
        "zimi.switch.dhkg05",
        "xiaomi.controller.86v1",
        "cuco.plug.cp7pd",
        "cuco.plug.v3",
        "qmi.plug.psv3",
        "zimi.plug.zncz01",
    ] {
        assert!(
            compile_spec(model, &public_spec(model))
                .unwrap()
                .features
                .iter()
                .all(|feature| feature.definition.role == FeatureRole::Load),
            "{model}"
        );
    }
    for model in ["lumi.acpartner.mcn02", "lumi.acpartner.mcn04"] {
        assert_eq!(
            compile_spec(model, &public_spec(model)).unwrap().features[0]
                .definition
                .role,
            FeatureRole::Climate,
            "{model}"
        );
    }
    for model in [
        "miaomiaoce.sensor_ht.t2",
        "miaomiaoce.sensor_ht.t6",
        "miaomiaoce.sensor_ht.t8",
        "miaomiaoce.sensor_ht.t9",
        "xiaomi.sensor_ht.mini",
    ] {
        let roles = compile_spec(model, &public_spec(model))
            .unwrap()
            .features
            .into_iter()
            .map(|feature| feature.definition.role)
            .collect::<Vec<_>>();
        assert!(
            roles.contains(&FeatureRole::TemperatureSensor)
                && roles.contains(&FeatureRole::HumiditySensor),
            "{model}"
        );
    }
}

#[test]
fn high_risk_model_boundaries_follow_public_specs() {
    let panel = compile_spec(
        "xiaomi.controller.86v1",
        &public_spec("xiaomi.controller.86v1"),
    )
    .unwrap();
    assert_eq!(
        panel
            .features
            .iter()
            .map(|feature| feature.definition.service_instance)
            .collect::<Vec<_>>(),
        vec![10, 11, 12]
    );

    let devcea = compile_spec("devcea.light.ls2307", &public_spec("devcea.light.ls2307")).unwrap();
    assert!(
        !devcea.features[0]
            .definition
            .capabilities
            .0
            .contains(&Capability::Color)
    );
    assert!(
        devcea.features[0]
            .definition
            .capabilities
            .0
            .iter()
            .any(|capability| matches!(capability, Capability::Brightness(_)))
    );

    let p1 = compile_spec(
        "xiaomi.sensor_occupy.p1",
        &public_spec("xiaomi.sensor_occupy.p1"),
    )
    .unwrap();
    assert_eq!(p1.features[0].definition.service_instance, 2);
    let isa = compile_spec("isa.magnet.dw2hl", &public_spec("isa.magnet.dw2hl")).unwrap();
    assert!(
        !isa.features[0]
            .definition
            .capabilities
            .0
            .iter()
            .any(|capability| matches!(capability, Capability::Illuminance(_)))
    );

    let bath = compile_spec(
        "yeelink.bhf_light.v13",
        &public_spec("yeelink.bhf_light.v13"),
    )
    .unwrap();
    let climate = bath
        .features
        .iter()
        .find(|feature| feature.definition.role == FeatureRole::BathHeaterClimate)
        .unwrap();
    assert!(climate.definition.capabilities.0.iter().any(|capability| matches!(capability, Capability::TargetTemperature(range) if range.minimum == 25. && range.maximum == 45.)));
    assert_eq!(
        climate
            .binding
            .encode(&DeviceCommand::SetPower(false))
            .unwrap(),
        vec![WireOperation::SetProperty {
            siid: 3,
            piid: 3,
            value: WireValue::Boolean(false)
        }]
    );
}
