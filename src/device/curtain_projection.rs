use super::{CurtainMotion, CurtainSnapshot, CurtainValue, curtain_command::CurtainToken};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Waiting,
    Stopped,
}

/// Tracks only explicit stop projections; native values stay intact.
#[derive(Default)]
pub(super) struct CurtainProjection {
    queued: Vec<CurtainToken>,
    flight: Option<(CurtainToken, Phase)>,
    accepted_waiting: bool,
    accepted_stopped: bool,
}
impl CurtainProjection {
    pub(super) fn target(&self, snapshot: CurtainSnapshot) -> Option<u32> {
        if self.accepted_stopped
            || self
                .flight
                .is_some_and(|(_, phase)| phase == Phase::Stopped)
        {
            snapshot.current_position.value
        } else {
            snapshot.target_position.value
        }
    }
    pub(super) fn queue_stop(&mut self, token: CurtainToken) {
        self.queued.push(token);
    }
    pub(super) fn target_command(&mut self) {
        self.queued.clear();
        self.flight = None;
        self.accepted_waiting = false;
        self.accepted_stopped = false;
    }
    pub(super) fn sent(&mut self, token: CurtainToken) {
        if let Some(index) = self.queued.iter().position(|queued| *queued == token) {
            self.queued.remove(index);
            self.flight = Some((token, Phase::Waiting));
        }
    }
    pub(super) fn accepted(&mut self, token: CurtainToken) {
        if self.flight.is_some_and(|(current, _)| current == token) {
            let (_, phase) = self.flight.take().unwrap();
            match phase {
                Phase::Waiting => self.accepted_waiting = true,
                Phase::Stopped => self.accepted_stopped = true,
            }
        }
    }
    pub(super) fn failed(&mut self, token: CurtainToken) {
        self.queued.retain(|queued| *queued != token);
        if self.flight.is_some_and(|(current, _)| current == token) {
            self.flight = None;
        }
    }
    pub(super) fn cancel_unsent(&mut self) {
        self.queued.clear();
    }
    pub(super) fn observe(&mut self, value: &CurtainValue) {
        match value {
            CurtainValue::TargetPosition(_) => self.target_command(),
            CurtainValue::Status(CurtainMotion::Stopped) => {
                if self.accepted_waiting {
                    self.accepted_waiting = false;
                    self.accepted_stopped = true;
                }
                if let Some((token, Phase::Waiting)) = self.flight {
                    self.flight = Some((token, Phase::Stopped));
                }
            }
            CurtainValue::Status(CurtainMotion::Opening | CurtainMotion::Closing) => {
                if self.accepted_stopped
                    || self
                        .flight
                        .is_some_and(|(_, phase)| phase == Phase::Stopped)
                {
                    self.accepted_stopped = false;
                    self.accepted_waiting = false;
                    self.flight = None;
                }
            }
            CurtainValue::CurrentPosition(_) => {}
        }
    }
}
