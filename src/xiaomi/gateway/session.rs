use super::{GatewayNotification, MipsEnvelope};
use crate::xiaomi::{
    catalog::WireValue,
    discovery::NetworkEpoch,
    mqtt::{MqttError, MqttHandle, MqttMessage, MqttSendGuard},
};
use flume::{Receiver, Sender};
use futures_lite::future;
use futures_util::{FutureExt, StreamExt, future::LocalBoxFuture, stream::FuturesUnordered};
use serde_json::{Map, Value, json};
use std::{
    collections::{HashMap, HashSet},
    error::Error as StdError,
    fmt,
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
    time::{Duration, Instant},
};

const LOCAL_CONTROL_LIMIT: Duration = Duration::from_secs(3);
const SESSION_QUEUE: usize = 32;
const NOTIFICATION_QUEUE: usize = 128;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GatewayDevice {
    pub did: String,
    pub name: String,
    pub urn: String,
    pub model: String,
    pub online: Option<bool>,
    pub spec_v2_access: Option<bool>,
    pub push_available: Option<bool>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GatewayEvidence {
    pub gateway_did: u64,
    pub peer_did: String,
    pub epoch: NetworkEpoch,
    pub devices: Vec<GatewayDevice>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GatewayErrorKind {
    InvalidInput,
    Protocol,
    Business(i64),
    Timeout,
    Transport,
    Superseded,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GatewayError {
    operation: &'static str,
    kind: GatewayErrorKind,
    may_have_been_sent: bool,
}

impl GatewayError {
    fn new(operation: &'static str, kind: GatewayErrorKind) -> Self {
        Self {
            operation,
            kind,
            may_have_been_sent: false,
        }
    }

    #[cfg(test)]
    pub(crate) fn for_test(kind: GatewayErrorKind) -> Self {
        Self::new("test gateway operation", kind)
    }

    fn transport(operation: &'static str, error: MqttError) -> Self {
        Self {
            operation,
            kind: if error.kind() == &crate::xiaomi::mqtt::MqttErrorKind::Timeout {
                GatewayErrorKind::Timeout
            } else {
                GatewayErrorKind::Transport
            },
            may_have_been_sent: false,
        }
    }

    fn with_send_state(mut self, guard: &MqttSendGuard) -> Self {
        if !matches!(self.kind, GatewayErrorKind::Business(_)) {
            self.may_have_been_sent |= guard.may_have_been_sent();
        }
        self
    }

    pub fn kind(&self) -> &GatewayErrorKind {
        &self.kind
    }

    pub fn may_have_been_sent(&self) -> bool {
        self.may_have_been_sent
    }
}

impl fmt::Display for GatewayError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "Failed to {}: ", self.operation)?;
        match self.kind {
            GatewayErrorKind::InvalidInput => formatter.write_str("invalid input"),
            GatewayErrorKind::Protocol => formatter.write_str("invalid gateway response"),
            GatewayErrorKind::Business(code) => write!(formatter, "gateway code {code}"),
            GatewayErrorKind::Timeout => formatter.write_str("gateway request timed out"),
            GatewayErrorKind::Transport => formatter.write_str("gateway transport failed"),
            GatewayErrorKind::Superseded => {
                formatter.write_str("gateway notification selection was superseded")
            }
        }
    }
}

impl StdError for GatewayError {}

enum RequestKind {
    Devices,
    Property,
    Set { did: String, siid: u32, piid: u32 },
    Action { did: String, siid: u32, aiid: u32 },
}

enum Reply {
    Devices(GatewayEvidence),
    Property(Option<WireValue>),
    Accepted,
}

enum Command {
    Request {
        mid: u32,
        topic: &'static str,
        payload: String,
        kind: RequestKind,
        deadline: Instant,
        guard: MqttSendGuard,
        reply: Sender<Result<Reply, GatewayError>>,
    },
    Select {
        dids: Vec<String>,
        deadline: Instant,
        guard: MqttSendGuard,
        reply: Sender<Result<u64, GatewayError>>,
    },
}

