use super::{CentralError, tls, wire};
use crate::{storage::XiaomiRecord, xiaomi::capability::PropertyBinding};
use bytes::BytesMut;
use event_listener::Event;
use futures_lite::{
    future,
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
};
use mqttbytes::{QoS, v5::*};
use serde_json::{Value, json};
use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, VecDeque},
    future::Future,
    net::SocketAddr,
    pin::Pin,
    rc::Rc,
    sync::{
        OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    task::Poll,
    time::{Duration, Instant},
};

const LIMIT: usize = 64;
const MAX_PACKET: usize = 1024 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeviceAccess {
    pub did: String,
    pub model: String,
    pub instance_type: String,
    pub online: bool,
    pub spec_v2_access: bool,
    pub push_available: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CentralEvent {
    Property {
        did: String,
        binding: PropertyBinding,
        value: bool,
    },
    DeviceListChanged {
        dids: Vec<String>,
    },
}

#[derive(Clone, Copy)]
pub(super) struct Timing {
    pub(super) request: Duration,
    pub(super) keepalive: Option<Duration>,
}
impl Default for Timing {
    fn default() -> Self {
        Self {
            request: Duration::from_secs(5),
            keepalive: Some(Duration::from_secs(60)),
        }
    }
}

#[derive(Clone)]
enum Response {
    Device(Option<DeviceAccess>),
    Power(bool),
    Unit,
}
type ResultValue = Result<Response, CentralError>;
type Callback = Box<dyn FnOnce(ResultValue)>;
struct Completion {
    result: RefCell<Option<ResultValue>>,
    event: Event,
}
struct Request {
    kind: RequestKind,
    deadline: Instant,
    completion: Rc<Completion>,
    callback: Option<Callback>,
    id: Option<u32>,
}
#[derive(Clone)]
enum RequestKind {
    List {
        did: String,
    },
    Subscribe {
        did: String,
        binding: PropertyBinding,
    },
    Read {
        did: String,
        binding: PropertyBinding,
    },
    Write {
        did: String,
        binding: PropertyBinding,
        value: bool,
    },
}
impl RequestKind {
    fn operation(&self) -> &'static str {
        match self {
            Self::List { .. } => "getDevList",
            Self::Subscribe { .. } => "subscribe",
            Self::Read { .. } => "get",
            Self::Write { .. } => "set",
        }
    }
    fn did(&self) -> &str {
        match self {
            Self::List { did }
            | Self::Subscribe { did, .. }
            | Self::Read { did, .. }
            | Self::Write { did, .. } => did,
        }
    }
}
impl Request {
    fn finish(mut self, result: ResultValue) {
        if let Some(callback) = self.callback.take() {
            callback(result.clone());
        }
        *self.completion.result.borrow_mut() = Some(result);
        self.completion.event.notify(usize::MAX);
    }
}
struct Shared {
    queue: RefCell<VecDeque<Request>>,
    event: Event,
    closed: Cell<bool>,
    outstanding: Cell<usize>,
    generation: u64,
}

