use crate::{
    device::{DeviceCommand, Property, PropertyValue},
    xiaomi::{
        catalog::{Mcn02LegacyMapping, WireValue},
        discovery::{LocalBinder, NetworkEpoch},
    },
};
use async_io::{Async, Timer};
use event_listener::Event as WakeEvent;
use flume::{Receiver, Sender};
use futures_lite::future;
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    future::Future,
    net::{SocketAddr, SocketAddrV4, UdpSocket},
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use super::{
    LanError, LanErrorKind, LanSubscriptionHint, LanTarget,
    discovery::parse_hello,
    packet::{LEGACY_PROBE, MAX_PACKET_LENGTH, decode_packet, encode_packet, native_probe},
    protocol::{
        accept_action, accept_legacy, accept_result, decode_wire, encode_wire, is_rpc_response,
        parse_notifications, parse_reads, parse_writes, reject_duplicate_properties, result_code,
        valid_authentication_response, validate_property,
    },
};

const QUEUE_CAPACITY: usize = 32;
const EVENT_CAPACITY: usize = 128;
const DEDUPE_CAPACITY: usize = 128;
const LOCAL_LIMIT: Duration = Duration::from_secs(3);
const CLOCK_SKEW_LIMIT: i32 = 30;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct LanProperty {
    pub siid: u32,
    pub piid: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct LanPropertyWrite {
    pub property: LanProperty,
    pub value: WireValue,
}

#[derive(Clone, Debug, PartialEq)]
pub enum LanReadOutcome {
    Value(WireValue),
    Unknown,
    Error(i64),
}

#[derive(Clone, Debug, PartialEq)]
pub struct LanPropertyRead {
    pub property: LanProperty,
    pub outcome: LanReadOutcome,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LanWriteOutcome {
    Accepted,
    Error(i64),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LanEvidence {
    pub did: u64,
    pub address: SocketAddrV4,
    pub interface_index: u32,
    pub epoch: NetworkEpoch,
    pub signed_timestamp: u32,
    pub native_supported: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LanSubscription {
    pub generation: u64,
    pub timestamp: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct LanEventArgument {
    pub piid: u32,
    pub value: WireValue,
}

#[derive(Clone, Debug, PartialEq)]
pub enum LanEventArguments {
    Keyed(Vec<LanEventArgument>),
    Positional(Vec<WireValue>),
}

#[derive(Clone, Debug, PartialEq)]
pub enum LanNotification {
    Property {
        did: u64,
        siid: u32,
        piid: u32,
        value: WireValue,
        epoch: NetworkEpoch,
        generation: u64,
        timestamp: u32,
    },
    Event {
        did: u64,
        siid: u32,
        eiid: u32,
        arguments: LanEventArguments,
        epoch: NetworkEpoch,
        generation: u64,
        timestamp: u32,
    },
    SubscriptionHint {
        did: u64,
        hint: LanSubscriptionHint,
        epoch: NetworkEpoch,
    },
}

#[derive(Clone)]
pub struct LanSendGuard {
    active: Arc<AtomicBool>,
    started: Arc<AtomicBool>,
    check: Rc<dyn Fn() -> Result<bool, LanError>>,
    cancelled: Arc<WakeEvent>,
    observer: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl LanSendGuard {
    pub fn new() -> Self {
        Self {
            active: Arc::new(AtomicBool::new(true)),
            started: Arc::new(AtomicBool::new(false)),
            check: Rc::new(|| Ok(true)),
            cancelled: Arc::new(WakeEvent::new()),
            observer: None,
        }
    }

    pub fn with_check(check: impl Fn() -> Result<bool, LanError> + 'static) -> Self {
        Self {
            active: Arc::new(AtomicBool::new(true)),
            started: Arc::new(AtomicBool::new(false)),
            check: Rc::new(check),
            cancelled: Arc::new(WakeEvent::new()),
            observer: None,
        }
    }

    pub fn with_send_observer(mut self, observer: impl Fn() + Send + Sync + 'static) -> Self {
        self.observer = Some(Arc::new(observer));
        self
    }

    pub fn revoke(&self) {
        self.active.store(false, Ordering::Release);
        self.cancelled.notify(usize::MAX);
    }

    pub fn may_have_been_sent(&self) -> bool {
        self.started.load(Ordering::Acquire)
    }

    pub fn child(&self) -> Self {
        let parent = self.clone();
        let cancelled = parent.cancelled.clone();
        Self {
            active: Arc::new(AtomicBool::new(true)),
            started: Arc::new(AtomicBool::new(false)),
            check: Rc::new(move || parent.check()),
            cancelled,
            observer: self.observer.clone(),
        }
    }

    pub(super) fn check(&self) -> Result<bool, LanError> {
        if !self.active.load(Ordering::Acquire) {
            return Ok(false);
        }
        (self.check)()
    }

    pub(super) async fn cancelled_signal(&self) {
        self.cancelled.listen().await;
    }

    fn mark_sent(&self) {
        self.started.store(true, Ordering::Release);
        if let Some(observer) = &self.observer {
            observer();
        }
    }
}

impl Default for LanSendGuard {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for LanSendGuard {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LanSendGuard")
            .finish_non_exhaustive()
    }
}

#[derive(Clone)]
pub struct LanHandle {
    commands: Sender<Command>,
    did: u64,
    model: String,
    virtual_did: u64,
    address: SocketAddrV4,
    interface_index: u32,
    epoch: NetworkEpoch,
    running: Arc<AtomicBool>,
    next_id: Arc<AtomicU32>,
    stopped: Arc<WakeEvent>,
    needs_hello: Arc<AtomicBool>,
    last_subscription_timestamp: Arc<AtomicU32>,
}

enum Command {
    Hello {
        deadline: Instant,
        guard: LanSendGuard,
        reply: Sender<Result<u32, LanError>>,
    },
    Request {
        id: u32,
        method: &'static str,
        params: Value,
        purpose: RequestPurpose,
        deadline: Instant,
        guard: LanSendGuard,
        reply: Sender<Result<Response, LanError>>,
    },
    Stop,
}

#[derive(Clone, Copy)]
enum RequestPurpose {
    Authenticate { property: LanProperty },
    General,
    Subscribe { timestamp: u32 },
    Unsubscribe,
}

struct Pending {
    purpose: RequestPurpose,
    deadline: Instant,
    guard: LanSendGuard,
    reply: Sender<Result<Response, LanError>>,
    selection_generation: Option<u64>,
}

struct PendingHello {
    deadline: Instant,
    guard: LanSendGuard,
    reply: Sender<Result<u32, LanError>>,
}

struct Response {
    value: Value,
    timestamp: u32,
    subscription_generation: Option<u64>,
    may_have_been_sent: bool,
}

#[derive(Clone)]
struct ActiveSubscription {
    public: LanSubscription,
    authority: LanSendGuard,
}

struct Cancellation {
    guard: LanSendGuard,
    armed: bool,
}

impl Drop for Cancellation {
    fn drop(&mut self) {
        if self.armed {
            self.guard.revoke();
        }
    }
}

pub struct LanSession {
    target: LanTarget,
    virtual_did: u64,
    socket: Async<UdpSocket>,
    commands: Receiver<Command>,
    notifications: Sender<LanNotification>,
    running: Arc<AtomicBool>,
    stopped: Arc<WakeEvent>,
    needs_hello: Arc<AtomicBool>,
}

impl LanSession {
    pub fn new(
        target: LanTarget,
        virtual_did: u64,
    ) -> Result<(Self, LanHandle, Receiver<LanNotification>), LanError> {
        if virtual_did == 0 {
            return Err(LanError::new(
                "configure LAN session",
                LanErrorKind::InvalidInput,
            ));
        }
        let socket = LocalBinder::new(target.interface().clone())
            .udp_socket()
            .map_err(|_| LanError::new("bind LAN socket", LanErrorKind::Transport))?;
        Self::with_socket(target, virtual_did, socket)
    }

    fn with_socket(
        target: LanTarget,
        virtual_did: u64,
        socket: Async<UdpSocket>,
    ) -> Result<(Self, LanHandle, Receiver<LanNotification>), LanError> {
        if virtual_did == 0 {
            return Err(LanError::new(
                "configure LAN session",
                LanErrorKind::InvalidInput,
            ));
        }
        let (command_sender, commands) = flume::bounded(QUEUE_CAPACITY);
        let (notifications, notification_receiver) = flume::bounded(EVENT_CAPACITY);
        let running = Arc::new(AtomicBool::new(true));
        let stopped = Arc::new(WakeEvent::new());
        let next_id = Arc::new(AtomicU32::new(rand::random::<u32>().max(1)));
        let needs_hello = Arc::new(AtomicBool::new(target.timestamp_hint().is_none()));
        let last_subscription_timestamp = Arc::new(AtomicU32::new(0));
        let handle = LanHandle {
            commands: command_sender,
            did: target.did(),
            model: target.model().into(),
            virtual_did,
            address: target.address(),
            interface_index: target.interface().index(),
            epoch: target.epoch(),
            running: running.clone(),
            next_id,
            stopped: stopped.clone(),
            needs_hello: needs_hello.clone(),
            last_subscription_timestamp,
        };
        Ok((
            Self {
                target,
                virtual_did,
                socket,
                commands,
                notifications,
                running,
                stopped,
                needs_hello,
            },
            handle,
            notification_receiver,
        ))
    }

    #[cfg(test)]
    pub(super) fn for_test(
        target: LanTarget,
        virtual_did: u64,
        socket: Async<UdpSocket>,
    ) -> Result<(Self, LanHandle, Receiver<LanNotification>), LanError> {
        let result = Self::with_socket(target, virtual_did, socket)?;
        result.1.needs_hello.store(false, Ordering::Release);
        Ok(result)
    }

    #[cfg(test)]
    pub(super) fn for_test_requiring_hello(
        target: LanTarget,
        virtual_did: u64,
        socket: Async<UdpSocket>,
    ) -> Result<(Self, LanHandle, Receiver<LanNotification>), LanError> {
        Self::with_socket(target, virtual_did, socket)
    }

    #[cfg(test)]
    pub(super) fn local_address_for_test(&self) -> SocketAddrV4 {
        match self.socket.get_ref().local_addr() {
            Ok(SocketAddr::V4(address)) => address,
            _ => unreachable!("test LAN session uses IPv4 loopback"),
        }
    }

    pub async fn run(self) -> Result<(), LanError> {
        let mut pending = HashMap::<u32, Pending>::new();
        let mut pending_hello = None::<PendingHello>;
        let mut authenticated = false;
        let mut clock_anchor = self
            .target
            .timestamp_hint()
            .map(|timestamp| (timestamp, Instant::now()));
        let mut subscription = None::<ActiveSubscription>;
        let mut subscription_generation = rand::random::<u64>().max(1);
        let mut subscription_selection = 0_u64;
        let mut dedupe = HashSet::<u32>::new();
        let mut dedupe_order = VecDeque::<u32>::new();
        let mut receive_buffer = [0_u8; MAX_PACKET_LENGTH + 1];
        let mut datagrams_since_yield = 0_u8;
        loop {
            if datagrams_since_yield >= 32 {
                future::yield_now().await;
                datagrams_since_yield = 0;
            }
            if !self.running.load(Ordering::Acquire) {
                return Ok(());
            }
            let now = Instant::now();
            if pending_hello
                .as_ref()
                .is_some_and(|hello| hello.reply.is_disconnected() || now >= hello.deadline)
                && let Some(hello) = pending_hello.take()
                && !hello.reply.is_disconnected()
            {
                let kind = if now >= hello.deadline {
                    LanErrorKind::Timeout
                } else {
                    LanErrorKind::Cancelled
                };
                let _ = hello.reply.send(Err(LanError {
                    operation: "wait for LAN hello",
                    kind,
                    may_have_been_sent: hello.guard.may_have_been_sent(),
                }));
            }
            pending.retain(|_, request| {
                if request.reply.is_disconnected() {
                    request.guard.revoke();
                    return false;
                }
                if now >= request.deadline {
                    let _ = request.reply.send(Err(LanError {
                        operation: "wait for LAN reply",
                        kind: LanErrorKind::Timeout,
                        may_have_been_sent: request.guard.may_have_been_sent(),
                    }));
                    return false;
                }
                true
            });
            enum Event {
                Command(Result<Command, flume::RecvError>),
                Datagram(std::io::Result<(usize, SocketAddr)>),
                Deadline,
            }
            let next_deadline = pending
                .values()
                .map(|request| request.deadline)
                .chain(pending_hello.iter().map(|hello| hello.deadline))
                .min();
            let event = future::or(
                async { Event::Command(self.commands.recv_async().await) },
                future::or(
                    async { Event::Datagram(self.socket.recv_from(&mut receive_buffer).await) },
                    async move {
                        if let Some(deadline) = next_deadline {
                            Timer::after(deadline.saturating_duration_since(Instant::now())).await;
                            Event::Deadline
                        } else {
                            future::pending::<Event>().await
                        }
                    },
                ),
            )
            .await;
            match event {
                Event::Command(Ok(Command::Stop)) | Event::Command(Err(_)) => return Ok(()),
                Event::Command(Ok(Command::Hello {
                    deadline,
                    guard,
                    reply,
                })) => {
                    if pending_hello.is_some() || reply.is_disconnected() {
                        let _ = reply.send(Err(LanError::new(
                            "start LAN hello",
                            LanErrorKind::Cancelled,
                        )));
                        continue;
                    }
                    match guard.check() {
                        Ok(true) => {}
                        Ok(false) => {
                            let _ = reply.send(Err(LanError::new(
                                "start LAN hello",
                                LanErrorKind::Cancelled,
                            )));
                            continue;
                        }
                        Err(error) => {
                            let _ = reply.send(Err(error));
                            continue;
                        }
                    }
                    let mut send_error = None;
                    for probe in [native_probe(self.virtual_did), LEGACY_PROBE] {
                        if let Err(error) = send_datagram(
                            &self.socket,
                            &probe,
                            self.target.address(),
                            SendContext {
                                guard: &guard,
                                running: &self.running,
                                stopped: &self.stopped,
                                deadline,
                                force_pending: None,
                            },
                        )
                        .await
                        {
                            send_error = Some(error.after_send(guard.may_have_been_sent()));
                            break;
                        }
                        guard.mark_sent();
                    }
                    if let Some(error) = send_error {
                        let _ = reply.send(Err(error));
                        continue;
                    }
                    pending_hello = Some(PendingHello {
                        deadline,
                        guard,
                        reply,
                    });
                }
                Event::Command(Ok(Command::Request {
                    id,
                    method,
                    params,
                    purpose,
                    deadline,
                    guard,
                    reply,
                })) => {
                    if reply.is_disconnected() {
                        continue;
                    }
                    match guard.check() {
                        Ok(true) => {}
                        Ok(false) => {
                            let _ = reply.send(Err(LanError::new(
                                "send LAN request",
                                LanErrorKind::Cancelled,
                            )));
                            continue;
                        }
                        Err(error) => {
                            let _ = reply.send(Err(error.after_send(guard.may_have_been_sent())));
                            continue;
                        }
                    }
                    if Instant::now() >= deadline {
                        let _ = reply.send(Err(LanError::new(
                            "send LAN request",
                            LanErrorKind::Timeout,
                        )));
                        continue;
                    }
                    if !matches!(purpose, RequestPurpose::Authenticate { .. }) && !authenticated {
                        let _ = reply.send(Err(LanError::new(
                            "send LAN request",
                            LanErrorKind::NotAuthenticated,
                        )));
                        continue;
                    }
                    if pending.len() >= QUEUE_CAPACITY || pending.contains_key(&id) {
                        let _ = reply.send(Err(LanError::new(
                            "send LAN request",
                            LanErrorKind::Transport,
                        )));
                        continue;
                    }
                    let selection_generation = if matches!(
                        purpose,
                        RequestPurpose::Subscribe { .. } | RequestPurpose::Unsubscribe
                    ) {
                        subscription_selection = subscription_selection.wrapping_add(1).max(1);
                        if matches!(purpose, RequestPurpose::Unsubscribe) {
                            subscription = None;
                        }
                        Some(subscription_selection)
                    } else {
                        None
                    };
                    let timestamp = clock_timestamp(clock_anchor);
                    let plaintext = json!({"id":id,"method":method,"params":params}).to_string();
                    let packet = encode_packet(
                        self.target.did(),
                        timestamp,
                        self.target.token(),
                        plaintext.as_bytes(),
                    )?;
                    match send_datagram(
                        &self.socket,
                        &packet,
                        self.target.address(),
                        SendContext {
                            guard: &guard,
                            running: &self.running,
                            stopped: &self.stopped,
                            deadline,
                            force_pending: None,
                        },
                    )
                    .await
                    {
                        Ok(()) => {
                            guard.mark_sent();
                            pending.insert(
                                id,
                                Pending {
                                    purpose,
                                    deadline,
                                    guard,
                                    reply,
                                    selection_generation,
                                },
                            );
                        }
                        Err(error) => {
                            let _ = reply.send(Err(error));
                        }
                    }
                }
                Event::Datagram(Ok((length, source))) => {
                    datagrams_since_yield = datagrams_since_yield.saturating_add(1);
                    if length > MAX_PACKET_LENGTH {
                        continue;
                    }
                    if source != SocketAddr::V4(self.target.address()) {
                        continue;
                    }
                    if length == 32 {
                        let Some(hello) = parse_hello(&receive_buffer[..length]) else {
                            continue;
                        };
                        if hello.did != self.target.did() {
                            continue;
                        }
                        if let Some(pending) = pending_hello.take()
                            && Instant::now() < pending.deadline
                        {
                            clock_anchor = Some((hello.timestamp, Instant::now()));
                            self.needs_hello.store(false, Ordering::Release);
                            let _ = pending.reply.send(Ok(hello.timestamp));
                        }
                        if let Some(hint) = hello.subscription_hint {
                            let _ =
                                self.notifications
                                    .try_send(LanNotification::SubscriptionHint {
                                        did: self.target.did(),
                                        hint,
                                        epoch: self.target.epoch(),
                                    });
                        }
                        continue;
                    }
                    let Ok(packet) = decode_packet(
                        &receive_buffer[..length],
                        self.target.did(),
                        self.target.token(),
                    ) else {
                        continue;
                    };
                    let Some(id) = packet
                        .message
                        .get("id")
                        .and_then(Value::as_u64)
                        .and_then(|id| u32::try_from(id).ok())
                    else {
                        continue;
                    };
                    if pending.contains_key(&id) && is_rpc_response(&packet.message) {
                        let request = pending.remove(&id).ok_or_else(|| {
                            LanError::new("correlate LAN reply", LanErrorKind::Protocol)
                        })?;
                        if Instant::now() >= request.deadline {
                            let _ = request.reply.send(Err(LanError {
                                operation: "wait for LAN reply",
                                kind: LanErrorKind::Timeout,
                                may_have_been_sent: request.guard.may_have_been_sent(),
                            }));
                            continue;
                        }
                        if matches!(request.purpose, RequestPurpose::Authenticate { property } if !valid_authentication_response(self.target.did(), property, &packet.message))
                        {
                            let _ = request.reply.send(Err(LanError {
                                operation: "authenticate LAN peer",
                                kind: LanErrorKind::Protocol,
                                may_have_been_sent: request.guard.may_have_been_sent(),
                            }));
                            continue;
                        }
                        if clock_anchor.is_some_and(|anchor| {
                            !timestamp_is_current(packet.timestamp, clock_timestamp(Some(anchor)))
                        }) {
                            authenticated = false;
                            subscription = None;
                            self.needs_hello.store(true, Ordering::Release);
                            let _ = request.reply.send(Err(LanError {
                                operation: "validate LAN clock",
                                kind: LanErrorKind::Protocol,
                                may_have_been_sent: request.guard.may_have_been_sent(),
                            }));
                            continue;
                        }
                        if matches!(request.purpose, RequestPurpose::Authenticate { .. }) {
                            authenticated = true;
                            clock_anchor = Some((packet.timestamp, Instant::now()));
                        }
                        let mut response_generation = None;
                        let current_selection =
                            request.selection_generation == Some(subscription_selection);
                        match request.purpose {
                            RequestPurpose::Subscribe { timestamp }
                                if current_selection && result_code(&packet.message) == Some(0) =>
                            {
                                let generation = subscription_generation;
                                subscription_generation =
                                    subscription_generation.wrapping_add(1).max(1);
                                response_generation = Some(generation);
                                dedupe.clear();
                                dedupe_order.clear();
                                subscription = Some(ActiveSubscription {
                                    public: LanSubscription {
                                        generation,
                                        timestamp,
                                    },
                                    authority: request.guard.child(),
                                });
                                clock_anchor = Some((packet.timestamp, Instant::now()));
                            }
                            RequestPurpose::Unsubscribe
                                if current_selection && result_code(&packet.message) == Some(0) =>
                            {
                                subscription = None;
                                clock_anchor = Some((packet.timestamp, Instant::now()));
                            }
                            _ => {}
                        }
                        let _ = request.reply.send(Ok(Response {
                            value: packet.message,
                            timestamp: packet.timestamp,
                            subscription_generation: response_generation,
                            may_have_been_sent: request.guard.may_have_been_sent(),
                        }));
                        continue;
                    }
                    let Some(active) = subscription.clone() else {
                        continue;
                    };
                    match active.authority.check() {
                        Ok(true) => {}
                        Ok(false) | Err(_) => {
                            subscription = None;
                            continue;
                        }
                    }
                    let notifications = parse_notifications(
                        self.target.did(),
                        self.target.epoch(),
                        active.public.generation,
                        packet.timestamp,
                        &packet.message,
                    );
                    let Ok(notifications) = notifications else {
                        continue;
                    };
                    if clock_anchor.is_some_and(|anchor| {
                        !timestamp_is_current(packet.timestamp, clock_timestamp(Some(anchor)))
                    }) {
                        authenticated = false;
                        subscription = None;
                        self.needs_hello.store(true, Ordering::Release);
                        continue;
                    }
                    let duplicate = !dedupe.insert(id);
                    if !duplicate {
                        dedupe_order.push_back(id);
                        if dedupe_order.len() > DEDUPE_CAPACITY
                            && let Some(expired) = dedupe_order.pop_front()
                        {
                            dedupe.remove(&expired);
                        }
                    }
                    if !duplicate {
                        let available = self
                            .notifications
                            .capacity()
                            .map_or(usize::MAX, |capacity| {
                                capacity.saturating_sub(self.notifications.len())
                            });
                        if notifications.len() > available {
                            return Err(LanError::new(
                                "deliver LAN notification",
                                LanErrorKind::Transport,
                            ));
                        }
                    }
                    let acknowledgement = json!({"id":id,"result":{"code":0}}).to_string();
                    let encoded = encode_packet(
                        self.target.did(),
                        clock_timestamp(clock_anchor),
                        self.target.token(),
                        acknowledgement.as_bytes(),
                    )?;
                    let acknowledgement_guard = active.authority.child();
                    send_datagram(
                        &self.socket,
                        &encoded,
                        self.target.address(),
                        SendContext {
                            guard: &acknowledgement_guard,
                            running: &self.running,
                            stopped: &self.stopped,
                            deadline: Instant::now() + LOCAL_LIMIT,
                            force_pending: None,
                        },
                    )
                    .await?;
                    if !duplicate {
                        for notification in notifications {
                            self.notifications.try_send(notification).map_err(|_| {
                                LanError::new("deliver LAN notification", LanErrorKind::Transport)
                            })?;
                        }
                    }
                }
                Event::Datagram(Err(_)) => {
                    return Err(LanError::new("receive LAN packet", LanErrorKind::Transport));
                }
                Event::Deadline => {}
            }
        }
    }
}

impl LanHandle {
    pub(crate) fn did(&self) -> u64 {
        self.did
    }

    pub(crate) fn model(&self) -> &str {
        &self.model
    }

    pub async fn authenticate(
        &self,
        property: LanProperty,
        deadline: Instant,
        guard: LanSendGuard,
    ) -> Result<LanEvidence, LanError> {
        self.authenticate_with_limit(property, deadline, guard, LOCAL_LIMIT)
            .await
    }

    async fn authenticate_with_limit(
        &self,
        property: LanProperty,
        deadline: Instant,
        guard: LanSendGuard,
        local_limit: Duration,
    ) -> Result<LanEvidence, LanError> {
        validate_property(&property)?;
        let deadline = deadline.min(Instant::now() + local_limit);
        let guard = guard.child();
        if self.needs_hello.load(Ordering::Acquire) {
            self.hello(deadline, guard.child()).await?;
        }
        let value = self
            .request(
                "get_properties",
                json!([{"did":self.did.to_string(),"siid":property.siid,"piid":property.piid}]),
                RequestPurpose::Authenticate { property },
                deadline,
                guard,
            )
            .await?;
        Ok(LanEvidence {
            did: self.did,
            address: self.address,
            interface_index: self.interface_index,
            epoch: self.epoch,
            signed_timestamp: value.timestamp,
            native_supported: parse_reads(self.did, std::slice::from_ref(&property), &value.value)
                .is_ok_and(|reads| !matches!(reads[0].outcome, LanReadOutcome::Error(_))),
        })
    }

    #[cfg(test)]
    pub(super) async fn authenticate_with_limit_for_test(
        &self,
        property: LanProperty,
        deadline: Instant,
        guard: LanSendGuard,
        local_limit: Duration,
    ) -> Result<LanEvidence, LanError> {
        self.authenticate_with_limit(property, deadline, guard, local_limit)
            .await
    }

    pub async fn read_properties(
        &self,
        properties: &[LanProperty],
        deadline: Instant,
        guard: LanSendGuard,
    ) -> Result<Vec<LanPropertyRead>, LanError> {
        if properties.is_empty()
            || properties
                .iter()
                .any(|property| validate_property(property).is_err())
        {
            return Err(LanError::new(
                "read LAN properties",
                LanErrorKind::InvalidInput,
            ));
        }
        reject_duplicate_properties(properties.iter())?;
        let params = properties.iter().map(|property| {
            json!({"did":self.did.to_string(),"siid":property.siid,"piid":property.piid})
        }).collect();
        let response = self
            .request(
                "get_properties",
                Value::Array(params),
                RequestPurpose::General,
                deadline,
                guard,
            )
            .await?;
        parse_reads(self.did, properties, &response.value)
            .map_err(|error| error.after_send(response.may_have_been_sent))
    }

    pub async fn set_properties(
        &self,
        writes: &[LanPropertyWrite],
        deadline: Instant,
        guard: LanSendGuard,
    ) -> Result<Vec<LanWriteOutcome>, LanError> {
        if writes.is_empty()
            || writes
                .iter()
                .any(|write| validate_property(&write.property).is_err())
        {
            return Err(LanError::new(
                "set LAN properties",
                LanErrorKind::InvalidInput,
            ));
        }
        reject_duplicate_properties(writes.iter().map(|write| &write.property))?;
        let params = writes
            .iter()
            .map(|write| {
                Ok(json!({
                    "did":self.did.to_string(),
                    "siid":write.property.siid,
                    "piid":write.property.piid,
                    "value":encode_wire(&write.value)?,
                }))
            })
            .collect::<Result<Vec<_>, LanError>>()?;
        let response = self
            .request(
                "set_properties",
                Value::Array(params),
                RequestPurpose::General,
                deadline,
                guard,
            )
            .await?;
        parse_writes(self.did, writes, &response.value)
            .map_err(|error| error.after_send(response.may_have_been_sent))
    }

    pub async fn invoke_action(
        &self,
        siid: u32,
        aiid: u32,
        input: &[WireValue],
        deadline: Instant,
        guard: LanSendGuard,
    ) -> Result<(), LanError> {
        if siid == 0 || aiid == 0 {
            return Err(LanError::new(
                "invoke LAN action",
                LanErrorKind::InvalidInput,
            ));
        }
        let input = input
            .iter()
            .map(encode_wire)
            .collect::<Result<Vec<_>, _>>()?;
        let response = self
            .request(
                "action",
                json!({"did":self.did.to_string(),"siid":siid,"aiid":aiid,"in":input}),
                RequestPurpose::General,
                deadline,
                guard,
            )
            .await?;
        accept_action(&response.value, self.did, siid, aiid)
            .map_err(|error| error.after_send(response.may_have_been_sent))
    }

    pub async fn execute_mcn02(
        &self,
        command: &DeviceCommand,
        deadline: Instant,
        guard: LanSendGuard,
    ) -> Result<(), LanError> {
        if self.model != "lumi.acpartner.mcn02" {
            return Err(LanError::new(
                "control legacy LAN device",
                LanErrorKind::Unsupported,
            ));
        }
        let operation = Mcn02LegacyMapping::new()
            .encode(command)
            .map_err(|_| LanError::new("control legacy LAN device", LanErrorKind::InvalidInput))?;
        let params = operation
            .arguments
            .iter()
            .map(encode_wire)
            .collect::<Result<Vec<_>, _>>()?;
        let response = self
            .request(
                operation.method,
                Value::Array(params),
                RequestPurpose::General,
                deadline,
                guard,
            )
            .await?;
        accept_legacy(&response.value, "control legacy LAN device")
            .map_err(|error| error.after_send(response.may_have_been_sent))
    }

    pub async fn read_mcn02(
        &self,
        deadline: Instant,
        guard: LanSendGuard,
    ) -> Result<Vec<(Property, Option<PropertyValue>)>, LanError> {
        if self.model != "lumi.acpartner.mcn02" {
            return Err(LanError::new(
                "read legacy LAN device",
                LanErrorKind::Unsupported,
            ));
        }
        let mapping = Mcn02LegacyMapping::new();
        let response = self
            .request(
                "get_prop",
                json!(mapping.read_fields()),
                RequestPurpose::General,
                deadline,
                guard,
            )
            .await?;
        let values = response
            .value
            .get("result")
            .and_then(Value::as_array)
            .ok_or_else(|| LanError::new("read legacy LAN device", LanErrorKind::Protocol))
            .map_err(|error| error.after_send(response.may_have_been_sent))?;
        if values.len() != mapping.read_fields().len() {
            return Err(
                LanError::new("read legacy LAN device", LanErrorKind::Protocol)
                    .after_send(response.may_have_been_sent),
            );
        }
        mapping
            .read_fields()
            .iter()
            .zip(values)
            .map(|(field, value)| {
                let wire = if value.is_null() {
                    WireValue::String(String::new())
                } else {
                    decode_wire(value).ok_or_else(|| {
                        LanError::new("read legacy LAN device", LanErrorKind::Protocol)
                    })?
                };
                mapping
                    .decode(field, &wire)
                    .ok_or_else(|| LanError::new("read legacy LAN device", LanErrorKind::Protocol))
            })
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.after_send(response.may_have_been_sent))
    }

    pub async fn subscribe(
        &self,
        deadline: Instant,
        guard: LanSendGuard,
    ) -> Result<LanSubscription, LanError> {
        let timestamp = self.next_subscription_timestamp()?;
        let response = self.request("miIO.sub", json!({"version":"2.0","did":self.virtual_did.to_string(),"update_ts":timestamp,"sub_method":"."}), RequestPurpose::Subscribe { timestamp }, deadline, guard).await?;
        accept_result(&response.value, "subscribe LAN notifications")
            .map_err(|error| error.after_send(response.may_have_been_sent))?;
        let generation = response
            .subscription_generation
            .ok_or_else(|| LanError::new("subscribe LAN notifications", LanErrorKind::Protocol))?;
        Ok(LanSubscription {
            generation,
            timestamp,
        })
    }

    pub async fn unsubscribe(
        &self,
        subscription: &LanSubscription,
        deadline: Instant,
        guard: LanSendGuard,
    ) -> Result<(), LanError> {
        let response = self.request("miIO.unsub", json!({"version":"2.0","did":self.virtual_did.to_string(),"update_ts":subscription.timestamp,"sub_method":"."}), RequestPurpose::Unsubscribe, deadline, guard).await?;
        accept_result(&response.value, "unsubscribe LAN notifications")
            .map_err(|error| error.after_send(response.may_have_been_sent))
    }

    pub fn stop(&self) {
        self.running.store(false, Ordering::Release);
        self.stopped.notify(usize::MAX);
        let _ = self.commands.try_send(Command::Stop);
    }

    async fn request(
        &self,
        method: &'static str,
        params: Value,
        purpose: RequestPurpose,
        deadline: Instant,
        guard: LanSendGuard,
    ) -> Result<Response, LanError> {
        let guard = guard.child();
        let deadline = deadline.min(Instant::now() + LOCAL_LIMIT);
        if deadline <= Instant::now() {
            return Err(LanError::new("queue LAN request", LanErrorKind::Timeout));
        }
        let (reply, response) = flume::bounded(1);
        let mut cancellation = Cancellation {
            guard: guard.clone(),
            armed: true,
        };
        let id = self.next_request_id();
        bounded_stage(
            self.commands.send_async(Command::Request {
                id,
                method,
                params,
                purpose,
                deadline,
                guard: guard.clone(),
                reply,
            }),
            deadline,
            &guard,
            &self.running,
            &self.stopped,
            "queue LAN request",
        )
        .await?
        .map_err(|_| LanError::new("queue LAN request", LanErrorKind::Transport))?;
        let result = bounded_stage(
            response.recv_async(),
            deadline,
            &guard,
            &self.running,
            &self.stopped,
            "wait for LAN reply",
        )
        .await?
        .map_err(|_| LanError {
            operation: "wait for LAN reply",
            kind: LanErrorKind::Transport,
            may_have_been_sent: guard.may_have_been_sent(),
        })?;
        cancellation.armed = false;
        result
    }

    async fn hello(&self, deadline: Instant, guard: LanSendGuard) -> Result<u32, LanError> {
        let deadline = deadline.min(Instant::now() + LOCAL_LIMIT);
        if deadline <= Instant::now() {
            return Err(LanError::new("queue LAN hello", LanErrorKind::Timeout));
        }
        let mut cancellation = Cancellation {
            guard: guard.clone(),
            armed: true,
        };
        let (reply, response) = flume::bounded(1);
        bounded_stage(
            self.commands.send_async(Command::Hello {
                deadline,
                guard: guard.clone(),
                reply,
            }),
            deadline,
            &guard,
            &self.running,
            &self.stopped,
            "queue LAN hello",
        )
        .await?
        .map_err(|_| LanError::new("queue LAN hello", LanErrorKind::Transport))?;
        let result = bounded_stage(
            response.recv_async(),
            deadline,
            &guard,
            &self.running,
            &self.stopped,
            "wait for LAN hello",
        )
        .await?
        .map_err(|_| LanError::new("wait for LAN hello", LanErrorKind::Transport))?;
        cancellation.armed = false;
        result
    }

    fn next_request_id(&self) -> u32 {
        loop {
            let id = self.next_id.fetch_add(1, Ordering::Relaxed);
            if id != 0 {
                return id;
            }
        }
    }

    fn next_subscription_timestamp(&self) -> Result<u32, LanError> {
        let now = unix_seconds()?;
        let mut previous = self.last_subscription_timestamp.load(Ordering::Acquire);
        loop {
            let next = now.max(previous.saturating_add(1));
            match self.last_subscription_timestamp.compare_exchange_weak(
                previous,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Ok(next),
                Err(current) => previous = current,
            }
        }
    }
}

async fn bounded_stage<T>(
    stage: impl Future<Output = T>,
    deadline: Instant,
    guard: &LanSendGuard,
    running: &AtomicBool,
    stopped: &WakeEvent,
    operation: &'static str,
) -> Result<T, LanError> {
    futures_lite::pin!(stage);
    enum Completed<T> {
        Value(T),
        Timeout,
        Cancelled,
        Stopped,
    }
    loop {
        let delivered = guard.may_have_been_sent();
        let cancelled = async {
            if delivered {
                future::pending::<()>().await;
            } else {
                guard.cancelled.listen().await;
            }
        };
        let stop = stopped.listen();
        let allowed = if delivered { true } else { guard.check()? };
        if !running.load(Ordering::Acquire) || !allowed {
            return Err(LanError {
                operation,
                kind: LanErrorKind::Cancelled,
                may_have_been_sent: guard.may_have_been_sent(),
            });
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        match future::or(
            async { Completed::Value(stage.as_mut().await) },
            future::or(
                async {
                    Timer::after(remaining).await;
                    Completed::Timeout
                },
                future::or(
                    async {
                        cancelled.await;
                        Completed::Cancelled
                    },
                    async {
                        stop.await;
                        Completed::Stopped
                    },
                ),
            ),
        )
        .await
        {
            Completed::Value(value) => return Ok(value),
            Completed::Timeout => {
                return Err(LanError {
                    operation,
                    kind: LanErrorKind::Timeout,
                    may_have_been_sent: guard.may_have_been_sent(),
                });
            }
            Completed::Cancelled => continue,
            Completed::Stopped => {
                return Err(LanError {
                    operation,
                    kind: LanErrorKind::Cancelled,
                    may_have_been_sent: guard.may_have_been_sent(),
                });
            }
        }
    }
}

impl Drop for LanSession {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Release);
        self.stopped.notify(usize::MAX);
    }
}

struct SendContext<'a> {
    guard: &'a LanSendGuard,
    running: &'a AtomicBool,
    stopped: &'a WakeEvent,
    deadline: Instant,
    force_pending: Option<&'a AtomicBool>,
}

async fn send_datagram(
    socket: &Async<UdpSocket>,
    packet: &[u8],
    destination: SocketAddrV4,
    context: SendContext<'_>,
) -> Result<(), LanError> {
    loop {
        let cancelled = context.guard.cancelled.listen();
        let stop = context.stopped.listen();
        if !context.running.load(Ordering::Acquire) || !context.guard.check()? {
            return Err(LanError::new("send LAN packet", LanErrorKind::Cancelled));
        }
        if Instant::now() >= context.deadline {
            return Err(LanError::new("send LAN packet", LanErrorKind::Timeout));
        }
        let send = if context
            .force_pending
            .is_some_and(|pending| pending.load(Ordering::Acquire))
        {
            Err(std::io::Error::from(std::io::ErrorKind::WouldBlock))
        } else {
            socket.get_ref().send_to(packet, destination)
        };
        match send {
            Ok(written) if written == packet.len() => return Ok(()),
            Ok(_) => return Err(LanError::new("send LAN packet", LanErrorKind::Transport)),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                enum Wake {
                    Writable(std::io::Result<()>),
                    Deadline,
                    Cancelled,
                    Stopped,
                }
                let remaining = context.deadline.saturating_duration_since(Instant::now());
                let forced = context
                    .force_pending
                    .is_some_and(|pending| pending.load(Ordering::Acquire));
                match future::or(
                    async {
                        if forced {
                            future::pending::<Wake>().await
                        } else {
                            Wake::Writable(socket.writable().await)
                        }
                    },
                    future::or(
                        async {
                            Timer::after(remaining).await;
                            Wake::Deadline
                        },
                        future::or(
                            async {
                                cancelled.await;
                                Wake::Cancelled
                            },
                            async {
                                stop.await;
                                Wake::Stopped
                            },
                        ),
                    ),
                )
                .await
                {
                    Wake::Writable(Ok(())) => {}
                    Wake::Writable(Err(_)) => {
                        return Err(LanError::new("send LAN packet", LanErrorKind::Transport));
                    }
                    Wake::Deadline => {
                        return Err(LanError::new("send LAN packet", LanErrorKind::Timeout));
                    }
                    Wake::Cancelled => continue,
                    Wake::Stopped => {
                        return Err(LanError::new("send LAN packet", LanErrorKind::Cancelled));
                    }
                }
            }
            Err(_) => return Err(LanError::new("send LAN packet", LanErrorKind::Transport)),
        }
    }
}

#[cfg(test)]
pub(super) async fn force_pending_send_for_test(
    socket: &Async<UdpSocket>,
    destination: SocketAddrV4,
    guard: &LanSendGuard,
    deadline: Instant,
) -> Result<(), LanError> {
    let running = AtomicBool::new(true);
    let stopped = WakeEvent::new();
    let pending = AtomicBool::new(true);
    send_datagram(
        socket,
        b"test",
        destination,
        SendContext {
            guard,
            running: &running,
            stopped: &stopped,
            deadline,
            force_pending: Some(&pending),
        },
    )
    .await
}

#[cfg(test)]
pub(super) async fn force_pending_stop_for_test(
    socket: &Async<UdpSocket>,
    destination: SocketAddrV4,
    guard: &LanSendGuard,
) -> Result<(), LanError> {
    let running = AtomicBool::new(true);
    let stopped = WakeEvent::new();
    let pending = AtomicBool::new(true);
    let send = send_datagram(
        socket,
        b"test",
        destination,
        SendContext {
            guard,
            running: &running,
            stopped: &stopped,
            deadline: Instant::now() + Duration::from_secs(1),
            force_pending: Some(&pending),
        },
    );
    future::zip(send, async {
        Timer::after(Duration::from_millis(5)).await;
        running.store(false, Ordering::Release);
        stopped.notify(usize::MAX);
    })
    .await
    .0
}

fn clock_timestamp(anchor: Option<(u32, Instant)>) -> u32 {
    anchor.map_or(0, |(timestamp, instant)| {
        timestamp.wrapping_add(u32::try_from(instant.elapsed().as_secs()).unwrap_or(u32::MAX))
    })
}

fn timestamp_is_current(received: u32, expected: u32) -> bool {
    let delta = received.wrapping_sub(expected) as i32;
    (-CLOCK_SKEW_LIMIT..=CLOCK_SKEW_LIMIT).contains(&delta)
}

fn unix_seconds() -> Result<u32, LanError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| u32::try_from(duration.as_secs()).ok())
        .ok_or_else(|| LanError::new("read LAN clock", LanErrorKind::Transport))
}
