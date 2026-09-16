use super::{MqttError, MqttErrorKind};
use flume::{Receiver, Sender};
use rumqttc::v5::mqttbytes::{QoS, v5};
use rumqttc::v5::{AsyncClient, Event, EventLoop, MqttOptions};
use rumqttc::{Outgoing, Transport};
use std::{
    collections::VecDeque,
    fmt,
    rc::Rc,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

const REQUEST_CAPACITY: usize = 32;
const MESSAGE_CAPACITY: usize = 128;
const OPERATION_CAPACITY: usize = 16;

#[derive(Clone)]
pub struct MqttConfig {
    client_id: String,
    login: Option<(String, String)>,
    keep_alive: Duration,
    host: Option<String>,
    port: Option<u16>,
    tls: Option<Arc<rustls::ClientConfig>>,
    #[cfg(test)]
    operation_observer: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl MqttConfig {
    pub fn new(
        client_id: impl Into<String>,
        login: Option<(String, String)>,
        keep_alive: Duration,
    ) -> Self {
        Self {
            client_id: client_id.into(),
            login,
            keep_alive,
            host: None,
            port: None,
            tls: None,
            #[cfg(test)]
            operation_observer: None,
        }
    }
    pub fn with_endpoint(mut self, host: impl Into<String>, port: u16) -> Self {
        self.host = Some(host.into());
        self.port = Some(port);
        self
    }
    pub fn with_tls(mut self, config: Arc<rustls::ClientConfig>) -> Self {
        self.tls = Some(config);
        self
    }
    #[cfg(test)]
    pub(super) fn with_operation_observer(
        mut self,
        observer: impl Fn() + Send + Sync + 'static,
    ) -> Self {
        self.operation_observer = Some(Arc::new(observer));
        self
    }
}

impl fmt::Debug for MqttConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MqttConfig")
            .field("client_id", &self.client_id)
            .field(
                "login",
                &self.login.as_ref().map(|(name, _)| (name, "[REDACTED]")),
            )
            .field("keep_alive", &self.keep_alive)
            .field("host", &self.host)
            .field("port", &self.port)
            .field("tls", &self.tls.is_some())
            .finish()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MqttMessage {
    topic: String,
    payload: Vec<u8>,
    retained: bool,
    received_at: Instant,
}
impl MqttMessage {
    pub fn topic(&self) -> &str {
        &self.topic
    }
    pub fn payload(&self) -> &[u8] {
        &self.payload
    }
    pub fn retained(&self) -> bool {
        self.retained
    }
    pub fn received_at(&self) -> Instant {
        self.received_at
    }
}

#[derive(Clone)]
pub struct MqttSendGuard {
    active: Arc<AtomicBool>,
    accepted: Arc<AtomicBool>,
    check: Rc<dyn Fn() -> Result<bool, MqttError>>,
    observer: Option<Arc<dyn Fn() + Send + Sync>>,
}
impl MqttSendGuard {
    pub fn new() -> Self {
        Self {
            active: Arc::new(AtomicBool::new(true)),
            accepted: Arc::new(AtomicBool::new(false)),
            check: Rc::new(|| Ok(true)),
            observer: None,
        }
    }
    pub fn with_check(check: impl Fn() -> Result<bool, MqttError> + 'static) -> Self {
        Self {
            check: Rc::new(check),
            ..Self::new()
        }
    }
    pub fn with_send_observer(mut self, observer: impl Fn() + Send + Sync + 'static) -> Self {
        self.observer = Some(Arc::new(observer));
        self
    }
    pub fn revoke(&self) {
        self.active.store(false, Ordering::Release);
    }
    pub fn may_have_been_sent(&self) -> bool {
        self.accepted.load(Ordering::Acquire)
    }
    pub fn child(&self) -> Self {
        let parent = self.clone();
        Self {
            active: Arc::new(AtomicBool::new(true)),
            accepted: Arc::new(AtomicBool::new(false)),
            check: Rc::new(move || parent.check()),
            observer: self.observer.clone(),
        }
    }
    fn check(&self) -> Result<bool, MqttError> {
        if !self.active.load(Ordering::Acquire) {
            Ok(false)
        } else {
            (self.check)()
        }
    }
    fn mark_accepted(&self) {
        self.accepted.store(true, Ordering::Release);
        if let Some(observer) = &self.observer {
            observer();
        }
    }
}
impl Default for MqttSendGuard {
    fn default() -> Self {
        Self::new()
    }
}
impl fmt::Debug for MqttSendGuard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MqttSendGuard").finish_non_exhaustive()
    }
}

