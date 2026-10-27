use super::super::{
    model::{self, basic_info},
    storage::StoreAdapter,
};
use super::support::{entry, model_handler, next_power, single_entry};
use crate::{
    device::{Availability, CommandError},
    storage::Store,
};
use event_listener::Event;
use futures_lite::future::{block_on, or};
use rs_matter::{
    MATTER_PORT, Matter,
    acl::{AclEntry, AuthMode},
    crypto::test_only_crypto,
    dm::{
        Privilege,
        clusters::{decl::bridged_device_basic_information as bridged, groups},
        devices::test::{TEST_DEV_ATT, TEST_DEV_COMM, TEST_DEV_DET},
    },
    error::Error,
    fabric::GroupKeyMapping,
    im::{
        AttrPath, AttrResp, CmdDataTag, CmdResp, EventPath, EventResp, GenericPath, IMStatusCode,
        InteractionModel, InteractionModelState, OpCode, StatusResp,
        client::{ImClient, SubscribeOutcome, TxOutcome},
        encoding::ReportDataResp,
    },
    persist::PERSISTENT_SUBSCRIPTIONS_START,
    respond::DefaultResponder,
    tlv::{FromTLV, TLVElement, TLVTag, TLVWrite},
    transport::{
        exchange::{Exchange, MatterBuffers},
        network::{Address, NetworkReceive, NetworkSend, NoNetwork},
        session::{NocCatIds, ReservedSession, SessionMode},
    },
};
use std::{
    cell::{Cell, RefCell},
    collections::VecDeque,
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    num::NonZeroU8,
    path::Path,
    time::Duration,
};

const SERVER_ID: u64 = 123456;
const CLIENT_ID: u64 = 445566;
const ADDRESS: Address = Address::Udp(SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)));

// Exercise real Matter exchanges without binding the live bridge's UDP port.
#[derive(Default)]
struct Pipe {
    packets: RefCell<VecDeque<Vec<u8>>>,
    ready: Event,
}

struct SendPipe<'a>(&'a Pipe);
struct ReceivePipe<'a>(&'a Pipe);

impl NetworkSend for SendPipe<'_> {
    async fn send_to(&mut self, data: &[u8], _address: Address) -> Result<(), Error> {
        self.0.packets.borrow_mut().push_back(data.to_vec());
        self.0.ready.notify(usize::MAX);
        Ok(())
    }
}

impl NetworkReceive for ReceivePipe<'_> {
    async fn wait_available(&mut self) -> Result<(), Error> {
        loop {
            let ready = self.0.ready.listen();
            if !self.0.packets.borrow().is_empty() {
                return Ok(());
            }
            ready.await;
        }
    }

    async fn recv_from(&mut self, buffer: &mut [u8]) -> Result<(usize, Address), Error> {
        self.wait_available().await?;
        let packet = self.0.packets.borrow_mut().pop_front().unwrap();
        buffer[..packet.len()].copy_from_slice(&packet);
        Ok((packet.len(), ADDRESS))
    }
}

// As in upstream's IM tests, install a test CASE session instead of testing
// commissioning. Each boot has fresh Matter instances and a new session ID.
fn connect(matter: &Matter<'_>, local: u64, peer: u64, session_id: u16) {
    let fabric = NonZeroU8::new(1).unwrap();
    if !matter.has_fabrics() {
        matter.with_state(|state| {
            state.fabrics.add_with_post_init(|_| Ok(())).unwrap();
            let mut acl = AclEntry::new(None, Privilege::ADMIN, AuthMode::Case);
            acl.add_subject(peer).unwrap();
            state
                .fabrics
                .fabric_mut(fabric)
                .unwrap()
                .acl_add(acl)
                .unwrap();
        });
    }
    let mut session = ReservedSession::reserve_now(matter, test_only_crypto()).unwrap();
    session
        .update(
            local,
            peer,
            session_id,
            session_id,
            ADDRESS,
            SessionMode::Case {
                fab_idx: fabric,
                cat_ids: NocCatIds::default(),
            },
            None,
            None,
            None,
            None,
        )
        .unwrap();
    session.complete();
}

async fn subscribe(client: &Matter<'_>, initial: bool) -> Result<u32, Error> {
    let paths = [
        AttrPath::from_gp(&GenericPath::new(Some(2), Some(6), Some(0))),
        AttrPath::from_gp(&GenericPath::new(Some(2), Some(57), Some(17))),
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
        assert_power(&chunk.response()?, initial);
        assert_eq!(
            chunk
                .response()?
                .attrs::<bool>(57, 17)
                .map(|(endpoint, value)| (endpoint, value.unwrap()))
                .collect::<Vec<_>>(),
            [(2, true)]
        );
        match chunk.complete().await? {
            SubscribeOutcome::NextChunk(next) => chunk = next,
            SubscribeOutcome::Established(subscription) => return Ok(subscription.subscription_id),
        }
    }
}