/// Requests are enqueued when these methods are called. Dropping the returned
/// future does not cancel an already sent write or suppress its result callback.
#[derive(Clone)]
pub struct SessionHandle {
    shared: Rc<Shared>,
    timing: Timing,
}
impl SessionHandle {
    pub fn generation(&self) -> u64 {
        self.shared.generation
    }
    pub fn close(&self) {
        self.shared.closed.set(true);
        self.shared.event.notify(usize::MAX);
    }
    pub fn get_device_list<F: FnOnce(Result<Option<DeviceAccess>, CentralError>) + 'static>(
        &self,
        did: &str,
        on_result: F,
    ) -> impl Future<Output = Result<Option<DeviceAccess>, CentralError>> + use<F> {
        let callback: Callback = Box::new(move |result| {
            on_result(result.and_then(|response| match response {
                Response::Device(device) => Ok(device),
                _ => Err(CentralError::Protocol),
            }))
        });
        let result = self.submit(
            RequestKind::List { did: did.into() },
            self.timing.request,
            Some(callback),
        );
        async move {
            match result.await? {
                Response::Device(device) => Ok(device),
                _ => Err(CentralError::Protocol),
            }
        }
    }
    pub fn subscribe_property(
        &self,
        did: &str,
        binding: PropertyBinding,
    ) -> impl Future<Output = Result<(), CentralError>> + use<> {
        let result = self.submit(
            RequestKind::Subscribe {
                did: did.into(),
                binding,
            },
            self.timing.request,
            None,
        );
        async move { result.await.map(|_| ()) }
    }
    pub fn read_property<F: FnOnce(Result<bool, CentralError>) + 'static>(
        &self,
        did: &str,
        binding: PropertyBinding,
        on_result: F,
    ) -> impl Future<Output = Result<bool, CentralError>> + use<F> {
        let callback: Callback = Box::new(move |result| {
            on_result(result.and_then(|response| match response {
                Response::Power(value) => Ok(value),
                _ => Err(CentralError::Protocol),
            }))
        });
        let result = self.submit(
            RequestKind::Read {
                did: did.into(),
                binding,
            },
            self.timing.request,
            Some(callback),
        );
        async move {
            match result.await? {
                Response::Power(value) => Ok(value),
                _ => Err(CentralError::Protocol),
            }
        }
    }
    pub fn set_property<F: FnOnce(Result<(), CentralError>) + 'static>(
        &self,
        did: &str,
        binding: PropertyBinding,
        value: bool,
        remaining: Duration,
        on_result: F,
    ) -> impl Future<Output = Result<(), CentralError>> + use<F> {
        let callback: Callback = Box::new(move |result| on_result(result.map(|_| ())));
        let result = self.submit(
            RequestKind::Write {
                did: did.into(),
                binding,
                value,
            },
            remaining,
            Some(callback),
        );
        async move { result.await.map(|_| ()) }
    }
    fn submit(
        &self,
        kind: RequestKind,
        remaining: Duration,
        callback: Option<Callback>,
    ) -> impl Future<Output = ResultValue> + use<> {
        let completion = Rc::new(Completion {
            result: RefCell::new(None),
            event: Event::new(),
        });
        let now = Instant::now();
        let deadline = now.checked_add(remaining);
        let request = Request {
            kind,
            deadline: deadline.unwrap_or(now),
            completion: completion.clone(),
            callback,
            id: None,
        };
        let error = if deadline.is_none() || !valid_request(&request.kind) {
            Some(CentralError::InvalidInput)
        } else if remaining.is_zero() {
            Some(CentralError::Timeout)
        } else if self.shared.closed.get() {
            Some(CentralError::Cancelled)
        } else if self.shared.outstanding.get() >= LIMIT {
            Some(CentralError::QueueFull)
        } else {
            None
        };
        if let Some(error) = error {
            request.finish(Err(error));
        } else {
            self.shared
                .outstanding
                .set(self.shared.outstanding.get() + 1);
            self.shared.queue.borrow_mut().push_back(request);
            self.shared.event.notify(usize::MAX);
        }
        async move {
            loop {
                let listener = completion.event.listen();
                if let Some(result) = completion.result.borrow_mut().take() {
                    return result;
                }
                listener.await;
            }
        }
    }
}

fn valid_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}
fn valid_request(kind: &RequestKind) -> bool {
    let (did, binding) = match kind {
        RequestKind::List { did } => (did, None),
        RequestKind::Subscribe { did, binding }
        | RequestKind::Read { did, binding }
        | RequestKind::Write { did, binding, .. } => (did, Some(binding)),
    };
    valid_identifier(did) && binding.is_none_or(|binding| binding.siid > 0 && binding.piid > 0)
}
fn next_mips_id() -> Result<u32, CentralError> {
    static IDS: OnceLock<AtomicU64> = OnceLock::new();
    let next = IDS
        .get_or_init(|| AtomicU64::new(u64::from(rand::random::<u32>() >> 1)))
        .fetch_add(1, Ordering::Relaxed);
    u32::try_from(next).map_err(|_| CentralError::Protocol)
}
fn property_topic(prefix: &str, did: &str, binding: PropertyBinding) -> String {
    format!(
        "{prefix}/appMsg/notify/iot/{did}/property/{}.{}",
        binding.siid, binding.piid
    )
}

pub(super) trait Transport: AsyncRead + AsyncWrite + Unpin {}
impl<T: AsyncRead + AsyncWrite + Unpin> Transport for T {}
struct Output {
    bytes: Vec<u8>,
    offset: usize,
    deadline: Instant,
    request: Option<u32>,
    pkid: Option<u16>,
}
struct Publication {
    mid: u32,
    released: bool,
    deadline: Instant,
}
struct Subscription {
    request: Option<Request>,
    deadline: Instant,
    count: usize,
}

/// One driver owns all TLS I/O and protocol state. Poll it continuously alongside
/// handle requests; callbacks and events execute in wire order on this driver.
pub struct SessionDriver {
    stream: Box<dyn Transport>,
    frames: wire::Frames,
    shared: Rc<Shared>,
    timing: Timing,
    client_id: String,
    output: VecDeque<Output>,
    pending: HashMap<u32, Request>,
    publications: HashMap<u16, Publication>,
    subscriptions: HashMap<u16, Subscription>,
    incoming: HashMap<u16, Publish>,
    property_topics: HashMap<String, (String, PropertyBinding)>,
    ready: bool,
    pkid: u16,
    last_sent: Instant,
    ping_deadline: Option<Instant>,
    max_packet: usize,
    receive_max: usize,
}

