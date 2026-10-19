use std::time::{Duration, Instant};

/// MQTT keepalive state: the most recent completed send, an outstanding PING
/// wait and the next protocol deadline. Drivers merge this with business
/// deadlines and own connection recovery.
pub(in crate::xiaomi) struct Keepalive {
    last_sent: Instant,
    ping_deadline: Option<Instant>,
}

impl Keepalive {
    pub(in crate::xiaomi) fn new(now: Instant) -> Self {
        Self {
            last_sent: now,
            ping_deadline: None,
        }
    }

    /// Record a frame that was fully written and flushed.
    pub(in crate::xiaomi) fn sent(&mut self, now: Instant) {
        self.last_sent = now;
    }

    /// A PINGRESP completed the outstanding wait.
    pub(in crate::xiaomi) fn pong(&mut self) {
        self.ping_deadline = None;
    }

    /// Start a PING wait when the keepalive interval elapsed without one,
    /// returning the deadline for the PINGREQ frame.
    pub(in crate::xiaomi) fn begin_ping(
        &mut self,
        now: Instant,
        interval: Option<Duration>,
        response_timeout: Duration,
    ) -> Option<Instant> {
        if self.ping_deadline.is_none()
            && let Some(interval) = interval
            && now >= self.last_sent + interval
        {
            let deadline = now + response_timeout;
            self.ping_deadline = Some(deadline);
            return Some(deadline);
        }
        None
    }

    /// True when an outstanding PING wait elapsed without a response.
    pub(in crate::xiaomi) fn ping_expired(&self, now: Instant) -> bool {
        self.ping_deadline.is_some_and(|deadline| now >= deadline)
    }

    /// Next protocol deadline: an outstanding PING wait, otherwise the next
    /// keepalive interval after the most recent completed send.
    pub(in crate::xiaomi) fn next_deadline(&self, interval: Option<Duration>) -> Option<Instant> {
        self.ping_deadline
            .or_else(|| interval.map(|interval| self.last_sent + interval))
    }
}