struct Pending {
    kind: RequestKind,
    deadline: Instant,
    reply: Sender<Result<Reply, GatewayError>>,
    guard: MqttSendGuard,
}

struct RequestCancellation {
    guard: MqttSendGuard,
    wake: Arc<event_listener::Event>,
    armed: bool,
}

impl Drop for RequestCancellation {
    fn drop(&mut self) {
        if self.armed {
            self.guard.revoke();
            self.wake.notify(usize::MAX);
        }
    }
}

enum Completed {
    Publish {
        mid: u32,
        result: Result<(), MqttError>,
    },
    Selection {
        step: SelectionStep,
        result: Result<(), MqttError>,
    },
}

enum SelectionStep {
    Remove(Vec<String>),
    Add(Vec<String>),
}

struct DesiredSelection {
    dids: Vec<String>,
    generation: u64,
    cutoff: Instant,
    deadline: Instant,
    guard: MqttSendGuard,
    reply: Sender<Result<u64, GatewayError>>,
}

#[derive(Clone)]
pub struct GatewayHandle {
    commands: Sender<Command>,
    next_mid: Arc<AtomicU32>,
    cancel_wake: Arc<event_listener::Event>,
}

impl GatewayHandle {
    pub async fn get_devices(
        &self,
        deadline: Instant,
        guard: MqttSendGuard,
    ) -> Result<GatewayEvidence, GatewayError> {
        match self
            .request(
                "read gateway device list",
                "master/proxy/getDevList",
                "{}".into(),
                RequestKind::Devices,
                deadline,
                guard,
            )
            .await?
        {
            Reply::Devices(value) => Ok(value),
            _ => Err(GatewayError::new(
                "read gateway device list",
                GatewayErrorKind::Protocol,
            )),
        }
    }

    pub async fn get_property(
        &self,
        did: &str,
        siid: u32,
        piid: u32,
        deadline: Instant,
        guard: MqttSendGuard,
    ) -> Result<Option<WireValue>, GatewayError> {
        validate_target(did, siid, piid, "read gateway property")?;
        match self
            .request(
                "read gateway property",
                "master/proxy/get",
                json!({"did":did,"siid":siid,"piid":piid}).to_string(),
                RequestKind::Property,
                deadline,
                guard,
            )
            .await?
        {
            Reply::Property(value) => Ok(value),
            _ => Err(GatewayError::new(
                "read gateway property",
                GatewayErrorKind::Protocol,
            )),
        }
    }

    pub async fn set_property(
        &self,
        did: &str,
        siid: u32,
        piid: u32,
        value: WireValue,
        deadline: Instant,
        guard: MqttSendGuard,
    ) -> Result<(), GatewayError> {
        validate_target(did, siid, piid, "set gateway property")?;
        let value = encode_wire(value, "set gateway property")?;
        let rpc_id = self.next_id();
        let payload = json!({"did":did,"rpc":{"id":rpc_id,"method":"set_properties","params":[{"did":did,"siid":siid,"piid":piid,"value":value}]}}).to_string();
        self.accepted(
            "set gateway property",
            payload,
            RequestKind::Set {
                did: did.into(),
                siid,
                piid,
            },
            deadline,
            guard,
        )
        .await
    }

    pub async fn invoke_action(
        &self,
        did: &str,
        siid: u32,
        aiid: u32,
        input: Vec<WireValue>,
        deadline: Instant,
        guard: MqttSendGuard,
    ) -> Result<(), GatewayError> {
        validate_target(did, siid, aiid, "invoke gateway action")?;
        let input = input
            .into_iter()
            .map(|value| encode_wire(value, "invoke gateway action"))
            .collect::<Result<Vec<_>, _>>()?;
        let rpc_id = self.next_id();
        let payload = json!({"did":did,"rpc":{"id":rpc_id,"method":"action","params":{"did":did,"siid":siid,"aiid":aiid,"in":input}}}).to_string();
        self.accepted(
            "invoke gateway action",
            payload,
            RequestKind::Action {
                did: did.into(),
                siid,
                aiid,
            },
            deadline,
            guard,
        )
        .await
    }

