use super::CLIENT_ID;
use crate::xiaomi::{
    catalog::WireValue,
    gateway::EventArguments,
    mqtt::{MqttConfig, MqttConnection, MqttError, MqttHandle, MqttMessage, MqttSendGuard},
};
use flume::{Receiver, Sender};
use futures_lite::future;
use futures_util::{FutureExt, future::LocalBoxFuture};
use serde_json::{Map, Value};
use std::{collections::HashSet, error::Error as StdError, fmt, time::Instant};

pub const CLOUD_MQTT_HOST: &str = "cn-ha.mqtt.io.mi.com";
pub const CLOUD_MQTT_PORT: u16 = 8883;

const COMMAND_CAPACITY: usize = 16;
const NOTIFICATION_CAPACITY: usize = 128;

#[derive(Clone, Debug, PartialEq)]
pub enum CloudNotification {
    Property {
        did: String,
        siid: u32,
        piid: u32,
        value: Option<WireValue>,
        generation: u64,
    },
    Event {
        did: String,
        siid: u32,
        eiid: u32,
        arguments: EventArguments,
        generation: u64,
    },
    State {
        did: String,
        online: bool,
        generation: u64,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CloudNotificationError;

impl fmt::Display for CloudNotificationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Invalid Xiaomi cloud notification")
    }
}

impl StdError for CloudNotificationError {}

enum Command {
    Select {
        dids: Vec<String>,
        deadline: Instant,
        guard: MqttSendGuard,
        reply: Sender<Result<u64, MqttError>>,
    },
}

#[derive(Clone)]
pub struct CloudNotificationHandle {
    commands: Sender<Command>,
}

impl CloudNotificationHandle {
    pub async fn select_dids(
        &self,
        dids: Vec<String>,
        deadline: Instant,
    ) -> Result<u64, MqttError> {
        if dids.iter().any(|did| !valid_component(did)) {
            return Err(MqttError::guard_failed());
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
            .map_err(|_| MqttError::guard_failed())?;
        response
            .recv_async()
            .await
            .map_err(|_| MqttError::guard_failed())?
    }

    pub async fn select_dids_guarded(
        &self,
        dids: Vec<String>,
        deadline: Instant,
        guard: MqttSendGuard,
    ) -> Result<u64, MqttError> {
        if dids.iter().any(|did| !valid_component(did)) {
            return Err(MqttError::guard_failed());
        }
        let (reply, response) = flume::bounded(1);
        self.commands
            .send_async(Command::Select {
                dids,
                deadline,
                guard,
                reply,
            })
            .await
            .map_err(|_| MqttError::guard_failed())?;
        response
            .recv_async()
            .await
            .map_err(|_| MqttError::guard_failed())?
    }
}

pub struct CloudNotificationSession {
    mqtt: MqttHandle,
    messages: Receiver<MqttMessage>,
    commands: Receiver<Command>,
    notifications: Sender<CloudNotification>,
}

impl CloudNotificationSession {
    pub fn new(
        oauth_client_uuid: &str,
        access_token: &str,
        keep_alive: std::time::Duration,
        host: &str,
        port: u16,
        tls: Option<std::sync::Arc<rustls::ClientConfig>>,
    ) -> Result<
        (
            MqttConnection,
            Self,
            CloudNotificationHandle,
            Receiver<CloudNotification>,
        ),
        MqttError,
    > {
        if !valid_component(oauth_client_uuid) || access_token.is_empty() {
            return Err(MqttError::guard_failed());
        }
        let mut config = MqttConfig::new(
            format!("ha.{oauth_client_uuid}"),
            Some((CLIENT_ID.to_string(), access_token.to_owned())),
            keep_alive,
        )
        .with_endpoint(host, port);
        if let Some(tls) = tls {
            config = config.with_tls(tls);
        }
        let (connection, mqtt, messages) = MqttConnection::new(config)?;
        let (command_sender, commands) = flume::bounded(COMMAND_CAPACITY);
        let (notifications, notification_receiver) = flume::bounded(NOTIFICATION_CAPACITY);
        Ok((
            connection,
            Self {
                mqtt,
                messages,
                commands,
                notifications,
            },
            CloudNotificationHandle {
                commands: command_sender,
            },
            notification_receiver,
        ))
    }

