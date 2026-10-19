use super::super::CentralError;
use super::{
    CentralEvent, LIMIT, SessionHandle, Timing, Transport,
    request::{Request, RequestStage, RequestTimeout, Shared},
};
use crate::xiaomi::capability::PropertyBinding;
use crate::xiaomi::mqtt;
use event_listener::Event;
use futures_lite::{future, io::AsyncRead};
use mqttbytes::v5::Packet;
use std::{
    collections::{BTreeSet, HashMap, VecDeque},
    pin::Pin,
    rc::Rc,
    task::Poll,
    time::Instant,
};

struct Output {
    kind: &'static str,
    frame: mqtt::SendFrame,
    deadline: Instant,
    request: Option<u32>,
    pkid: Option<u16>,
}
#[derive(Debug, Eq, PartialEq)]
pub(super) enum PublicationStage {
    AwaitPubRec,
    AwaitPubComp,
}
pub(super) struct Publication {
    pub(super) mid: u32,
    pub(super) stage: PublicationStage,
    pub(super) deadline: Instant,
}
pub(super) struct Subscription {
    pub(super) request: Option<Request>,
    pub(super) deadline: Instant,
    pub(super) count: usize,
    pub(super) kind: SubscriptionKind,
}

#[derive(Clone)]
pub(super) struct PropertySubscription {
    pub(super) did: String,
    pub(super) binding: PropertyBinding,
    pub(super) generation: u64,
}

#[derive(Clone, Eq, PartialEq)]
pub(super) enum SubscriptionKind {
    Baseline,
    Property {
        did: String,
        binding: PropertyBinding,
    },
    Unsubscribe,
}

/// One driver owns all TLS I/O and protocol state. Poll it continuously alongside
/// handle requests; callbacks and events execute in wire order on this driver.
pub struct SessionDriver {
    stream: Box<dyn Transport>,
    frames: mqtt::Frames,
    pub(super) shared: Rc<Shared>,
    pub(super) timing: Timing,
    pub(super) client_id: String,
    pub(super) reply_topic: String,
    pub(super) list_topic: String,
    output: VecDeque<Output>,
    pub(super) pending: HashMap<u32, Request>,
    pub(super) publications: HashMap<u16, Publication>,
    pub(super) subscriptions: HashMap<u16, Subscription>,
    pub(super) incoming: mqtt::QoS2<Option<u64>>,
    pub(super) property_topics: HashMap<String, PropertySubscription>,
    pub(super) property_generation: u64,
    pub(super) retired_topics: HashMap<String, BTreeSet<String>>,
    pub(super) ready: bool,
    pkid: u16,
    pub(super) keepalive: mqtt::Keepalive,
    max_packet: usize,
    pub(super) receive_max: usize,
}