    pub async fn select_notifications(
        &self,
        dids: Vec<String>,
        deadline: Instant,
    ) -> Result<u64, GatewayError> {
        if dids.iter().any(|did| !valid_component(did)) {
            return Err(GatewayError::new(
                "select gateway notifications",
                GatewayErrorKind::InvalidInput,
            ));
        }
        let (reply, response) = flume::bounded(1);
        let guard = MqttSendGuard::new();
        self.commands
            .send_async(Command::Select {
                dids,
                deadline,
                guard,
                reply,
            })
            .await
            .map_err(|_| {
                GatewayError::new("select gateway notifications", GatewayErrorKind::Transport)
            })?;
        response.recv_async().await.map_err(|_| {
            GatewayError::new("select gateway notifications", GatewayErrorKind::Transport)
        })?
    }

    async fn accepted(
        &self,
        operation: &'static str,
        payload: String,
        kind: RequestKind,
        deadline: Instant,
        guard: MqttSendGuard,
    ) -> Result<(), GatewayError> {
        match self
            .request(
                operation,
                "master/proxy/rpcReq",
                payload,
                kind,
                deadline,
                guard,
            )
            .await?
        {
            Reply::Accepted => Ok(()),
            _ => Err(GatewayError::new(operation, GatewayErrorKind::Protocol)),
        }
    }

    async fn request(
        &self,
        operation: &'static str,
        topic: &'static str,
        payload: String,
        kind: RequestKind,
        deadline: Instant,
        guard: MqttSendGuard,
    ) -> Result<Reply, GatewayError> {
        let deadline = deadline.min(Instant::now() + LOCAL_CONTROL_LIMIT);
        if deadline <= Instant::now() {
            return Err(GatewayError::new(operation, GatewayErrorKind::Timeout));
        }
        let (reply, response) = flume::bounded(1);
        let request_guard = guard.child();
        let mut cancellation = RequestCancellation {
            guard: request_guard.clone(),
            wake: self.cancel_wake.clone(),
            armed: true,
        };
        self.commands
            .send_async(Command::Request {
                mid: self.next_id(),
                topic,
                payload,
                kind,
                deadline,
                guard: request_guard.clone(),
                reply,
            })
            .await
            .map_err(|_| GatewayError::new(operation, GatewayErrorKind::Transport))?;
        let result = response.recv_async().await.map_err(|_| GatewayError {
            operation,
            kind: GatewayErrorKind::Transport,
            may_have_been_sent: request_guard.may_have_been_sent(),
        })?;
        cancellation.armed = false;
        result
    }

    fn next_id(&self) -> u32 {
        self.next_mid.fetch_add(1, Ordering::Relaxed).max(1)
    }
}

pub struct GatewaySession {
    virtual_did: String,
    gateway_did: u64,
    peer_did: String,
    epoch: NetworkEpoch,
    mqtt: MqttHandle,
    messages: Receiver<MqttMessage>,
    commands: Receiver<Command>,
    notifications: Sender<GatewayNotification>,
    cancel_wake: Arc<event_listener::Event>,
}

impl GatewaySession {
    pub fn new(
        virtual_did: &str,
        gateway_did: u64,
        peer_did: &str,
        epoch: NetworkEpoch,
        mqtt: MqttHandle,
        messages: Receiver<MqttMessage>,
    ) -> Result<(Self, GatewayHandle, Receiver<GatewayNotification>), GatewayError> {
        if !valid_component(virtual_did) || gateway_did == 0 || !valid_component(peer_did) {
            return Err(GatewayError::new(
                "configure gateway session",
                GatewayErrorKind::InvalidInput,
            ));
        }
        let (command_sender, commands) = flume::bounded(SESSION_QUEUE);
        let (notifications, notification_receiver) = flume::bounded(NOTIFICATION_QUEUE);
        let cancel_wake = Arc::new(event_listener::Event::new());
        Ok((
            Self {
                virtual_did: virtual_did.into(),
                gateway_did,
                peer_did: peer_did.into(),
                epoch,
                mqtt,
                messages,
                commands,
                notifications,
                cancel_wake: cancel_wake.clone(),
            },
            GatewayHandle {
                commands: command_sender,
                next_mid: Arc::new(AtomicU32::new(1)),
                cancel_wake,
            },
            notification_receiver,
        ))
    }

