use super::super::{
    bridged_info,
    model::{self, NODE, basic_info},
    storage::StoreAdapter,
};
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
        Dataver, Privilege,
        clusters::{decl::bridged_device_basic_information as bridged, groups, identify},
        devices::test::{TEST_DEV_ATT, TEST_DEV_COMM, TEST_DEV_DET},
        networks::eth::EthNetwork,
    },
    error::Error,
    fabric::GroupKeyMapping,
    im::{
        AttrPath, AttrResp, CmdDataTag, CmdResp, EthInteractionModelState, EventPath, EventResp,
        GenericPath, IMStatusCode, InteractionModel, OpCode, StatusResp,
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

async fn wait_committed(state: &EthInteractionModelState) {
    // SubscribeResponse can reach the client before the server commits its
    // priming report. Start device operations after that transaction completes.
    while !state
        .subscriptions()
        .has_subscription_for(NonZeroU8::new(1).unwrap(), CLIENT_ID)
    {
        futures_lite::future::yield_now().await;
    }
}

fn run_boot(directory: &Path, boot: u16, previous_subscription: Option<u32>) -> u32 {
    let store = Store::open(directory).unwrap();
    let identity = store.load_identity().unwrap();
    let info = basic_info(&identity);
    let matter = Matter::new(&info, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
    let protocol = StoreAdapter::new(store.matter());
    let kv = matter.kv(protocol.clone());
    matter.startup(&kv).unwrap();
    let client = Matter::new(&TEST_DEV_DET, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
    if boot > 1 {
        assert!(matter.has_fabrics());
    }
    connect(&matter, SERVER_ID, CLIENT_ID, boot);
    if boot == 1 {
        matter.with_state(|state| {
            state
                .fabrics
                .fabric_mut(NonZeroU8::new(1).unwrap())
                .unwrap()
                .groups_mut()
                .key_map_add(GroupKeyMapping {
                    group_id: 123,
                    group_key_set_id: 1,
                })
                .unwrap()
        });
    }
    connect(&client, CLIENT_ID, SERVER_ID, boot);
    let crypto = test_only_crypto();
    let buffers: MatterBuffers = MatterBuffers::new();
    let state: EthInteractionModelState = EthInteractionModelState::new(EthNetwork::new_default());
    let (device, mut receiver) = super::test_device();
    let child = store
        .load_or_create_device_identity(&device.description().key)
        .unwrap();
    let initial = boot == 3;
    let rejection = RefCell::new(None::<CommandError>);
    let accepted = Cell::new(0);
    let identify = identify::IdentifyHandler::new(Dataver::new(boot.into()));
    let inner = model::on_off(&device, Dataver::new(boot.into()));
    let handler = model::handler(
        &device,
        &child.unique_id,
        bridged_info::load_label(&store.matter(), "Test Outlet").unwrap(),
        &identify,
        &inner,
        rand::rng(),
    );
    let im = InteractionModel::new(&matter, &crypto, &buffers, (NODE, &handler), &kv, &state);
    let incoming = Pipe::default();
    let outgoing = Pipe::default();
    let responder = DefaultResponder::new(&im);

    let id = block_on(async {
        im.startup().await.unwrap();
        let services = async {
            let adapter = async {
                loop {
                    let request = receiver.next().await.unwrap();
                    if let Some(error) = rejection.borrow_mut().take() {
                        request.reject(error);
                    } else {
                        request.accept().unwrap();
                        accepted.set(accepted.get() + 1);
                    }
                }
            };
            or(
                adapter,
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
                ),
            )
            .await
            .unwrap();
            panic!("Matter services exited during subscription test");
        };
        let controller = async {
            if boot == 1 {
                group_request(&client, true).await;
            }
            if boot == 1 {
                read_state(&client, None, false, &[]).await;
                invoke_status(&client, 1, true, IMStatusCode::Failure).await;
                invoke_status(&client, 99, true, IMStatusCode::UnsupportedCommand).await;
                invoke_status(&client, 1, false, IMStatusCode::InvalidCommand).await;
                assert_eq!(accepted.get(), 0);
                device.set_availability(Availability::Available);
                invoke_status(&client, 2, true, IMStatusCode::Failure).await;
                read_state(&client, None, false, &[]).await;
                device.observe(false);
                *rejection.borrow_mut() = Some(CommandError::Rejected("denied".into()));
                invoke_status(&client, 1, true, IMStatusCode::Failure).await;
                *rejection.borrow_mut() = Some(CommandError::Timeout);
                invoke_status(&client, 1, true, IMStatusCode::Failure).await;
                *rejection.borrow_mut() =
                    Some(CommandError::SendFailed("transport stopped".into()));
                invoke_status(&client, 1, true, IMStatusCode::Failure).await;
                invoke_status(&client, 1, true, IMStatusCode::Success).await;
                read_state(&client, Some(true), true, &[true]).await;
                device.set_availability(Availability::Unavailable("offline".into()));
                read_state(&client, Some(true), false, &[true, false]).await;
                device.set_availability(Availability::Unavailable("another reason".into()));
                read_state(&client, Some(true), false, &[true, false]).await;
                device.set_availability(Availability::Available);
                read_state(&client, Some(true), true, &[true, false, true]).await;
                device.observe(initial);
            }
            let mut id = if let Some(id) = previous_subscription {
                // No new SubscribeRequest: resume under the original ID and
                // report a fresh source value. Real CASE is validated manually.
                expect_report(&client, id, None, Some(false)).await.unwrap();
                device.observe(initial);
                device.set_availability(Availability::Available);
                expect_report(&client, id, Some(initial), Some(true))
                    .await
                    .unwrap();
                id
            } else {
                let first = subscribe(&client, initial).await.unwrap();
                wait_committed(&state).await;
                let replacement = subscribe(&client, initial).await.unwrap();
                assert_ne!(first, replacement);
                replacement
            };
            if boot > 1 {
                group_request(&client, false).await;
            }
            for (command, power) in if initial {
                [("off", false), ("on", true)]
            } else {
                [("on", true), ("off", false)]
            } {
                wait_committed(&state).await;
                super::terminal_command(&device, command).await;
                expect_report(&client, id, Some(power), None).await.unwrap();
            }
            wait_committed(&state).await;
            if boot == 2 {
                // A new subscription must not reuse the restored ID.
                let replacement = subscribe(&client, device.snapshot().power.value().unwrap())
                    .await
                    .unwrap();
                assert!(replacement > id);
                id = replacement;
            }
            wait_committed(&state).await;
            let final_power = !device.snapshot().power.value().unwrap();
            super::terminal_command(&device, if final_power { "on" } else { "off" }).await;
            expect_report(&client, id, Some(final_power), None)
                .await
                .unwrap();
            id
        };
        or(
            services,
            or(controller, async {
                async_io::Timer::after(Duration::from_secs(5)).await;
                panic!("timed out waiting for a subscription report on boot {boot}");
            }),
        )
        .await
    });
    protocol.check_failure().unwrap();
    assert!(
        store
            .matter()
            .contains(rs_matter::persist::FABRIC_KEYS_START + 1)
            .unwrap()
    );
    assert!(
        Store::open(directory)
            .unwrap()
            .matter()
            .contains(PERSISTENT_SUBSCRIPTIONS_START)
            .unwrap()
    );
    id
}

#[test]
fn test_fabric_groups_and_subscription_survive_restarts_and_report_terminal_changes() {
    env_logger::Builder::from_default_env().is_test(true).init();
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            let directory = tempfile::tempdir().unwrap();
            let identity = Store::open(directory.path())
                .unwrap()
                .load_identity()
                .unwrap();
            let mut subscription = None;
            for boot in 1..=3 {
                subscription = Some(run_boot(directory.path(), boot, subscription));
                assert_eq!(
                    Store::open(directory.path())
                        .unwrap()
                        .load_identity()
                        .unwrap(),
                    identity
                );
            }
        })
        .unwrap()
        .join()
        .unwrap();
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