    pub async fn run(self) -> Result<(), MqttError> {
        let mut selected = Vec::<String>::new();
        let mut generation = 0_u64;
        let mut generation_since = Instant::now();
        let mut confirmed = HashSet::<String>::new();
        let mut desired = None::<DesiredSelection>;
        let mut operation = None::<LocalBoxFuture<'static, SelectionCompleted>>;
        loop {
            enum Event {
                Command(Result<Command, flume::RecvError>),
                Message(Result<MqttMessage, flume::RecvError>),
                Completed(SelectionCompleted),
            }
            let event = future::or(
                async {
                    if operation.is_none() {
                        future::pending::<()>().await;
                        unreachable!()
                    }
                    Event::Completed(operation.as_mut().unwrap().await)
                },
                future::or(
                    async { Event::Command(self.commands.recv_async().await) },
                    async { Event::Message(self.messages.recv_async().await) },
                ),
            )
            .await;
            match event {
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
                    let target = topics(&dids).into_iter().collect::<HashSet<_>>();
                    let (next_generation, cutoff) = if same_pending {
                        let previous = previous.as_ref().unwrap();
                        (previous.generation, previous.cutoff)
                    } else if selected == dids && operation.is_none() && confirmed == target {
                        (generation, generation_since)
                    } else {
                        (generation.wrapping_add(1).max(1), Instant::now())
                    };
                    if let Some(previous) = previous {
                        let _ = previous.reply.send(Err(MqttError::superseded()));
                    }
                    desired = Some(DesiredSelection {
                        dids,
                        generation: next_generation,
                        cutoff,
                        deadline,
                        guard,
                        reply,
                    });
                }
                Event::Completed(completed) => {
                    operation = None;
                    match completed.result {
                        Ok(()) => match completed.step {
                            SelectionStep::Remove(topics) => {
                                for topic in topics {
                                    confirmed.remove(&topic);
                                }
                            }
                            SelectionStep::Add(topics) => confirmed.extend(topics),
                        },
                        Err(error) => {
                            if let Some(selection) = desired.take() {
                                let _ = selection.reply.send(Err(error.clone()));
                            }
                            return Err(error);
                        }
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
                    if let Ok(notification) = Self::parse(
                        message.topic(),
                        message.payload(),
                        message.retained(),
                        &visible,
                        generation,
                    ) {
                        self.notifications
                            .try_send(notification)
                            .map_err(|_| MqttError::guard_failed())?;
                    }
                }
                Event::Command(Err(_)) | Event::Message(Err(_)) => {
                    return Err(MqttError::guard_failed());
                }
            }
            if operation.is_none()
                && let Some(selection) = desired.as_ref()
            {
                let target = topics(&selection.dids).into_iter().collect::<HashSet<_>>();
                let remove = confirmed.difference(&target).cloned().collect::<Vec<_>>();
                let add = target.difference(&confirmed).cloned().collect::<Vec<_>>();
                if remove.is_empty() && add.is_empty() {
                    let completed = desired.take().unwrap();
                    selected = completed.dids;
                    generation = completed.generation;
                    generation_since = completed.cutoff;
                    let _ = completed.reply.send(Ok(generation));
                } else {
                    let (step, values) = if remove.is_empty() {
                        (SelectionStep::Add(add.clone()), add)
                    } else {
                        (SelectionStep::Remove(remove.clone()), remove)
                    };
                    let mqtt = self.mqtt.clone();
                    let deadline = selection.deadline;
                    let guard = selection.guard.clone();
                    operation = Some(
                        async move {
                            let result = match &step {
                                SelectionStep::Remove(_) => {
                                    mqtt.unsubscribe_guarded(values, deadline, guard).await
                                }
                                SelectionStep::Add(_) => {
                                    mqtt.subscribe_guarded(values, deadline, guard).await
                                }
                            };
                            SelectionCompleted { step, result }
                        }
                        .boxed_local(),
                    );
                }
            }
        }
    }