enum Operation {
    Subscribe {
        topics: Vec<String>,
        deadline: Instant,
        reply: Sender<Result<(), MqttError>>,
    },
    Unsubscribe {
        topics: Vec<String>,
        deadline: Instant,
        reply: Sender<Result<(), MqttError>>,
    },
}

#[derive(Clone)]
pub struct MqttHandle {
    client: AsyncClient,
    operations: Sender<Operation>,
    stop: Sender<()>,
}
impl MqttHandle {
    pub async fn publish(
        &self,
        topic: impl Into<String>,
        payload: Vec<u8>,
        deadline: Instant,
    ) -> Result<(), MqttError> {
        self.publish_guarded(topic, payload, deadline, MqttSendGuard::new())
            .await
    }
    pub async fn publish_guarded(
        &self,
        topic: impl Into<String>,
        payload: Vec<u8>,
        deadline: Instant,
        guard: MqttSendGuard,
    ) -> Result<(), MqttError> {
        check_dispatch(&guard, deadline)?;
        self.client
            .try_publish(topic, QoS::ExactlyOnce, false, payload)
            .map_err(map_client_error)?;
        guard.mark_accepted();
        Ok(())
    }
    pub async fn subscribe(&self, topics: Vec<String>, deadline: Instant) -> Result<(), MqttError> {
        self.subscribe_guarded(topics, deadline, MqttSendGuard::new())
            .await
    }
    pub async fn subscribe_guarded(
        &self,
        topics: Vec<String>,
        deadline: Instant,
        guard: MqttSendGuard,
    ) -> Result<(), MqttError> {
        check_dispatch(&guard, deadline)?;
        request_operation(&self.operations, true, topics, deadline).await
    }
    pub async fn unsubscribe(
        &self,
        topics: Vec<String>,
        deadline: Instant,
    ) -> Result<(), MqttError> {
        self.unsubscribe_guarded(topics, deadline, MqttSendGuard::new())
            .await
    }
    pub async fn unsubscribe_guarded(
        &self,
        topics: Vec<String>,
        deadline: Instant,
        guard: MqttSendGuard,
    ) -> Result<(), MqttError> {
        check_dispatch(&guard, deadline)?;
        request_operation(&self.operations, false, topics, deadline).await
    }
    pub async fn stop(&self) {
        let _ = self.stop.try_send(());
    }
}

async fn request_operation(
    sender: &Sender<Operation>,
    subscribe: bool,
    topics: Vec<String>,
    deadline: Instant,
) -> Result<(), MqttError> {
    let (reply, response) = flume::bounded(1);
    let operation = if subscribe {
        Operation::Subscribe {
            topics,
            deadline,
            reply,
        }
    } else {
        Operation::Unsubscribe {
            topics,
            deadline,
            reply,
        }
    };
    sender.try_send(operation).map_err(|error| match error {
        flume::TrySendError::Full(_) => {
            MqttError::new("queue MQTT request", MqttErrorKind::Capacity)
        }
        flume::TrySendError::Disconnected(_) => {
            MqttError::new("queue MQTT request", MqttErrorKind::Disconnected)
        }
    })?;
    response
        .recv_async()
        .await
        .map_err(|_| MqttError::new("wait for MQTT request", MqttErrorKind::Disconnected))?
}
fn check_dispatch(guard: &MqttSendGuard, deadline: Instant) -> Result<(), MqttError> {
    if Instant::now() >= deadline {
        return Err(MqttError::new(
            "dispatch MQTT request",
            MqttErrorKind::Timeout,
        ));
    }
    if !guard.check()? {
        return Err(MqttError::guard_failed());
    }
    Ok(())
}

