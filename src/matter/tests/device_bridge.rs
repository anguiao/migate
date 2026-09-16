use super::super::{DeviceBridgeModel, device_bridge::config_signature, sensors};
use super::handlers::Context;
use super::subscriptions::{Pipe, ReceivePipe, SendPipe, connect};
use crate::{
    device::{
        AccountId, Capability, DeviceDid, DeviceService, FeatureCapabilities, FeatureIdentity,
        FeatureRole, HomeId, NumericRange, NumericUnit, Percent, PhysicalDeviceId, Property,
        PropertyValue, SensingModality, StateReport, StateSource,
    },
    storage::{MatterStore, Store},
};
use futures_lite::future::{block_on, or};
use rs_matter::{
    MATTER_PORT, Matter,
    crypto::test_only_crypto,
    dm::{
        Metadata,
        clusters::decl::{occupancy_sensing, power_source, temperature_measurement},
        devices::test::{TEST_DEV_ATT, TEST_DEV_COMM, TEST_DEV_DET},
        endpoints,
        networks::{SysNetifs, eth::EthNetwork},
    },
    error::Error,
    im::{
        AttrDataTag, AttrPath, CmdDataTag, EthInteractionModelState, EventPath, GenericPath,
        IMStatusCode, InteractionModel, OpCode, StatusResp,
        client::{ImClient, SubscribeOutcome, TxOutcome},
        encoding::ReportDataResp,
    },
    respond::DefaultResponder,
    tlv::{FromTLV, Nullable, TLVElement, TLVTag, TLVWrite, ToTLV, Utf8Str},
    transport::{
        exchange::{Exchange, MatterBuffers},
        network::NoNetwork,
    },
};
use std::{cell::Cell, collections::BTreeMap, num::NonZeroU8, time::Duration};

fn feature(role: FeatureRole) -> FeatureIdentity {
    FeatureIdentity {
        physical: PhysicalDeviceId {
            account: AccountId::new("u").unwrap(),
            home: HomeId::new("h").unwrap(),
            parent_did: DeviceDid::new("d").unwrap(),
        },
        service_instance: 2,
        role,
    }
}

fn configuration_version(store: &MatterStore) -> u32 {
    let bytes = store
        .get(rs_matter::persist::BASIC_INFO_KEY)
        .unwrap()
        .unwrap();
    rs_matter::dm::clusters::basic_info::BasicInfoSettings::from_tlv(&TLVElement::new(&bytes))
        .unwrap()
        .configuration_version
}

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
fn presence_and_illuminance_use_distinct_simple_endpoints() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    let service = DeviceService::new();
    let id = feature(FeatureRole::OccupancySensor);
    let illuminance = feature(FeatureRole::IlluminanceSensor);
    service.publish(
        id.clone(),
        "Hall presence",
        FeatureCapabilities(vec![
            Capability::Occupancy,
            Capability::SensingModalities(vec![SensingModality::Pir, SensingModality::Radar]),
        ]),
    );
    service.publish(
        illuminance.clone(),
        "Hall illuminance",
        FeatureCapabilities(vec![Capability::Illuminance(NumericRange {
            minimum: 0.0,
            maximum: 10_000.0,
            step: 1.0,
            unit: NumericUnit::Lux,
        })]),
    );
    let allocation = store.devices().allocate_feature(&id).unwrap();
    let illuminance_allocation = store.devices().allocate_feature(&illuminance).unwrap();
    let public_id = allocation.public_id.clone();
    let endpoint = allocation.endpoint;
    let model = DeviceBridgeModel::new(service, store.devices(), store.matter()).unwrap();

    model.access(|node| {
        let sensor = node.endpoint(endpoint).unwrap();
        assert_eq!(sensor.unique_id, Some(public_id.as_str()));
        assert!(sensor.cluster(sensors::ILLUMINANCE_CLUSTER.id).is_none());
        let occupancy = sensor.cluster(occupancy_sensing::FULL_CLUSTER.id).unwrap();
        assert_eq!(
            occupancy.feature_map,
            (occupancy_sensing::Feature::PASSIVE_INFRARED | occupancy_sensing::Feature::RADAR)
                .bits()
        );
        let illuminance = node.endpoint(illuminance_allocation.endpoint).unwrap();
        assert!(
            illuminance
                .cluster(sensors::ILLUMINANCE_CLUSTER.id)
                .is_some()
        );
        assert!(
            illuminance
                .cluster(occupancy_sensing::FULL_CLUSTER.id)
                .is_none()
        );
    });
}

#[test]
fn lux_conversion_uses_matter_logarithmic_encoding_without_fake_zero() {
    assert_eq!(sensors::matter_lux(0.0), Some(0));
    assert_eq!(sensors::matter_lux(0.1), Some(0));
    assert_eq!(sensors::matter_lux(1.0), Some(1));
    assert_eq!(sensors::matter_lux(100.0), Some(20_001));
    assert_eq!(sensors::matter_lux(-1.0), None);
    assert_eq!(sensors::matter_lux(f64::NAN), None);
}