    pub fn parse(
        topic: &str,
        payload: &[u8],
        retained: bool,
        selected: &[String],
        generation: u64,
    ) -> Result<CloudNotification, CloudNotificationError> {
        if retained {
            return Err(CloudNotificationError);
        }
        let segments = topic.split('/').collect::<Vec<_>>();
        if segments.len() < 4
            || segments[0] != "device"
            || !selected.iter().any(|did| did == segments[1])
        {
            return Err(CloudNotificationError);
        }
        let did = segments[1];
        let value: Value = serde_json::from_slice(payload).map_err(|_| CloudNotificationError)?;
        let outer = value.as_object().ok_or(CloudNotificationError)?;
        match segments.get(2..) {
            Some(["up", "properties_changed", siid, piid]) => {
                let params = params(outer)?;
                validate_optional_did(params, did)?;
                let topic_siid = positive(siid)?;
                let topic_piid = positive(piid)?;
                if integer(params, "siid")? != topic_siid || integer(params, "piid")? != topic_piid
                {
                    return Err(CloudNotificationError);
                }
                Ok(CloudNotification::Property {
                    did: did.into(),
                    siid: topic_siid,
                    piid: topic_piid,
                    value: params.get("value").and_then(wire_value),
                    generation,
                })
            }
            Some(["up", "event_occured", siid, eiid]) => {
                let params = params(outer)?;
                validate_optional_did(params, did)?;
                let topic_siid = positive(siid)?;
                let topic_eiid = positive(eiid)?;
                if integer(params, "siid")? != topic_siid || integer(params, "eiid")? != topic_eiid
                {
                    return Err(CloudNotificationError);
                }
                Ok(CloudNotification::Event {
                    did: did.into(),
                    siid: topic_siid,
                    eiid: topic_eiid,
                    arguments: parse_cloud_arguments(
                        params.get("arguments").ok_or(CloudNotificationError)?,
                    )?,
                    generation,
                })
            }
            Some(["state", _]) if !did.starts_with("blt.") && !did.starts_with("proxy.") => {
                if outer.get("device_id").and_then(Value::as_str) != Some(did) {
                    return Err(CloudNotificationError);
                }
                let online = match outer.get("event").and_then(Value::as_str) {
                    Some("online") => true,
                    Some("offline") => false,
                    _ => return Err(CloudNotificationError),
                };
                Ok(CloudNotification::State {
                    did: did.into(),
                    online,
                    generation,
                })
            }
            _ => Err(CloudNotificationError),
        }
    }
}

struct SelectionCompleted {
    step: SelectionStep,
    result: Result<(), MqttError>,
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
    reply: Sender<Result<u64, MqttError>>,
}

fn topics(dids: &[String]) -> Vec<String> {
    dids.iter()
        .flat_map(|did| {
            let mut topics = vec![
                format!("device/{did}/up/properties_changed/#"),
                format!("device/{did}/up/event_occured/#"),
            ];
            if !did.starts_with("blt.") && !did.starts_with("proxy.") {
                topics.push(format!("device/{did}/state/#"));
            }
            topics
        })
        .collect()
}

fn params(object: &Map<String, Value>) -> Result<&Map<String, Value>, CloudNotificationError> {
    object
        .get("params")
        .and_then(Value::as_object)
        .ok_or(CloudNotificationError)
}

fn validate_optional_did(
    object: &Map<String, Value>,
    did: &str,
) -> Result<(), CloudNotificationError> {
    if object
        .get("did")
        .is_some_and(|value| value.as_str() != Some(did))
    {
        return Err(CloudNotificationError);
    }
    Ok(())
}

fn parse_cloud_arguments(value: &Value) -> Result<EventArguments, CloudNotificationError> {
    let values = value.as_array().ok_or(CloudNotificationError)?;
    if values.len() == 1
        && let Some(positional) = values[0].get("value").and_then(Value::as_array)
    {
        return positional
            .iter()
            .map(|value| wire_value(value).ok_or(CloudNotificationError))
            .collect::<Result<Vec<_>, _>>()
            .map(EventArguments::Positional);
    }
    let mut keyed = Vec::new();
    for value in values {
        let object = value.as_object().ok_or(CloudNotificationError)?;
        let piid = integer(object, "piid")?;
        let value = object
            .get("value")
            .and_then(wire_value)
            .ok_or(CloudNotificationError)?;
        if keyed.iter().any(|(existing, _)| *existing == piid) {
            return Err(CloudNotificationError);
        }
        keyed.push((piid, value));
    }
    Ok(EventArguments::Keyed(keyed))
}

fn wire_value(value: &Value) -> Option<WireValue> {
    match value {
        Value::Bool(value) => Some(WireValue::Boolean(*value)),
        Value::Number(value) if value.is_i64() => value.as_i64().map(WireValue::Integer),
        Value::Number(value) => value
            .as_f64()
            .filter(|value| value.is_finite())
            .map(WireValue::Number),
        Value::String(value) => Some(WireValue::String(value.clone())),
        _ => None,
    }
}

fn positive(value: &str) -> Result<u32, CloudNotificationError> {
    value
        .parse()
        .ok()
        .filter(|value| *value > 0)
        .ok_or(CloudNotificationError)
}

fn integer(object: &Map<String, Value>, key: &str) -> Result<u32, CloudNotificationError> {
    object
        .get(key)
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .filter(|value| *value > 0)
        .ok_or(CloudNotificationError)
}

fn valid_component(value: &str) -> bool {
    !value.is_empty()
        && !value.contains(['/', '+', '#', '\0'])
        && !value.chars().any(char::is_control)
}