pub struct MqttConnection {
    eventloop: EventLoop,
    client: AsyncClient,
    operations: Receiver<Operation>,
    messages: Sender<MqttMessage>,
    stop: Receiver<()>,
    #[cfg(test)]
    operation_observer: Option<Arc<dyn Fn() + Send + Sync>>,
}
impl MqttConnection {
    pub fn new(config: MqttConfig) -> Result<(Self, MqttHandle, Receiver<MqttMessage>), MqttError> {
        let host = config
            .host
            .as_deref()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| MqttError::new("configure MQTT session", MqttErrorKind::InvalidInput))?;
        let port = config
            .port
            .filter(|port| *port != 0)
            .ok_or_else(|| MqttError::new("configure MQTT session", MqttErrorKind::InvalidInput))?;
        if !valid_component(&config.client_id)
            || config.keep_alive < Duration::from_secs(5)
            || config.login.as_ref().is_some_and(|(name, password)| {
                name.is_empty() || password.is_empty() || name.chars().any(char::is_control)
            })
        {
            return Err(MqttError::new(
                "configure MQTT session",
                MqttErrorKind::InvalidInput,
            ));
        }
        let mut options = MqttOptions::new(config.client_id, host, port);
        options
            .set_keep_alive(config.keep_alive)
            .set_clean_start(true)
            .set_max_packet_size(Some(256 * 1024));
        if let Some((username, password)) = config.login {
            options.set_credentials(username, password);
        }
        if let Some(tls) = config.tls {
            options.set_transport(Transport::tls_with_config(
                rumqttc::TlsConfiguration::Rustls(tls),
            ));
        }
        let (client, eventloop) = AsyncClient::new(options, REQUEST_CAPACITY);
        let (operations_tx, operations) = flume::bounded(OPERATION_CAPACITY);
        let (messages, message_rx) = flume::bounded(MESSAGE_CAPACITY);
        let (stop_tx, stop) = flume::bounded(1);
        Ok((
            Self {
                eventloop,
                client: client.clone(),
                operations,
                messages,
                stop,
                #[cfg(test)]
                operation_observer: config.operation_observer,
            },
            MqttHandle {
                client,
                operations: operations_tx,
                stop: stop_tx,
            },
            message_rx,
        ))
    }
    pub async fn run(self) -> Result<(), MqttError> {
        let mut task = mqtt_runtime().spawn(run_eventloop(self));
        let abort = AbortOnDrop(task.abort_handle());
        let result = (&mut task)
            .await
            .map_err(|_| MqttError::new("run MQTT session", MqttErrorKind::Disconnected))?;
        drop(abort);
        result
    }
}

struct AbortOnDrop(tokio::task::AbortHandle);
impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

struct PendingOperation {
    kind: PendingKind,
    expected: usize,
    packet_id: Option<u16>,
    remaining: VecDeque<String>,
    deadline: Instant,
    reply: Sender<Result<(), MqttError>>,
}
#[derive(Clone, Copy, Eq, PartialEq)]
enum PendingKind {
    Subscribe,
    Unsubscribe,
}

async fn run_eventloop(connection: MqttConnection) -> Result<(), MqttError> {
    let MqttConnection {
        mut eventloop,
        client,
        operations,
        messages,
        stop,
        #[cfg(test)]
        operation_observer,
    } = connection;
    let mut pending: Option<PendingOperation> = None;
    let mut stopping = false;
    let mut stop_deadline = None::<tokio::time::Instant>;
    let mut operations_open = true;
    'events: loop {
        let poll = eventloop.poll();
        tokio::pin!(poll);
        loop {
            if pending
                .as_ref()
                .is_some_and(|value| Instant::now() >= value.deadline)
            {
                let value = pending.take().expect("checked pending operation");
                let _ = value.reply.try_send(Err(MqttError::new(
                    "wait for MQTT acknowledgement",
                    MqttErrorKind::Timeout,
                )));
                return Err(MqttError::new(
                    "wait for MQTT acknowledgement",
                    MqttErrorKind::Timeout,
                ));
            }
            let wait = pending
                .as_ref()
                .map(|value| value.deadline.saturating_duration_since(Instant::now()))
                .unwrap_or(Duration::from_secs(3600));
            tokio::select! {
                biased;
                _ = stop.recv_async(), if !stopping => {
                    client.try_disconnect().map_err(map_client_error)?;
                    stopping = true;
                    stop_deadline = Some(tokio::time::Instant::now() + Duration::from_secs(1));
                },
                operation = operations.recv_async(), if pending.is_none() && operations_open => {
                    match operation {
                        Ok(operation) => {
                            #[cfg(test)]
                            if let Some(observer) = &operation_observer {
                                observer();
                            }
                            pending = start_operation(&client, operation);
                        }
                        Err(_) => operations_open = false,
                    }
                }
                event = &mut poll => {
                    let event = event.map_err(map_connection_error)?;
                    if stopping && matches!(event, Event::Outgoing(Outgoing::Disconnect)) { return Ok(()); }
                    handle_event(event, &client, &messages, &mut pending)?;
                    continue 'events;
                },
                _ = tokio::time::sleep(wait), if pending.is_some() => {}
                _ = tokio::time::sleep_until(stop_deadline.unwrap_or_else(tokio::time::Instant::now)), if stop_deadline.is_some() => return Ok(()),
            }
        }
    }
}

