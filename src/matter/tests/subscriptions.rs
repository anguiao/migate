use super::super::{
    model::{self, basic_info},
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
        Metadata, Privilege,
        clusters::{decl::bridged_device_basic_information as bridged, groups},
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
    collections::{BTreeMap, VecDeque},
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
    let initial = boot == 3;
    let rejection = RefCell::new(None::<CommandError>);
    let accepted = Cell::new(0);
    let registry = super::test_registry(&store, &device);
    let handler = model::handler(&registry, &store, protocol.clone(), rand::rng()).unwrap();
    let im = InteractionModel::new(&matter, &crypto, &buffers, &handler, &kv, &state);
    let incoming = Pipe::default();
    let outgoing = Pipe::default();
    let responder = DefaultResponder::new(&im);

    let id = block_on(async {
        im.startup().await.unwrap();
        handler.initialize(&im).unwrap();
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
                read_state(&client, None, true, &[true]).await;
                device.functions()[0].observe(false);
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
                device.functions()[0].observe(initial);
            }
            let mut id = if let Some(id) = previous_subscription {
                // No new SubscribeRequest: resume under the original ID and
                // report a fresh source value. Real CASE is validated manually.
                expect_report(&client, id, None, Some(false)).await.unwrap();
                device.functions()[0].observe(initial);
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
                let replacement = subscribe(
                    &client,
                    device.functions()[0].snapshot().power.value().unwrap(),
                )
                .await
                .unwrap();
                assert!(replacement > id);
                id = replacement;
            }
            wait_committed(&state).await;
            let final_power = !device.functions()[0].snapshot().power.value().unwrap();
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

fn recovery_race_boot(directory: &Path, previous_subscription: Option<u32>) -> u32 {
    let store = Store::open(directory).unwrap();
    let identity = store.load_identity().unwrap();
    let info = basic_info(&identity);
    let matter = Matter::new(&info, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
    let protocol = StoreAdapter::new(store.matter());
    let kv = matter.kv(protocol.clone());
    matter.startup(&kv).unwrap();
    let client = Matter::new(&TEST_DEV_DET, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
    if previous_subscription.is_none() {
        connect(&matter, SERVER_ID, CLIENT_ID, 41);
        connect(&client, CLIENT_ID, SERVER_ID, 41);
    }
    if previous_subscription.is_none() {
        let mut fabrics = rs_matter::fabric::FabricPersist::new(&kv);
        matter.with_state(|state| {
            fabrics
                .store(state.fabrics.fabric(NonZeroU8::new(1).unwrap()).unwrap())
                .unwrap();
        });
        fabrics.run().unwrap();
    }
    let crypto = test_only_crypto();
    let buffers: MatterBuffers = MatterBuffers::new();
    let state: EthInteractionModelState = EthInteractionModelState::new(EthNetwork::new_default());
    let (device, _receiver) = super::test_device();
    device.set_availability(Availability::Available);
    device.functions()[0].observe(false);
    let registry = super::test_registry(&store, &device);
    let handler = model::handler(&registry, &store, protocol.clone(), rand::rng()).unwrap();
    let im = InteractionModel::new(&matter, &crypto, &buffers, &handler, &kv, &state);
    let incoming = Pipe::default();
    let outgoing = Pipe::default();
    let responder = DefaultResponder::new(&im);

    let id = block_on(async {
        im.startup().await.unwrap();
        handler.initialize(&im).unwrap();
        if previous_subscription.is_some() {
            assert!(matter.has_fabrics());
            assert!(
                state
                    .subscriptions()
                    .has_subscription_for(NonZeroU8::new(1).unwrap(), CLIENT_ID)
            );
        }
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
            panic!("Matter services exited during the session recovery race");
        };
        let controller = async {
            if let Some(previous) = previous_subscription {
                // Wait until the restored report is resolving the peer
                // before the new inbound CASE session arrives.
                let service = matter.transport().wait_mdns_resolve_request().await;
                assert!(matches!(
                    service,
                    rs_matter::transport::network::MatterRemoteService::Operational {
                        node_id: CLIENT_ID,
                        ..
                    }
                ));
                assert!(matter.transport().mdns_resolve_in_flight());
                connect(&matter, SERVER_ID, CLIENT_ID, 42);
                connect(&client, CLIENT_ID, SERVER_ID, 42);
                // Hold an idle exchange bound to this exact session so that
                // its public API can verify liveness without another lookup.
                let session_probe = Exchange::initiate(
                    &matter,
                    test_only_crypto(),
                    NonZeroU8::new(1).unwrap(),
                    CLIENT_ID,
                )
                .await
                .unwrap();
                // No resolver answer is deposited; the real resolve timer
                // produces the failure while the inbound session exists.
                assert!(matter.transport().mdns_resolve_in_flight());
                // Synchronize with the actual cleanup instead of a time delay.
                state.subscriptions().wait_report_failed().await;
                assert!(
                    session_probe.pending_retrans().is_ok(),
                    "a failed restored-subscription operation removed the new inbound CASE session"
                );
                // The restored subscription retries on the inbound session,
                // preserving its ID and continuing with subsequent changes.
                expect_report(&client, previous, Some(false), Some(true))
                    .await
                    .unwrap();
                wait_committed(&state).await;
                for power in [true, false] {
                    device.functions()[0].observe(power);
                    expect_report(&client, previous, Some(power), None)
                        .await
                        .unwrap();
                    wait_committed(&state).await;
                }
                previous
            } else {
                let id = subscribe(&client, false).await.unwrap();
                wait_committed(&state).await;
                id
            }
        };
        or(
            services,
            or(controller, async {
                async_io::Timer::after(Duration::from_secs(15)).await;
                panic!("timed out waiting for the session recovery race");
            }),
        )
        .await
    });
    protocol.check_failure().unwrap();
    assert!(
        store
            .matter()
            .contains(PERSISTENT_SUBSCRIPTIONS_START)
            .unwrap()
    );
    id
}

#[test]
fn resumed_resolve_failure_must_preserve_new_inbound_session() {
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            let directory = tempfile::tempdir().unwrap();
            let previous = recovery_race_boot(directory.path(), None);
            assert_eq!(
                recovery_race_boot(directory.path(), Some(previous)),
                previous
            );
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

#[derive(Default, Debug)]
struct Observed {
    powers: std::collections::BTreeMap<u16, (u32, bool)>,
    parts: std::collections::BTreeMap<u16, Vec<u16>>,
    versions: std::collections::BTreeMap<u16, u32>,
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

fn persisted_config_version(store: &Store) -> Option<u32> {
    let bytes = store
        .matter()
        .get(rs_matter::persist::BASIC_INFO_KEY)
        .unwrap()?;
    Some(
        rs_matter::dm::clusters::basic_info::BasicInfoSettings::from_tlv(&TLVElement::new(&bytes))
            .unwrap()
            .configuration_version,
    )
}

async fn subscribe_wildcard(client: &Matter<'_>) -> Result<u32, Error> {
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
                    .attr_requests_from(&[AttrPath::from_gp(&GenericPath::new(None, None, None))])?
                    .fabric_filtered(false)?
                    .end()?;
            }
            TxOutcome::GotResponse(chunk) => break chunk,
        }
    };
    loop {
        match chunk.complete().await? {
            SubscribeOutcome::NextChunk(next) => chunk = next,
            SubscribeOutcome::Established(subscription) => {
                return Ok(subscription.subscription_id);
            }
        }
    }
}

#[derive(Default, Debug)]
struct StructuralAction {
    versions: Vec<u32>,
    server_lists: BTreeMap<u16, Vec<u32>>,
    powers: BTreeMap<u16, (u32, bool)>,
}

impl StructuralAction {
    fn add(&mut self, report: &ReportDataResp<'_>) {
        let Some(attributes) = &report.attr_reports else {
            return;
        };
        for response in attributes.iter() {
            let Ok(AttrResp::Data(data)) = response else {
                continue;
            };
            let (Some(endpoint), Some(cluster), Some(attribute)) =
                (data.path.endpoint, data.path.cluster, data.path.attr)
            else {
                continue;
            };
            if cluster == 0x28 && attribute == 0x18 {
                if let Ok(version) = data.data.u32() {
                    self.versions.push(version);
                }
            } else if cluster == 6 && attribute == 0 {
                self.powers.insert(
                    endpoint,
                    (data.data_ver.unwrap(), data.data.bool().unwrap()),
                );
            } else if cluster == 0x1d
                && attribute == 1
                && let Ok(array) = data.data.array()
            {
                let list = array
                    .iter()
                    .filter_map(|value| value.ok()?.u16().ok())
                    .map(u32::from)
                    .collect();
                self.server_lists.insert(endpoint, list);
            }
        }
    }
}

/// Receive one whole Report Data Action (all chunks) and, for a chunked action,
/// run `mid_action` after the first chunk is parsed and before it is
/// acknowledged, so the caller can mutate the model while the action is in
/// flight.
async fn receive_structural_action<F, Fut>(
    client: &Matter<'_>,
    id: u32,
    require_chunks: bool,
    mid_action: F,
) -> Result<StructuralAction, Error>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let mut exchange = Exchange::accept(client).await?;
    let mut action = StructuralAction::default();
    let mut mid_action = Some(mid_action);
    let mut first = true;
    loop {
        exchange.recv_fetch().await?;
        let more = {
            let rx = exchange.rx()?;
            assert_eq!(rx.meta().proto_opcode, OpCode::ReportData as u8);
            let report = ReportDataResp::from_tlv(&TLVElement::new(rx.payload()))?;
            assert_eq!(report.subscription_id, Some(id));
            action.add(&report);
            report.more_chunks.unwrap_or(false)
        };
        if first {
            first = false;
            if require_chunks {
                assert!(
                    more,
                    "the structural batch must span multiple chunks for this regression"
                );
            }
            let mid_action = mid_action.take().expect("the mid-action hook runs once");
            mid_action().await;
        }
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
    Ok(action)
}

fn topology_boot(directory: &Path, boot: u16, previous: Option<(u32, String)>) -> (u32, String) {
    let store = Store::open(directory).unwrap();
    let identity = store.load_identity().unwrap();
    let info = basic_info(&identity);
    let matter = Matter::new(&info, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
    let protocol = StoreAdapter::new(store.matter());
    let kv = matter.kv(protocol.clone());
    matter.startup(&kv).unwrap();
    if boot > 1 {
        assert!(matter.has_fabrics());
    }
    let client = Matter::new(&TEST_DEV_DET, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
    connect(&matter, SERVER_ID, CLIENT_ID, boot + 20);
    connect(&client, CLIENT_ID, SERVER_ID, boot + 20);
    if boot == 1 {
        let mut fabrics = rs_matter::fabric::FabricPersist::new(&kv);
        matter.with_state(|state| {
            fabrics
                .store(state.fabrics.fabric(NonZeroU8::new(1).unwrap()).unwrap())
                .unwrap();
        });
        fabrics.run().unwrap();
    }
    let crypto = test_only_crypto();
    let buffers: MatterBuffers = MatterBuffers::new();
    let state: EthInteractionModelState = EthInteractionModelState::new(EthNetwork::new_default());
    let registry = crate::device::DeviceRegistry::new();
    let (single, _receivers) = super::physical("dynamic-single", false);
    let (multi, _multi_receivers) = super::physical("dynamic-multi", true);
    let single_stored = store.devices().publish(&single.description()).unwrap();
    assert_eq!(single_stored.endpoint, 2);
    registry.insert(single.clone());
    let unique_id = if boot == 2 {
        store.devices().remove(multi.key()).unwrap();
        previous.as_ref().unwrap().1.clone()
    } else {
        let stored = store.devices().publish(&multi.description()).unwrap();
        assert_eq!(stored.endpoint, 3);
        if let Some((_, id)) = &previous {
            assert_eq!(&stored.unique_id, id);
            assert_eq!(stored.label, "Named multi");
        }
        store
            .devices()
            .set_device_label(multi.key(), "Named multi")
            .unwrap();
        registry.insert(multi.clone());
        stored.unique_id
    };
    for device in [&single, &multi] {
        device.set_availability(Availability::Available);
        for function in device.functions() {
            function.observe(false);
        }
    }
    let handler = model::handler(&registry, &store, protocol.clone(), rand::rng()).unwrap();
    let im = InteractionModel::new(&matter, &crypto, &buffers, &handler, &kv, &state);
    let incoming = Pipe::default();
    let outgoing = Pipe::default();
    let responder = DefaultResponder::new(&im);
    let id = block_on(async {
        im.startup().await.unwrap();
        handler.initialize(&im).unwrap();
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
            panic!("ordinary state changes must not restart Matter");
        };
        let controller = async {
            let paths = [
                AttrPath::from_gp(&GenericPath::new(Some(0), Some(0x1d), Some(3))),
                AttrPath::from_gp(&GenericPath::new(Some(1), Some(0x1d), Some(3))),
                AttrPath::from_gp(&GenericPath::new(None, Some(6), Some(0))),
                AttrPath::from_gp(&GenericPath::new(Some(4), Some(6), Some(0))),
            ];
            let (id, report) = if let Some((id, _)) = previous {
                (id, receive_observed(&client, id).await.unwrap())
            } else {
                subscribe_paths(&client, &paths).await.unwrap()
            };
            if boot == 2 {
                assert_eq!(report.parts[&0], [1, 2]);
                assert_eq!(report.parts[&1], [2]);
                assert_eq!(report.unsupported, [4]);
                assert_eq!(report.powers.keys().copied().collect::<Vec<_>>(), [2]);
            } else {
                assert_eq!(report.parts[&0], [1, 2, 3, 4, 5, 6]);
                assert_eq!(report.parts[&1], [2, 3, 4, 5, 6]);
                assert!(report.unsupported.is_empty());
                assert_eq!(
                    report.powers.keys().copied().collect::<Vec<_>>(),
                    [2, 4, 5, 6]
                );
                wait_committed(&state).await;
                multi.function("usb").unwrap().observe(true);
                let changed = receive_observed(&client, id).await.unwrap();
                assert_eq!(
                    changed.powers[&6],
                    (report.powers[&6].0.wrapping_add(1), true)
                );
                assert!(!changed.powers.contains_key(&2));
            }
            wait_committed(&state).await;
            single.functions()[0].observe(true);
            let changed = receive_observed(&client, id).await.unwrap();
            assert!(changed.powers[&2].1);
            wait_committed(&state).await;
            id
        };
        or(
            services,
            or(controller, async {
                async_io::Timer::after(Duration::from_secs(5)).await;
                panic!("topology subscription boot {boot} timed out");
            }),
        )
        .await
    });
    protocol.check_failure().unwrap();
    assert!(
        store
            .matter()
            .contains(PERSISTENT_SUBSCRIPTIONS_START)
            .unwrap()
    );
    (id, unique_id)
}

#[test]
fn subscription_recovers_added_and_deleted_paths_after_topology_rebuilds() {
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            let directory = tempfile::tempdir().unwrap();
            let id = topology_boot(directory.path(), 1, None);
            for boot in 2..=3 {
                assert_eq!(topology_boot(directory.path(), boot, Some(id.clone())), id);
            }
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn lighting_subscription_reports_active_color_groups_modes_and_shared_versions() {
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            use crate::device::{LightMode, LightPatch, Rgb};
            let directory = tempfile::tempdir().unwrap();
            let store = Store::open(directory.path()).unwrap();
            let identity = store.load_identity().unwrap();
            let info = basic_info(&identity);
            let matter = Matter::new(&info, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
            let protocol = StoreAdapter::new(store.matter());
            let kv = matter.kv(protocol.clone());
            matter.startup(&kv).unwrap();
            let client = Matter::new(&TEST_DEV_DET, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
            connect(&matter, SERVER_ID, CLIENT_ID, 31);
            connect(&client, CLIENT_ID, SERVER_ID, 31);
            let crypto = test_only_crypto();
            let buffers: MatterBuffers = MatterBuffers::new();
            let state: EthInteractionModelState =
                EthInteractionModelState::new(EthNetwork::new_default());
            let (device, _receiver) = super::support::light(true);
            device.set_availability(Availability::Available);
            let function = device.functions()[0].clone();
            function
                .observe_light(LightPatch {
                    power: Some(true),
                    brightness_percent: Some(50.0),
                    temperature_kelvin: Some(6500),
                    color: Some(Rgb {
                        red: 255,
                        green: 0,
                        blue: 0,
                    }),
                    mode: Some(LightMode::Rgb),
                })
                .unwrap();
            let registry = super::test_registry(&store, &device);
            let handler = model::handler(&registry, &store, protocol.clone(), rand::rng()).unwrap();
            let im = InteractionModel::new(&matter, &crypto, &buffers, &handler, &kv, &state);
            let incoming = Pipe::default();
            let outgoing = Pipe::default();
            let responder = DefaultResponder::new(&im);
            block_on(async {
                im.startup().await.unwrap();
                handler.initialize(&im).unwrap();
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
                    panic!("lighting services exited");
                };
                let controller = async {
                    let assert_colors = |report: &Observed, expected: &[(u32, u16)]| {
                        let colors = report
                            .lighting
                            .iter()
                            .filter(|((endpoint, cluster, _), _)| {
                                *endpoint == 2 && *cluster == 0x300
                            })
                            .map(|((_, _, attribute), (_, value))| (*attribute, *value))
                            .collect::<Vec<_>>();
                        assert_eq!(colors, expected);
                        if let Some((attribute, _)) = expected.first() {
                            let version = report.lighting[&(2, 0x300, *attribute)].0;
                            for (attribute, _) in expected {
                                assert_eq!(report.lighting[&(2, 0x300, *attribute)].0, version);
                            }
                        }
                    };
                    let cool_white = [
                        (0, 218),
                        (1, 7),
                        (3, 20554),
                        (4, 21209),
                        (7, 154),
                        (8, 2),
                        (0x4001, 2),
                    ];
                    let paths = [
                        (8, 0),
                        (0x300, 0),
                        (0x300, 1),
                        (0x300, 3),
                        (0x300, 4),
                        (0x300, 7),
                        (0x300, 8),
                        (0x300, 0x4001),
                    ]
                    .map(|(c, a)| AttrPath::from_gp(&GenericPath::new(Some(2), Some(c), Some(a))));
                    let (id, initial) = subscribe_paths(&client, &paths).await.unwrap();
                    wait_committed(&state).await;
                    assert_eq!(
                        initial.lighting[&(2, 8, 0)].1,
                        crate::device::brightness_to_matter_level(50.0).unwrap() as u16
                    );
                    assert_eq!(initial.lighting[&(2, 0x300, 0)].1, 0);
                    assert_eq!(initial.lighting[&(2, 0x300, 1)].1, 254);
                    assert_eq!(initial.lighting[&(2, 0x300, 7)].1, 154);
                    assert_eq!(initial.lighting[&(2, 0x300, 8)].1, 0);
                    assert_eq!(initial.lighting[&(2, 0x300, 0x4001)].1, 0);

                    // The active value must accompany a mode-only change even
                    // when its cached value was already included in priming.
                    function
                        .observe_light(LightPatch {
                            mode: Some(LightMode::Temperature),
                            ..Default::default()
                        })
                        .unwrap();
                    let temperature = receive_observed(&client, id).await.unwrap();
                    assert_colors(&temperature, &cool_white);
                    assert_ne!(
                        temperature.lighting[&(2, 0x300, 7)].0,
                        initial.lighting[&(2, 0x300, 7)].0
                    );
                    wait_committed(&state).await;

                    function
                        .observe_light(LightPatch {
                            mode: Some(LightMode::Rgb),
                            ..Default::default()
                        })
                        .unwrap();
                    let rgb = receive_observed(&client, id).await.unwrap();
                    let red = [
                        (0, 0),
                        (1, 254),
                        (3, initial.lighting[&(2, 0x300, 3)].1),
                        (4, initial.lighting[&(2, 0x300, 4)].1),
                        (8, 0),
                        (0x4001, 0),
                    ];
                    assert_colors(&rgb, &red);
                    assert_ne!(
                        rgb.lighting[&(2, 0x300, 0)].0,
                        temperature.lighting[&(2, 0x300, 7)].0
                    );
                    wait_committed(&state).await;

                    function
                        .observe_light(LightPatch {
                            temperature_kelvin: Some(2700),
                            mode: Some(LightMode::Temperature),
                            ..Default::default()
                        })
                        .unwrap();
                    let warm = receive_observed(&client, id).await.unwrap();
                    assert_colors(
                        &warm,
                        &[
                            (0, 21),
                            (1, 165),
                            (3, 30132),
                            (4, 26930),
                            (7, 370),
                            (8, 2),
                            (0x4001, 2),
                        ],
                    );
                    wait_committed(&state).await;
                    function
                        .observe_light(LightPatch {
                            temperature_kelvin: Some(6500),
                            ..Default::default()
                        })
                        .unwrap();
                    let cool = receive_observed(&client, id).await.unwrap();
                    assert_colors(&cool, &cool_white[..5]);
                    assert_ne!(
                        cool.lighting[&(2, 0x300, 7)].0,
                        warm.lighting[&(2, 0x300, 7)].0
                    );
                    wait_committed(&state).await;

                    // Both Kelvin values encode as 154 mireds. The mode edge
                    // must still report CT, while switching away reports RGB only.
                    function
                        .observe_light(LightPatch {
                            temperature_kelvin: Some(6494),
                            mode: Some(LightMode::Rgb),
                            ..Default::default()
                        })
                        .unwrap();
                    let rgb = receive_observed(&client, id).await.unwrap();
                    assert_colors(&rgb, &red);
                    wait_committed(&state).await;
                    function
                        .observe_light(LightPatch {
                            temperature_kelvin: Some(6500),
                            mode: Some(LightMode::Temperature),
                            ..Default::default()
                        })
                        .unwrap();
                    let temperature = receive_observed(&client, id).await.unwrap();
                    assert_colors(&temperature, &cool_white);
                    assert_ne!(
                        temperature.lighting[&(2, 0x300, 7)].0,
                        rgb.lighting[&(2, 0x300, 0)].0
                    );
                    wait_committed(&state).await;

                    function
                        .observe_light(LightPatch {
                            brightness_percent: Some(23.5),
                            temperature_kelvin: Some(3333),
                            color: Some(Rgb {
                                red: 255,
                                green: 128,
                                blue: 64,
                            }),
                            mode: Some(LightMode::Rgb),
                            ..Default::default()
                        })
                        .unwrap();
                    let report = receive_observed(&client, id).await.unwrap();
                    assert_eq!(
                        report.lighting[&(2, 8, 0)].1,
                        crate::device::brightness_to_matter_level(23.5).unwrap() as u16
                    );
                    assert_eq!(report.lighting[&(2, 0x300, 3)].1, 33935);
                    assert_eq!(report.lighting[&(2, 0x300, 4)].1, 25220);
                    assert!(!report.lighting.contains_key(&(2, 0x300, 7)));
                    assert_eq!(report.lighting[&(2, 0x300, 0)].1, 14);
                    assert_eq!(report.lighting[&(2, 0x300, 1)].1, 190);
                    assert_eq!(report.lighting[&(2, 0x300, 8)].1, 0);
                    assert_eq!(report.lighting[&(2, 0x300, 0x4001)].1, 0);
                    for attribute in [0, 1, 8, 0x4001] {
                        assert_eq!(
                            report.lighting[&(2, 0x300, attribute)].0,
                            report.lighting[&(2, 0x300, 3)].0,
                        );
                    }
                    assert_ne!(
                        report.lighting[&(2, 8, 0)].0,
                        initial.lighting[&(2, 8, 0)].0
                    );
                    assert_ne!(
                        report.lighting[&(2, 0x300, 3)].0,
                        initial.lighting[&(2, 0x300, 3)].0
                    );
                    wait_committed(&state).await;

                    function
                        .observe_light(LightPatch {
                            temperature_kelvin: Some(6500),
                            mode: Some(LightMode::Temperature),
                            ..Default::default()
                        })
                        .unwrap();
                    let temperature = receive_observed(&client, id).await.unwrap();
                    assert_colors(&temperature, &cool_white);
                    wait_committed(&state).await;

                    // Priming includes every requested attribute. Its HS/XY
                    // must describe the active white light, not the RGB cache.
                    let started = std::time::Instant::now();
                    let (id, priming) = subscribe_paths_with_min_interval(&client, &paths, 2)
                        .await
                        .unwrap();
                    assert_colors(&priming, &cool_white);
                    wait_committed(&state).await;
                    function
                        .observe_light(LightPatch {
                            color: Some(Rgb {
                                red: 255,
                                green: 0,
                                blue: 0,
                            }),
                            mode: Some(LightMode::Rgb),
                            ..Default::default()
                        })
                        .unwrap();
                    // A read verifies the intermediate RGB state without
                    // consuming the subscription's pending attribute changes.
                    let intermediate = read_paths(&client, &paths).await.unwrap();
                    assert_eq!(intermediate.lighting[&(2, 0x300, 0)].1, 0);
                    assert_eq!(intermediate.lighting[&(2, 0x300, 1)].1, 254);
                    assert_eq!(intermediate.lighting[&(2, 0x300, 8)].1, 0);
                    assert!(started.elapsed() < Duration::from_secs(2));
                    function
                        .observe_light(LightPatch {
                            mode: Some(LightMode::Temperature),
                            ..Default::default()
                        })
                        .unwrap();
                    // Both mode changes fall within the same report interval.
                    // Even previously marked RGB attributes must encode the
                    // final CT projection when the actual ReportData is built.
                    let merged = receive_observed(&client, id).await.unwrap();
                    assert_colors(&merged, &cool_white);
                    assert_ne!(
                        merged.lighting[&(2, 0x300, 7)].0,
                        priming.lighting[&(2, 0x300, 7)].0
                    );
                };
                or(
                    services,
                    or(controller, async {
                        async_io::Timer::after(Duration::from_secs(10)).await;
                        panic!("lighting subscription timed out");
                    }),
                )
                .await;
            });
            protocol.check_failure().unwrap();
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn light_groups_commands_route_through_real_capabilities() {
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            let directory = tempfile::tempdir().unwrap();
            let store = Store::open(directory.path()).unwrap();
            let identity = store.load_identity().unwrap();
            let info = basic_info(&identity);
            let matter = Matter::new(&info, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
            let protocol = StoreAdapter::new(store.matter());
            let kv = matter.kv(protocol.clone());
            matter.startup(&kv).unwrap();
            let client = Matter::new(&TEST_DEV_DET, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
            connect(&matter, SERVER_ID, CLIENT_ID, 32);
            connect(&client, CLIENT_ID, SERVER_ID, 32);
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
            let crypto = test_only_crypto();
            let buffers: MatterBuffers = MatterBuffers::new();
            let state: EthInteractionModelState =
                EthInteractionModelState::new(EthNetwork::new_default());
            let (device, _receiver) = super::support::light(true);
            device.set_availability(Availability::Available);
            let registry = super::test_registry(&store, &device);
            let handler = model::handler(&registry, &store, protocol.clone(), rand::rng()).unwrap();
            let im = InteractionModel::new(&matter, &crypto, &buffers, &handler, &kv, &state);
            let incoming = Pipe::default();
            let outgoing = Pipe::default();
            let responder = DefaultResponder::new(&im);
            block_on(async {
                im.startup().await.unwrap();
                handler.initialize(&im).unwrap();
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
                    panic!("light group services exited");
                };
                let controller = async {
                    group_request(&client, true).await;
                    group_request(&client, false).await;
                };
                or(services, controller).await;
            });
            protocol.check_failure().unwrap();
        })
        .unwrap()
        .join()
        .unwrap();
}

async fn read_structural_action<F, Fut>(
    client: &Matter<'_>,
    mid_action: F,
) -> Result<StructuralAction, Error>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = ()>,
{
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
                .attr_requests_from(&[AttrPath::from_gp(&GenericPath::new(None, None, None))])?
                .fabric_filtered(false)?
                .end()
        })
        .await?;
    assert_eq!(chunk.response()?.more_chunks, Some(true));
    let mut action = StructuralAction::default();
    action.add(&chunk.response()?);
    mid_action().await;
    while let Some(next) = chunk.complete().await? {
        chunk = next;
        action.add(&chunk.response()?);
    }
    Ok(action)
}

async fn subscribe_structural_action<F, Fut>(
    client: &Matter<'_>,
    mid_action: F,
) -> Result<(u32, StructuralAction), Error>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = ()>,
{
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
                    .attr_requests_from(&[AttrPath::from_gp(&GenericPath::new(None, None, None))])?
                    .fabric_filtered(false)?
                    .end()?;
            }
            TxOutcome::GotResponse(chunk) => break chunk,
        }
    };
    assert_eq!(chunk.response()?.more_chunks, Some(true));
    let mut action = StructuralAction::default();
    action.add(&chunk.response()?);
    mid_action().await;
    loop {
        match chunk.complete().await? {
            SubscribeOutcome::NextChunk(next) => {
                chunk = next;
                action.add(&chunk.response()?);
            }
            SubscribeOutcome::Established(subscription) => {
                return Ok((subscription.subscription_id, action));
            }
        }
    }
}

