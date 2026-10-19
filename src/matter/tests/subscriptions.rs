use super::super::{
    model::{self, basic_info},
    storage::StoreAdapter,
};
use super::support::{model_handler, next_power};
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
    let command_function = device.functions()[0].clone();
    let initial = boot == 3;
    let rejection = RefCell::new(None::<CommandError>);
    let accepted = Cell::new(0);
    let registry = super::test_registry(&store, &device);
    let handler = model_handler(&registry, &store, rand::rng()).unwrap();
    let im = InteractionModel::new(&matter, &crypto, &buffers, &handler, &kv, &state);
    let incoming = Pipe::default();
    let outgoing = Pipe::default();
    let responder = DefaultResponder::new(&im);

    let id = block_on(async {
        im.startup().await.unwrap();
        let services = async {
            let adapter = async {
                loop {
                    let request = next_power(&mut receiver).await.unwrap();
                    if let Some(error) = rejection.borrow_mut().take() {
                        request.reject(error);
                    } else {
                        command_function.observe(request.target);
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
    let handler = model_handler(&registry, &store, rand::rng()).unwrap();
    let im = InteractionModel::new(&matter, &crypto, &buffers, &handler, &kv, &state);
    let incoming = Pipe::default();
    let outgoing = Pipe::default();
    let responder = DefaultResponder::new(&im);

    let id = block_on(async {
        im.startup().await.unwrap();
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
            if previous_subscription.is_some() {
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
                // Synchronize with the actual retirement instead of a time delay.
                // Upstream gives up on a restore whose resolve failed without
                // signalling `wait_report_failed`, so wait for the old
                // subscription to disappear.
                while state
                    .subscriptions()
                    .has_subscription_for(NonZeroU8::new(1).unwrap(), CLIENT_ID)
                {
                    futures_lite::future::yield_now().await;
                }
                assert!(
                    session_probe.pending_retrans().is_ok(),
                    "a failed restored-subscription operation removed the new inbound CASE session"
                );
                // A replacement subscription is established over the same
                // inbound session that the race already bound; no reconnect.
                let replacement = subscribe(&client, false).await.unwrap();
                wait_committed(&state).await;
                for power in [true, false] {
                    device.functions()[0].observe(power);
                    expect_report(&client, replacement, Some(power), None)
                        .await
                        .unwrap();
                    wait_committed(&state).await;
                }
                replacement
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
            // Upstream retires a failed restore and the replacement subscription
            // gets its own ID, so no relationship with `previous` is required.
            recovery_race_boot(directory.path(), Some(previous));
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

fn dynamic_boot(directory: &Path, boot: u16, previous: Option<(u32, String)>) -> (u32, String) {
    let store = Store::open(directory).unwrap();
    let identity = store.load_identity().unwrap();
    let info = basic_info(&identity);
    let matter = Matter::new(&info, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
    let protocol = StoreAdapter::new(store.matter());
    let kv = matter.kv(protocol.clone());
    matter.startup(&kv).unwrap();
    let client = Matter::new(&TEST_DEV_DET, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
    connect(&matter, SERVER_ID, CLIENT_ID, boot + 20);
    connect(&client, CLIENT_ID, SERVER_ID, boot + 20);
    let crypto = test_only_crypto();
    let buffers: MatterBuffers = MatterBuffers::new();
    let state: EthInteractionModelState = EthInteractionModelState::new(EthNetwork::new_default());
    let (single, _single_receivers) = super::physical("dynamic-single", false);
    let (multi, _multi_receivers) = super::physical("dynamic-multi", true);
    if boot == 1 {
        for device in [&single, &multi] {
            device.set_availability(Availability::Available);
            for function in device.functions() {
                function.observe(false);
            }
            super::support::add_device(&store, &device.description());
        }
    }
    // Each boot serves exactly the committed published set, frozen.
    let registry = crate::device::DeviceRegistry::new();
    let mut single_physical = None;
    let mut multi_physical = None;
    let mut multi_stored = None;
    let configuration = store.devices().load_configuration().unwrap();
    let derived = configuration.derive();
    for stored in &configuration.devices {
        if !derived.published.contains(&stored.key) {
            continue;
        }
        let (physical, _receivers) =
            crate::device::PhysicalDevice::new(derived.served[&stored.key].description.clone());
        physical.set_availability(Availability::Available);
        for function in physical.functions() {
            function.observe(false);
        }
        registry.insert(physical.clone());
        match stored.key.id.as_str() {
            "dynamic-single" => single_physical = Some(physical),
            "dynamic-multi" => {
                multi_physical = Some(physical);
                multi_stored = Some(stored.clone());
            }
            _ => {}
        }
    }
    let single_physical = single_physical.unwrap();
    let handler = model_handler(&registry, &store, rand::rng()).unwrap();
    let im = InteractionModel::new(&matter, &crypto, &buffers, &handler, &kv, &state);
    let incoming = Pipe::default();
    let outgoing = Pipe::default();
    let responder = DefaultResponder::new(&im);
    let id = block_on(async {
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
            panic!("dynamic Matter services exited");
        };
        let controller = async {
            let mut paths = vec![
                AttrPath::from_gp(&GenericPath::new(Some(0), Some(0x1d), Some(3))),
                AttrPath::from_gp(&GenericPath::new(Some(1), Some(0x1d), Some(3))),
            ];
            let (id, unique_id) = match boot {
                1 => {
                    let multi_physical = multi_physical.as_ref().unwrap();
                    let multi_stored = multi_stored.clone().unwrap();
                    let (_, first) = subscribe_paths(&client, &paths).await.unwrap();
                    wait_committed(&state).await;
                    assert!(first.powers.is_empty());
                    assert_eq!(first.parts[&0], [1, 2, 3, 4, 5, 6]);
                    assert_eq!(first.parts[&1], [2, 3, 4, 5, 6]);
                    paths.insert(
                        0,
                        AttrPath::from_gp(&GenericPath::new(None, Some(6), Some(0))),
                    );
                    paths.push(AttrPath::from_gp(&GenericPath::new(
                        Some(4),
                        Some(6),
                        Some(0),
                    )));
                    paths.push(AttrPath::from_gp(&GenericPath::new(
                        Some(2),
                        Some(0x39),
                        Some(24),
                    )));
                    let (id, baseline) = subscribe_paths(&client, &paths).await.unwrap();
                    wait_committed(&state).await;
                    assert_eq!(
                        baseline.powers.keys().copied().collect::<Vec<_>>(),
                        [2, 4, 5, 6]
                    );
                    assert!(baseline.powers.values().all(|(_, power)| !power));
                    assert_eq!(baseline.bdbis[&2].1, 1);
                    multi_physical.function("usb").unwrap().observe(true);
                    let changed = receive_observed(&client, id).await.unwrap();
                    assert_eq!(
                        changed.powers[&6],
                        (baseline.powers[&6].0.wrapping_add(1), true)
                    );
                    assert!(!changed.powers.contains_key(&2));
                    wait_committed(&state).await;
                    let registry = crate::device::DeviceRegistry::new();
                    registry.insert(multi_physical.clone());
                    super::support::renamer(&registry, &store)
                        .write_label(&multi_stored.key, "Named multi")
                        .unwrap();
                    commit_presence(&store, "dynamic-multi", None, 2, 2);
                    (id, multi_stored.unique_id.clone())
                }
                2 => {
                    let (id, unique_id) = previous.unwrap();
                    let restored = receive_observed(&client, id).await.unwrap();
                    assert_eq!(restored.unsupported, [4]);
                    assert_eq!(restored.parts[&0], [1, 2]);
                    assert_eq!(restored.parts[&1], [2]);
                    assert_eq!(restored.powers.keys().copied().collect::<Vec<_>>(), [2]);
                    wait_committed(&state).await;
                    commit_presence(
                        &store,
                        "dynamic-multi",
                        Some(super::support::native_for(&multi.description())),
                        3,
                        3,
                    );
                    let readded = store
                        .devices()
                        .load_configuration()
                        .unwrap()
                        .device(multi.key())
                        .cloned()
                        .unwrap();
                    assert_eq!(readded.unique_id, unique_id);
                    assert_eq!(readded.label, "Named multi");
                    (id, readded.unique_id)
                }
                _ => {
                    let multi_physical = multi_physical.as_ref().unwrap();
                    let (id, unique_id) = previous.unwrap();
                    let added = receive_observed(&client, id).await.unwrap();
                    assert_eq!(added.parts[&0], [1, 2, 3, 4, 5, 6]);
                    assert_eq!(added.parts[&1], [2, 3, 4, 5, 6]);
                    assert_eq!(
                        added.powers.keys().copied().collect::<Vec<_>>(),
                        [2, 4, 5, 6]
                    );
                    assert!(!added.powers[&2].1);
                    assert!(!added.powers[&4].1);
                    wait_committed(&state).await;
                    multi_physical.function("outlet_1").unwrap().observe(true);
                    let changed = receive_observed(&client, id).await.unwrap();
                    assert_eq!(
                        changed.powers[&4],
                        (added.powers[&4].0.wrapping_add(1), true)
                    );
                    wait_committed(&state).await;
                    (id, unique_id)
                }
            };
            single_physical.functions()[0].observe(true);
            let changed = receive_observed(&client, id).await.unwrap();
            assert!(changed.powers[&2].1);
            wait_committed(&state).await;
            (id, unique_id)
        };
        or(
            services,
            or(controller, async {
                async_io::Timer::after(Duration::from_secs(5)).await;
                panic!("dynamic subscription boot {boot} timed out");
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

fn commit_presence(
    store: &Store,
    id: &str,
    native: Option<crate::xiaomi::capability::NativeDescription>,
    version: u32,
    root: u32,
) {
    let mut configuration = store.devices().load_configuration().unwrap();
    let record = configuration
        .devices
        .iter_mut()
        .find(|record| record.key.id == id)
        .unwrap();
    record.native = native;
    record.configuration_version = version;
    configuration.root_version = root;
    store
        .devices()
        .commit_configuration(&configuration, &[])
        .unwrap();
}

#[test]
fn dynamic_subscription_reports_add_delete_and_restores_deleted_paths_after_restart() {
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            let directory = tempfile::tempdir().unwrap();
            let id = dynamic_boot(directory.path(), 1, None);
            let (id, unique_id) = dynamic_boot(directory.path(), 2, Some(id));
            assert_eq!(
                dynamic_boot(directory.path(), 3, Some((id, unique_id))).0,
                id
            );
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
            let handler = model_handler(&registry, &store, rand::rng()).unwrap();
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
                        crate::matter::units::level_from_brightness(50.0).unwrap() as u16
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
                        crate::matter::units::level_from_brightness(23.5).unwrap() as u16
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

fn frozen_light_boot(directory: &Path, boot: u16, previous: Option<u32>) -> (u32, Observed) {
    let store = Store::open(directory).unwrap();
    let identity = store.load_identity().unwrap();
    let info = basic_info(&identity);
    let matter = Matter::new(&info, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
    let protocol = StoreAdapter::new(store.matter());
    let kv = matter.kv(protocol.clone());
    matter.startup(&kv).unwrap();
    let root_version = if boot == 1 { 1 } else { 2 };
    let settings = model::prepare_basic_info(&kv, root_version).unwrap();
    matter.with_state(|state| state.basic_info_settings = settings);
    let client = Matter::new(&TEST_DEV_DET, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
    connect(&matter, SERVER_ID, CLIENT_ID, boot + 30);
    connect(&client, CLIENT_ID, SERVER_ID, boot + 30);
    let crypto = test_only_crypto();
    let buffers: MatterBuffers = MatterBuffers::new();
    let state: EthInteractionModelState = EthInteractionModelState::new(EthNetwork::new_default());
    let (device, _receiver) = super::support::light(false);
    if boot == 1 {
        device.set_availability(Availability::Available);
        super::support::add_device(&store, &device.description());
    }
    let configuration = store.devices().load_configuration().unwrap();
    let derived = configuration.derive();
    let stored = configuration.device(device.key()).unwrap().clone();
    assert!(derived.published.contains(&stored.key));
    let (physical, _receivers) =
        crate::device::PhysicalDevice::new(derived.served[&stored.key].description.clone());
    physical.set_availability(Availability::Available);
    let registry = crate::device::DeviceRegistry::new();
    registry.insert(physical);
    let handler = model_handler(&registry, &store, rand::rng()).unwrap();
    let im = InteractionModel::new(&matter, &crypto, &buffers, &handler, &kv, &state);
    let incoming = Pipe::default();
    let outgoing = Pipe::default();
    let responder = DefaultResponder::new(&im);
    let (id, observed) = block_on(async {
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
            panic!("frozen light services exited");
        };
        let controller = async {
            let paths = [
                (Some(0), 0x28, Some(24)),
                (None, 0x39, Some(24)),
                (None, 0x300, Some(0x400b)),
                (None, 0x300, Some(0x400c)),
            ]
            .map(|(endpoint, cluster, attr)| {
                AttrPath::from_gp(&GenericPath::new(endpoint, Some(cluster), attr))
            });
            match boot {
                1 => {
                    let (id, initial) = subscribe_paths(&client, &paths).await.unwrap();
                    wait_committed(&state).await;
                    (id, initial)
                }
                _ => {
                    let id = previous.unwrap();
                    let changed = receive_observed(&client, id).await.unwrap();
                    wait_committed(&state).await;
                    (id, changed)
                }
            }
        };
        or(
            services,
            or(controller, async {
                async_io::Timer::after(Duration::from_secs(5)).await;
                panic!("frozen light boot {boot} timed out");
            }),
        )
        .await
    });
    if boot == 1 {
        // Commit the fixed change between instances: narrower CT bounds, a
        // bumped endpoint version and a bumped root version.
        let description = device.description();
        let mut functions = description.functions().to_vec();
        functions[0].kind =
            crate::device::FunctionKind::Light(crate::device::LightProfile::ColorTemperature(
                crate::device::TemperatureInterval::new(3000, 4000).unwrap(),
            ));
        let (replacement, _receivers) = crate::device::PhysicalDevice::new(
            crate::device::PhysicalDeviceDescription::new(
                description.key().clone(),
                description.label(),
                functions,
            )
            .unwrap(),
        );
        commit_presence(
            &store,
            &stored.key.id,
            Some(super::support::native_for(&replacement.description())),
            2,
            2,
        );
    }
    protocol.check_failure().unwrap();
    (id, observed)
}

#[test]
fn frozen_light_report_action_carries_root_bdbi_and_changed_bounds() {
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            let directory = tempfile::tempdir().unwrap();
            let (subscription, before) = frozen_light_boot(directory.path(), 1, None);
            assert_eq!(before.roots[&0].1, 1);
            assert_eq!(before.bdbis[&2].1, 1);
            assert_eq!(before.lighting[&(2, 0x300, 0x400b)].1, 154);
            assert_eq!(before.lighting[&(2, 0x300, 0x400c)].1, 370);

            let (_, after) = frozen_light_boot(directory.path(), 2, Some(subscription));
            // One real ReportDataAction (fragments permitted) carries the
            // committed root, frozen endpoint version and changed effective
            // bounds coherently through the protocol.
            assert_eq!(after.roots[&0].1, 2);
            assert_eq!(after.bdbis[&2].1, 2);
            assert_eq!(after.lighting[&(2, 0x300, 0x400b)].1, 250);
            assert_eq!(after.lighting[&(2, 0x300, 0x400c)].1, 333);
        })
        .unwrap()
        .join()
        .unwrap();
}