fn start_operation(client: &AsyncClient, operation: Operation) -> Option<PendingOperation> {
    match operation {
        Operation::Subscribe {
            topics,
            deadline,
            reply,
        } => {
            if Instant::now() >= deadline {
                let _ = reply.try_send(Err(MqttError::new(
                    "subscribe MQTT topics",
                    MqttErrorKind::Timeout,
                )));
                return None;
            }
            if topics.is_empty() || topics.iter().any(|topic| topic.is_empty()) {
                let _ = reply.try_send(Err(MqttError::new(
                    "subscribe MQTT topics",
                    MqttErrorKind::InvalidInput,
                )));
                return None;
            }
            let expected = topics.len();
            let filters = topics.into_iter().map(|path| v5::Filter {
                path,
                qos: QoS::ExactlyOnce,
                nolocal: false,
                preserve_retain: false,
                retain_forward_rule: v5::RetainForwardRule::Never,
            });
            if let Err(error) = client.try_subscribe_many(filters).map_err(map_client_error) {
                let _ = reply.try_send(Err(error));
                return None;
            }
            Some(PendingOperation {
                kind: PendingKind::Subscribe,
                expected,
                packet_id: None,
                remaining: VecDeque::new(),
                deadline,
                reply,
            })
        }
        Operation::Unsubscribe {
            topics,
            deadline,
            reply,
        } => {
            if Instant::now() >= deadline {
                let _ = reply.try_send(Err(MqttError::new(
                    "unsubscribe MQTT topics",
                    MqttErrorKind::Timeout,
                )));
                return None;
            }
            if topics.is_empty() || topics.iter().any(|topic| topic.is_empty()) {
                let _ = reply.try_send(Err(MqttError::new(
                    "unsubscribe MQTT topics",
                    MqttErrorKind::InvalidInput,
                )));
                return None;
            }
            let mut remaining = VecDeque::from(topics);
            let topic = remaining.pop_front().expect("validated nonempty topics");
            if let Err(error) = client.try_unsubscribe(topic).map_err(map_client_error) {
                let _ = reply.try_send(Err(error));
                return None;
            }
            Some(PendingOperation {
                kind: PendingKind::Unsubscribe,
                expected: 1,
                packet_id: None,
                remaining,
                deadline,
                reply,
            })
        }
    }
}