fn report(
    service: &DeviceService,
    id: &FeatureIdentity,
    values: impl IntoIterator<Item = (Property, PropertyValue)>,
) {
    service.apply_report(StateReport::new(
        id.clone(),
        service.next_report_version(),
        StateSource::Lan,
        1,
        values,
    ));
}

fn value_element(bytes: &[u8]) -> TLVElement<'_> {
    TLVElement::new(bytes)
        .structure()
        .unwrap()
        .find_ctx(1)
        .unwrap()
        .structure()
        .unwrap()
        .find_ctx(2)
        .unwrap()
}

#[test]
fn sensor_tlv_reads_preserve_fractional_values_unknowns_and_contact_polarity() {
    block_on(async {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let service = DeviceService::new();
        let humidity = feature(FeatureRole::HumiditySensor);
        let presence = feature(FeatureRole::OccupancySensor);
        let illuminance = feature(FeatureRole::IlluminanceSensor);
        let contact = feature(FeatureRole::ContactSensor);
        service.publish(
            humidity.clone(),
            "Humidity",
            FeatureCapabilities(vec![Capability::Humidity(NumericRange {
                minimum: 0.0,
                maximum: 100.0,
                step: 0.1,
                unit: NumericUnit::Percent,
            })]),
        );
        service.publish(
            presence.clone(),
            "Presence",
            FeatureCapabilities(vec![
                Capability::Occupancy,
                Capability::SensingModalities(vec![SensingModality::Radar]),
            ]),
        );
        service.publish(
            illuminance.clone(),
            "Illuminance",
            FeatureCapabilities(vec![Capability::Illuminance(NumericRange {
                minimum: 0.0,
                maximum: 10_000.0,
                step: 0.1,
                unit: NumericUnit::Lux,
            })]),
        );
        service.publish(
            contact.clone(),
            "Door",
            FeatureCapabilities(vec![Capability::Contact]),
        );
        let humidity_ep = store
            .devices()
            .allocate_feature(&humidity)
            .unwrap()
            .endpoint;
        let presence_ep = store
            .devices()
            .allocate_feature(&presence)
            .unwrap()
            .endpoint;
        let illuminance_ep = store
            .devices()
            .allocate_feature(&illuminance)
            .unwrap()
            .endpoint;
        let contact_ep = store.devices().allocate_feature(&contact).unwrap().endpoint;
        report(
            &service,
            &humidity,
            [(
                Property::Humidity,
                PropertyValue::Percent(Percent::new(55.5).unwrap()),
            )],
        );
        report(
            &service,
            &presence,
            [
                (
                    Property::Occupancy,
                    PropertyValue::Occupancy(crate::device::PresenceState::Occupied),
                ),
                (Property::Motion, PropertyValue::Motion(true)),
            ],
        );
        report(
            &service,
            &illuminance,
            [(Property::Illuminance, PropertyValue::Illuminance(0.1))],
        );
        report(
            &service,
            &contact,
            [(Property::Contact, PropertyValue::ContactOpen(true))],
        );
        let model =
            DeviceBridgeModel::new(service.clone(), store.devices(), store.matter()).unwrap();
        let identity = store.load_identity().unwrap();
        let info = super::super::basic_info(&identity);
        let matter = Matter::new(&info, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
        let crypto = test_only_crypto();
        let buffers: MatterBuffers = MatterBuffers::new();
        let state: EthInteractionModelState =
            EthInteractionModelState::new(EthNetwork::new_default());
        let kv = matter.kv(super::super::storage::StoreAdapter::new(store.matter()));
        let mut random = rand::rng();
        let handler = endpoints::EthSysHandlerBuilder::new()
            .netif_diag(&SysNetifs)
            .build(&mut random)
            .chain(|endpoint, _| endpoint != 0, &model);
        let im = InteractionModel::new(&matter, &crypto, &buffers, (&model, &handler), &kv, &state);

        let bytes = Context::new_at(&im, humidity_ep, 0x0405, 0)
            .read_tlv(&model)
            .await;
        assert_eq!(
            Nullable::<u16>::from_tlv(&value_element(&bytes))
                .unwrap()
                .into_option(),
            Some(5550)
        );
        let bytes = Context::new_at(&im, illuminance_ep, 0x0400, 0)
            .read_tlv(&model)
            .await;
        assert_eq!(
            Nullable::<u16>::from_tlv(&value_element(&bytes))
                .unwrap()
                .into_option(),
            Some(0)
        );
        let bytes = Context::new_at(&im, illuminance_ep, 0x0400, 1)
            .read_tlv(&model)
            .await;
        assert_eq!(
            Nullable::<u16>::from_tlv(&value_element(&bytes))
                .unwrap()
                .into_option(),
            Some(1)
        );
        let bytes = Context::new_at(&im, presence_ep, 0x0406, 0)
            .read_tlv(&model)
            .await;
        assert_eq!(
            occupancy_sensing::OccupancyBitmap::from_tlv(&value_element(&bytes)).unwrap(),
            occupancy_sensing::OccupancyBitmap::OCCUPIED
        );
        let bytes = Context::new_at(&im, contact_ep, 0x0045, 0)
            .read_tlv(&model)
            .await;
        assert!(!value_element(&bytes).bool().unwrap());

        service.apply_unknown(
            &presence,
            Property::Occupancy,
            service.next_report_version(),
        );
        assert!(
            Context::new_at(&im, presence_ep, 0x0406, 0)
                .read_tlv_result(&model)
                .await
                .is_err()
        );
        service.apply_unknown(&contact, Property::Contact, service.next_report_version());
        assert!(
            Context::new_at(&im, contact_ep, 0x0045, 0)
                .read_tlv_result(&model)
                .await
                .is_err()
        );
    });
}

#[test]
fn battery_capability_adds_battery_power_source_shape_to_sensor_endpoint() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    let service = DeviceService::new();
    let id = feature(FeatureRole::TemperatureSensor);
    service.publish(
        id.clone(),
        "Temperature",
        FeatureCapabilities(vec![
            Capability::Temperature(NumericRange {
                minimum: -20.0,
                maximum: 60.0,
                step: 0.1,
                unit: NumericUnit::Celsius,
            }),
            Capability::Battery(NumericRange {
                minimum: 0.0,
                maximum: 100.0,
                step: 1.0,
                unit: NumericUnit::Percent,
            }),
        ]),
    );
    let allocation = store.devices().allocate_feature(&id).unwrap();
    let endpoint = allocation.endpoint;
    let model = DeviceBridgeModel::new(service, store.devices(), store.matter()).unwrap();
    model.access(|node| {
        assert!(node.endpoint(endpoint).unwrap().cluster(0x002f).is_some());
    });
}