fn assert_power(report: &ReportDataResp<'_>, power: bool) {
    let values = report
        .attrs::<bool>(6, 0)
        .map(|(endpoint, value)| (endpoint, value.unwrap()))
        .collect::<Vec<_>>();
    assert_eq!(values, [(2, power)]);
}

async fn expect_report(
    client: &Matter<'_>,
    subscription_id: u32,
    power: Option<bool>,
    reachable: Option<bool>,
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
        if let Some(power) = power {
            assert_power(&report, power);
        } else {
            let responses = report
                .attr_reports
                .as_ref()
                .unwrap()
                .iter()
                .collect::<Result<Vec<_>, _>>()?;
            assert!(responses.iter().any(|response| matches!(response, AttrResp::Status(status) if status.path.cluster == Some(6) && status.status.status == IMStatusCode::Failure)));
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

async fn wait_committed(state: &InteractionModelState) {
    // SubscribeResponse can reach the client before the server commits its
    // priming report. Start device operations after that transaction completes.
    while !state
        .subscriptions()
        .has_subscription_for(NonZeroU8::new(1).unwrap(), CLIENT_ID)
    {
        futures_lite::future::yield_now().await;
    }
}

async fn invoke_status(client: &Matter<'_>, command: u32, valid: bool, expected: IMStatusCode) {
    let exchange = Exchange::initiate(
        client,
        test_only_crypto(),
        NonZeroU8::new(1).unwrap(),
        SERVER_ID,
    )
    .await
    .unwrap();
    let mut sender = exchange.invoke_sender(None).await.unwrap();
    let chunk = loop {
        match sender.tx().await.unwrap() {
            TxOutcome::BuildRequest(builder) => {
                sender = builder
                    .invoke_requests()
                    .unwrap()
                    .push()
                    .unwrap()
                    .path(2, 6, command)
                    .unwrap()
                    .data(|writer| {
                        let tag = TLVTag::Context(CmdDataTag::Data as u8);
                        if valid {
                            writer.start_struct(&tag)?;
                            writer.end_container()
                        } else {
                            writer.bool(&tag, true)
                        }
                    })
                    .unwrap()
                    .end()
                    .unwrap()
                    .end()
                    .unwrap()
                    .end()
                    .unwrap();
            }
            TxOutcome::GotResponse(chunk) => break chunk,
        }
    };
    let actual = match chunk.response().unwrap() {
        Some(response) => {
            let mut entries = response.invoke_responses.as_ref().unwrap().iter();
            let CmdResp::Status(status) = entries.next().unwrap().unwrap() else {
                panic!("command status expected")
            };
            assert_eq!(status.path.endpoint, Some(2));
            assert_eq!(status.path.cluster, Some(6));
            assert_eq!(status.path.cmd, Some(command));
            assert!(entries.next().is_none());
            status.status.status
        }
        None => {
            assert!(chunk.is_status_only());
            IMStatusCode::Success
        }
    };
    assert_eq!(actual, expected);
    assert!(chunk.complete().await.unwrap().is_none());
}
async fn read_state(
    client: &Matter<'_>,
    power: Option<bool>,
    reachable: bool,
    expected_events: &[bool],
) {
    let attrs = [
        AttrPath::from_gp(&GenericPath::new(Some(2), Some(6), Some(0))),
        AttrPath::from_gp(&GenericPath::new(Some(2), Some(57), Some(17))),
    ];
    let events = [EventPath::from_gp(&GenericPath::new(
        Some(2),
        Some(57),
        Some(3),
    ))];
    let exchange = Exchange::initiate(
        client,
        test_only_crypto(),
        NonZeroU8::new(1).unwrap(),
        SERVER_ID,
    )
    .await
    .unwrap();
    let mut sender = exchange.read_sender().await.unwrap();
    let mut chunk = loop {
        match sender.tx().await.unwrap() {
            TxOutcome::BuildRequest(builder) => {
                sender = builder
                    .attr_requests_from(&attrs)
                    .unwrap()
                    .event_requests_from(&events)
                    .unwrap()
                    .fabric_filtered(false)
                    .unwrap()
                    .end()
                    .unwrap();
            }
            TxOutcome::GotResponse(chunk) => break chunk,
        }
    };
    let mut observed_events = Vec::new();
    let mut saw_power = false;
    let mut saw_reachable = false;
    loop {
        {
            let report = chunk.response().unwrap();
            if let Some(attrs) = &report.attr_reports {
                for attr in attrs.iter() {
                    match attr.unwrap() {
                        AttrResp::Data(data) if data.path.cluster == Some(6) => {
                            assert_eq!(Some(data.data.bool().unwrap()), power);
                            saw_power = true;
                        }
                        AttrResp::Status(status) if status.path.cluster == Some(6) => {
                            assert_eq!(power, None);
                            assert_eq!(status.status.status, IMStatusCode::Failure);
                            saw_power = true;
                        }
                        AttrResp::Data(data) if data.path.cluster == Some(57) => {
                            assert_eq!(data.data.bool().unwrap(), reachable);
                            saw_reachable = true;
                        }
                        _ => panic!("unexpected attribute response"),
                    }
                }
            }
            if let Some(events) = &report.event_reports {
                for event in events.iter() {
                    let EventResp::Data(event) = event.unwrap() else {
                        panic!("event data expected")
                    };
                    assert_eq!(event.path.endpoint, Some(2));
                    assert_eq!(event.path.cluster, Some(57));
                    assert_eq!(event.path.event, Some(3));
                    observed_events.push(
                        bridged::ReachableChanged::from_tlv(&event.data)
                            .unwrap()
                            .reachable_new_value()
                            .unwrap(),
                    );
                }
            }
        }
        match chunk.complete().await.unwrap() {
            Some(next) => chunk = next,
            None => break,
        }
    }
    assert!(saw_power && saw_reachable);
    assert_eq!(observed_events, expected_events);
}

async fn group_request(client: &Matter<'_>, add: bool) {
    let exchange = Exchange::initiate(
        client,
        test_only_crypto(),
        NonZeroU8::new(1).unwrap(),
        SERVER_ID,
    )
    .await
    .unwrap();
    let mut sender = exchange.invoke_sender(None).await.unwrap();
    let chunk = loop {
        match sender.tx().await.unwrap() {
            TxOutcome::BuildRequest(builder) => {
                sender = builder
                    .invoke_requests()
                    .unwrap()
                    .push()
                    .unwrap()
                    .path(2, 4, if add { 0 } else { 2 })
                    .unwrap()
                    .data(|writer| {
                        writer.start_struct(&TLVTag::Context(CmdDataTag::Data as u8))?;
                        if add {
                            writer.u16(&TLVTag::Context(0), 123)?;
                            writer.utf8(&TLVTag::Context(1), "Room")?;
                        } else {
                            writer.start_array(&TLVTag::Context(0))?;
                            writer.end_container()?;
                        }
                        writer.end_container()
                    })
                    .unwrap()
                    .end()
                    .unwrap()
                    .end()
                    .unwrap()
                    .end()
                    .unwrap();
            }
            TxOutcome::GotResponse(chunk) => break chunk,
        }
    };
    {
        let response = chunk.response().unwrap().unwrap();
        if add {
            let (endpoint, result) = response
                .responses::<groups::AddGroupResponse<'_>>(4, 0)
                .next()
                .unwrap();
            let result = result.unwrap();
            assert_eq!(endpoint, 2);
            assert_eq!(result.status().unwrap(), IMStatusCode::Success as u8);
            assert_eq!(result.group_id().unwrap(), 123);
        } else {
            let (endpoint, result) = response
                .responses::<groups::GetGroupMembershipResponse<'_>>(4, 2)
                .next()
                .unwrap();
            assert_eq!(endpoint, 2);
            assert_eq!(
                result
                    .unwrap()
                    .group_list()
                    .unwrap()
                    .iter()
                    .collect::<Result<Vec<_>, _>>()
                    .unwrap(),
                [123]
            );
        }
    }
    assert!(chunk.complete().await.unwrap().is_none());
}

#[derive(Default, Debug)]
struct Observed {
    powers: std::collections::BTreeMap<u16, (u32, bool)>,
    parts: std::collections::BTreeMap<u16, Vec<u16>>,
    versions: std::collections::BTreeMap<u16, u32>,
    bdbis: std::collections::BTreeMap<u16, (u32, u32)>,
    roots: std::collections::BTreeMap<u16, (u32, u32)>,
    unsupported: Vec<u16>,
    lighting: std::collections::BTreeMap<(u16, u32, u32), (u32, u16)>,
}
impl Observed {
    fn add(&mut self, report: &ReportDataResp<'_>) {
        if let Some(attributes) = &report.attr_reports {
            for response in attributes.iter() {
                match response.unwrap() {
                    AttrResp::Data(data) => {
                        let endpoint = data.path.endpoint.unwrap();
                        match (data.path.cluster, data.path.attr) {
                            (Some(6), Some(0)) => {
                                self.powers.insert(
                                    endpoint,
                                    (data.data_ver.unwrap(), data.data.bool().unwrap()),
                                );
                            }
                            (Some(0x1d), Some(3)) => {
                                self.parts.insert(
                                    endpoint,
                                    data.data
                                        .array()
                                        .unwrap()
                                        .iter()
                                        .map(|value| value.unwrap().u16().unwrap())
                                        .collect(),
                                );
                                self.versions.insert(endpoint, data.data_ver.unwrap());
                            }
                            (Some(0x39), Some(24)) => {
                                self.bdbis.insert(
                                    endpoint,
                                    (data.data_ver.unwrap(), data.data.u32().unwrap()),
                                );
                            }
                            (Some(0x28), Some(24)) => {
                                self.roots.insert(
                                    endpoint,
                                    (data.data_ver.unwrap(), data.data.u32().unwrap()),
                                );
                            }
                            (Some(cluster @ (8 | 0x300)), Some(attribute)) => {
                                self.lighting.insert(
                                    (endpoint, cluster, attribute),
                                    (data.data_ver.unwrap(), data.data.u16().unwrap()),
                                );
                            }
                            _ => panic!("unexpected dynamic attribute {data:?}"),
                        }
                    }
                    AttrResp::Status(status) => {
                        assert_eq!(status.status.status, IMStatusCode::UnsupportedEndpoint);
                        self.unsupported.push(status.path.endpoint.unwrap());
                    }
                }
            }
        }
    }
}
async fn subscribe_paths(
    client: &Matter<'_>,
    paths: &[AttrPath],
) -> Result<(u32, Observed), Error> {
    subscribe_paths_with_min_interval(client, paths, 0).await
}
async fn subscribe_paths_with_min_interval(
    client: &Matter<'_>,
    paths: &[AttrPath],
    min_interval: u16,
) -> Result<(u32, Observed), Error> {
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
                    .min_int_floor(min_interval)?
                    .max_int_ceil(60)?
                    .attr_requests_from(paths)?
                    .fabric_filtered(false)?
                    .end()?;
            }
            TxOutcome::GotResponse(chunk) => break chunk,
        }
    };
    let mut observed = Observed::default();
    loop {
        observed.add(&chunk.response()?);
        match chunk.complete().await? {
            SubscribeOutcome::NextChunk(next) => chunk = next,
            SubscribeOutcome::Established(subscription) => {
                return Ok((subscription.subscription_id, observed));
            }
        }
    }
}
async fn read_paths(client: &Matter<'_>, paths: &[AttrPath]) -> Result<Observed, Error> {
    let exchange = Exchange::initiate(
        client,
        test_only_crypto(),
        NonZeroU8::new(1).unwrap(),
        SERVER_ID,
    )
    .await?;
    let mut chunk = exchange
        .read_with(|builder| {
            builder
                .attr_requests_from(paths)?
                .fabric_filtered(false)?
                .end()
        })
        .await?;
    let mut observed = Observed::default();
    loop {
        observed.add(&chunk.response()?);
        match chunk.complete().await? {
            Some(next) => chunk = next,
            None => return Ok(observed),
        }
    }
}
async fn receive_observed(client: &Matter<'_>, id: u32) -> Result<Observed, Error> {
    let mut exchange = Exchange::accept(client).await?;
    let mut observed = Observed::default();
    loop {
        exchange.recv_fetch().await?;
        let more = {
            let rx = exchange.rx()?;
            assert_eq!(rx.meta().proto_opcode, OpCode::ReportData as u8);
            let report = ReportDataResp::from_tlv(&TLVElement::new(rx.payload()))?;
            assert_eq!(report.subscription_id, Some(id));
            observed.add(&report);
            report.more_chunks.unwrap_or(false)
        };
        exchange
            .send_with(|_, buffer| {
                StatusResp::write(buffer, IMStatusCode::Success)?;
                Ok(Some(OpCode::StatusResponse.into()))
            })
            .await?;
        if !more {
            break;
        }
    }
    exchange.acknowledge().await?;
    Ok(observed)
}

mod lighting;
mod persistence;
mod recovery;
mod sensors;
mod topology;