pub async fn connect(
    address: SocketAddr,
    credentials: &XiaomiRecord,
) -> Result<(SessionHandle, SessionDriver), CentralError> {
    let stream = tls::connect_tls(address, credentials).await?;
    start(
        Box::new(stream),
        &credentials.virtual_did,
        Timing::default(),
    )
    .await
}
pub(super) async fn start(
    mut stream: Box<dyn Transport>,
    client_id: &str,
    mut timing: Timing,
) -> Result<(SessionHandle, SessionDriver), CentralError> {
    if !valid_identifier(client_id) {
        return Err(CentralError::InvalidInput);
    }
    let mut connect = Connect::new(client_id);
    connect.keep_alive = timing.keepalive.map_or(0, |duration| {
        duration.as_secs().clamp(1, u64::from(u16::MAX)) as u16
    });
    connect.properties = Some(ConnectProperties {
        session_expiry_interval: Some(0),
        receive_maximum: Some(LIMIT as u16),
        max_packet_size: Some(MAX_PACKET as u32),
        topic_alias_max: Some(0),
        request_response_info: None,
        request_problem_info: Some(0),
        user_properties: Vec::new(),
        authentication_method: None,
        authentication_data: None,
    });
    let mut frames = wire::Frames::default();
    let handshake = async {
        stream
            .write_all(&encode_packet(&Packet::Connect(connect))?)
            .await
            .map_err(|_| CentralError::Io)?;
        stream.flush().await.map_err(|_| CentralError::Io)?;
        loop {
            if let Some(packet) = frames.next_packet()? {
                return match packet {
                    Packet::ConnAck(ack) => Ok(ack),
                    _ => Err(CentralError::Protocol),
                };
            }
            let mut bytes = [0; 4096];
            let len = stream
                .read(&mut bytes)
                .await
                .map_err(|_| CentralError::Io)?;
            if len == 0 {
                return Err(CentralError::Disconnected);
            }
            frames.push(&bytes[..len])?;
        }
    };
    let ack = future::race(handshake, async {
        async_io::Timer::after(timing.request).await;
        Err(CentralError::Timeout)
    })
    .await?;
    if ack.code != ConnectReturnCode::Success {
        return Err(CentralError::Rejected(ack.code as i64));
    }
    if ack.session_present {
        return Err(CentralError::Protocol);
    }
    let mut max_packet = MAX_PACKET;
    let mut receive_max = LIMIT;
    if let Some(properties) = ack.properties {
        if properties.max_qos.is_some_and(|qos| qos < 2)
            || properties.receive_max == Some(0)
            || properties.max_packet_size == Some(0)
            || properties.authentication_method.is_some()
            || properties.authentication_data.is_some()
        {
            return Err(CentralError::Protocol);
        }
        max_packet = properties
            .max_packet_size
            .map_or(MAX_PACKET, |size| (size as usize).min(MAX_PACKET));
        receive_max = usize::from(properties.receive_max.unwrap_or(LIMIT as u16)).min(LIMIT);
        if let Some(seconds) = properties.server_keep_alive {
            timing.keepalive = (seconds > 0).then(|| Duration::from_secs(u64::from(seconds)));
        }
    }
    static GENERATION: AtomicU64 = AtomicU64::new(1);
    let shared = Rc::new(Shared {
        queue: RefCell::new(VecDeque::new()),
        event: Event::new(),
        closed: Cell::new(false),
        outstanding: Cell::new(0),
        generation: GENERATION.fetch_add(1, Ordering::Relaxed),
    });
    let now = Instant::now();
    let mut driver = SessionDriver {
        stream,
        frames,
        shared: shared.clone(),
        timing,
        client_id: client_id.into(),
        output: VecDeque::new(),
        pending: HashMap::new(),
        publications: HashMap::new(),
        subscriptions: HashMap::new(),
        incoming: HashMap::new(),
        property_topics: HashMap::new(),
        ready: false,
        pkid: 0,
        last_sent: now,
        ping_deadline: None,
        max_packet,
        receive_max,
    };
    let mut subscribe = Subscribe::new_many([
        SubscribeFilter::new(format!("{client_id}/#"), QoS::ExactlyOnce),
        SubscribeFilter::new("master/appMsg/devListChange".into(), QoS::ExactlyOnce),
    ]);
    subscribe.pkid = driver.next_pkid()?;
    let deadline = now + timing.request;
    driver.subscriptions.insert(
        subscribe.pkid,
        Subscription {
            request: None,
            deadline,
            count: 2,
        },
    );
    driver.enqueue(Packet::Subscribe(subscribe), deadline, None, None)?;
    Ok((SessionHandle { shared, timing }, driver))
}

