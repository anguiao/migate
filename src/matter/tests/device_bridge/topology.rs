use super::*;

#[test]
fn empty_bridge_has_only_root_and_aggregator() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    let model =
        DeviceBridgeModel::new(DeviceService::new(), store.devices(), store.matter()).unwrap();
    model.access(|node| {
        assert_eq!(
            node.endpoints
                .iter()
                .map(|endpoint| endpoint.id)
                .collect::<Vec<_>>(),
            [0, 1]
        )
    });
}

#[test]
fn all_approved_fixtures_publish_each_compiled_functional_endpoint() {
    block_on(async {
        let specs = [
            (
                "cuco.plug.cp7pd",
                include_str!("../../../../tests/fixtures/miot_specs/cuco.plug.cp7pd.json"),
            ),
            (
                "cuco.plug.v3",
                include_str!("../../../../tests/fixtures/miot_specs/cuco.plug.v3.json"),
            ),
            (
                "devcea.light.ls2307",
                include_str!("../../../../tests/fixtures/miot_specs/devcea.light.ls2307.json"),
            ),
            (
                "dmaker.fan.p5c",
                include_str!("../../../../tests/fixtures/miot_specs/dmaker.fan.p5c.json"),
            ),
            (
                "isa.magnet.dw2hl",
                include_str!("../../../../tests/fixtures/miot_specs/isa.magnet.dw2hl.json"),
            ),
            (
                "izq.sensor_occupy.trio",
                include_str!("../../../../tests/fixtures/miot_specs/izq.sensor_occupy.trio.json"),
            ),
            (
                "lemesh.light.wy0c15",
                include_str!("../../../../tests/fixtures/miot_specs/lemesh.light.wy0c15.json"),
            ),
            (
                "linp.magnet.m1",
                include_str!("../../../../tests/fixtures/miot_specs/linp.magnet.m1.json"),
            ),
            (
                "linp.sensor_occupy.hb01",
                include_str!("../../../../tests/fixtures/miot_specs/linp.sensor_occupy.hb01.json"),
            ),
            (
                "lumi.acpartner.mcn02",
                include_str!("../../../../tests/fixtures/miot_specs/lumi.acpartner.mcn02.json"),
            ),
            (
                "lumi.acpartner.mcn04",
                include_str!("../../../../tests/fixtures/miot_specs/lumi.acpartner.mcn04.json"),
            ),
            (
                "miaomiaoce.sensor_ht.t2",
                include_str!("../../../../tests/fixtures/miot_specs/miaomiaoce.sensor_ht.t2.json"),
            ),
            (
                "miaomiaoce.sensor_ht.t6",
                include_str!("../../../../tests/fixtures/miot_specs/miaomiaoce.sensor_ht.t6.json"),
            ),
            (
                "miaomiaoce.sensor_ht.t8",
                include_str!("../../../../tests/fixtures/miot_specs/miaomiaoce.sensor_ht.t8.json"),
            ),
            (
                "miaomiaoce.sensor_ht.t9",
                include_str!("../../../../tests/fixtures/miot_specs/miaomiaoce.sensor_ht.t9.json"),
            ),
            (
                "mijia.light.group3",
                include_str!("../../../../tests/fixtures/miot_specs/mijia.light.group3.json"),
            ),
            (
                "qmi.plug.psv3",
                include_str!("../../../../tests/fixtures/miot_specs/qmi.plug.psv3.json"),
            ),
            (
                "xiaomi.controller.86v1",
                include_str!("../../../../tests/fixtures/miot_specs/xiaomi.controller.86v1.json"),
            ),
            (
                "xiaomi.curtain.acn010",
                include_str!("../../../../tests/fixtures/miot_specs/xiaomi.curtain.acn010.json"),
            ),
            (
                "xiaomi.light.bar2",
                include_str!("../../../../tests/fixtures/miot_specs/xiaomi.light.bar2.json"),
            ),
            (
                "xiaomi.light.ceil04",
                include_str!("../../../../tests/fixtures/miot_specs/xiaomi.light.ceil04.json"),
            ),
            (
                "xiaomi.motion.pir1",
                include_str!("../../../../tests/fixtures/miot_specs/xiaomi.motion.pir1.json"),
            ),
            (
                "xiaomi.sensor_ht.mini",
                include_str!("../../../../tests/fixtures/miot_specs/xiaomi.sensor_ht.mini.json"),
            ),
            (
                "xiaomi.sensor_occupy.03",
                include_str!("../../../../tests/fixtures/miot_specs/xiaomi.sensor_occupy.03.json"),
            ),
            (
                "xiaomi.sensor_occupy.p1",
                include_str!("../../../../tests/fixtures/miot_specs/xiaomi.sensor_occupy.p1.json"),
            ),
            (
                "xiaomi.switch.w3",
                include_str!("../../../../tests/fixtures/miot_specs/xiaomi.switch.w3.json"),
            ),
            (
                "xiaomi.vacuum.c104",
                include_str!("../../../../tests/fixtures/miot_specs/xiaomi.vacuum.c104.json"),
            ),
            (
                "yeelink.bhf_light.v13",
                include_str!("../../../../tests/fixtures/miot_specs/yeelink.bhf_light.v13.json"),
            ),
            (
                "yeelink.light.light3",
                include_str!("../../../../tests/fixtures/miot_specs/yeelink.light.light3.json"),
            ),
            (
                "yeelink.light.ml9",
                include_str!("../../../../tests/fixtures/miot_specs/yeelink.light.ml9.json"),
            ),
            (
                "yeelink.light.spot2",
                include_str!("../../../../tests/fixtures/miot_specs/yeelink.light.spot2.json"),
            ),
            (
                "zimi.plug.zncz01",
                include_str!("../../../../tests/fixtures/miot_specs/zimi.plug.zncz01.json"),
            ),
            (
                "zimi.switch.dhkg01",
                include_str!("../../../../tests/fixtures/miot_specs/zimi.switch.dhkg01.json"),
            ),
            (
                "zimi.switch.dhkg02",
                include_str!("../../../../tests/fixtures/miot_specs/zimi.switch.dhkg02.json"),
            ),
            (
                "zimi.switch.dhkg05",
                include_str!("../../../../tests/fixtures/miot_specs/zimi.switch.dhkg05.json"),
            ),
        ];
        assert_eq!(specs.len(), 35);
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let service = DeviceService::new();
        let mut features = Vec::new();
        for (model, document) in specs {
            for descriptor in compile_spec(model, document).unwrap().features {
                let id = FeatureIdentity {
                    physical: PhysicalDeviceId {
                        account: AccountId::new("u").unwrap(),
                        home: HomeId::new("h").unwrap(),
                        parent_did: DeviceDid::new(model).unwrap(),
                    },
                    service_instance: descriptor.definition.service_instance,
                    role: descriptor.definition.role,
                };
                service.publish(
                    id.clone(),
                    descriptor.definition.name,
                    descriptor.definition.capabilities,
                );
                store.devices().allocate_feature(&id).unwrap();
                features.push(id);
            }
        }
        assert_eq!(features.len(), 58);
        let model = DeviceBridgeModel::new(service, store.devices(), store.matter()).unwrap();
        let identity = store.load_identity().unwrap();
        let basic_info = crate::matter::common::basic_info(&identity);
        let matter = Matter::new(&basic_info, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
        let buffers: MatterBuffers = MatterBuffers::new();
        let state: EthInteractionModelState =
            EthInteractionModelState::new(EthNetwork::new_default());
        let crypto = test_only_crypto();
        let kv = matter.kv(crate::matter::storage::StoreAdapter::new(store.matter()));
        crate::matter::common::initialize_basic_info(&matter, &kv, true).unwrap();
        let im = InteractionModel::new(&matter, &crypto, &buffers, (&model, &model), &kv, &state);
        for feature in features {
            let endpoint = model
                .endpoint_for(&feature)
                .unwrap_or_else(|| panic!("missing endpoint for {feature:?}"));
            let (cluster, attribute, expected_device_types): (u32, u32, &[u16]) = match feature.role
            {
                FeatureRole::Light | FeatureRole::BathHeaterLight => (
                    on_off::FULL_CLUSTER.id,
                    on_off::AttributeId::OnOff as _,
                    &[0x0100, 0x0101, 0x010c, 0x010d],
                ),
                FeatureRole::Load => (
                    on_off::FULL_CLUSTER.id,
                    on_off::AttributeId::OnOff as _,
                    &[0x010a],
                ),
                FeatureRole::Climate | FeatureRole::BathHeaterClimate => (
                    thermostat::FULL_CLUSTER.id,
                    thermostat::AttributeId::SystemMode as _,
                    &[0x0301],
                ),
                FeatureRole::Curtain => (
                    window_covering::FULL_CLUSTER.id,
                    window_covering::AttributeId::CurrentPositionLiftPercent100ths as _,
                    &[0x0202],
                ),
                FeatureRole::Fan
                | FeatureRole::BathHeaterSupplyFan
                | FeatureRole::BathHeaterExhaustFan => (
                    fan_control::FULL_CLUSTER.id,
                    fan_control::AttributeId::FanMode as _,
                    &[0x002b],
                ),
                FeatureRole::TemperatureSensor => (
                    temperature_measurement::FULL_CLUSTER.id,
                    temperature_measurement::AttributeId::MeasuredValue as _,
                    &[0x0302],
                ),
                FeatureRole::HumiditySensor => (
                    relative_humidity_measurement::FULL_CLUSTER.id,
                    relative_humidity_measurement::AttributeId::MeasuredValue as _,
                    &[0x0307],
                ),
                FeatureRole::IlluminanceSensor => (0x0400, 0, &[0x0106]),
                FeatureRole::MotionSensor | FeatureRole::OccupancySensor => (
                    occupancy_sensing::FULL_CLUSTER.id,
                    occupancy_sensing::AttributeId::Occupancy as _,
                    &[0x0107],
                ),
                FeatureRole::ContactSensor => (0x0045, 0, &[0x0015]),
                FeatureRole::Vacuum => (
                    rvc_run_mode::FULL_CLUSTER.id,
                    rvc_run_mode::AttributeId::SupportedModes as _,
                    &[0x0074],
                ),
            };
            model.access(|node| {
                let endpoint = node.endpoint(endpoint).unwrap();
                assert!(
                    endpoint.cluster(cluster).is_some(),
                    "missing cluster {cluster:#x} for {feature:?}"
                );
                assert!(
                    endpoint
                        .device_types
                        .iter()
                        .any(|item| expected_device_types.contains(&item.dtype)),
                    "missing application device type for {feature:?}"
                );
            });
            if let Err(error) = Context::new_at(&im, endpoint, cluster, attribute)
                .read_tlv_result(&model)
                .await
            {
                assert_eq!(
                    error.code(),
                    ErrorCode::Failure,
                    "handler failed for {feature:?}"
                );
            }
        }
    });
}

#[test]
fn visible_range_changes_configuration_but_native_step_does_not() {
    let capability = |minimum, maximum, step| {
        vec![Capability::Temperature(NumericRange {
            minimum,
            maximum,
            step,
            unit: NumericUnit::Celsius,
        })]
    };
    let original = config_signature("shape", &capability(-20.0, 60.0, 0.1));
    assert_eq!(
        original,
        config_signature("shape", &capability(-20.0, 60.0, 0.01))
    );
    assert_ne!(
        original,
        config_signature("shape", &capability(-30.0, 60.0, 0.1))
    );
}

#[test]
fn restart_restores_sensor_shapes_and_split_role_endpoints() {
    let directory = tempfile::tempdir().unwrap();
    let service = DeviceService::new();
    let occupancy = feature(FeatureRole::OccupancySensor);
    let illuminance = feature(FeatureRole::IlluminanceSensor);
    service.restore(
        occupancy.clone(),
        "Presence",
        FeatureCapabilities(vec![
            Capability::Occupancy,
            Capability::SensingModalities(vec![SensingModality::Radar]),
            Capability::Battery(NumericRange {
                minimum: 0.0,
                maximum: 100.0,
                step: 1.0,
                unit: NumericUnit::Percent,
            }),
        ]),
    );
    service.restore(
        illuminance.clone(),
        "Illuminance",
        FeatureCapabilities(vec![Capability::Illuminance(NumericRange {
            minimum: 0.0,
            maximum: 10_000.0,
            step: 0.1,
            unit: NumericUnit::Lux,
        })]),
    );
    let store = Store::open(directory.path()).unwrap();
    let occupancy_endpoint = store
        .devices()
        .allocate_feature(&occupancy)
        .unwrap()
        .endpoint;
    let illuminance_endpoint = store
        .devices()
        .allocate_feature(&illuminance)
        .unwrap()
        .endpoint;
    let first = DeviceBridgeModel::new(service.clone(), store.devices(), store.matter()).unwrap();
    assert_eq!(first.endpoint_for(&occupancy), Some(occupancy_endpoint));
    assert_eq!(first.endpoint_for(&illuminance), Some(illuminance_endpoint));
    drop(first);
    drop(store);

    let reopened = Store::open(directory.path()).unwrap();
    let restored =
        DeviceBridgeModel::new(service.clone(), reopened.devices(), reopened.matter()).unwrap();
    assert!(!service.is_available(&occupancy));
    restored.access(|node| {
        let presence = node.endpoint(occupancy_endpoint).unwrap();
        assert!(
            presence
                .cluster(occupancy_sensing::FULL_CLUSTER.id)
                .is_some()
        );
        assert!(presence.cluster(sensors::ILLUMINANCE_CLUSTER.id).is_none());
        assert!(presence.cluster(sensors::POWER_SOURCE_CLUSTER.id).is_some());
        let lux = node.endpoint(illuminance_endpoint).unwrap();
        assert!(lux.cluster(sensors::ILLUMINANCE_CLUSTER.id).is_some());
        assert!(lux.cluster(occupancy_sensing::FULL_CLUSTER.id).is_none());
    });
}

#[test]
fn reconciliation_reuses_handlers_and_only_loads_labels_for_new_endpoints() {
    block_on(async {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let identity = store.load_identity().unwrap();
        let service = DeviceService::new();
        let id = feature(FeatureRole::TemperatureSensor);
        let capabilities = |maximum| {
            FeatureCapabilities(vec![Capability::Temperature(NumericRange {
                minimum: -20.0,
                maximum,
                step: 0.1,
                unit: NumericUnit::Celsius,
            })])
        };
        service.publish(id.clone(), "Temperature", capabilities(60.0));
        let endpoint = store.devices().allocate_feature(&id).unwrap().endpoint;
        store
            .matter()
            .save_feature_label(endpoint, "Controller label")
            .unwrap();
        let model =
            DeviceBridgeModel::new(service.clone(), store.devices(), store.matter()).unwrap();
        let basic_info = crate::matter::common::basic_info(&identity);
        let matter = Matter::new(&basic_info, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
        let buffers: MatterBuffers = MatterBuffers::new();
        let state: EthInteractionModelState =
            EthInteractionModelState::new(EthNetwork::new_default());
        let crypto = test_only_crypto();
        let kv = matter.kv(crate::matter::storage::StoreAdapter::new(store.matter()));
        crate::matter::common::initialize_basic_info(&matter, &kv, true).unwrap();
        let im = InteractionModel::new(&matter, &crypto, &buffers, (&model, &model), &kv, &state);
        let context = Context::new_at(&im, endpoint, 3, 0);
        let mut run = std::pin::pin!(model.run(&context));
        assert!(poll_once(&mut run).await.is_none());
        let original_signature = model.topology_signature();
        assert_eq!(configuration_version(&store.matter()), 1);

        let identify_data = command_data(|writer| {
            writer.u16(&TLVTag::Context(0), 600).unwrap();
        });
        let identify = Context::command_at(&im, endpoint, 3, 0, &identify_data);
        model
            .invoke(
                &identify,
                InvokeReplyInstance::new(identify.cmd(), WriteBuf::new(&mut [0; 128])),
            )
            .await
            .unwrap();
        let database = rusqlite::Connection::open(store.path()).unwrap();
        database
            .execute_batch("ALTER TABLE matter_feature_labels RENAME TO unavailable_feature_labels")
            .unwrap();

        // Existing handlers already own their labels and live Identify state.
        service.publish(id.clone(), "Renamed temperature", capabilities(60.0));
        assert!(poll_once(&mut run).await.is_none());
        assert_eq!(model.topology_signature(), original_signature);
        assert_eq!(configuration_version(&store.matter()), 1);
        service.publish(id.clone(), "Renamed temperature", capabilities(100.0));
        assert!(poll_once(&mut run).await.is_none());
        let identify = context.read_tlv(&model).await;
        assert!(u16::from_tlv(&value_element(&identify)).unwrap() > 0);
        let label = Context::new_at(&im, endpoint, 57, 5).read_tlv(&model).await;
        assert_eq!(
            Utf8Str::from_tlv(&value_element(&label)).unwrap(),
            "Controller label"
        );
        let maximum = Context::new_at(
            &im,
            endpoint,
            temperature_measurement::FULL_CLUSTER.id,
            temperature_measurement::AttributeId::MaxMeasuredValue as _,
        )
        .read_tlv(&model)
        .await;
        assert_eq!(
            Nullable::<i16>::from_tlv(&value_element(&maximum))
                .unwrap()
                .into_option(),
            Some(10_000)
        );
        assert_eq!(model.endpoint_for(&id), Some(endpoint));
        assert!(!model.take_rebuild_request());
        assert_ne!(model.topology_signature(), original_signature);
        assert_eq!(configuration_version(&store.matter()), 2);
        model.check_failure().unwrap();

        // A new endpoint still loads persisted labels and retains storage failures.
        let mut added = id.clone();
        added.service_instance += 1;
        store.devices().allocate_feature(&added).unwrap();
        service.publish(added, "New temperature", capabilities(60.0));
        assert!(poll_once(&mut run).await.unwrap().is_err());
        assert!(model.check_failure().is_err());
    });
}

#[test]
fn shape_changes_request_rebuild_before_constructing_replacement_handlers() {
    block_on(async {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let identity = store.load_identity().unwrap();
        let service = DeviceService::new();
        let id = feature(FeatureRole::Light);
        service.publish(
            id.clone(),
            "Light",
            FeatureCapabilities(vec![Capability::Power { writable: true }]),
        );
        let endpoint = store.devices().allocate_feature(&id).unwrap().endpoint;
        let model =
            DeviceBridgeModel::new(service.clone(), store.devices(), store.matter()).unwrap();
        let basic_info = crate::matter::common::basic_info(&identity);
        let matter = Matter::new(&basic_info, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
        let buffers: MatterBuffers = MatterBuffers::new();
        let state: EthInteractionModelState =
            EthInteractionModelState::new(EthNetwork::new_default());
        let crypto = test_only_crypto();
        let kv = matter.kv(crate::matter::storage::StoreAdapter::new(store.matter()));
        crate::matter::common::initialize_basic_info(&matter, &kv, true).unwrap();
        let im = InteractionModel::new(&matter, &crypto, &buffers, (&model, &model), &kv, &state);
        let context = Context::new_at(&im, endpoint, 3, 0);
        let mut run = std::pin::pin!(model.run(&context));
        assert!(poll_once(&mut run).await.is_none());
        let original_signature = model.topology_signature();
        let database = rusqlite::Connection::open(store.path()).unwrap();
        database
            .execute_batch("ALTER TABLE matter_feature_labels RENAME TO unavailable_feature_labels")
            .unwrap();

        service.publish(
            id.clone(),
            "Dimmable light",
            FeatureCapabilities(vec![
                Capability::Power { writable: true },
                Capability::Brightness(NumericRange {
                    minimum: 0.0,
                    maximum: 100.0,
                    step: 1.0,
                    unit: NumericUnit::Percent,
                }),
            ]),
        );
        assert!(poll_once(&mut run).await.is_none());
        assert!(model.take_rebuild_request());
        assert_eq!(model.topology_signature(), original_signature);
        model.access(|node| {
            assert!(
                node.endpoint(endpoint)
                    .unwrap()
                    .cluster(level_control::FULL_CLUSTER.id)
                    .is_none()
            );
        });
        model.check_failure().unwrap();
        let requested_signature = store.matter().topology_signature().unwrap();
        assert_ne!(requested_signature, original_signature);
        assert_eq!(configuration_version(&store.matter()), 2);

        database
            .execute_batch("ALTER TABLE unavailable_feature_labels RENAME TO matter_feature_labels")
            .unwrap();
        let rebuilt = DeviceBridgeModel::new(service, store.devices(), store.matter()).unwrap();
        assert_eq!(rebuilt.endpoint_for(&id), Some(endpoint));
        assert_eq!(rebuilt.topology_signature(), requested_signature);
        rebuilt.access(|node| {
            assert!(
                node.endpoint(endpoint)
                    .unwrap()
                    .cluster(level_control::FULL_CLUSTER.id)
                    .is_some()
            );
        });
        assert_eq!(configuration_version(&store.matter()), 2);
    });
}
