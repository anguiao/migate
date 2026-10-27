use super::super::CentralError;
use super::{
    driver::{Publication, PublicationStage, SessionDriver, Subscription, SubscriptionKind},
    request::{Request, RequestKind, RequestTimeout, ResultValue, next_mips_id},
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
                let mut subscribe = Subscribe::new(binding.topic("master", did), QoS::ExactlyOnce);
                subscribe.pkid = pkid;
                let deadline = request.deadline;
                let kind = SubscriptionKind::Notification {
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
            if let RequestKind::Unsubscribe { did, scope } = &request.kind {
                let mut filters = Vec::new();
                if let Some(retired) = self.retired_topics.get_mut(did) {
                    retired.retain(|binding| {
                        if scope.includes(*binding) {
                            filters.push(binding.topic("master", did));
                            false
                        } else {
                            true
                        }
                    });
                }
                filters.sort_unstable();
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
            let mid = next_mips_id();
            let mut publish = request.kind.publication(mid, &self.reply_topic);
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

    pub(super) fn drain_notification_invalidations(&mut self) {
        let invalidations =
            std::mem::take(&mut *self.shared.notification_invalidations.borrow_mut());
        for notification in self.notification_topics.values_mut() {
            if invalidations.contains(&(notification.did.clone(), notification.binding)) {
                self.notification_generation = self.notification_generation.wrapping_add(1);
                notification.generation = self.notification_generation;
            }
        }
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
            self.notification_topics.retain(|_, notification| {
                if notification.did == did && cancellation.scope.includes(notification.binding) {
                    topics.insert(notification.binding);
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
                .filter_map(|(pkid, subscription)| {
                    let matching = match &subscription.kind {
                        SubscriptionKind::Notification {
                            did: subscribed,
                            binding,
                        } => subscribed == &did && cancellation.scope.includes(*binding),
                        _ => false,
                    };
                    matching.then_some(*pkid)
                })
                .collect::<Vec<_>>();
            for pkid in &subscriptions {
                let subscription = self.subscriptions.get_mut(pkid).unwrap();
                if let SubscriptionKind::Notification { did, binding } = &subscription.kind {
                    self.retired_topics
                        .entry(did.clone())
                        .or_default()
                        .insert(*binding);
                }
                if let Some(request) = subscription.request.take() {
                    self.finish(request, Err(CentralError::Cancelled));
                }
            }
            self.cancel_unsent_outputs(&pending, &subscriptions);
        }
    }
}