#[test]
fn ancillary_temperature_on_climate_role_does_not_create_a_sensor_endpoint() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    let service = DeviceService::new();
    let id = feature(FeatureRole::Climate);
    service.publish(
        id.clone(),
        "Air conditioner",
        FeatureCapabilities(vec![Capability::Temperature(NumericRange {
            minimum: 16.0,
            maximum: 32.0,
            step: 1.0,
            unit: NumericUnit::Celsius,
        })]),
    );
    store.devices().allocate_feature(&id).unwrap();

    let model = DeviceBridgeModel::new(service, store.devices(), store.matter()).unwrap();
    assert_eq!(model.endpoint_for(&id), None);
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

async fn subscribe_temperature(client: &Matter<'_>, endpoint: u16) -> Result<u32, Error> {
    let paths = [AttrPath::from_gp(&GenericPath::new(
        Some(endpoint),
        Some(temperature_measurement::FULL_CLUSTER.id),
        Some(temperature_measurement::AttributeId::MeasuredValue as _),
    ))];
    let exchange = Exchange::initiate(
        client,
        test_only_crypto(),
        NonZeroU8::new(1).unwrap(),
        123456,
    )
    .await?;
    let mut sender = exchange.subscribe_sender().await?;
    let mut chunk = loop {
        match sender.tx().await? {
            TxOutcome::BuildRequest(builder) => {
                sender = builder
                    .keep_subs(false)?
                    .min_int_floor(0)?
                    .max_int_ceil(60)?
                    .attr_requests_from(&paths)?
                    .fabric_filtered(false)?
                    .end()?;
            }
            TxOutcome::GotResponse(chunk) => break chunk,
        }
    };
    loop {
        let report = chunk.response()?;
        let values = report
            .attrs::<Nullable<i16>>(
                temperature_measurement::FULL_CLUSTER.id,
                temperature_measurement::AttributeId::MeasuredValue as _,
            )
            .map(|(_, value)| value.unwrap().into_option())
            .collect::<Vec<_>>();
        assert_eq!(values.len(), 1);
        match chunk.complete().await? {
            SubscribeOutcome::NextChunk(next) => chunk = next,
            SubscribeOutcome::Established(subscription) => return Ok(subscription.subscription_id),
        }
    }
}

async fn invoke_identify(client: &Matter<'_>, endpoint: u16, seconds: u16) -> Result<(), Error> {
    let exchange = Exchange::initiate(
        client,
        test_only_crypto(),
        NonZeroU8::new(1).unwrap(),
        123456,
    )
    .await?;
    let mut sender = exchange.invoke_sender(None).await?;
    let mut chunk = loop {
        match sender.tx().await? {
            TxOutcome::BuildRequest(builder) => {
                sender = builder
                    .suppress_response(false)?
                    .timed_request(false)?
                    .invoke_requests()?
                    .push()?
                    .path(endpoint, 3, 0)?
                    .data(|writer| {
                        writer.start_struct(&TLVTag::Context(CmdDataTag::Data as u8))?;
                        writer.u16(&TLVTag::Context(0), seconds)?;
                        writer.end_container()
                    })?
                    .end()?
                    .end()?
                    .end()?;
            }
            TxOutcome::GotResponse(chunk) => break chunk,
        }
    };
    loop {
        if let Some(response) = chunk.response()?
            && let Some(items) = &response.invoke_responses
        {
            for item in items.iter() {
                match item? {
                    rs_matter::im::CmdResp::Status(status) => {
                        assert_eq!(status.status.status, IMStatusCode::Success);
                    }
                    rs_matter::im::CmdResp::Cmd(_) => {}
                }
            }
        }
        match chunk.complete().await? {
            Some(next) => chunk = next,
            None => return Ok(()),
        }
    }
}