fn encode_packet(packet: &Packet) -> Result<Vec<u8>, CentralError> {
    let mut bytes = BytesMut::new();
    let result = match packet {
        Packet::Connect(packet) => packet.write(&mut bytes),
        Packet::ConnAck(packet) => {
            let mut packet = packet.clone();
            if packet.properties.is_none() {
                packet.properties = Some(ConnAckProperties::new());
            }
            packet.write(&mut bytes)
        }
        Packet::Publish(packet) => packet.write(&mut bytes),
        Packet::PubAck(packet) => packet.write(&mut bytes),
        Packet::PubRec(packet) => packet.write(&mut bytes),
        Packet::PubRel(packet) => packet.write(&mut bytes),
        Packet::PubComp(packet) => packet.write(&mut bytes),
        Packet::Subscribe(packet) => packet.write(&mut bytes),
        Packet::SubAck(packet) => packet.write(&mut bytes),
        Packet::Disconnect(packet) => packet.write(&mut bytes),
        Packet::PingReq => {
            bytes.extend_from_slice(&[0xc0, 0]);
            Ok(2)
        }
        Packet::PingResp => {
            bytes.extend_from_slice(&[0xd0, 0]);
            Ok(2)
        }
        _ => return Err(CentralError::Protocol),
    };
    result.map_err(|_| CentralError::Protocol)?;
    Ok(bytes.to_vec())
}

