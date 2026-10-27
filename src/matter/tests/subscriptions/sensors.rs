use super::*;

fn sensor_device() -> crate::device::PhysicalDevice {
    use crate::device::{
        DeviceKey, FunctionDescription, FunctionKind, MeasurementRange, PhysicalDevice,
        PhysicalDeviceDescription,
    };
    let (device, _receivers) = PhysicalDevice::new(
        PhysicalDeviceDescription::new(
            DeviceKey::new("xiaomi", "sensor"),
            "Sensor",
            vec![
                FunctionDescription {
                    id: "temperature".into(),
                    label: "Temperature".into(),
                    kind: FunctionKind::Temperature(MeasurementRange::new(-30.0, 100.0).unwrap()),
                },
                FunctionDescription {
                    id: "humidity".into(),
                    label: "Humidity".into(),
                    kind: FunctionKind::Humidity(MeasurementRange::new(0.0, 100.0).unwrap()),
                },
            ],
        )
        .unwrap(),
    );
    device
}

fn battery_sensor_device() -> crate::device::PhysicalDevice {
    let sensor = sensor_device();
    let (device, _receivers) =
        crate::device::PhysicalDevice::new(sensor.description().with_battery());
    device
}

fn assert_initial_battery(report: &ReportDataResp<'_>) {
    let responses = report
        .attr_reports
        .as_ref()
        .unwrap()
        .iter()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert!(responses.iter().any(|response| matches!(
        response,
        AttrResp::Data(data)
            if data.path.endpoint == Some(2)
                && data.path.cluster == Some(0x2f)
                && data.path.attr == Some(12)
                && data.data.null().is_ok()
    )));
    for attribute in [14, 15] {
        assert!(responses.iter().any(|response| matches!(
            response,
            AttrResp::Status(status)
                if status.path.endpoint == Some(2)
                    && status.path.cluster == Some(0x2f)
                    && status.path.attr == Some(attribute)
                    && status.status.status == IMStatusCode::Failure
        )));
    }
}

async fn subscribe_battery(client: &Matter<'_>) -> Result<u32, Error> {
    let paths = [
        AttrPath::from_gp(&GenericPath::new(Some(2), Some(57), Some(17))),
        AttrPath::from_gp(&GenericPath::new(Some(2), Some(0x2f), Some(12))),
        AttrPath::from_gp(&GenericPath::new(Some(2), Some(0x2f), Some(14))),
        AttrPath::from_gp(&GenericPath::new(Some(2), Some(0x2f), Some(15))),
    ];
    let exchange = Exchange::initiate(
        client,
        test_only_crypto(),
        NonZeroU8::new(1).unwrap(),
        SERVER_ID,
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
        assert_eq!(
            report
                .attrs::<bool>(57, 17)
                .map(|(endpoint, value)| (endpoint, value.unwrap()))
                .collect::<Vec<_>>(),
            [(2, true)]
        );
        assert_initial_battery(&report);
        match chunk.complete().await? {
            SubscribeOutcome::NextChunk(next) => chunk = next,
            SubscribeOutcome::Established(subscription) => return Ok(subscription.subscription_id),
        }
    }
}

async fn expect_battery_report(
    client: &Matter<'_>,
    subscription_id: u32,
    percent: u8,
    level: u8,
    replacement: bool,
) -> Result<(), Error> {
    let mut exchange = Exchange::accept(client).await?;
    exchange.recv_fetch().await?;
    {
        let rx = exchange.rx()?;
        assert_eq!(rx.meta().proto_opcode, OpCode::ReportData as u8);
        let report = ReportDataResp::from_tlv(&TLVElement::new(rx.payload()))?;
        assert_eq!(report.subscription_id, Some(subscription_id));
        assert_eq!(
            report
                .attrs::<u8>(0x2f, 12)
                .map(|(endpoint, value)| (endpoint, value.unwrap()))
                .collect::<Vec<_>>(),
            [(2, percent)]
        );
        assert_eq!(
            report
                .attrs::<u8>(0x2f, 14)
                .map(|(endpoint, value)| (endpoint, value.unwrap()))
                .collect::<Vec<_>>(),
            [(2, level)]
        );
        assert_eq!(
            report
                .attrs::<bool>(0x2f, 15)
                .map(|(endpoint, value)| (endpoint, value.unwrap()))
                .collect::<Vec<_>>(),
            [(2, replacement)]
        );
    }
    exchange
        .send_with(|_, buffer| {
            StatusResp::write(buffer, IMStatusCode::Success)?;
            Ok(Some(OpCode::StatusResponse.into()))
        })
        .await?;
    exchange.acknowledge().await
}