async fn read_wildcard_chunks(client: &Matter<'_>) -> Result<(usize, usize), Error> {
    let exchange = Exchange::initiate(
        client,
        test_only_crypto(),
        NonZeroU8::new(1).unwrap(),
        123456,
    )
    .await?;
    let mut sender = exchange.read_sender().await?;
    let paths = [AttrPath::from_gp(&GenericPath::new(None, None, None))];
    let mut chunk = loop {
        match sender.tx().await? {
            TxOutcome::BuildRequest(builder) => {
                sender = builder
                    .attr_requests_from(&paths)?
                    .fabric_filtered(false)?
                    .end()?;
            }
            TxOutcome::GotResponse(chunk) => break chunk,
        }
    };
    let mut chunks = 0;
    let mut attributes = 0;
    loop {
        chunks += 1;
        if let Some(reports) = &chunk.response()?.attr_reports {
            attributes += reports.iter().filter(|report| report.is_ok()).count();
        }
        match chunk.complete().await? {
            Some(next) => chunk = next,
            None => return Ok((chunks, attributes)),
        }
    }
}

async fn subscribe_reachable(client: &Matter<'_>, endpoint: u16) -> Result<u32, Error> {
    let paths = [EventPath::from_gp(&GenericPath::new(
        Some(endpoint),
        Some(57),
        Some(3),
    ))];
    let exchange = Exchange::initiate(
        client,
        test_only_crypto(),
        NonZeroU8::new(1).unwrap(),
        123456,
    )
    .await?;
    let mut sender = exchange.subscribe_sender().await?;
    let mut chunk = loop {
        match sender.tx().await? {
            TxOutcome::BuildRequest(builder) => {
                sender = builder
                    .keep_subs(false)?
                    .min_int_floor(0)?
                    .max_int_ceil(60)?
                    .event_requests_from(&paths)?
                    .fabric_filtered(false)?
                    .end()?;
            }
            TxOutcome::GotResponse(chunk) => break chunk,
        }
    };
    loop {
        match chunk.complete().await? {
            SubscribeOutcome::NextChunk(next) => chunk = next,
            SubscribeOutcome::Established(subscription) => return Ok(subscription.subscription_id),
        }
    }
}

async fn set_node_label(client: &Matter<'_>, endpoint: u16, label: &str) -> Result<(), Error> {
    let exchange = Exchange::initiate(
        client,
        test_only_crypto(),
        NonZeroU8::new(1).unwrap(),
        123456,
    )
    .await?;
    let mut sender = exchange.write_sender(None).await?;
    let handle = loop {
        match sender.tx().await? {
            TxOutcome::BuildRequest(builder) => {
                let entries = builder.write_requests()?;
                let entry = entries
                    .push()?
                    .path(endpoint, 57, 5)?
                    .data(|writer| label.to_tlv(&TLVTag::Context(AttrDataTag::Data as u8), writer))?
                    .end()?;
                sender = entry.end()?.end()?;
            }
            TxOutcome::GotResponse(handle) => break handle,
        }
    };
    for status in handle.response()?.write_responses.iter() {
        assert_eq!(status?.status.status, IMStatusCode::Success);
    }
    Ok(())
}

async fn subscribe_identify(client: &Matter<'_>, endpoint: u16) -> Result<u32, Error> {
    let paths = [AttrPath::from_gp(&GenericPath::new(
        Some(endpoint),
        Some(3),
        Some(0),
    ))];
    let exchange = Exchange::initiate(
        client,
        test_only_crypto(),
        NonZeroU8::new(1).unwrap(),
        123456,
    )
    .await?;
    let mut sender = exchange.subscribe_sender().await?;
    let mut chunk = loop {
        match sender.tx().await? {
            TxOutcome::BuildRequest(builder) => {
                sender = builder
                    .keep_subs(false)?
                    .min_int_floor(0)?
                    .max_int_ceil(60)?
                    .attr_requests_from(&paths)?
                    .fabric_filtered(false)?
                    .end()?;
            }
            TxOutcome::GotResponse(chunk) => break chunk,
        }
    };
    loop {
        let values = chunk
            .response()?
            .attrs::<u16>(3, 0)
            .map(|(_, value)| value.unwrap())
            .collect::<Vec<_>>();
        assert_eq!(values, [1]);
        match chunk.complete().await? {
            SubscribeOutcome::NextChunk(next) => chunk = next,
            SubscribeOutcome::Established(subscription) => return Ok(subscription.subscription_id),
        }
    }
}

