use super::{
    LIMIT, NotificationError, NotificationEvent, NotificationHandle, Timing, Transport,
    request::{Request, RequestKind, Shared, Target},
};
use crate::xiaomi::{cloud::response::Identifier, mqtt};
use futures_lite::{future, io::AsyncRead};
use mqttbytes::{QoS, v5::*};
use serde_json::Value;
use std::{
    collections::{HashMap, VecDeque, hash_map::Entry},
    pin::Pin,
    rc::Rc,
    task::Poll,
    time::Instant,
};

struct Output {
    frame: mqtt::SendFrame,
    deadline: Instant,
    pkid: Option<u16>,
}
struct Pending {
    request: Option<Request>,
    unsubscribe: bool,
    count: usize,
}
/// One ready registration captured when a QoS2 receipt arrives.
#[derive(Clone)]
struct Delivery {
    filter: String,
    generation: u64,
    delivery_generation: u64,
    target: Target,
}
/// Owns MQTT I/O. Subscription completions and notifications run on this driver
/// in wire order while handle operations are polled alongside it.
pub struct NotificationDriver {
    stream: Box<dyn Transport>,
    frames: mqtt::Frames,
    shared: Rc<Shared>,
    uid: String,
    timing: Timing,
    output: VecDeque<Output>,
    pending: HashMap<u16, Pending>,
    incoming: mqtt::QoS2<Option<Delivery>>,
    pkid: u16,
    max_packet: usize,
    keepalive: mqtt::Keepalive,
}

impl NotificationDriver {
    pub(super) fn new(
        stream: Box<dyn Transport>,
        frames: mqtt::Frames,
        uid: String,
        timing: Timing,
        max_packet: usize,
    ) -> (NotificationHandle, Self) {
        let shared = Shared::new();
        (
            NotificationHandle {
                shared: shared.clone(),
                timing,
            },
            Self {
                stream,
                frames,
                shared,
                uid,
                timing,
                output: VecDeque::new(),
                pending: HashMap::new(),
                incoming: mqtt::QoS2::new(LIMIT),
                pkid: 0,
                max_packet,
                keepalive: mqtt::Keepalive::new(Instant::now()),
            },
        )
    }

    pub async fn run(
        mut self,
        mut on_event: impl FnMut(NotificationEvent),
    ) -> Result<(), NotificationError> {
        let result = self.drive(&mut on_event).await;
        self.finish_all(
            result
                .as_ref()
                .err()
                .cloned()
                .unwrap_or(NotificationError::Cancelled),
        );
        result
    }

