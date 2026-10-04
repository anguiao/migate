use super::super::{CentralError, wire};
use super::{
    CentralEvent, LIMIT, SessionHandle, Timing, Transport,
    request::{Request, Shared},
};
use crate::xiaomi::capability::PropertyBinding;
use event_listener::Event;
use futures_lite::{
    future,
    io::{AsyncRead, AsyncWrite},
};
use mqttbytes::v5::{Packet, Publish};
use std::{
    collections::{HashMap, VecDeque},
    pin::Pin,
    rc::Rc,
    sync::atomic::{AtomicU64, Ordering},
    task::Poll,
    time::Instant,
};

struct Output {
    kind: &'static str,
    bytes: Vec<u8>,
    offset: usize,
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
}

/// One driver owns all TLS I/O and protocol state. Poll it continuously alongside
/// handle requests; callbacks and events execute in wire order on this driver.
pub struct SessionDriver {
    stream: Box<dyn Transport>,
    frames: wire::Frames,
    pub(super) shared: Rc<Shared>,
    pub(super) timing: Timing,
    pub(super) client_id: String,
    pub(super) reply_topic: String,
    pub(super) list_topic: String,
    output: VecDeque<Output>,
    pub(super) pending: HashMap<u32, Request>,
    pub(super) publications: HashMap<u16, Publication>,
    pub(super) subscriptions: HashMap<u16, Subscription>,
    pub(super) incoming: HashMap<u16, Publish>,
    pub(super) property_topics: HashMap<String, (String, PropertyBinding)>,
    pub(super) ready: bool,
    pkid: u16,
    last_sent: Instant,
    pub(super) ping_deadline: Option<Instant>,
    max_packet: usize,
    pub(super) receive_max: usize,
}

impl SessionDriver {
    pub(super) fn new(
        stream: Box<dyn Transport>,
        frames: wire::Frames,
        client_id: &str,
        timing: Timing,
        max_packet: usize,
        receive_max: usize,
    ) -> Result<(SessionHandle, Self), CentralError> {
        static GENERATION: AtomicU64 = AtomicU64::new(1);
        let shared = Rc::new(Shared {
            queue: Default::default(),
            event: Event::new(),
            closed: Default::default(),
            outstanding: Default::default(),
            generation: GENERATION.fetch_add(1, Ordering::Relaxed),
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
            incoming: HashMap::new(),
            property_topics: HashMap::new(),
            ready: false,
            pkid: 0,
            last_sent: now,
            ping_deadline: None,
            max_packet,
            receive_max,
        };
        driver.subscribe_baseline(now)?;
        Ok((SessionHandle { shared, timing }, driver))
    }
    pub async fn run(mut self, mut on_event: impl FnMut(CentralEvent)) -> Result<(), CentralError> {
        let result = self.drive(&mut on_event).await;
        log::debug!(
            "Gateway session stopped: generation={}, error={:?}, queued={}, pending={}, outgoing_qos2={}, incoming_qos2={}, subscriptions={}",
            self.shared.generation,
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
            if self.shared.closed.get() {
                return Ok(());
            }
            self.expire()?;
            // Process one complete frame at a time; callback completion never
            // depends on scheduling a separate request future.
            if let Some(packet) = self.frames.next_packet()? {
                let (kind, pkid) = wire::packet_context(&packet);
                self.packet(packet, on_event).inspect_err(|error| {
                    log::debug!(
                        "Gateway packet failed: generation={}, packet={kind}, packet_id={pkid:?}, error={error}",
                        self.shared.generation
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
        let bytes = wire::encode_packet(&packet)?;
        if bytes.len() > self.max_packet {
            return Err(CentralError::Protocol);
        }
        if self.output.len() >= LIMIT * 4 {
            return Err(CentralError::Protocol);
        }
        self.output.push_back(Output {
            kind: wire::packet_context(&packet).0,
            bytes,
            offset: 0,
            deadline,
            request,
            pkid,
        });
        Ok(())
    }
    fn expire(&mut self) -> Result<(), CentralError> {
        let now = Instant::now();
        if self.ping_deadline.is_some_and(|deadline| now >= deadline) {
            log::debug!(
                "Gateway timed out awaiting PINGRESP: generation={}",
                self.shared.generation
            );
            return Err(CentralError::Timeout);
        }
        if let Some((pkid, publication)) = self
            .publications
            .iter()
            .find(|(_, publication)| now >= publication.deadline)
        {
            log::debug!(
                "Gateway outgoing QoS2 timed out: generation={}, packet_id={pkid}, request_id={}, stage={:?}",
                self.shared.generation,
                publication.mid,
                publication.stage
            );
            return Err(CentralError::Timeout);
        }
        if let Some(output) = self.output.iter().find(|output| {
            now >= output.deadline
                && (output.offset > 0 || output.request.is_none() && output.pkid.is_none())
        }) {
            log::debug!(
                "Gateway output timed out: generation={}, packet={}, packet_id={:?}, bytes_written={}, bytes_total={}",
                self.shared.generation,
                output.kind,
                output.pkid,
                output.offset,
                output.bytes.len()
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
