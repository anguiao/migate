use super::ProtocolError;
use mqttbytes::v5::{PubRelReason, Publish};
use std::collections::HashMap;

/// The first capture of an inbound QoS2 PUBLISH. The context is opaque to this
/// module and is handed back unchanged when the handshake completes.
pub(in crate::xiaomi) struct Received<C> {
    pub(in crate::xiaomi) publish: Publish,
    pub(in crate::xiaomi) context: C,
}

/// Inbound QoS2 handshake state: packet identity, capacity and duplicate
/// content checks. Context is captured only for the first PUBLISH, so a
/// duplicate cannot replace the subscription context of the original receipt.
pub(in crate::xiaomi) struct QoS2<C> {
    received: HashMap<u16, Received<C>>,
    capacity: usize,
}

impl<C> QoS2<C> {
    pub(in crate::xiaomi) fn new(capacity: usize) -> Self {
        Self {
            received: HashMap::new(),
            capacity,
        }
    }

    pub(in crate::xiaomi) fn len(&self) -> usize {
        self.received.len()
    }

    /// Record an inbound QoS2 PUBLISH. A duplicate must repeat the captured
    /// content, and only a fresh packet captures `context`.
    pub(in crate::xiaomi) fn receive(
        &mut self,
        publish: &Publish,
        context: impl FnOnce() -> C,
    ) -> Result<(), ProtocolError> {
        if publish.pkid == 0 {
            return Err(super::invalid("zero QoS2 packet id"));
        }
        if let Some(previous) = self.received.get(&publish.pkid) {
            if !publish.dup
                || previous.publish.topic != publish.topic
                || previous.publish.payload != publish.payload
            {
                return Err(super::invalid("conflicting QoS2 duplicate"));
            }
            return Ok(());
        }
        if self.received.len() >= self.capacity {
            return Err(super::invalid("inbound QoS2 capacity"));
        }
        self.received.insert(
            publish.pkid,
            Received {
                publish: publish.clone(),
                context: context(),
            },
        );
        Ok(())
    }

    /// Complete a PUBREL handshake. The returned capture belongs to the
    /// original PUBLISH; its context validity is checked by the driver.
    pub(in crate::xiaomi) fn release(
        &mut self,
        pkid: u16,
        reason: PubRelReason,
    ) -> Result<Option<Received<C>>, ProtocolError> {
        if pkid == 0 || reason != PubRelReason::Success {
            return Err(super::invalid("invalid PUBREL"));
        }
        Ok(self.received.remove(&pkid))
    }
}

/// Validate the parts of an inbound PUBLISH that both drivers reject before
/// dispatching it.
pub(in crate::xiaomi) fn valid_publish(publish: &Publish) -> bool {
    !publish.topic.is_empty()
        && publish
            .properties
            .as_ref()
            .is_none_or(|properties| properties.topic_alias.is_none())
}