#[derive(Clone, Copy)]
enum ChunkedReport {
    Read,
    Subscribe,
    Incremental,
}

fn run_rebuild_during_report(kind: ChunkedReport) {
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(move || {
            let names = (0..16)
                .map(|index| format!("chunk-{index}"))
                .collect::<Vec<_>>();
            let members = names
                .iter()
                .map(|name| (name.as_str(), "devcea.light.ls2307", "Chunk light"))
                .collect::<Vec<_>>();
            let rig = super::composition::GroupRig::standalone(&members);
            for member in &rig.members {
                member.device.set_availability(Availability::Available);
                member.device.functions()[0].observe(false);
            }
            let target = rig.members.last().unwrap().endpoint;
            let subscription = Cell::new(None);
            let baseline = Cell::new(0);
            let interrupted = Cell::new(false);
            for boot in 1..=2 {
                let store = &rig.store;
                let identity = store.load_identity().unwrap();
                let info = basic_info(&identity);
                let matter = Matter::new(&info, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
                let protocol = StoreAdapter::new(store.matter());
                let kv = matter.kv(protocol.clone());
                matter.startup(&kv).unwrap();
                if boot == 2 {
                    assert!(matter.has_fabrics());
                }
                let client = Matter::new(&TEST_DEV_DET, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
                connect(&matter, SERVER_ID, CLIENT_ID, 40 + boot);
                connect(&client, CLIENT_ID, SERVER_ID, 40 + boot);
                if boot == 1 {
                    let mut fabrics = rs_matter::fabric::FabricPersist::new(&kv);
                    matter.with_state(|state| {
                        fabrics
                            .store(state.fabrics.fabric(NonZeroU8::new(1).unwrap()).unwrap())
                            .unwrap();
                    });
                    fabrics.run().unwrap();
                }
                let crypto = test_only_crypto();
                let buffers: MatterBuffers = MatterBuffers::new();
                let state: EthInteractionModelState =
                    EthInteractionModelState::new(EthNetwork::new_default());
                let handler =
                    model::handler(&rig.registry, store, protocol.clone(), rand::rng()).unwrap();
                let im = InteractionModel::new(&matter, &crypto, &buffers, &handler, &kv, &state);
                let incoming = Pipe::default();
                let outgoing = Pipe::default();
                let responder = DefaultResponder::new(&im);
                block_on(async {
                    im.startup().await.unwrap();
                    handler.initialize(&im).unwrap();
                    let version = persisted_config_version(store).unwrap();
                    if boot == 1 {
                        baseline.set(version);
                    } else {
                        assert_eq!(version, baseline.get() + 1);
                    }
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
                        assert_eq!(boot, 1);
                        assert!(interrupted.get());
                        assert!(handler.needs_rebuild());
                        // The old report never sees the new role or its version.
                        assert_eq!(persisted_config_version(store), Some(baseline.get()));
                        handler.access(|node| {
                            assert!(node.endpoint(target).unwrap().cluster(0x39).is_some())
                        });
                    };
                    let controller = async {
                        if boot == 2 {
                            let report = if let Some(id) = subscription.get() {
                                receive_structural_action(&client, id, true, || async {})
                                    .await
                                    .unwrap()
                            } else {
                                read_structural_action(&client, || async {}).await.unwrap()
                            };
                            assert_eq!(report.versions, [baseline.get() + 1], "{report:?}");
                            assert!(!report.server_lists[&target].contains(&0x39));
                            assert_eq!(report.powers.len(), rig.members.len());
                            if subscription.get().is_some() {
                                wait_committed(&state).await;
                            }
                            return;
                        }
                        if matches!(kind, ChunkedReport::Incremental) {
                            let id = subscribe_wildcard(&client).await.unwrap();
                            subscription.set(Some(id));
                            wait_committed(&state).await;
                            // State changes produce a multi-chunk incremental report
                            // without introducing an earlier topology transition.
                            for member in &rig.members {
                                member.device.functions()[0]
                                    .observe_light(crate::device::LightPatch {
                                        power: Some(true),
                                        brightness_percent: Some(60.0),
                                        temperature_kelvin: Some(4000),
                                        color: Some(crate::device::Rgb {
                                            red: 255,
                                            green: 64,
                                            blue: 0,
                                        }),
                                        mode: Some(crate::device::LightMode::Rgb),
                                    })
                                    .unwrap();
                            }
                        }
                        let stop_mid_action = || async {
                            interrupted.set(true);
                            let names = names.iter().map(String::as_str).collect::<Vec<_>>();
                            rig.recompose(&names);
                            // The controller withholds its next chunk acknowledgement.
                            // Rebuild must cancel services instead of waiting for it.
                            std::future::pending::<()>().await;
                        };
                        match kind {
                            ChunkedReport::Read => {
                                read_structural_action(&client, stop_mid_action)
                                    .await
                                    .unwrap();
                            }
                            ChunkedReport::Subscribe => {
                                subscribe_structural_action(&client, stop_mid_action)
                                    .await
                                    .unwrap();
                            }
                            ChunkedReport::Incremental => {
                                receive_structural_action(
                                    &client,
                                    subscription.get().unwrap(),
                                    true,
                                    stop_mid_action,
                                )
                                .await
                                .unwrap();
                            }
                        }
                        panic!("an interrupted old request must not complete");
                    };
                    or(
                        services,
                        or(controller, async {
                            async_io::Timer::after(Duration::from_secs(10)).await;
                            panic!("topology rebuild during report timed out on boot {boot}");
                        }),
                    )
                    .await;
                });
                protocol.check_failure().unwrap();
                // All old services, exchange futures, model and Matter state drop
                // before the next boot constructs a new service from the same store.
            }
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn rebuild_cancels_chunked_read_before_starting_new_topology() {
    run_rebuild_during_report(ChunkedReport::Read);
}

#[test]
fn rebuild_cancels_subscription_priming_before_starting_new_topology() {
    run_rebuild_during_report(ChunkedReport::Subscribe);
}

#[test]
fn rebuild_cancels_incremental_report_and_resumes_subscription_on_new_topology() {
    run_rebuild_during_report(ChunkedReport::Incremental);
}