fn handle_event(
    event: Event,
    client: &AsyncClient,
    messages: &Sender<MqttMessage>,
    pending: &mut Option<PendingOperation>,
) -> Result<(), MqttError> {
    match event {
        Event::Outgoing(Outgoing::Subscribe(id)) => assign_id(pending, PendingKind::Subscribe, id)?,
        Event::Outgoing(Outgoing::Unsubscribe(id)) => {
            assign_id(pending, PendingKind::Unsubscribe, id)?
        }
        Event::Incoming(v5::Packet::SubAck(ack)) => complete_suback(pending, ack)?,
        Event::Incoming(v5::Packet::UnsubAck(ack)) => complete_unsuback(client, pending, ack)?,
        Event::Incoming(v5::Packet::Publish(publish)) => messages
            .try_send(MqttMessage {
                topic: String::from_utf8(publish.topic.to_vec())
                    .map_err(|_| MqttError::new("decode MQTT topic", MqttErrorKind::Protocol))?,
                payload: publish.payload.to_vec(),
                retained: publish.retain,
                received_at: Instant::now(),
            })
            .map_err(|_| MqttError::new("deliver MQTT message", MqttErrorKind::Capacity))?,
        _ => {}
    }
    Ok(())
}
fn assign_id(
    pending: &mut Option<PendingOperation>,
    kind: PendingKind,
    id: u16,
) -> Result<(), MqttError> {
    let value = pending
        .as_mut()
        .filter(|value| value.kind == kind && value.packet_id.is_none())
        .ok_or_else(|| MqttError::new("correlate MQTT acknowledgement", MqttErrorKind::Protocol))?;
    value.packet_id = Some(id);
    Ok(())
}
fn complete_suback(
    pending: &mut Option<PendingOperation>,
    ack: v5::SubAck,
) -> Result<(), MqttError> {
    let value = pending
        .take()
        .filter(|value| value.kind == PendingKind::Subscribe && value.packet_id == Some(ack.pkid))
        .ok_or_else(|| MqttError::new("correlate MQTT SUBACK", MqttErrorKind::Protocol))?;
    let accepted = ack.return_codes.len() == value.expected
        && ack
            .return_codes
            .iter()
            .all(|reason| matches!(reason, v5::SubscribeReasonCode::Success(_)));
    let result = if accepted {
        Ok(())
    } else {
        Err(MqttError::new(
            "validate MQTT SUBACK",
            MqttErrorKind::Protocol,
        ))
    };
    let _ = value.reply.try_send(result);
    Ok(())
}
fn complete_unsuback(
    client: &AsyncClient,
    pending: &mut Option<PendingOperation>,
    ack: v5::UnsubAck,
) -> Result<(), MqttError> {
    let mut value = pending
        .take()
        .filter(|value| value.kind == PendingKind::Unsubscribe && value.packet_id == Some(ack.pkid))
        .ok_or_else(|| MqttError::new("correlate MQTT UNSUBACK", MqttErrorKind::Protocol))?;
    let accepted = ack.reasons.len() == value.expected
        && ack.reasons.iter().all(|reason| {
            matches!(
                reason,
                v5::UnsubAckReason::Success | v5::UnsubAckReason::NoSubscriptionExisted
            )
        });
    if accepted && let Some(topic) = value.remaining.pop_front() {
        match client.try_unsubscribe(topic).map_err(map_client_error) {
            Ok(()) => {
                value.packet_id = None;
                *pending = Some(value);
                return Ok(());
            }
            Err(error) => {
                let _ = value.reply.try_send(Err(error));
                return Ok(());
            }
        }
    }
    let result = if accepted {
        Ok(())
    } else {
        Err(MqttError::new(
            "validate MQTT UNSUBACK",
            MqttErrorKind::Protocol,
        ))
    };
    let _ = value.reply.try_send(result);
    Ok(())
}
fn map_client_error(error: rumqttc::v5::ClientError) -> MqttError {
    match error {
        rumqttc::v5::ClientError::TryRequest(_) => {
            MqttError::new("queue MQTT request", MqttErrorKind::Capacity)
        }
        _ => MqttError::new("queue MQTT request", MqttErrorKind::Disconnected),
    }
}
fn map_connection_error(error: rumqttc::v5::ConnectionError) -> MqttError {
    use rumqttc::v5::ConnectionError;
    match error {
        ConnectionError::ConnectionRefused(
            v5::ConnectReturnCode::NotAuthorized | v5::ConnectReturnCode::BadUserNamePassword,
        ) => MqttError::new("connect MQTT", MqttErrorKind::Unauthorized),
        ConnectionError::ConnectionRefused(_) | ConnectionError::MqttState(_) => {
            MqttError::new("process MQTT packet", MqttErrorKind::Protocol)
        }
        ConnectionError::Timeout(_) => {
            MqttError::new("communicate with MQTT broker", MqttErrorKind::Timeout)
        }
        _ => MqttError::new("communicate with MQTT broker", MqttErrorKind::Network),
    }
}
fn mqtt_runtime() -> &'static tokio::runtime::Handle {
    static HANDLE: OnceLock<tokio::runtime::Handle> = OnceLock::new();
    HANDLE.get_or_init(|| {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build private MQTT runtime");
        let handle = runtime.handle().clone();
        std::thread::Builder::new()
            .name("migate-mqtt".to_owned())
            .spawn(move || runtime.block_on(std::future::pending::<()>()))
            .expect("start private MQTT runtime");
        handle
    })
}
fn valid_component(value: &str) -> bool {
    !value.is_empty() && value.len() <= u16::MAX as usize && !value.chars().any(char::is_control)
}