    pub async fn run(self, startup_deadline: Instant) -> Result<(), GatewayError> {
        self.run_guarded(startup_deadline, MqttSendGuard::new())
            .await
    }

    pub async fn run_guarded(
        self,
        startup_deadline: Instant,
        guard: MqttSendGuard,
    ) -> Result<(), GatewayError> {
        self.mqtt
            .subscribe_guarded(
                vec![
                    format!("{}/#", self.virtual_did),
                    "master/appMsg/devListChange".into(),
                ],
                startup_deadline,
                guard,
            )
            .await
            .map_err(|error| GatewayError::transport("subscribe gateway session", error))?;
        self.run_loop().await
    }

    async fn run_loop(self) -> Result<(), GatewayError> {
        let mut pending = HashMap::<u32, Pending>::new();
        let mut selected = Vec::<String>::new();
        let mut selection_generation = 0_u64;
        let mut generation_since = Instant::now();
        let mut confirmed = HashSet::<String>::new();
        let mut desired = None::<DesiredSelection>;
        let mut selection_pending = false;
        let mut operations = FuturesUnordered::<LocalBoxFuture<'static, Completed>>::new();
        loop {
            let now = Instant::now();
            pending.retain(|_, request| {
                if request.reply.is_disconnected() {
                    request.guard.revoke();
                    return false;
                }
                if now >= request.deadline {
                    request.guard.revoke();
                    let _ = request.reply.send(Err(GatewayError {
                        operation: "wait for gateway reply",
                        kind: GatewayErrorKind::Timeout,
                        may_have_been_sent: request.guard.may_have_been_sent(),
                    }));
                    return false;
                }
                true
            });
            enum Event {
                Command(Result<Command, flume::RecvError>),
                Message(Result<MqttMessage, flume::RecvError>),
                Tick,
                Completed(Completed),
            }
            let next_deadline = pending.values().map(|request| request.deadline).min();
            let event = future::or(
                async {
                    if operations.is_empty() {
                        future::pending::<()>().await;
                        unreachable!()
                    }
                    Event::Completed(operations.next().await.unwrap())
                },
                future::or(
                    async { Event::Command(self.commands.recv_async().await) },
                    future::or(
                        async { Event::Message(self.messages.recv_async().await) },
                        async {
                            match next_deadline {
                                Some(deadline) => {
                                    future::or(
                                        async {
                                            async_io::Timer::at(deadline).await;
                                        },
                                        async { self.cancel_wake.listen().await },
                                    )
                                    .await
                                }
                                None => self.cancel_wake.listen().await,
                            }
                            Event::Tick
                        },
                    ),
                ),
            )
            .await;
            match event {
                Event::Command(Ok(Command::Request {
                    mid,
                    topic,
                    payload,
                    kind,
                    deadline,
                    guard,
                    reply,
                })) => {
                    if reply.is_disconnected() {
                        continue;
                    }
                    if deadline <= Instant::now() {
                        let _ = reply.send(Err(GatewayError::new(
                            "send gateway request",
                            GatewayErrorKind::Timeout,
                        )));
                        continue;
                    }
                    if pending.len() >= SESSION_QUEUE || operations.len() >= SESSION_QUEUE {
                        let _ = reply.send(Err(GatewayError::new(
                            "send gateway request",
                            GatewayErrorKind::Transport,
                        )));
                        continue;
                    }
                    let envelope = MipsEnvelope::request(
                        mid,
                        &format!("{}/reply", self.virtual_did),
                        &payload,
                    )
                    .map_err(|_| {
                        GatewayError::new("encode gateway request", GatewayErrorKind::Protocol)
                    })?;
                    pending.insert(
                        mid,
                        Pending {
                            kind,
                            deadline,
                            reply,
                            guard: guard.clone(),
                        },
                    );
                    let mqtt = self.mqtt.clone();
                    let payload = envelope.encode().map_err(|_| {
                        GatewayError::new("encode gateway request", GatewayErrorKind::Protocol)
                    })?;
                    operations.push(
                        async move {
                            Completed::Publish {
                                mid,
                                result: mqtt.publish_guarded(topic, payload, deadline, guard).await,
                            }
                        }
                        .boxed_local(),
                    );
                }
                Event::Command(Ok(Command::Select {
                    dids,
                    deadline,
                    guard,
                    reply,
                })) => {
                    let previous = desired.take();
                    let same_pending = previous
                        .as_ref()
                        .is_some_and(|selection| selection.dids == dids);
                    let target = selected_topics(&dids).into_iter().collect::<HashSet<_>>();
                    let (generation, cutoff) = if same_pending {
                        let previous = previous.as_ref().unwrap();
                        (previous.generation, previous.cutoff)
                    } else if selected == dids && !selection_pending && confirmed == target {
                        (selection_generation, generation_since)
                    } else {
                        (selection_generation.wrapping_add(1).max(1), Instant::now())
                    };
                    if let Some(previous) = previous {
                        let _ = previous.reply.send(Err(GatewayError::new(
                            "select gateway notifications",
                            GatewayErrorKind::Superseded,
                        )));
                    }
                    desired = Some(DesiredSelection {
                        dids,
                        generation,
                        cutoff,
                        deadline,
                        guard,
                        reply,
                    });
                }
                Event::Completed(Completed::Publish { mid, result }) => {
                    if let Err(error) = result
                        && let Some(request) = pending.remove(&mid)
                    {
                        let _ = request.reply.send(Err(GatewayError::transport(
                            "publish gateway request",
                            error,
                        )));
                    }
                }
                Event::Completed(Completed::Selection { step, result }) => {
                    selection_pending = false;
                    match result {
                        Ok(()) => match step {
                            SelectionStep::Remove(topics) => {
                                for topic in topics {
                                    confirmed.remove(&topic);
                                }
                            }
                            SelectionStep::Add(topics) => confirmed.extend(topics),
                        },
                        Err(error) => {
                            let failure =
                                GatewayError::transport("select gateway notifications", error);
                            if let Some(selection) = desired.take() {
                                let _ = selection.reply.send(Err(failure.clone()));
                            }
                            return Err(failure);
                        }
                    }
                }
                Event::Message(Ok(message))
                    if message.topic() == format!("{}/reply", self.virtual_did) =>
                {
                    let envelope = MipsEnvelope::decode(message.payload()).map_err(|_| {
                        GatewayError::new("decode gateway reply", GatewayErrorKind::Protocol)
                    })?;
                    if let Some(request) = pending.remove(&envelope.mid) {
                        let result = parse_reply(
                            request.kind,
                            &envelope.payload,
                            self.gateway_did,
                            &self.peer_did,
                            self.epoch,
                        )
                        .map_err(|error| error.with_send_state(&request.guard));
                        let _ = request.reply.send(result);
                    }
                }
                Event::Message(Ok(message)) => {
                    if message.received_at() < generation_since {
                        continue;
                    }
                    let visible = selected
                        .iter()
                        .filter(|did| {
                            desired
                                .as_ref()
                                .is_none_or(|selection| selection.dids.contains(did))
                        })
                        .cloned()
                        .collect::<Vec<_>>();
                    if let Ok(notification) = GatewayNotification::parse(
                        message.topic(),
                        message.payload(),
                        message.retained(),
                        &visible,
                        self.epoch.get(),
                        selection_generation,
                    ) {
                        self.notifications.try_send(notification).map_err(|_| {
                            GatewayError::new(
                                "deliver gateway notification",
                                GatewayErrorKind::Transport,
                            )
                        })?;
                    }
                }
                Event::Tick => {}
                Event::Command(Err(_)) | Event::Message(Err(_)) => {
                    return Err(GatewayError::new(
                        "run gateway session",
                        GatewayErrorKind::Transport,
                    ));
                }
            }
            if !selection_pending && let Some(selection) = desired.as_ref() {
                let target = selected_topics(&selection.dids)
                    .into_iter()
                    .collect::<HashSet<_>>();
                let remove = confirmed.difference(&target).cloned().collect::<Vec<_>>();
                let add = target.difference(&confirmed).cloned().collect::<Vec<_>>();
                if remove.is_empty() && add.is_empty() {
                    let completed = desired.take().unwrap();
                    selected = completed.dids;
                    selection_generation = completed.generation;
                    generation_since = completed.cutoff;
                    let _ = completed.reply.send(Ok(selection_generation));
                } else {
                    let (step, values) = if remove.is_empty() {
                        (SelectionStep::Add(add.clone()), add)
                    } else {
                        (SelectionStep::Remove(remove.clone()), remove)
                    };
                    let mqtt = self.mqtt.clone();
                    let deadline = selection.deadline;
                    let guard = selection.guard.clone();
                    selection_pending = true;
                    operations.push(
                        async move {
                            let result = match &step {
                                SelectionStep::Remove(_) => {
                                    mqtt.unsubscribe_guarded(values, deadline, guard).await
                                }
                                SelectionStep::Add(_) => {
                                    mqtt.subscribe_guarded(values, deadline, guard).await
                                }
                            };
                            Completed::Selection { step, result }
                        }
                        .boxed_local(),
                    );
                }
            }
        }
    }
}

