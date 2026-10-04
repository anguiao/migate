use super::super::CentralError;
use super::{
    driver::{Publication, PublicationStage, SessionDriver, Subscription},
    request::{Request, RequestKind, ResultValue, next_mips_id, property_topic},
};
use mqttbytes::{
    QoS,
    v5::{Packet, Subscribe},
};
use std::time::Instant;

impl SessionDriver {
    pub(super) fn finish(&self, request: Request, result: ResultValue) {
        log::debug!(
            "Gateway request completed: generation={}, operation={}, did={}, request_id={:?}, error={:?}",
            self.shared.generation,
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
    pub(super) fn finish_all(&mut self, error: CentralError) {
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
    pub(super) fn can_drain(&self) -> bool {
        let queue = self.shared.queue.borrow();
        !queue.is_empty()
            && (self.publications.len() < self.receive_max
                || queue
                    .iter()
                    .any(|request| matches!(request.kind, RequestKind::Subscribe { .. })))
    }
    pub(super) fn drain_requests(&mut self) -> Result<(), CentralError> {
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
}
