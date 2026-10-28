use super::{CurtainMotion, CurtainSnapshot, CurtainValue};

/// Settled targets follow physical feedback, independently of command receipts.
#[derive(Default)]
pub(super) enum CurtainProjection {
    #[default]
    Native,
    PendingTarget,
    Stopped,
}
impl CurtainProjection {
    pub(super) fn target(&self, snapshot: CurtainSnapshot) -> Option<u32> {
        if matches!(self, Self::Stopped) {
            snapshot.current_position.value
        } else {
            snapshot.target_position.value
        }
    }
    pub(super) fn target_command(&mut self) {
        *self = Self::PendingTarget;
    }
    pub(super) fn clear_pending_target(&mut self) {
        if matches!(self, Self::PendingTarget) {
            *self = Self::Native;
        }
    }
    pub(super) fn observe(
        &mut self,
        value: &CurtainValue,
        previous: CurtainSnapshot,
        notification: bool,
    ) {
        match value {
            // Baseline reads and cached targets do not announce a new movement.
            CurtainValue::TargetPosition(_) if notification => self.target_command(),
            CurtainValue::Status(CurtainMotion::Stopped) => {
                // A repeated stopped status must not restore the previous stop
                // after a new target. A moving-to-stopped transition is fresh.
                if !matches!(self, Self::PendingTarget)
                    || previous.status.value != Some(CurtainMotion::Stopped)
                {
                    *self = Self::Stopped;
                }
            }
            CurtainValue::Status(CurtainMotion::Opening | CurtainMotion::Closing) => {
                *self = Self::Native;
            }
            CurtainValue::CurrentPosition(_) | CurtainValue::TargetPosition(_) => {}
        }
    }
}