fn selected_topics(dids: &[String]) -> Vec<String> {
    dids.iter()
        .flat_map(|did| {
            [
                format!("master/appMsg/notify/iot/{did}/property/#"),
                format!("master/appMsg/notify/iot/{did}/event/#"),
            ]
        })
        .collect()
}

fn parse_reply(
    kind: RequestKind,
    payload: &str,
    gateway_did: u64,
    peer_did: &str,
    epoch: NetworkEpoch,
) -> Result<Reply, GatewayError> {
    let value: Value = serde_json::from_str(payload)
        .map_err(|_| GatewayError::new("decode gateway reply", GatewayErrorKind::Protocol))?;
    let object = value
        .as_object()
        .ok_or_else(|| GatewayError::new("decode gateway reply", GatewayErrorKind::Protocol))?;
    if let Some(error) = object.get("error") {
        let code = error
            .get("code")
            .and_then(Value::as_i64)
            .ok_or_else(|| GatewayError::new("decode gateway reply", GatewayErrorKind::Protocol))?;
        return Err(GatewayError::new(
            "execute gateway request",
            GatewayErrorKind::Business(code),
        ));
    }
    match kind {
        RequestKind::Devices => Ok(Reply::Devices(parse_devices(
            object,
            gateway_did,
            peer_did,
            epoch,
        )?)),
        RequestKind::Property => Ok(Reply::Property(
            object
                .get("value")
                .and_then(super::notification::wire_value),
        )),
        RequestKind::Set { did, siid, piid } => {
            let item = object
                .get("result")
                .and_then(Value::as_array)
                .filter(|items| items.len() == 1)
                .and_then(|items| items[0].as_object())
                .ok_or_else(|| {
                    GatewayError::new("set gateway property", GatewayErrorKind::Protocol)
                })?;
            validate_result(item, &did, siid, "piid", piid, "set gateway property")?;
            Ok(Reply::Accepted)
        }
        RequestKind::Action { did, siid, aiid } => {
            let item = object
                .get("result")
                .and_then(Value::as_object)
                .ok_or_else(|| {
                    GatewayError::new("invoke gateway action", GatewayErrorKind::Protocol)
                })?;
            validate_result(item, &did, siid, "aiid", aiid, "invoke gateway action")?;
            Ok(Reply::Accepted)
        }
    }
}