impl SessionDriver {
    pub(super) fn new(
        stream: Box<dyn Transport>,
        frames: mqtt::Frames,
        client_id: &str,
        timing: Timing,
        max_packet: usize,
        receive_max: usize,
    ) -> Result<(SessionHandle, Self), CentralError> {
        let shared = Rc::new(Shared {
            queue: Default::default(),
            event: Event::new(),
            closed: Default::default(),
            outstanding: Default::default(),
            cancellations: Default::default(),
            property_invalidations: Default::default(),
            cancel_unsent_business: Default::default(),
        });
        let now = Instant::now();
        let mut driver = SessionDriver {
            stream,
            frames,
            shared: shared.clone(),
            timing,
            client_id: client_id.into(),
            reply_topic: format!("{client_id}/reply"),
            list_topic: format!("{client_id}/appMsg/devListChange"),
            output: VecDeque::new(),
            pending: HashMap::new(),
            publications: HashMap::new(),
            subscriptions: HashMap::new(),
            incoming: mqtt::QoS2::new(LIMIT),
            property_topics: HashMap::new(),
            property_generation: 0,
            retired_topics: HashMap::new(),
            ready: false,
            pkid: 0,
            keepalive: mqtt::Keepalive::new(now),
            max_packet,
            receive_max,
        };
        driver.subscribe_baseline(now)?;
        Ok((SessionHandle { shared, timing }, driver))
    }
    pub async fn run(mut self, mut on_event: impl FnMut(CentralEvent)) -> Result<(), CentralError> {
        let result = self.drive(&mut on_event).await;
        log::debug!(
            "Gateway session stopped: error={:?}, queued={}, pending={}, outgoing_qos2={}, incoming_qos2={}, subscriptions={}",
            result.as_ref().err(),
            self.shared.queue.borrow().len(),
            self.pending.len(),
            self.publications.len(),
            self.incoming.len(),
            self.subscriptions.len()
        );
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
            self.drain_cancellations();
            if self.shared.closed.get() {
                return Ok(());
            }
            self.expire()?;
            self.drain_property_invalidations();
            // Process one complete frame at a time; callback completion never
            // depends on scheduling a separate request future.
            if let Some(packet) = self
                .frames
                .next_packet()
                .map_err(|_| CentralError::Protocol)?
            {
                let (kind, pkid) = mqtt::packet_context(&packet);
                self.packet(packet, on_event).inspect_err(|error| {
                    log::debug!(
                        "Gateway packet failed: packet={kind}, packet_id={pkid:?}, error={error}"
                    );
                })?;
                continue;
            }
            if self.ready {
                self.drain_requests()?;
            }
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
    pub(super) fn next_pkid(&mut self) -> Result<u16, CentralError> {
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
    pub(super) fn enqueue(
        &mut self,
        packet: Packet,
        deadline: Instant,
        request: Option<u32>,
        pkid: Option<u16>,
    ) -> Result<(), CentralError> {
        let bytes = mqtt::encode_packet(&packet).map_err(|_| CentralError::Protocol)?;
        if bytes.len() > self.max_packet {
            return Err(CentralError::Protocol);
        }
        if self.output.len() >= LIMIT * 4 {
            return Err(CentralError::Protocol);
        }
        self.output.push_back(Output {
            kind: mqtt::packet_context(&packet).0,
            frame: mqtt::SendFrame::new(bytes),
            deadline,
            request,
            pkid,
        });
        Ok(())
    }
    /// Remove complete unsent frames; partial frames and sent QoS exchanges remain owned here.
    pub(super) fn drain_unsent_business(&mut self) {
        if !self.shared.cancel_unsent_business.replace(false) {
            return;
        }
        let mut retired = {
            let mut queue = self.shared.queue.borrow_mut();
            let (retired, retained): (VecDeque<_>, _) = queue
                .drain(..)
                .partition(|request| request.kind.is_read_or_list());
            *queue = retained;
            retired
        };
        let mids = self
            .pending
            .iter()
            .filter_map(|(mid, request)| {
                (request.kind.is_read_or_list()
                    && request.stage == RequestStage::NotSent
                    && self
                        .output
                        .iter()
                        .any(|output| output.request == Some(*mid) && !output.frame.started()))
                .then_some(*mid)
            })
            .collect::<Vec<_>>();
        self.cancel_unsent_outputs(&mids, &[]);
        for mid in mids {
            retired.push_back(self.pending.remove(&mid).unwrap());
        }
        for request in retired {
            self.finish(request, Err(CentralError::Cancelled));
        }
    }
    pub(super) fn cancel_unsent_outputs(&mut self, requests: &[u32], subscriptions: &[u16]) {
        let mut removed = Vec::new();
        self.output.retain(|output| {
            let cancel = !output.frame.started()
                && (output.request.is_some_and(|mid| requests.contains(&mid))
                    || output
                        .pkid
                        .is_some_and(|pkid| subscriptions.contains(&pkid)));
            if cancel && let Some(pkid) = output.pkid {
                removed.push(pkid);
            }
            !cancel
        });
        for pkid in removed {
            self.publications.remove(&pkid);
            self.subscriptions.remove(&pkid);
        }
    }
    fn expire(&mut self) -> Result<(), CentralError> {
        let now = Instant::now();
        if self.keepalive.ping_expired(now) {
            log::debug!("Gateway timed out awaiting PINGRESP");
            return Err(CentralError::Timeout);
        }
        if let Some((pkid, publication)) = self
            .publications
            .iter()
            .find(|(_, publication)| now >= publication.deadline)
        {
            log::debug!(
                "Gateway outgoing QoS2 timed out: packet_id={pkid}, request_id={}, stage={:?}",
                publication.mid,
                publication.stage
            );
            return Err(CentralError::Timeout);
        }
        if let Some(output) = self.output.iter().find(|output| {
            now >= output.deadline
                && (output.frame.started() || output.request.is_none() && output.pkid.is_none())
        }) {
            log::debug!(
                "Gateway output timed out: packet={}, packet_id={:?}, bytes_written={}, bytes_total={}",
                output.kind,
                output.pkid,
                output.frame.offset(),
                output.frame.bytes().len()
            );
            return Err(CentralError::Timeout);
        }
        let expired: Vec<_> = self
            .pending
            .iter()
            .filter_map(|(id, request)| (now >= request.deadline).then_some(*id))
            .collect();
        for id in expired {
            if let Some(request) = self.pending.remove(&id) {
                let timeout = if request.stage == RequestStage::NotSent {
                    RequestTimeout::Queue
                } else {
                    RequestTimeout::Response
                };
                self.finish_timeout(request, timeout);
            }
        }
        let expired: Vec<_> = self
            .subscriptions
            .iter()
            .filter_map(|(id, subscription)| {
                (now >= subscription.deadline
                    && (subscription.request.is_some()
                        || subscription.kind == SubscriptionKind::Baseline))
                    .then_some(*id)
            })
            .collect();
        for id in expired {
            let subscription = self.subscriptions.get_mut(&id).unwrap();
            if let SubscriptionKind::Property { did, binding } = &subscription.kind {
                self.retired_topics
                    .entry(did.clone())
                    .or_default()
                    .insert(super::request::property_topic("master", did, *binding));
            }
            if let Some(request) = subscription.request.take() {
                self.finish_timeout(request, RequestTimeout::Subscription);
            } else if subscription.kind == SubscriptionKind::Baseline {
                return Err(CentralError::Timeout);
            }
        }
        let expired: Vec<_> = self
            .output
            .iter()
            .filter_map(|output| {
                (now >= output.deadline && !output.frame.started())
                    .then_some(output.pkid)
                    .flatten()
            })
            .collect();
        self.output
            .retain(|output| now < output.deadline || output.frame.started());
        for pkid in expired {
            self.publications.remove(&pkid);
            self.subscriptions.remove(&pkid);
        }
        let expired = {
            let mut queue = self.shared.queue.borrow_mut();
            let (expired, retained): (VecDeque<_>, _) =
                queue.drain(..).partition(|request| now >= request.deadline);
            *queue = retained;
            expired
        };
        for request in expired {
            self.finish_timeout(request, RequestTimeout::Queue);
        }
        Ok(())
    }
    fn keepalive(&mut self) -> Result<(), CentralError> {
        if let Some(deadline) =
            self.keepalive
                .begin_ping(Instant::now(), self.timing.keepalive, self.timing.request)
        {
            self.enqueue(Packet::PingReq, deadline, None, None)?;
        }
        Ok(())
    }
    fn next_deadline(&self) -> Instant {
        let mut deadline = Instant::now() + self.timing.request;
        if let Some(protocol) = self.keepalive.next_deadline(self.timing.keepalive) {
            deadline = deadline.min(protocol);
        }
        for request in self.pending.values() {
            deadline = deadline.min(request.deadline)
        }
        for request in self.shared.queue.borrow().iter() {
            deadline = deadline.min(request.deadline)
        }
        for subscription in self.subscriptions.values() {
            if subscription.request.is_some() || subscription.kind == SubscriptionKind::Baseline {
                deadline = deadline.min(subscription.deadline)
            }
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
        self.drain_cancellations();
        // A timer and a writable socket can wake together. Recheck before
        // touching the stream so an unsent expired request stays unsent.
        if let Err(error) = self.expire() {
            return Poll::Ready(Err(error));
        }
        // A response timeout callback may retire queued business in this poll.
        self.drain_cancellations();
        // Pending socket writes have made no progress: recheck on every poll.
        let guarded = self.output.front().and_then(|output| {
            (!output.frame.started())
                .then_some(output.request)
                .flatten()
        });
        if let Some(mid) = guarded {
            let decision = self
                .pending
                .get_mut(&mid)
                .and_then(|request| request.hooks.as_mut().map(|hooks| (hooks.guard)()));
            let error = match decision {
                Some(super::request::SendDecision::Withdraw) => Some(CentralError::Withdrawn),
                Some(super::request::SendDecision::Cancel) => Some(CentralError::Cancelled),
                _ => None,
            };
            if let Some(error) = error {
                self.cancel_unsent_outputs(&[mid], &[]);
                if let Some(request) = self.pending.remove(&mid) {
                    self.finish(request, Err(error));
                }
                return Poll::Ready(Ok(()));
            }
        }
        let mut progressed = false;
        let status = match self.output.front_mut() {
            Some(output) => Some((
                output.frame.poll_io(cx, &mut self.stream),
                output.request,
                output.pkid,
            )),
            None => None,
        };
        match status {
            None => {}
            Some((Err(_), _, _)) => return Poll::Ready(Err(CentralError::Io)),
            Some((Ok(mqtt::SendStatus::Pending), _, _)) => {}
            Some((Ok(mqtt::SendStatus::Wrote { complete, .. }), request, pkid)) => {
                self.mark_sent(request, pkid);
                progressed = true;
                if complete && let Err(error) = self.flush_front(cx) {
                    return Poll::Ready(Err(error));
                }
            }
            Some((Ok(mqtt::SendStatus::Flushed), _, _)) => {
                self.output.pop_front();
                self.keepalive.sent(Instant::now());
                progressed = true;
            }
        }
        let mut bytes = [0; 4096];
        match Pin::new(&mut self.stream).poll_read(cx, &mut bytes) {
            Poll::Ready(Ok(0)) => return Poll::Ready(Err(CentralError::Disconnected)),
            Poll::Ready(Ok(len)) => {
                if self.frames.push(&bytes[..len]).is_err() {
                    return Poll::Ready(Err(CentralError::Protocol));
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
    /// Continue a fully written frame with its flush in the same poll, after
    /// the send progress was recorded.
    fn flush_front(&mut self, cx: &mut std::task::Context<'_>) -> Result<(), CentralError> {
        let flushed = match self.output.front_mut() {
            Some(output) => output.frame.poll_flush(cx, &mut self.stream),
            None => return Ok(()),
        };
        match flushed {
            Ok(true) => {
                self.output.pop_front();
                self.keepalive.sent(Instant::now());
                Ok(())
            }
            Ok(false) => Ok(()),
            Err(_) => Err(CentralError::Io),
        }
    }

    /// Record that request bytes reached the socket. Called in the same poll as
    /// the accepted write so hooks observe the first byte immediately.
    fn mark_sent(&mut self, request: Option<u32>, pkid: Option<u16>) {
        if let Some(mid) = request
            && let Some(request) = self.pending.get_mut(&mid)
        {
            request.stage = RequestStage::MayHaveBeenSent;
            if let Some(hooks) = request.hooks.take() {
                (hooks.first_send)();
            }
        } else if let Some(pkid) = pkid
            && let Some(request) = self
                .subscriptions
                .get_mut(&pkid)
                .and_then(|subscription| subscription.request.as_mut())
        {
            request.stage = RequestStage::MayHaveBeenSent;
        }
    }
}
impl Drop for SessionDriver {
    fn drop(&mut self) {
        self.finish_all(CentralError::Cancelled);
    }
}

#[cfg(test)]
mod tests;