impl SessionDriver {
    pub async fn run(mut self, mut on_event: impl FnMut(CentralEvent)) -> Result<(), CentralError> {
        let result = self.drive(&mut on_event).await;
        self.finish_all(
            result
                .as_ref()
                .err()
                .cloned()
                .unwrap_or(CentralError::Cancelled),
        );
        result
    }
    async fn drive(&mut self, on_event: &mut impl FnMut(CentralEvent)) -> Result<(), CentralError> {
        loop {
            if self.shared.closed.get() {
                return Ok(());
            }
            self.expire()?;
            // Process one complete frame at a time; callback completion never
            // depends on scheduling a separate request future.
            if let Some(packet) = self.frames.next_packet()? {
                self.packet(packet, on_event)?;
                continue;
            }
            if self.ready {
                self.drain_requests()?;
            }
            self.keepalive()?;
            let shared = self.shared.clone();
            let listener = shared.event.listen();
            if self.ready && self.can_drain() {
                continue;
            }
            let deadline = self.next_deadline();
            future::race(
                async { future::poll_fn(|cx| self.poll_io(cx)).await },
                async {
                    future::race(
                        async {
                            listener.await;
                        },
                        async {
                            async_io::Timer::at(deadline).await;
                        },
                    )
                    .await;
                    Ok(())
                },
            )
            .await?;
        }
    }
    fn next_pkid(&mut self) -> Result<u16, CentralError> {
        for _ in 0..u16::MAX {
            self.pkid = self.pkid.wrapping_add(1).max(1);
            if !self.publications.contains_key(&self.pkid)
                && !self.subscriptions.contains_key(&self.pkid)
            {
                return Ok(self.pkid);
            }
        }
        Err(CentralError::QueueFull)
    }
    fn enqueue(
        &mut self,
        packet: Packet,
        deadline: Instant,
        request: Option<u32>,
        pkid: Option<u16>,
    ) -> Result<(), CentralError> {
        let bytes = encode_packet(&packet)?;
        if bytes.len() > self.max_packet {
            return Err(CentralError::Protocol);
        }
        if self.output.len() >= LIMIT * 4 {
            return Err(CentralError::Protocol);
        }
        self.output.push_back(Output {
            bytes,
            offset: 0,
            deadline,
            request,
            pkid,
        });
        Ok(())
    }
    fn finish(&self, request: Request, result: ResultValue) {
        log::debug!(
            "Gateway request completed: operation={}, did={}, request_id={:?}, error={:?}",
            request.kind.operation(),
            request.kind.did(),
            request.id,
            result.as_ref().err()
        );
        self.shared
            .outstanding
            .set(self.shared.outstanding.get() - 1);
        request.finish(result);
    }
    fn finish_all(&mut self, error: CentralError) {
        self.shared.closed.set(true);
        let mut requests: Vec<_> = self.shared.queue.borrow_mut().drain(..).collect();
        requests.extend(self.pending.drain().map(|(_, request)| request));
        requests.extend(
            self.subscriptions
                .drain()
                .filter_map(|(_, subscription)| subscription.request),
        );
        for request in requests {
            self.finish(request, Err(error.clone()));
        }
        self.shared.event.notify(usize::MAX);
    }
    fn can_drain(&self) -> bool {
        let queue = self.shared.queue.borrow();
        !queue.is_empty()
            && (self.publications.len() < self.receive_max
                || queue
                    .iter()
                    .any(|request| matches!(request.kind, RequestKind::Subscribe { .. })))
    }
    fn drain_requests(&mut self) -> Result<(), CentralError> {
        loop {
            let request = {
                let mut queue = self.shared.queue.borrow_mut();
                if self.publications.len() < self.receive_max {
                    queue.pop_front()
                } else {
                    let index = queue
                        .iter()
                        .position(|request| matches!(request.kind, RequestKind::Subscribe { .. }));
                    index.and_then(|index| queue.remove(index))
                }
            };
            let Some(mut request) = request else { break };
            if Instant::now() >= request.deadline {
                self.finish(request, Err(CentralError::Timeout));
                continue;
            }
            let pkid = match self.next_pkid() {
                Ok(pkid) => pkid,
                Err(error) => {
                    self.finish(request, Err(error.clone()));
                    return Err(error);
                }
            };
            if let RequestKind::Subscribe { did, binding } = &request.kind {
                let mut subscribe =
                    Subscribe::new(property_topic("master", did, *binding), QoS::ExactlyOnce);
                subscribe.pkid = pkid;
                let deadline = request.deadline;
                self.subscriptions.insert(
                    pkid,
                    Subscription {
                        request: Some(request),
                        deadline,
                        count: 1,
                    },
                );
                self.enqueue(Packet::Subscribe(subscribe), deadline, None, Some(pkid))?;
                continue;
            }
            let mid = match next_mips_id() {
                Ok(mid) => mid,
                Err(error) => {
                    self.finish(request, Err(error.clone()));
                    return Err(error);
                }
            };
            let rpc_id = if matches!(request.kind, RequestKind::Write { .. }) {
                match next_mips_id() {
                    Ok(id) => id,
                    Err(error) => {
                        self.finish(request, Err(error.clone()));
                        return Err(error);
                    }
                }
            } else {
                mid
            };
            let (topic, payload) = match &request.kind {
                RequestKind::List { did } => (
                    "proxy/getDevList",
                    json!({"filter":{"did":[did]},"info":["model","urn","online","specV2Access","pushAvailable"]}),
                ),
                RequestKind::Read { did, binding } => (
                    "proxy/get",
                    json!({"did":did,"siid":binding.siid,"piid":binding.piid}),
                ),
                RequestKind::Write {
                    did,
                    binding,
                    value,
                } => (
                    "proxy/rpcReq",
                    json!({"did":did,"rpc":{"id":rpc_id,"method":"set_properties","params":[{"did":did,"siid":binding.siid,"piid":binding.piid,"value":value}]}}),
                ),
                RequestKind::Subscribe { .. } => unreachable!(),
            };
            let mut publish = Publish::new(
                format!("master/{topic}"),
                QoS::ExactlyOnce,
                wire::encode_mips(mid, Some(&format!("{}/reply", self.client_id)), &payload),
            );
            publish.pkid = pkid;
            let deadline = request.deadline;
            request.id = Some(mid);
            log::debug!(
                "Gateway request queued: operation={}, did={}, request_id={mid}",
                request.kind.operation(),
                request.kind.did()
            );
            self.pending.insert(mid, request);
            self.publications.insert(
                pkid,
                Publication {
                    mid,
                    released: false,
                    deadline: deadline.max(Instant::now() + self.timing.request),
                },
            );
            self.enqueue(Packet::Publish(publish), deadline, Some(mid), Some(pkid))?;
        }
        Ok(())
    }
    fn packet(
        &mut self,
        packet: Packet,
        on_event: &mut impl FnMut(CentralEvent),
    ) -> Result<(), CentralError> {
        match packet {
            Packet::SubAck(ack) => {
                if ack.pkid == 0 {
                    return Err(CentralError::Protocol);
                }
                let Some(subscription) = self.subscriptions.remove(&ack.pkid) else {
                    return Ok(());
                };
                let error = if ack.return_codes.len() != subscription.count {
                    Some(CentralError::Protocol)
                } else {
                    ack.return_codes
                        .iter()
                        .find(|code| **code as u8 >= 128)
                        .map(|code| CentralError::Rejected(*code as i64))
                };
                if let Some(request) = subscription.request {
                    if let Some(error) = error {
                        self.finish(request, Err(error));
                    } else if Instant::now() >= request.deadline {
                        self.finish(request, Err(CentralError::Timeout));
                    } else {
                        if let RequestKind::Subscribe { did, binding } = &request.kind {
                            self.property_topics.insert(
                                property_topic(&self.client_id, did, *binding),
                                (did.clone(), *binding),
                            );
                        }
                        self.finish(request, Ok(Response::Unit));
                    }
                } else if let Some(error) = error {
                    return Err(error);
                } else {
                    self.ready = true;
                }
            }
            Packet::PubRec(ack) => {
                let Some(publication) = self.publications.get_mut(&ack.pkid) else {
                    return Err(CentralError::Protocol);
                };
                if ack.reason as u8 >= 128 {
                    let mid = publication.mid;
                    self.publications.remove(&ack.pkid);
                    if let Some(request) = self.pending.remove(&mid) {
                        self.finish(request, Err(CentralError::Rejected(ack.reason as i64)));
                    }
                } else {
                    publication.released = true;
                    let deadline = publication.deadline;
                    self.enqueue(Packet::PubRel(PubRel::new(ack.pkid)), deadline, None, None)?;
                }
            }
            Packet::PubComp(ack) => {
                let Some(publication) = self.publications.remove(&ack.pkid) else {
                    return Err(CentralError::Protocol);
                };
                if !publication.released {
                    return Err(CentralError::Protocol);
                }
                if ack.reason != PubCompReason::Success
                    && let Some(request) = self.pending.remove(&publication.mid)
                {
                    self.finish(request, Err(CentralError::Rejected(ack.reason as i64)));
                }
            }
            Packet::Publish(publish) => {
                if publish.topic.is_empty()
                    || publish
                        .properties
                        .as_ref()
                        .is_some_and(|properties| properties.topic_alias.is_some())
                {
                    return Err(CentralError::Protocol);
                }
                match publish.qos {
                    QoS::AtMostOnce => self.message(publish, on_event)?,
                    QoS::AtLeastOnce => {
                        if publish.pkid == 0 {
                            return Err(CentralError::Protocol);
                        }
                        self.enqueue(
                            Packet::PubAck(PubAck::new(publish.pkid)),
                            Instant::now() + self.timing.request,
                            None,
                            None,
                        )?;
                        self.message(publish, on_event)?;
                    }
                    QoS::ExactlyOnce => {
                        if publish.pkid == 0 {
                            return Err(CentralError::Protocol);
                        }
                        if let Some(previous) = self.incoming.get(&publish.pkid) {
                            if !publish.dup
                                || previous.topic != publish.topic
                                || previous.payload != publish.payload
                            {
                                return Err(CentralError::Protocol);
                            }
                        } else {
                            if self.incoming.len() >= LIMIT {
                                return Err(CentralError::Protocol);
                            }
                            self.incoming.insert(publish.pkid, publish.clone());
                        }
                        self.enqueue(
                            Packet::PubRec(PubRec::new(publish.pkid)),
                            Instant::now() + self.timing.request,
                            None,
                            None,
                        )?;
                    }
                }
            }
            Packet::PubRel(ack) => {
                if ack.pkid == 0 || ack.reason != PubRelReason::Success {
                    return Err(CentralError::Protocol);
                }
                if let Some(publish) = self.incoming.remove(&ack.pkid) {
                    self.message(publish, on_event)?;
                }
                self.enqueue(
                    Packet::PubComp(PubComp::new(ack.pkid)),
                    Instant::now() + self.timing.request,
                    None,
                    None,
                )?;
            }
            Packet::PingResp => {
                self.ping_deadline = None;
            }
            Packet::Disconnect(packet) => {
                return Err(
                    if packet.reason_code == DisconnectReasonCode::NormalDisconnection {
                        CentralError::Disconnected
                    } else {
                        CentralError::Rejected(packet.reason_code as i64)
                    },
                );
            }
            _ => return Err(CentralError::Protocol),
        }
        Ok(())
    }
    fn message(
        &mut self,
        publish: Publish,
        on_event: &mut impl FnMut(CentralEvent),
    ) -> Result<(), CentralError> {
        let topic = publish.topic.as_str();
        let reply = topic == format!("{}/reply", self.client_id);
        let list = topic == format!("{}/appMsg/devListChange", self.client_id);
        let property = self.property_topics.get(topic).cloned();
        if !reply && !list && property.is_none() {
            return Ok(());
        }
        let message = wire::decode_mips(&publish.payload)?;
        if reply {
            if let Some(request) = self.pending.remove(&message.id) {
                let result = if Instant::now() >= request.deadline {
                    Err(CentralError::Timeout)
                } else {
                    parse_response(&request.kind, &message.payload)
                };
                log::debug!(
                    "Gateway reply: operation={}, did={}, request_id={}, error={:?}",
                    request.kind.operation(),
                    request.kind.did(),
                    message.id,
                    result.as_ref().err()
                );
                self.finish(request, result);
            }
        } else if list {
            let dids = message
                .payload
                .get("devList")
                .and_then(Value::as_array)
                .ok_or(CentralError::Protocol)?
                .iter()
                .map(|did| {
                    did.as_str()
                        .filter(|did| valid_identifier(did))
                        .map(str::to_owned)
                        .ok_or(CentralError::Protocol)
                })
                .collect::<Result<Vec<_>, _>>()?;
            on_event(CentralEvent::DeviceListChanged { dids });
        } else if let Some((did, binding)) = property {
            validate_identity(&message.payload, &did, binding, true)?;
            let value = message
                .payload
                .get("value")
                .and_then(Value::as_bool)
                .ok_or(CentralError::Protocol)?;
            on_event(CentralEvent::Property {
                did,
                binding,
                value,
            });
        }
        Ok(())
    }
    fn expire(&mut self) -> Result<(), CentralError> {
        let now = Instant::now();
        if self.ping_deadline.is_some_and(|deadline| now >= deadline) {
            return Err(CentralError::Timeout);
        }
        if self
            .publications
            .values()
            .any(|publication| now >= publication.deadline)
        {
            return Err(CentralError::Timeout);
        }
        if self.output.iter().any(|output| {
            now >= output.deadline
                && (output.offset > 0 || output.request.is_none() && output.pkid.is_none())
        }) {
            return Err(CentralError::Timeout);
        }
        let expired: Vec<_> = self
            .pending
            .iter()
            .filter_map(|(id, request)| (now >= request.deadline).then_some(*id))
            .collect();
        for id in expired {
            if let Some(request) = self.pending.remove(&id) {
                self.finish(request, Err(CentralError::Timeout));
            }
        }
        let expired: Vec<_> = self
            .subscriptions
            .iter()
            .filter_map(|(id, subscription)| (now >= subscription.deadline).then_some(*id))
            .collect();
        for id in expired {
            let subscription = self.subscriptions.remove(&id).unwrap();
            if let Some(request) = subscription.request {
                self.finish(request, Err(CentralError::Timeout));
            } else {
                return Err(CentralError::Timeout);
            }
        }
        let expired: Vec<_> = self
            .output
            .iter()
            .filter_map(|output| {
                (now >= output.deadline && output.offset == 0)
                    .then_some(output.pkid)
                    .flatten()
            })
            .collect();
        self.output
            .retain(|output| !(now >= output.deadline && output.offset == 0));
        for pkid in expired {
            self.publications.remove(&pkid);
        }
        let expired = {
            let mut queue = self.shared.queue.borrow_mut();
            let mut expired = Vec::new();
            let mut retained = VecDeque::new();
            while let Some(request) = queue.pop_front() {
                if now >= request.deadline {
                    expired.push(request)
                } else {
                    retained.push_back(request)
                }
            }
            *queue = retained;
            expired
        };
        for request in expired {
            self.finish(request, Err(CentralError::Timeout));
        }
        Ok(())
    }
    fn keepalive(&mut self) -> Result<(), CentralError> {
        if self.ping_deadline.is_none()
            && self
                .timing
                .keepalive
                .is_some_and(|interval| Instant::now() >= self.last_sent + interval)
        {
            let deadline = Instant::now() + self.timing.request;
            self.ping_deadline = Some(deadline);
            self.enqueue(Packet::PingReq, deadline, None, None)?;
        }
        Ok(())
    }
    fn next_deadline(&self) -> Instant {
        let mut deadline = Instant::now() + self.timing.request;
        if let Some(ping) = self.ping_deadline {
            deadline = deadline.min(ping)
        } else if let Some(interval) = self.timing.keepalive {
            deadline = deadline.min(self.last_sent + interval)
        }
        for request in self.pending.values() {
            deadline = deadline.min(request.deadline)
        }
        for request in self.shared.queue.borrow().iter() {
            deadline = deadline.min(request.deadline)
        }
        for subscription in self.subscriptions.values() {
            deadline = deadline.min(subscription.deadline)
        }
        for publication in self.publications.values() {
            deadline = deadline.min(publication.deadline)
        }
        for output in &self.output {
            deadline = deadline.min(output.deadline)
        }
        deadline
    }
    fn poll_io(&mut self, cx: &mut std::task::Context<'_>) -> Poll<Result<(), CentralError>> {
        // A timer and a writable socket can wake together. Recheck before
        // touching the stream so an unsent expired request stays unsent.
        if let Err(error) = self.expire() {
            return Poll::Ready(Err(error));
        }
        let mut progressed = false;
        if let Some(output) = self.output.front_mut() {
            if output.offset < output.bytes.len() {
                match Pin::new(&mut self.stream).poll_write(cx, &output.bytes[output.offset..]) {
                    Poll::Ready(Ok(0)) => return Poll::Ready(Err(CentralError::Io)),
                    Poll::Ready(Ok(count)) => {
                        output.offset += count;
                        progressed = true;
                    }
                    Poll::Ready(Err(_)) => return Poll::Ready(Err(CentralError::Io)),
                    Poll::Pending => {}
                }
            }
            if output.offset == output.bytes.len() {
                match Pin::new(&mut self.stream).poll_flush(cx) {
                    Poll::Ready(Ok(())) => {
                        self.output.pop_front();
                        self.last_sent = Instant::now();
                        progressed = true;
                    }
                    Poll::Ready(Err(_)) => return Poll::Ready(Err(CentralError::Io)),
                    Poll::Pending => {}
                }
            }
        }
        let mut bytes = [0; 4096];
        match Pin::new(&mut self.stream).poll_read(cx, &mut bytes) {
            Poll::Ready(Ok(0)) => return Poll::Ready(Err(CentralError::Disconnected)),
            Poll::Ready(Ok(len)) => {
                if let Err(error) = self.frames.push(&bytes[..len]) {
                    return Poll::Ready(Err(error));
                }
                progressed = true;
            }
            Poll::Ready(Err(_)) => return Poll::Ready(Err(CentralError::Io)),
            Poll::Pending => {}
        }
        if progressed {
            Poll::Ready(Ok(()))
        } else {
            Poll::Pending
        }
    }
}
impl Drop for SessionDriver {
    fn drop(&mut self) {
        self.finish_all(CentralError::Cancelled);
    }
}

