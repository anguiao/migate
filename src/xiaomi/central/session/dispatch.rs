use super::super::CentralError;
use super::{
    driver::{Publication, PublicationStage, SessionDriver, Subscription, SubscriptionKind},
    request::{Request, RequestKind, RequestTimeout, ResultValue, next_mips_id, property_topic},
};
use mqttbytes::{
    QoS,
    v5::{Packet, Subscribe, Unsubscribe},
};
use std::time::Instant;

impl SessionDriver {
    pub(super) fn finish(&self, request: Request, result: ResultValue) {
        self.finish_outcome(request, result, None);
    }

    pub(super) fn finish_timeout(&self, request: Request, timeout: RequestTimeout) {
        self.finish_outcome(request, Err(CentralError::Timeout), Some(timeout));
    }

    fn finish_outcome(
        &self,
        request: Request,
        result: ResultValue,
        timeout: Option<RequestTimeout>,
    ) {
        log::debug!(
            "Gateway request completed: operation={}, did={}, request_id={:?}, stage={:?}, timeout={:?}, error={:?}",
            request.kind.operation(),
            request.kind.did(),
            request.id,
            request.stage,
            timeout,
            result.as_ref().err()
        );
        self.shared
            .outstanding
            .set(self.shared.outstanding.get() - 1);
        (request.complete)(result, request.stage, timeout);
    }
    pub(super) fn finish_all(&mut self, error: CentralError) {
        self.shared.closed.set(true);
        let mut requests: Vec<_> = self.shared.queue.borrow_mut().drain(..).collect();
        for cancelled in self.shared.cancellations.borrow_mut().drain(..) {
            requests.extend(cancelled.queued);
        }
        requests.extend(self.pending.drain().map(|(_, request)| request));
        requests.extend(
            self.subscriptions
                .drain()
                .filter_map(|(_, subscription)| subscription.request),
        );
        for request in requests {
            let timeout = (error == CentralError::Timeout).then_some(RequestTimeout::Transport);
            self.finish_outcome(request, Err(error.clone()), timeout);
        }
        self.shared.event.notify(usize::MAX);
    }
    pub(super) fn drain_requests(&mut self) -> Result<(), CentralError> {
        loop {
            let request = {
                let mut queue = self.shared.queue.borrow_mut();
                if self.publications.len() < self.receive_max {
                    queue.pop_front()
                } else {
                    let index = queue.iter().position(|request| {
                        matches!(
                            request.kind,
                            RequestKind::Subscribe { .. } | RequestKind::Unsubscribe { .. }
                        )
                    });
                    index.and_then(|index| queue.remove(index))
                }
            };
            let Some(mut request) = request else { break };
            if Instant::now() >= request.deadline {
                self.finish_timeout(request, RequestTimeout::Queue);
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
                let kind = SubscriptionKind::Property {
                    did: did.clone(),
                    binding: *binding,
                };
                self.subscriptions.insert(
                    pkid,
                    Subscription {
                        request: Some(request),
                        deadline,
                        count: 1,
                        kind,
                    },
                );
                self.enqueue(Packet::Subscribe(subscribe), deadline, None, Some(pkid))?;
                continue;
            }
            if let RequestKind::Unsubscribe { did } = &request.kind {
                let filters = self
                    .retired_topics
                    .remove(did)
                    .unwrap_or_default()
                    .into_iter()
                    .collect::<Vec<_>>();
                if filters.is_empty() {
                    self.finish(request, Ok(super::request::Response::Unit));
                    continue;
                }
                let packet = Unsubscribe {
                    pkid,
                    filters,
                    properties: None,
                };
                let deadline = request.deadline;
                self.subscriptions.insert(
                    pkid,
                    Subscription {
                        request: Some(request),
                        deadline,
                        count: packet.filters.len(),
                        kind: SubscriptionKind::Unsubscribe,
                    },
                );
                self.enqueue(Packet::Unsubscribe(packet), deadline, None, Some(pkid))?;
                continue;
            }
            let mid = match next_mips_id() {
                Ok(mid) => mid,
                Err(error) => {
                    self.finish(request, Err(error.clone()));
                    return Err(error);
                }
            };
            let mut publish = match request.kind.publication(mid, &self.reply_topic) {
                Ok(publish) => publish,
                Err(error) => {
                    self.finish(request, Err(error.clone()));
                    return Err(error);
                }
            };
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
                    stage: PublicationStage::AwaitPubRec,
                    deadline: deadline.max(Instant::now() + self.timing.request),
                },
            );
            self.enqueue(Packet::Publish(publish), deadline, Some(mid), Some(pkid))?;
        }
        Ok(())
    }

    pub(super) fn drain_cancellations(&mut self) {
        self.drain_unsent_business();
        loop {
            let cancellation = self.shared.cancellations.borrow_mut().pop_front();
            let Some(cancellation) = cancellation else {
                break;
            };
            for request in cancellation.queued {
                self.finish(request, Err(CentralError::Cancelled));
            }
            let did = cancellation.did;
            let topics = self.retired_topics.entry(did.clone()).or_default();
            self.property_topics.retain(|_, property| {
                if property.did == did {
                    topics.insert(property_topic("master", &property.did, property.binding));
                    false
                } else {
                    true
                }
            });
            let pending = self
                .pending
                .iter()
                .filter_map(|(mid, request)| {
                    request
                        .kind
                        .cancelled_for(&did, cancellation.scope)
                        .then_some(*mid)
                })
                .collect::<Vec<_>>();
            for mid in &pending {
                if let Some(request) = self.pending.remove(mid) {
                    self.finish(request, Err(CentralError::Cancelled));
                }
            }
            let subscriptions = self
                .subscriptions
                .iter()
                .filter_map(|(pkid, subscription)| matches!(&subscription.kind, SubscriptionKind::Property { did: subscribed_did, .. } if *subscribed_did == did).then_some(*pkid))
                .collect::<Vec<_>>();
            for pkid in &subscriptions {
                let subscription = self.subscriptions.get_mut(pkid).unwrap();
                if let SubscriptionKind::Property { did, binding } = &subscription.kind {
                    self.retired_topics
                        .entry(did.clone())
                        .or_default()
                        .insert(property_topic("master", did, *binding));
                }
                if let Some(request) = subscription.request.take() {
                    self.finish(request, Err(CentralError::Cancelled));
                }
            }
            self.cancel_unsent_outputs(&pending, &subscriptions);
        }
    }
}