    async fn drive(
        &mut self,
        on_event: &mut impl FnMut(NotificationEvent),
    ) -> Result<(), NotificationError> {
        loop {
            self.retire_pending();
            if self.shared.closed.get() {
                return Ok(());
            }
            self.expire()?;
            if let Some(packet) = self
                .frames
                .next_packet()
                .map_err(|_| NotificationError::Protocol)?
            {
                self.packet(packet, on_event)?;
                continue;
            }
            self.drain_requests()?;
            self.keepalive()?;
            let shared = self.shared.clone();
            let listener = shared.event.listen();
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

    fn finish_all(&mut self, error: NotificationError) {
        self.shared.closed.set(true);
        let mut requests: Vec<_> = self.shared.queue.borrow_mut().drain(..).collect();
        requests.extend(
            self.pending
                .drain()
                .filter_map(|(_, pending)| pending.request),
        );
        for request in requests {
            self.shared.finish(&request, Err(error.clone()));
        }
        self.shared.registrations.borrow_mut().clear();
        self.shared.event.notify(usize::MAX);
    }

    fn retire_pending(&mut self) {
        for pending in self.pending.values_mut() {
            let retired = pending.request.as_ref().is_some_and(|request| {
                matches!(&request.kind, RequestKind::Subscribe { topic, generation }
                    if !self.shared.current(topic, *generation))
            });
            if retired {
                let request = pending.request.take().unwrap();
                self.shared
                    .finish(&request, Err(NotificationError::Cancelled));
            }
        }
        let unsent: Vec<_> = self
            .output
            .iter()
            .filter_map(|output| {
                output.pkid.filter(|pkid| {
                    !output.frame.started()
                        && self
                            .pending
                            .get(pkid)
                            .is_some_and(|pending| pending.request.is_none())
                })
            })
            .collect();
        self.output
            .retain(|output| !output.pkid.is_some_and(|pkid| unsent.contains(&pkid)));
        for pkid in unsent {
            self.pending.remove(&pkid);
        }
    }

    fn next_pkid(&mut self) -> Result<u16, NotificationError> {
        if self.pending.len() >= LIMIT {
            // Timed-out or cancelled exchanges retain their packet IDs until
            // acknowledged. A full table must retire this connection; returning
            // QueueFull alone would leave every later subscription stuck forever.
            return Err(NotificationError::Timeout);
        }
        loop {
            self.pkid = self.pkid.wrapping_add(1).max(1);
            if !self.pending.contains_key(&self.pkid) {
                return Ok(self.pkid);
            }
        }
    }

    fn drain_requests(&mut self) -> Result<(), NotificationError> {
        loop {
            let request = self.shared.queue.borrow_mut().pop_front();
            let Some(request) = request else {
                break;
            };
            if Instant::now() >= request.deadline {
                self.shared
                    .finish(&request, Err(NotificationError::Timeout));
                continue;
            }
            if matches!(&request.kind,RequestKind::Subscribe { topic,generation } if !self.shared.current(topic,*generation))
            {
                self.shared
                    .finish(&request, Err(NotificationError::Cancelled));
                continue;
            }
            let pkid = match self.next_pkid() {
                Ok(pkid) => pkid,
                Err(error) => {
                    self.shared.finish(&request, Err(error.clone()));
                    return Err(error);
                }
            };
            let (packet, unsubscribe, count) = match &request.kind {
                RequestKind::Subscribe { topic, .. } => {
                    let mut sub = Subscribe::new(topic.clone(), QoS::ExactlyOnce);
                    sub.pkid = pkid;
                    (Packet::Subscribe(sub), false, 1)
                }
                RequestKind::Unsubscribe { filters } => (
                    Packet::Unsubscribe(Unsubscribe {
                        pkid,
                        filters: filters.clone(),
                        properties: None,
                    }),
                    true,
                    filters.len(),
                ),
            };
            let deadline = request.deadline;
            self.pending.insert(
                pkid,
                Pending {
                    request: Some(request),
                    unsubscribe,
                    count,
                },
            );
            self.enqueue(packet, deadline, Some(pkid))?;
        }
        Ok(())
    }

    fn enqueue(
        &mut self,
        packet: Packet,
        deadline: Instant,
        pkid: Option<u16>,
    ) -> Result<(), NotificationError> {
        let bytes = mqtt::encode_packet(&packet).map_err(|_| NotificationError::Protocol)?;
        if bytes.len() > self.max_packet || self.output.len() >= LIMIT * 4 {
            return Err(NotificationError::Protocol);
        }
        self.output.push_back(Output {
            frame: mqtt::SendFrame::new(bytes),
            deadline,
            pkid,
        });
        Ok(())
    }

    fn expire(&mut self) -> Result<(), NotificationError> {
        let now = Instant::now();
        if self.keepalive.ping_expired(now)
            || self.output.iter().any(|output| {
                now >= output.deadline && (output.frame.started() || output.pkid.is_none())
            })
        {
            return Err(NotificationError::Timeout);
        }
        for pending in self.pending.values_mut() {
            if pending
                .request
                .as_ref()
                .is_some_and(|request| now >= request.deadline)
            {
                let request = pending.request.take().unwrap();
                self.shared
                    .finish(&request, Err(NotificationError::Timeout));
            }
        }
        let unsent: Vec<_> = self
            .output
            .iter()
            .filter_map(|output| {
                (!output.frame.started() && now >= output.deadline)
                    .then_some(output.pkid)
                    .flatten()
            })
            .collect();
        self.output
            .retain(|output| output.frame.started() || now < output.deadline);
        for pkid in unsent {
            self.pending.remove(&pkid);
        }
        let expired = {
            let mut queue = self.shared.queue.borrow_mut();
            let (expired, retained): (VecDeque<_>, _) =
                queue.drain(..).partition(|request| now >= request.deadline);
            *queue = retained;
            expired
        };
        for request in expired {
            self.shared
                .finish(&request, Err(NotificationError::Timeout));
        }
        Ok(())
    }

    fn keepalive(&mut self) -> Result<(), NotificationError> {
        if let Some(deadline) =
            self.keepalive
                .begin_ping(Instant::now(), self.timing.keepalive, self.timing.request)
        {
            self.enqueue(Packet::PingReq, deadline, None)?;
        }
        Ok(())
    }

    fn next_deadline(&self) -> Instant {
        let mut deadline = Instant::now() + self.timing.request;
        if let Some(protocol) = self.keepalive.next_deadline(self.timing.keepalive) {
            deadline = deadline.min(protocol);
        }
        for pending in self.pending.values() {
            if let Some(request) = &pending.request {
                deadline = deadline.min(request.deadline);
            }
        }
        for request in self.shared.queue.borrow().iter() {
            deadline = deadline.min(request.deadline);
        }
        for output in &self.output {
            deadline = deadline.min(output.deadline);
        }
        deadline
    }

    fn poll_io(&mut self, cx: &mut std::task::Context<'_>) -> Poll<Result<(), NotificationError>> {
        if let Err(error) = self.expire() {
            return Poll::Ready(Err(error));
        }
        let mut progressed = false;
        loop {
            let status = match self.output.front_mut() {
                Some(output) => Some(output.frame.poll_io(cx, &mut self.stream)),
                None => None,
            };
            match status {
                None => break,
                Some(Err(_)) => return Poll::Ready(Err(NotificationError::Io)),
                Some(Ok(mqtt::SendStatus::Pending)) => break,
                Some(Ok(mqtt::SendStatus::Wrote { complete, .. })) => {
                    progressed = true;
                    if !complete {
                        break;
                    }
                    // Flush the completed frame in this same poll.
                }
                Some(Ok(mqtt::SendStatus::Flushed)) => {
                    self.output.pop_front();
                    self.keepalive.sent(Instant::now());
                    progressed = true;
                    break;
                }
            }
        }
        let mut bytes = [0; 4096];
        match Pin::new(&mut self.stream).poll_read(cx, &mut bytes) {
            Poll::Ready(Ok(0)) => return Poll::Ready(Err(NotificationError::Disconnected)),
            Poll::Ready(Ok(len)) => {
                if let Err(_error) = self.frames.push(&bytes[..len]) {
                    return Poll::Ready(Err(NotificationError::Protocol));
                }
                progressed = true;
            }
            Poll::Ready(Err(_)) => return Poll::Ready(Err(NotificationError::Io)),
            Poll::Pending => {}
        }
        if progressed {
            Poll::Ready(Ok(()))
        } else {
            Poll::Pending
        }
    }

    fn packet(
        &mut self,
        packet: Packet,
        on_event: &mut impl FnMut(NotificationEvent),
    ) -> Result<(), NotificationError> {
        match packet {
            Packet::SubAck(ack) => {
                if ack.pkid == 0 {
                    return Err(NotificationError::Protocol);
                }
                let Entry::Occupied(pending) = self.pending.entry(ack.pkid) else {
                    return Ok(());
                };
                if pending.get().unsubscribe {
                    return Err(NotificationError::Protocol);
                }
                let pending = pending.remove();
                if let Some(request) = pending.request {
                    let error = if ack.return_codes.len() != pending.count {
                        Some(NotificationError::Protocol)
                    } else {
                        ack.return_codes
                            .iter()
                            .find(|code| **code as u8 >= 128)
                            .map(|code| reject(*code as u8))
                    };
                    if let Some(error) = error {
                        self.shared.finish(&request, Err(error));
                    } else if Instant::now() >= request.deadline {
                        self.shared
                            .finish(&request, Err(NotificationError::Timeout));
                    } else if let RequestKind::Subscribe { topic, generation } = &request.kind {
                        if self.shared.current(topic, *generation) {
                            self.shared
                                .registrations
                                .borrow_mut()
                                .get_mut(topic)
                                .unwrap()
                                .ready = true;
                            self.shared.finish(&request, Ok(()));
                        } else {
                            self.shared
                                .finish(&request, Err(NotificationError::Cancelled));
                        }
                    }
                }
            }
            Packet::UnsubAck(ack) => {
                if ack.pkid == 0 {
                    return Err(NotificationError::Protocol);
                }
                let Entry::Occupied(pending) = self.pending.entry(ack.pkid) else {
                    return Ok(());
                };
                if !pending.get().unsubscribe {
                    return Err(NotificationError::Protocol);
                }
                let pending = pending.remove();
                if let Some(request) = pending.request {
                    let result = if ack.reasons.len() != pending.count {
                        Err(NotificationError::Protocol)
                    } else if let Some(reason) =
                        ack.reasons.iter().find(|reason| **reason as u8 >= 128)
                    {
                        Err(reject(*reason as u8))
                    } else if Instant::now() >= request.deadline {
                        Err(NotificationError::Timeout)
                    } else {
                        Ok(())
                    };
                    self.shared.finish(&request, result);
                }
            }
            Packet::Publish(publish) => {
                if !mqtt::valid_publish(&publish) {
                    return Err(NotificationError::Protocol);
                }
                match publish.qos {
                    QoS::AtMostOnce => self.message(publish, on_event),
                    QoS::AtLeastOnce => {
                        if publish.pkid == 0 {
                            return Err(NotificationError::Protocol);
                        }
                        self.enqueue(
                            Packet::PubAck(PubAck::new(publish.pkid)),
                            Instant::now() + self.timing.request,
                            None,
                        )?;
                        self.message(publish, on_event);
                    }
                    QoS::ExactlyOnce => {
                        let shared = &self.shared;
                        self.incoming
                            .receive(&publish, || delivery(shared, &publish.topic))
                            .map_err(|_| NotificationError::Protocol)?;
                        self.enqueue(
                            Packet::PubRec(PubRec::new(publish.pkid)),
                            Instant::now() + self.timing.request,
                            None,
                        )?;
                    }
                }
            }
            Packet::PubRel(ack) => {
                let received = self
                    .incoming
                    .release(ack.pkid, ack.reason)
                    .map_err(|_| NotificationError::Protocol)?;
                if let Some(received) = received {
                    // Dispatch only the registration captured at PUBLISH, and
                    // only while it is still current; otherwise drop. Never
                    // resolve a later registration for this receipt.
                    if let Some(delivery) = received.context.filter(|delivery| {
                        self.shared.current_delivery(
                            &delivery.filter,
                            delivery.generation,
                            delivery.delivery_generation,
                        )
                    }) {
                        self.message_target(received.publish, delivery.target, on_event);
                    }
                }
                self.enqueue(
                    Packet::PubComp(PubComp::new(ack.pkid)),
                    Instant::now() + self.timing.request,
                    None,
                )?;
            }
            Packet::PingResp => self.keepalive.pong(),
            Packet::Disconnect(packet) => {
                return Err(
                    if packet.reason_code == DisconnectReasonCode::NormalDisconnection {
                        NotificationError::Disconnected
                    } else {
                        reject(packet.reason_code as u8)
                    },
                );
            }
            _ => return Err(NotificationError::Protocol),
        }
        Ok(())
    }

    fn message(&self, publish: Publish, on_event: &mut impl FnMut(NotificationEvent)) {
        // QoS0/1 dispatch through the current ready registration.
        let Some(delivery) = delivery(&self.shared, &publish.topic) else {
            return;
        };
        self.message_target(publish, delivery.target, on_event);
    }

    fn message_target(
        &self,
        publish: Publish,
        target: Target,
        on_event: &mut impl FnMut(NotificationEvent),
    ) {
        if let Err(error) = self.dispatch_target(publish, target, on_event) {
            log::warn!("Ignoring invalid cloud notification: {error}");
        }
    }

    fn dispatch_target(
        &self,
        publish: Publish,
        target: Target,
        on_event: &mut impl FnMut(NotificationEvent),
    ) -> Result<(), NotificationError> {
        let payload: Value =
            serde_json::from_slice(&publish.payload).map_err(|_| NotificationError::Protocol)?;
        self.validate_uid(&payload)?;
        match target {
            Target::Event { did, binding } => {
                if publish.retain {
                    return Ok(());
                }
                let params = payload
                    .get("params")
                    .and_then(Value::as_object)
                    .ok_or(NotificationError::Protocol)?;
                for candidate in [payload.get("did"), params.get("did")] {
                    if let Some(candidate) = candidate
                        && candidate.as_str() != Some(&did)
                    {
                        return Err(NotificationError::Protocol);
                    }
                }
                if params.get("siid").and_then(Value::as_u64) != Some(u64::from(binding.siid))
                    || params.get("eiid").and_then(Value::as_u64) != Some(u64::from(binding.eiid))
                {
                    return Err(NotificationError::Protocol);
                }
                let arguments = params
                    .get("arguments")
                    .and_then(Value::as_array)
                    .ok_or(NotificationError::Protocol)?
                    .clone();
                on_event(NotificationEvent::EventOccurred {
                    did,
                    binding,
                    arguments,
                    source_time: payload.get("timestamp").and_then(Value::as_i64),
                });
            }
            Target::Property { did, binding } => {
                let params = payload
                    .get("params")
                    .and_then(Value::as_object)
                    .ok_or(NotificationError::Protocol)?;
                for candidate in [payload.get("did"), params.get("did")] {
                    if let Some(candidate) = candidate
                        && candidate.as_str() != Some(&did)
                    {
                        return Err(NotificationError::Protocol);
                    }
                }
                if params.get("siid").and_then(Value::as_u64) != Some(u64::from(binding.siid))
                    || params.get("piid").and_then(Value::as_u64) != Some(u64::from(binding.piid))
                {
                    return Err(NotificationError::Protocol);
                }
                let value = params
                    .get("value")
                    .filter(|value| crate::xiaomi::property::valid_value(value))
                    .ok_or(NotificationError::Protocol)?;
                let update_time = crate::xiaomi::property::update_time_map(params);
                let event = if publish.retain {
                    NotificationEvent::PropertySnapshot {
                        did,
                        binding,
                        value: value.clone(),
                        update_time,
                    }
                } else {
                    NotificationEvent::PropertyValue {
                        did,
                        binding,
                        value: value.clone(),
                        update_time,
                    }
                };
                on_event(event);
            }
            Target::Online { did } => {
                if payload.get("device_id").and_then(Value::as_str) != Some(&did) {
                    return Err(NotificationError::Protocol);
                }
                let online = match payload.get("event").and_then(Value::as_str) {
                    Some("online") => true,
                    Some("offline") => false,
                    _ => return Err(NotificationError::Protocol),
                };
                on_event(NotificationEvent::Online { did, online });
            }
        }
        Ok(())
    }

    fn validate_uid(&self, payload: &Value) -> Result<(), NotificationError> {
        if let Some(uid) = payload.get("uid") {
            let uid: Identifier =
                serde_json::from_value(uid.clone()).map_err(|_| NotificationError::Protocol)?;
            if uid.into_string() != self.uid {
                return Err(NotificationError::Protocol);
            }
        }
        Ok(())
    }
}

/// Capture the ready exact property or wildcard online registration.
fn delivery(shared: &Shared, topic: &str) -> Option<Delivery> {
    let registrations = shared.registrations.borrow();
    let ready = |filter: &str| {
        registrations
            .get(filter)
            .filter(|registration| registration.ready)
            .map(|registration| Delivery {
                filter: filter.to_string(),
                generation: registration.generation,
                delivery_generation: registration.delivery_generation,
                target: registration.target.clone(),
            })
    };
    if let Some(exact) = ready(topic) {
        return Some(exact);
    }
    if let Some((did, suffix)) = topic
        .strip_prefix("device/")
        .and_then(|rest| rest.split_once('/'))
        && (suffix == "state" || suffix.starts_with("state/"))
    {
        return ready(&format!("device/{did}/state/#"));
    }
    None
}

fn reject(code: u8) -> NotificationError {
    if matches!(code, 0x86 | 0x87) {
        NotificationError::Unauthorized
    } else {
        NotificationError::Rejected(code)
    }
}

impl Drop for NotificationDriver {
    fn drop(&mut self) {
        self.finish_all(NotificationError::Cancelled);
    }
}