fn validate_identity(
    value: &Value,
    did: &str,
    binding: PropertyBinding,
    required: bool,
) -> Result<(), CentralError> {
    for (name, expected) in [("siid", binding.siid), ("piid", binding.piid)] {
        if (required || value.get(name).is_some())
            && value.get(name).and_then(Value::as_u64) != Some(u64::from(expected))
        {
            return Err(CentralError::Protocol);
        }
    }
    if (required || value.get("did").is_some())
        && value.get("did").and_then(Value::as_str) != Some(did)
    {
        return Err(CentralError::Protocol);
    }
    Ok(())
}
fn parse_response(kind: &RequestKind, value: &Value) -> ResultValue {
    if let Some(error) = value.get("error") {
        return Err(CentralError::Rejected(
            error
                .get("code")
                .and_then(Value::as_i64)
                .ok_or(CentralError::Protocol)?,
        ));
    }
    match kind {
        RequestKind::Read { did, binding } => {
            validate_identity(value, did, *binding, false)?;
            Ok(Response::Power(
                value
                    .get("value")
                    .and_then(Value::as_bool)
                    .ok_or(CentralError::Protocol)?,
            ))
        }
        RequestKind::Write { did, binding, .. } => {
            let result = value
                .get("result")
                .and_then(Value::as_array)
                .filter(|result| result.len() == 1)
                .ok_or(CentralError::Protocol)?;
            let result = &result[0];
            if result.get("did").and_then(Value::as_str) != Some(did) {
                return Err(CentralError::Protocol);
            }
            validate_identity(result, did, *binding, false)?;
            match result
                .get("code")
                .and_then(Value::as_i64)
                .ok_or(CentralError::Protocol)?
            {
                0 | 1 => Ok(Response::Unit),
                code => Err(CentralError::Rejected(code)),
            }
        }
        RequestKind::List { did } => {
            let list = value
                .get("devList")
                .and_then(Value::as_object)
                .ok_or(CentralError::Protocol)?;
            let Some(info) = list.get(did) else {
                return Ok(Response::Device(None));
            };
            let text = |key| {
                info.get(key)
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                    .map(str::to_owned)
                    .ok_or(CentralError::Protocol)
            };
            let flag = |key| {
                info.get(key)
                    .and_then(Value::as_bool)
                    .ok_or(CentralError::Protocol)
            };
            Ok(Response::Device(Some(DeviceAccess {
                did: did.clone(),
                model: text("model")?,
                instance_type: text("urn")?,
                online: flag("online")?,
                spec_v2_access: flag("specV2Access")?,
                push_available: flag("pushAvailable")?,
            })))
        }
        RequestKind::Subscribe { .. } => Err(CentralError::Protocol),
    }
}

#[cfg(test)]
pub(crate) mod tests;