fn with_sensor_subscription(
    battery: bool,
    scenario: impl AsyncFnOnce(&Matter<'_>, &InteractionModelState, &crate::device::PhysicalDevice)
    + Send
    + 'static,
) {
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(move || {
            let directory = tempfile::tempdir().unwrap();
            let store = Store::open(directory.path()).unwrap();
            let identity = store.load_identity().unwrap();
            let info = basic_info(&identity);
            let matter = Matter::new(&info, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
            let protocol = StoreAdapter::new(store.matter());
            let kv = matter.kv(protocol.clone());
            matter.startup(&kv).unwrap();
            let client = Matter::new(&TEST_DEV_DET, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
            connect(&matter, SERVER_ID, CLIENT_ID, 71);
            connect(&client, CLIENT_ID, SERVER_ID, 71);
            let crypto = test_only_crypto();
            let buffers: MatterBuffers = MatterBuffers::new();
            let state: InteractionModelState = InteractionModelState::new();
            let device = if battery {
                battery_sensor_device()
            } else {
                sensor_device()
            };
            device.set_availability(Availability::Available);
            device.functions()[0]
                .observe_measurement(21.5, Some(1), 2)
                .unwrap();
            device.functions()[1]
                .observe_measurement(45.5, Some(1), 2)
                .unwrap();
            let handler = crate::matter::tests::support::model_handler(vec![
                crate::matter::tests::support::entry(
                    &device,
                    2,
                    &[("temperature", 3), ("humidity", 4)],
                ),
            ]);
            let im = InteractionModel::new(&matter, &crypto, &buffers, &handler, &kv, &state);
            let incoming = Pipe::default();
            let outgoing = Pipe::default();
            let responder = DefaultResponder::new(&im);

            block_on(async {
                im.startup().await.unwrap();
                let services = async {
                    or(
                        matter.run(
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
                            or(responder.run::<4, 4>(), im.run()),
                        ),
                    )
                    .await
                    .unwrap();
                    panic!("Matter services exited during sensor subscription test");
                };
                let controller = scenario(&client, &state, &device);
                or(
                    services,
                    or(controller, async {
                        async_io::Timer::after(Duration::from_secs(5)).await;
                        panic!("timed out waiting for a sensor subscription report");
                    }),
                )
                .await
            });
            protocol.check_failure().unwrap();
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn battery_subscription_reports_shared_percentage_charge_and_replacement() {
    with_sensor_subscription(true, async |client, state, device| {
        let id = subscribe_battery(client).await.unwrap();
        wait_committed(state).await;
        device.observe_battery(55.0, Some(2), 3).unwrap();
        expect_battery_report(client, id, 110, 0, false)
            .await
            .unwrap();
        device.observe_battery(5.0, Some(3), 4).unwrap();
        expect_battery_report(client, id, 10, 2, true)
            .await
            .unwrap();
        device.observe_battery(5.0, Some(4), 5).unwrap();
        async_io::Timer::after(Duration::from_millis(50)).await;
    });
}

async fn subscribe_sensor(client: &Matter<'_>) -> Result<u32, Error> {
    let paths = [
        AttrPath::from_gp(&GenericPath::new(Some(2), Some(57), Some(17))),
        AttrPath::from_gp(&GenericPath::new(Some(3), Some(0x402), Some(0))),
        AttrPath::from_gp(&GenericPath::new(Some(4), Some(0x405), Some(0))),
    ];
    let exchange = Exchange::initiate(
        client,
        test_only_crypto(),
        NonZeroU8::new(1).unwrap(),
        SERVER_ID,
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
        assert_eq!(
            report
                .attrs::<bool>(57, 17)
                .map(|(endpoint, value)| (endpoint, value.unwrap()))
                .collect::<Vec<_>>(),
            [(2, true)]
        );
        assert_eq!(
            report
                .attrs::<i16>(0x402, 0)
                .map(|(endpoint, value)| (endpoint, value.unwrap()))
                .collect::<Vec<_>>(),
            [(3, 2150)]
        );
        assert_eq!(
            report
                .attrs::<u16>(0x405, 0)
                .map(|(endpoint, value)| (endpoint, value.unwrap()))
                .collect::<Vec<_>>(),
            [(4, 4550)]
        );
        match chunk.complete().await? {
            SubscribeOutcome::NextChunk(next) => chunk = next,
            SubscribeOutcome::Established(subscription) => return Ok(subscription.subscription_id),
        }
    }
}

async fn expect_sensor_report(
    client: &Matter<'_>,
    subscription_id: u32,
    reachable: Option<bool>,
    temperature: Option<i16>,
    humidity: Option<u16>,
) -> Result<(), Error> {
    let mut exchange = Exchange::accept(client).await?;
    exchange.recv_fetch().await?;
    {
        let rx = exchange.rx()?;
        assert_eq!(rx.meta().proto_opcode, OpCode::ReportData as u8);
        let report = ReportDataResp::from_tlv(&TLVElement::new(rx.payload()))?;
        assert_eq!(report.subscription_id, Some(subscription_id));
        if let Some(expected) = reachable {
            assert_eq!(
                report
                    .attrs::<bool>(57, 17)
                    .map(|(endpoint, value)| (endpoint, value.unwrap()))
                    .collect::<Vec<_>>(),
                [(2, expected)]
            );
        }
        if let Some(expected) = temperature {
            assert_eq!(
                report
                    .attrs::<i16>(0x402, 0)
                    .map(|(endpoint, value)| (endpoint, value.unwrap()))
                    .collect::<Vec<_>>(),
                [(3, expected)]
            );
        } else {
            assert!(
                report.attrs::<i16>(0x402, 0).next().is_none(),
                "unexpected temperature attribute"
            );
        }
        if let Some(expected) = humidity {
            assert_eq!(
                report
                    .attrs::<u16>(0x405, 0)
                    .map(|(endpoint, value)| (endpoint, value.unwrap()))
                    .collect::<Vec<_>>(),
                [(4, expected)]
            );
        } else {
            assert!(
                report.attrs::<u16>(0x405, 0).next().is_none(),
                "unexpected humidity attribute"
            );
        }
    }
    exchange
        .send_with(|_, buffer| {
            StatusResp::write(buffer, IMStatusCode::Success)?;
            Ok(Some(OpCode::StatusResponse.into()))
        })
        .await?;
    exchange.acknowledge().await
}

#[test]
fn sensor_subscription_reports_measured_values_and_reachable_changes() {
    with_sensor_subscription(false, async |client, state, device| {
        let id = subscribe_sensor(client).await.unwrap();
        wait_committed(state).await;
        device.functions()[0]
            .observe_measurement(22.0, Some(2), 3)
            .unwrap();
        expect_sensor_report(client, id, None, Some(2200), None)
            .await
            .unwrap();
        device.functions()[1]
            .observe_measurement(46.0, Some(3), 4)
            .unwrap();
        expect_sensor_report(client, id, None, None, Some(4600))
            .await
            .unwrap();
        device.set_availability(Availability::Unavailable("read path lost".into()));
        expect_sensor_report(client, id, Some(false), None, None)
            .await
            .unwrap();
        device.set_availability(Availability::Available);
        expect_sensor_report(client, id, Some(true), None, None)
            .await
            .unwrap();
    });
}
