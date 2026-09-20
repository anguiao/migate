use super::*;

pub(super) async fn subscribe_temperature(
    client: &Matter<'_>,
    endpoint: u16,
) -> Result<u32, Error> {
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

pub(super) async fn subscribe_fan_percent(
    client: &Matter<'_>,
    endpoint: u16,
) -> Result<u32, Error> {
    let paths = [AttrPath::from_gp(&GenericPath::new(
        Some(endpoint),
        Some(fan_control::FULL_CLUSTER.id),
        Some(fan_control::AttributeId::PercentCurrent as _),
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
            .attrs::<u8>(
                fan_control::FULL_CLUSTER.id,
                fan_control::AttributeId::PercentCurrent as _,
            )
            .map(|(_, value)| value.unwrap())
            .collect::<Vec<_>>();
        assert_eq!(values.len(), 1);
        match chunk.complete().await? {
            SubscribeOutcome::NextChunk(next) => chunk = next,
            SubscribeOutcome::Established(subscription) => return Ok(subscription.subscription_id),
        }
    }
}

pub(super) async fn subscribe_thermostat_cooling(
    client: &Matter<'_>,
    endpoint: u16,
) -> Result<u32, Error> {
    let paths = [AttrPath::from_gp(&GenericPath::new(
        Some(endpoint),
        Some(thermostat::FULL_CLUSTER.id),
        Some(thermostat::AttributeId::OccupiedCoolingSetpoint as _),
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
            .attrs::<i16>(
                thermostat::FULL_CLUSTER.id,
                thermostat::AttributeId::OccupiedCoolingSetpoint as _,
            )
            .map(|(_, value)| value.unwrap())
            .collect::<Vec<_>>();
        assert_eq!(values.len(), 1);
        match chunk.complete().await? {
            SubscribeOutcome::NextChunk(next) => chunk = next,
            SubscribeOutcome::Established(subscription) => return Ok(subscription.subscription_id),
        }
    }
}

pub(super) async fn subscribe_curtain_position(
    client: &Matter<'_>,
    endpoint: u16,
) -> Result<u32, Error> {
    let paths = [AttrPath::from_gp(&GenericPath::new(
        Some(endpoint),
        Some(window_covering::FULL_CLUSTER.id),
        Some(window_covering::AttributeId::CurrentPositionLiftPercent100ths as _),
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
            .attrs::<Nullable<u16>>(
                window_covering::FULL_CLUSTER.id,
                window_covering::AttributeId::CurrentPositionLiftPercent100ths as _,
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

pub(super) async fn subscribe_level(client: &Matter<'_>, endpoint: u16) -> Result<u32, Error> {
    let paths = [AttrPath::from_gp(&GenericPath::new(
        Some(endpoint),
        Some(8),
        Some(level_control::AttributeId::CurrentLevel as _),
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
            .attrs::<Nullable<u8>>(8, level_control::AttributeId::CurrentLevel as _)
            .map(|(_, value)| value.unwrap().into_option())
            .collect::<Vec<_>>();
        assert_eq!(values.len(), 1);
        match chunk.complete().await? {
            SubscribeOutcome::NextChunk(next) => chunk = next,
            SubscribeOutcome::Established(subscription) => return Ok(subscription.subscription_id),
        }
    }
}

pub(super) async fn invoke_identify(
    client: &Matter<'_>,
    endpoint: u16,
    seconds: u16,
) -> Result<(), Error> {
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

pub(super) async fn invoke_command(
    client: &Matter<'_>,
    fabric: NonZeroU8,
    endpoint: u16,
    cluster: u32,
    command: u32,
    data: &[u8],
) -> Result<(), Error> {
    let exchange = Exchange::initiate(client, test_only_crypto(), fabric, 123456).await?;
    let mut sender = exchange.invoke_sender(None).await?;
    let mut chunk = loop {
        match sender.tx().await? {
            TxOutcome::BuildRequest(builder) => {
                sender = builder
                    .suppress_response(false)?
                    .timed_request(false)?
                    .invoke_requests()?
                    .push()?
                    .path(endpoint, cluster, command)?
                    .data(|writer| {
                        TLVElement::new(data)
                            .to_tlv(&TLVTag::Context(CmdDataTag::Data as u8), writer)
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
                    rs_matter::im::CmdResp::Status(status) => assert_eq!(
                        status.status.status,
                        IMStatusCode::Success,
                        "endpoint {endpoint} cluster {cluster:#x} command {command:#x}"
                    ),
                    rs_matter::im::CmdResp::Cmd(response) => {
                        if let Ok(status) = response.data.structure()?.ctx(0)?.u8() {
                            assert_eq!(
                                status, 0,
                                "endpoint {endpoint} cluster {cluster:#x} command {command:#x}"
                            );
                        }
                    }
                }
            }
        }
        match chunk.complete().await? {
            Some(next) => chunk = next,
            None => return Ok(()),
        }
    }
}

pub(super) async fn group_membership(
    client: &Matter<'_>,
    fabric: NonZeroU8,
    endpoint: u16,
) -> Result<Vec<u16>, Error> {
    let request = command_data(|writer| {
        writer.start_array(&TLVTag::Context(0)).unwrap();
        writer.end_container().unwrap();
    });
    let exchange = Exchange::initiate(client, test_only_crypto(), fabric, 123456).await?;
    let mut sender = exchange.invoke_sender(None).await?;
    let mut chunk = loop {
        match sender.tx().await? {
            TxOutcome::BuildRequest(builder) => {
                sender = builder
                    .suppress_response(false)?
                    .timed_request(false)?
                    .invoke_requests()?
                    .push()?
                    .path(
                        endpoint,
                        groups::FULL_CLUSTER.id,
                        groups::CommandId::GetGroupMembership as _,
                    )?
                    .data(|writer| {
                        TLVElement::new(&request)
                            .to_tlv(&TLVTag::Context(CmdDataTag::Data as u8), writer)
                    })?
                    .end()?
                    .end()?
                    .end()?;
            }
            TxOutcome::GotResponse(chunk) => break chunk,
        }
    };
    let mut result = None;
    loop {
        if let Some(response) = chunk.response()?
            && let Some(items) = &response.invoke_responses
        {
            for item in items.iter() {
                if let rs_matter::im::CmdResp::Cmd(response) = item? {
                    result = Some(
                        groups::GetGroupMembershipResponse::new(response.data.clone())
                            .group_list()?
                            .into_iter()
                            .collect::<Result<Vec<_>, _>>()?,
                    );
                }
            }
        }
        match chunk.complete().await? {
            Some(next) => chunk = next,
            None => return result.ok_or_else(|| ErrorCode::Failure.into()),
        }
    }
}

pub(super) async fn scene_info(
    client: &Matter<'_>,
    fabric: NonZeroU8,
    endpoint: u16,
) -> Result<(u8, u8, u16, bool, u8), Error> {
    let exchange = Exchange::initiate(client, test_only_crypto(), fabric, 123456).await?;
    let mut sender = exchange.read_sender().await?;
    let paths = [AttrPath::from_gp(&GenericPath::new(
        Some(endpoint),
        Some(scenes_management::FULL_CLUSTER.id),
        Some(scenes_management::AttributeId::FabricSceneInfo as _),
    ))];
    let mut chunk = loop {
        match sender.tx().await? {
            TxOutcome::BuildRequest(builder) => {
                sender = builder
                    .attr_requests_from(&paths)?
                    .fabric_filtered(true)?
                    .end()?;
            }
            TxOutcome::GotResponse(chunk) => break chunk,
        }
    };
    loop {
        let report = chunk.response()?;
        if let Some((_, value)) = report
            .attrs::<rs_matter::tlv::TLVArray<'_, scenes_management::SceneInfoStruct<'_>>>(
                scenes_management::FULL_CLUSTER.id,
                scenes_management::AttributeId::FabricSceneInfo as _,
            )
            .next()
        {
            let value = value?;
            let mut rows = value.iter();
            let row = rows.next().ok_or(ErrorCode::InvalidData)??;
            return Ok((
                row.scene_count()?,
                row.current_scene()?.ok_or(ErrorCode::InvalidData)?,
                row.current_group()?.ok_or(ErrorCode::InvalidData)?,
                row.scene_valid()?.ok_or(ErrorCode::InvalidData)?,
                row.remaining_capacity()?,
            ));
        }
        match chunk.complete().await? {
            Some(next) => chunk = next,
            None => return Err(ErrorCode::InvalidData.into()),
        }
    }
}

pub(super) async fn read_wildcard_chunks(client: &Matter<'_>) -> Result<(usize, usize), Error> {
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

pub(super) async fn subscribe_reachable(client: &Matter<'_>, endpoint: u16) -> Result<u32, Error> {
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

pub(super) async fn subscribe_rvc_fault(client: &Matter<'_>, endpoint: u16) -> Result<u32, Error> {
    let paths = [EventPath::from_gp(&GenericPath::new(
        Some(endpoint),
        Some(rvc_operational_state::FULL_CLUSTER.id),
        Some(rvc_operational_state::EventId::OperationalError as _),
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
        if let Some(events) = &chunk.response()?.event_reports {
            assert!(
                events.iter().collect::<Result<Vec<_>, _>>()?.is_empty(),
                "the initial RVC subscription replayed a cached fault"
            );
        }
        match chunk.complete().await? {
            SubscribeOutcome::NextChunk(next) => chunk = next,
            SubscribeOutcome::Established(subscription) => return Ok(subscription.subscription_id),
        }
    }
}

pub(super) async fn set_node_label(
    client: &Matter<'_>,
    endpoint: u16,
    label: &str,
) -> Result<(), Error> {
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

pub(super) async fn subscribe_identify(client: &Matter<'_>, endpoint: u16) -> Result<u32, Error> {
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

pub(super) async fn expect_temperature_report(
    client: &Matter<'_>,
    subscription_id: u32,
    expected: Option<i16>,
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
                .attrs::<Nullable<i16>>(
                    temperature_measurement::FULL_CLUSTER.id,
                    temperature_measurement::AttributeId::MeasuredValue as _,
                )
                .map(|(_, value)| value.unwrap().into_option())
                .collect::<Vec<_>>(),
            [expected]
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