fn validate_result(
    object: &Map<String, Value>,
    did: &str,
    siid: u32,
    iid_key: &str,
    iid: u32,
    operation: &'static str,
) -> Result<(), GatewayError> {
    if object.get("did").and_then(Value::as_str) != Some(did)
        || object.get("siid").and_then(Value::as_u64) != Some(siid.into())
        || object.get(iid_key).and_then(Value::as_u64) != Some(iid.into())
    {
        return Err(GatewayError::new(operation, GatewayErrorKind::Protocol));
    }
    let code = object
        .get("code")
        .and_then(Value::as_i64)
        .ok_or_else(|| GatewayError::new(operation, GatewayErrorKind::Protocol))?;
    if code != 0 {
        return Err(GatewayError::new(
            operation,
            GatewayErrorKind::Business(code),
        ));
    }
    Ok(())
}

fn parse_devices(
    object: &Map<String, Value>,
    gateway_did: u64,
    peer_did: &str,
    epoch: NetworkEpoch,
) -> Result<GatewayEvidence, GatewayError> {
    let list = object
        .get("devList")
        .and_then(Value::as_object)
        .ok_or_else(|| GatewayError::new("read gateway device list", GatewayErrorKind::Protocol))?;
    let mut devices = Vec::with_capacity(list.len());
    for (did, value) in list {
        if !valid_component(did) {
            return Err(GatewayError::new(
                "read gateway device list",
                GatewayErrorKind::Protocol,
            ));
        }
        let item = value.as_object().ok_or_else(|| {
            GatewayError::new("read gateway device list", GatewayErrorKind::Protocol)
        })?;
        devices.push(GatewayDevice {
            did: did.clone(),
            name: required_string(item, "name")?,
            urn: required_string(item, "urn")?,
            model: required_string(item, "model")?,
            online: optional_bool(item, "online")?,
            spec_v2_access: optional_bool(item, "specV2Access")?,
            push_available: optional_bool(item, "pushAvailable")?,
        });
    }
    Ok(GatewayEvidence {
        gateway_did,
        peer_did: peer_did.into(),
        epoch,
        devices,
    })
}