#[test]
fn real_im_temperature_subscription_reports_changes_once_and_ignores_same_value() {
    std::thread::Builder::new().stack_size(16 * 1024 * 1024).spawn(|| block_on(async {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let identity = store.load_identity().unwrap();
        let service = DeviceService::new();
        let id = feature(FeatureRole::TemperatureSensor);
        let allocation = store.devices().allocate_feature(&id).unwrap();
        let endpoint = allocation.endpoint;
        let model = DeviceBridgeModel::new(service.clone(), store.devices(), store.matter()).unwrap();
        service.publish(id.clone(), "Temperature", FeatureCapabilities(vec![
            Capability::Temperature(NumericRange {
                minimum: -20.0, maximum: 60.0, step: 0.05, unit: NumericUnit::Celsius,
            }),
            Capability::Battery(NumericRange {
                minimum: 0.0, maximum: 100.0, step: 1.0, unit: NumericUnit::Percent,
            }),
        ]));
        for index in 0..24 {
            let extra = FeatureIdentity {
                physical: PhysicalDeviceId {
                    account: AccountId::new("u").unwrap(),
                    home: HomeId::new("h").unwrap(),
                    parent_did: DeviceDid::new(format!("extra-{index}")).unwrap(),
                },
                service_instance: 2,
                role: FeatureRole::TemperatureSensor,
            };
            store.devices().allocate_feature(&extra).unwrap();
            service.publish(extra, format!("Extra {index}"), FeatureCapabilities(vec![
                Capability::Temperature(NumericRange {
                    minimum: -20.0,
                    maximum: 60.0,
                    step: 0.1,
                    unit: NumericUnit::Celsius,
                }),
            ]));
        }
        let basic_info = super::super::basic_info(&identity);
        let server = Matter::new(&basic_info, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
        let client = Matter::new(&TEST_DEV_DET, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
        let crypto = test_only_crypto();
        let buffers: MatterBuffers = MatterBuffers::new();
        let state: EthInteractionModelState = EthInteractionModelState::new(EthNetwork::new_default());
        let kv = server.kv(super::super::storage::StoreAdapter::new(store.matter()));
        super::super::model::initialize_basic_info(&server, &kv, true).unwrap();
        connect(&server, 123456, 445566, 71);
        connect(&client, 445566, 123456, 71);
        let mut random = rand::rng();
        let handler = endpoints::EthSysHandlerBuilder::new().netif_diag(&SysNetifs).build(&mut random)
            .chain(|endpoint, _| endpoint != 0, &model);
        let im = InteractionModel::new(&server, &crypto, &buffers, (&model, &handler), &kv, &state);
        let incoming = Pipe::default();
        let outgoing = Pipe::default();
        let responder = DefaultResponder::new(&im);
        im.startup().await.unwrap();
        let services = async {
            or(server.run(&crypto, SendPipe(&outgoing), ReceivePipe(&incoming), NoNetwork),
                or(client.run(&crypto, SendPipe(&incoming), ReceivePipe(&outgoing), NoNetwork),
                    or(responder.run::<4, 4>(), im.run()))).await.unwrap();
            panic!("Matter service loop stopped before the controller completed");
        };
        let controller = async {
            async_io::Timer::after(Duration::from_millis(10)).await;
            while model.endpoint_for(&id).is_none() {
                futures_lite::future::yield_now().await;
            }
            assert_eq!(configuration_version(&store.matter()), 1);
            let (chunks, attributes) = read_wildcard_chunks(&client).await.unwrap();
            assert!(chunks > 1, "many endpoint reports must be chunked");
            assert!(attributes > 100, "all endpoint attributes must be reported");
            let subscription = subscribe_temperature(&client, endpoint).await.unwrap();
            while !state.subscriptions().has_subscription_for(NonZeroU8::new(1).unwrap(), 445566) {
                futures_lite::future::yield_now().await;
            }
            let values = BTreeMap::from([
                (Property::Temperature, PropertyValue::Temperature(21.25)),
                (Property::Battery, PropertyValue::Percent(Percent::new(75.0).unwrap())),
            ]);
            service.apply_report(StateReport::new(id.clone(), 1, StateSource::Lan, 1, values.clone()));
            let mut exchange = Exchange::accept(&client).await.unwrap();
            exchange.recv_fetch().await.unwrap();
            {
                let rx = exchange.rx().unwrap();
                assert_eq!(rx.meta().proto_opcode, OpCode::ReportData as u8);
                let report = ReportDataResp::from_tlv(&TLVElement::new(rx.payload())).unwrap();
                assert_eq!(report.subscription_id, Some(subscription));
                assert_eq!(
                    report.attrs::<Nullable<i16>>(temperature_measurement::FULL_CLUSTER.id, temperature_measurement::AttributeId::MeasuredValue as _)
                        .map(|(_, value)| value.unwrap().into_option())
                        .collect::<Vec<_>>(),
                    [Some(2125)]
                );
            }
            exchange.send_with(|_, buffer| {
                StatusResp::write(buffer, IMStatusCode::Success)?;
                Ok(Some(OpCode::StatusResponse.into()))
            }).await.unwrap();
            exchange.acknowledge().await.unwrap();
            let battery = Context::new_at(&im, endpoint, 0x002f, 12).read_tlv(&model).await;
            let battery = TLVElement::new(&battery).structure().unwrap().find_ctx(1).unwrap()
                .structure().unwrap().find_ctx(2).unwrap();
            assert_eq!(Nullable::<u8>::from_tlv(&battery).unwrap().into_option(), Some(150));
            let status = Context::new_at(&im, endpoint, 0x002f, 0).read_tlv(&model).await;
            assert_eq!(
                power_source::PowerSourceStatusEnum::from_tlv(&value_element(&status)).unwrap(),
                power_source::PowerSourceStatusEnum::Unspecified
            );
            service.apply_report(StateReport::new(id.clone(), 2, StateSource::Lan, 2, values));
            let duplicate = or(
                async { Exchange::accept(&client).await.map(|_| true) },
                async { async_io::Timer::after(Duration::from_millis(100)).await; Ok(false) },
            ).await.unwrap();
            assert!(!duplicate, "same exposed value produced a second Matter report");
            for index in 0..300 {
                let temperature = if index == 299 { 22.0 } else if index % 2 == 0 { 20.0 } else { 21.0 };
                service.apply_report(StateReport::new(
                    id.clone(), service.next_report_version(), StateSource::Lan, index,
                    [(Property::Temperature, PropertyValue::Temperature(temperature))],
                ));
            }
            let mut exchange = Exchange::accept(&client).await.unwrap();
            exchange.recv_fetch().await.unwrap();
            {
                let rx = exchange.rx().unwrap();
                let report = ReportDataResp::from_tlv(&TLVElement::new(rx.payload())).unwrap();
                assert_eq!(report.subscription_id, Some(subscription));
                assert_eq!(
                    report.attrs::<Nullable<i16>>(
                        temperature_measurement::FULL_CLUSTER.id,
                        temperature_measurement::AttributeId::MeasuredValue as _,
                    ).map(|(_, value)| value.unwrap().into_option()).collect::<Vec<_>>(),
                    [Some(2200)]
                );
            }
            exchange.send_with(|_, buffer| {
                StatusResp::write(buffer, IMStatusCode::Success)?;
                Ok(Some(OpCode::StatusResponse.into()))
            }).await.unwrap();
            exchange.acknowledge().await.unwrap();
            assert!(!model.take_rebuild_request());
            assert_eq!(configuration_version(&store.matter()), 1);
            let reachable_subscription = subscribe_reachable(&client, endpoint).await.unwrap();
            service.set_state_availability(&id, false);
            let mut exchange = Exchange::accept(&client).await.unwrap();
            exchange.recv_fetch().await.unwrap();
            {
                let rx = exchange.rx().unwrap();
                let report = ReportDataResp::from_tlv(&TLVElement::new(rx.payload())).unwrap();
                assert_eq!(report.subscription_id, Some(reachable_subscription));
                let events = report.event_reports.as_ref().unwrap().iter().collect::<Result<Vec<_>, _>>().unwrap();
                assert_eq!(events.len(), 1);
                let rs_matter::im::EventResp::Data(event) = &events[0] else { panic!("reachable report was not event data") };
                assert_eq!(event.path.to_gp(), GenericPath::new(Some(endpoint), Some(57), Some(3)));
                let event = rs_matter::dm::clusters::decl::bridged_device_basic_information::ReachableChanged::from_tlv(&event.data).unwrap();
                assert!(!event.reachable_new_value().unwrap());
            }
            exchange.send_with(|_, buffer| {
                StatusResp::write(buffer, IMStatusCode::Success)?;
                Ok(Some(OpCode::StatusResponse.into()))
            }).await.unwrap();
            exchange.acknowledge().await.unwrap();
            service.set_state_availability(&id, true);
            let mut exchange = Exchange::accept(&client).await.unwrap();
            exchange.recv_fetch().await.unwrap();
            {
                let rx = exchange.rx().unwrap();
                let report = ReportDataResp::from_tlv(&TLVElement::new(rx.payload())).unwrap();
                assert_eq!(report.subscription_id, Some(reachable_subscription));
                let events = report.event_reports.as_ref().unwrap().iter().collect::<Result<Vec<_>, _>>().unwrap();
                let rs_matter::im::EventResp::Data(event) = &events[0] else { panic!("reachable report was not event data") };
                let event = rs_matter::dm::clusters::decl::bridged_device_basic_information::ReachableChanged::from_tlv(&event.data).unwrap();
                assert!(event.reachable_new_value().unwrap());
            }
            exchange.send_with(|_, buffer| {
                StatusResp::write(buffer, IMStatusCode::Success)?;
                Ok(Some(OpCode::StatusResponse.into()))
            }).await.unwrap();
            exchange.acknowledge().await.unwrap();
            invoke_identify(&client, endpoint, 1).await.unwrap();
            let identify_subscription = subscribe_identify(&client, endpoint).await.unwrap();
            let mut exchange = Exchange::accept(&client).await.unwrap();
            exchange.recv_fetch().await.unwrap();
            {
                let rx = exchange.rx().unwrap();
                let report = ReportDataResp::from_tlv(&TLVElement::new(rx.payload())).unwrap();
                assert_eq!(report.subscription_id, Some(identify_subscription));
                assert_eq!(report.attrs::<u16>(3, 0).map(|(_, value)| value.unwrap()).collect::<Vec<_>>(), [0]);
            }
            exchange.send_with(|_, buffer| {
                StatusResp::write(buffer, IMStatusCode::Success)?;
                Ok(Some(OpCode::StatusResponse.into()))
            }).await.unwrap();
            exchange.acknowledge().await.unwrap();
            drop(exchange);
            service.publish(id.clone(), "Catalog default", FeatureCapabilities(vec![
                Capability::Temperature(NumericRange {
                    minimum: -20.0, maximum: 60.0, step: 0.05, unit: NumericUnit::Celsius,
                }),
                Capability::Battery(NumericRange {
                    minimum: 0.0, maximum: 100.0, step: 1.0, unit: NumericUnit::Percent,
                }),
            ]));
            futures_lite::future::yield_now().await;
            let label = Context::new_at(&im, endpoint, 57, 5).read_tlv(&model).await;
            assert_eq!(Utf8Str::from_tlv(&value_element(&label)).unwrap(), "Catalog default");
            set_node_label(&client, endpoint, "Controller label").await.unwrap();
            assert_eq!(
                store.matter().feature_label(endpoint).unwrap().as_deref(),
                Some("Controller label")
            );
            set_node_label(&client, endpoint, "").await.unwrap();
            assert_eq!(store.matter().feature_label(endpoint).unwrap().as_deref(), Some(""));
            service.publish(id.clone(), "Renamed by catalog", FeatureCapabilities(vec![
                Capability::Temperature(NumericRange {
                    minimum: -20.0, maximum: 60.0, step: 0.05, unit: NumericUnit::Celsius,
                }),
                Capability::Battery(NumericRange {
                    minimum: 0.0, maximum: 100.0, step: 1.0, unit: NumericUnit::Percent,
                }),
            ]));
            futures_lite::future::yield_now().await;
            assert_eq!(
                store.matter().feature_label(endpoint).unwrap().as_deref(),
                Some("")
            );
            let label = Context::new_at(&im, endpoint, 57, 5).read_tlv(&model).await;
            assert_eq!(Utf8Str::from_tlv(&value_element(&label)).unwrap(), "");
            service.publish(id.clone(), "Unsupported", FeatureCapabilities(vec![
                Capability::Power { writable: false },
            ]));
            while model.endpoint_for(&id).is_some() {
                futures_lite::future::yield_now().await;
            }
            assert_eq!(configuration_version(&store.matter()), 2);
            service.publish(id.clone(), "Restored sensor", FeatureCapabilities(vec![
                Capability::Temperature(NumericRange {
                    minimum: -20.0, maximum: 60.0, step: 0.05, unit: NumericUnit::Celsius,
                }),
            ]));
            while model.endpoint_for(&id).is_none() {
                futures_lite::future::yield_now().await;
            }
            assert_eq!(configuration_version(&store.matter()), 3);
            service.remove(&id);
            while model.endpoint_for(&id).is_some() {
                futures_lite::future::yield_now().await;
            }
            assert_eq!(configuration_version(&store.matter()), 4);
        };
        or(services, or(controller, async {
            async_io::Timer::after(Duration::from_secs(5)).await;
            panic!("timed out waiting for dynamic sensor subscription");
        })).await;
    })).unwrap().join().unwrap();
}

#[test]
fn shape_rebuild_restores_existing_subscription_on_the_new_model() {
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            block_on(async {
                let directory = tempfile::tempdir().unwrap();
                let store = Store::open(directory.path()).unwrap();
                let identity = store.load_identity().unwrap();
                let service = DeviceService::new();
                let id = feature(FeatureRole::TemperatureSensor);
                let endpoint = store.devices().allocate_feature(&id).unwrap().endpoint;
                service.publish(
                    id.clone(),
                    "Temperature",
                    FeatureCapabilities(vec![
                        Capability::Temperature(NumericRange {
                            minimum: -20.0,
                            maximum: 60.0,
                            step: 0.1,
                            unit: NumericUnit::Celsius,
                        }),
                        Capability::Battery(NumericRange {
                            minimum: 0.0,
                            maximum: 100.0,
                            step: 1.0,
                            unit: NumericUnit::Percent,
                        }),
                    ]),
                );
                let basic_info = super::super::basic_info(&identity);
                let server = Matter::new(&basic_info, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
                let client = Matter::new(&TEST_DEV_DET, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
                let kv = server.kv(super::super::storage::StoreAdapter::new(store.matter()));
                super::super::model::initialize_basic_info(&server, &kv, true).unwrap();
                connect(&server, 123456, 445566, 72);
                connect(&client, 445566, 123456, 72);
                let crypto = test_only_crypto();
                let buffers: MatterBuffers = MatterBuffers::new();
                let state: EthInteractionModelState =
                    EthInteractionModelState::new(EthNetwork::new_default());
                let incoming = Pipe::default();
                let outgoing = Pipe::default();
                let generations = Cell::new(0_u8);
                let protocol = async {
                    loop {
                        let model = DeviceBridgeModel::new(
                            service.clone(),
                            store.devices(),
                            store.matter(),
                        )
                        .unwrap();
                        let mut random = rand::rng();
                        let handler = endpoints::EthSysHandlerBuilder::new()
                            .netif_diag(&SysNetifs)
                            .build(&mut random)
                            .chain(|endpoint, _| endpoint != 0, &model);
                        let im = InteractionModel::new(
                            &server,
                            &crypto,
                            &buffers,
                            (&model, &handler),
                            &kv,
                            &state,
                        );
                        im.startup().await.unwrap();
                        generations.set(generations.get() + 1);
                        if generations.get() > 1 {
                            assert!(
                                state
                                    .subscriptions()
                                    .has_subscription_for(NonZeroU8::new(1).unwrap(), 445566,)
                            );
                        }
                        let responder = DefaultResponder::new(&im);
                        let rebuilt = or(
                            async {
                                responder.run::<4, 4>().await.unwrap();
                                panic!("Matter responder stopped before rebuild")
                            },
                            or(
                                async {
                                    im.run().await.unwrap();
                                    panic!("Matter model stopped before rebuild")
                                },
                                async {
                                    model.rebuild_requested().await;
                                    true
                                },
                            ),
                        )
                        .await;
                        assert!(rebuilt);
                    }
                };
                let services = async {
                    or(
                        server.run(
                            &crypto,
                            SendPipe(&outgoing),
                            ReceivePipe(&incoming),
                            NoNetwork,
                        ),
                        or(
                            client.run(
                                &crypto,
                                SendPipe(&incoming),
                                ReceivePipe(&outgoing),
                                NoNetwork,
                            ),
                            protocol,
                        ),
                    )
                    .await
                    .unwrap();
                    panic!("Matter service loop stopped before the controller completed");
                };
                let controller = async {
                    while generations.get() < 1 {
                        futures_lite::future::yield_now().await;
                    }
                    let subscription = subscribe_temperature(&client, endpoint).await.unwrap();
                    while !state
                        .subscriptions()
                        .has_subscription_for(NonZeroU8::new(1).unwrap(), 445566)
                    {
                        futures_lite::future::yield_now().await;
                    }
                    assert!(store.matter().contains(0x0800).unwrap());
                    service.publish(
                        id.clone(),
                        "Temperature",
                        FeatureCapabilities(vec![Capability::Temperature(NumericRange {
                            minimum: -20.0,
                            maximum: 60.0,
                            step: 0.1,
                            unit: NumericUnit::Celsius,
                        })]),
                    );
                    while generations.get() < 2 {
                        futures_lite::future::yield_now().await;
                    }
                    assert!(store.matter().contains(0x0800).unwrap());
                    let mut priming = Exchange::accept(&client).await.unwrap();
                    priming.recv_fetch().await.unwrap();
                    {
                        let rx = priming.rx().unwrap();
                        let report =
                            ReportDataResp::from_tlv(&TLVElement::new(rx.payload())).unwrap();
                        assert_eq!(report.subscription_id, Some(subscription));
                    }
                    priming
                        .send_with(|_, buffer| {
                            StatusResp::write(buffer, IMStatusCode::Success)?;
                            Ok(Some(OpCode::StatusResponse.into()))
                        })
                        .await
                        .unwrap();
                    priming.acknowledge().await.unwrap();
                    drop(priming);
                    while !state
                        .subscriptions()
                        .has_subscription_for(NonZeroU8::new(1).unwrap(), 445566)
                    {
                        futures_lite::future::yield_now().await;
                    }
                    service.apply_report(StateReport::new(
                        id,
                        service.next_report_version(),
                        StateSource::Lan,
                        1,
                        [(Property::Temperature, PropertyValue::Temperature(23.5))],
                    ));
                    let mut exchange = Exchange::accept(&client).await.unwrap();
                    exchange.recv_fetch().await.unwrap();
                    let rx = exchange.rx().unwrap();
                    let report = ReportDataResp::from_tlv(&TLVElement::new(rx.payload())).unwrap();
                    assert_eq!(report.subscription_id, Some(subscription));
                    assert_eq!(
                        report
                            .attrs::<Nullable<i16>>(
                                temperature_measurement::FULL_CLUSTER.id,
                                temperature_measurement::AttributeId::MeasuredValue as _,
                            )
                            .map(|(_, value)| value.unwrap().into_option())
                            .collect::<Vec<_>>(),
                        [Some(2350)]
                    );
                };
                or(
                    services,
                    or(controller, async {
                        async_io::Timer::after(Duration::from_secs(5)).await;
                        panic!("timed out waiting for shape rebuild subscription report");
                    }),
                )
                .await;
            })
        })
        .unwrap()
        .join()
        .unwrap();
}