fn required_string(object: &Map<String, Value>, key: &str) -> Result<String, GatewayError> {
    object
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| GatewayError::new("read gateway device list", GatewayErrorKind::Protocol))
}

fn optional_bool(object: &Map<String, Value>, key: &str) -> Result<Option<bool>, GatewayError> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(value)) => Ok(Some(*value)),
        _ => Err(GatewayError::new(
            "read gateway device list",
            GatewayErrorKind::Protocol,
        )),
    }
}

fn validate_target(
    did: &str,
    siid: u32,
    iid: u32,
    operation: &'static str,
) -> Result<(), GatewayError> {
    if !valid_component(did) || siid == 0 || iid == 0 {
        return Err(GatewayError::new(operation, GatewayErrorKind::InvalidInput));
    }
    Ok(())
}

fn valid_component(value: &str) -> bool {
    !value.is_empty()
        && !value.contains(['/', '+', '#', '\0'])
        && !value.chars().any(char::is_control)
}

fn encode_wire(value: WireValue, operation: &'static str) -> Result<Value, GatewayError> {
    match value {
        WireValue::Boolean(value) => Ok(Value::Bool(value)),
        WireValue::Integer(value) => Ok(Value::Number(value.into())),
        WireValue::Number(value) => serde_json::Number::from_f64(value)
            .map(Value::Number)
            .ok_or_else(|| GatewayError::new(operation, GatewayErrorKind::InvalidInput)),
        WireValue::String(value) => Ok(Value::String(value)),
    }
}
