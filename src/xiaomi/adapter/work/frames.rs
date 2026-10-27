use super::*;

struct FrameOwnership {
    token: ActionToken,
    epoch: u64,
    members: Rc<Vec<SourceRef>>,
}

/// The newest RGB frame on a group function. A slow member checks ownership
/// before submitting so a newer frame, Stop or takeover retires queued writes
/// without cancelling a successful final acceptance.
#[derive(Default)]
pub(in crate::xiaomi::adapter) struct LightFrames {
    light: Option<FrameOwnership>,
}
impl LightFrames {
    pub(in crate::xiaomi::adapter) fn next_light(
        &mut self,
        token: ActionToken,
        snapshot: impl FnOnce() -> Vec<SourceRef>,
    ) -> (u64, Rc<Vec<SourceRef>>) {
        if let Some(frame) = self.light.as_mut()
            && frame.token == token
        {
            frame.epoch += 1;
            return (frame.epoch, frame.members.clone());
        }
        let members = Rc::new(snapshot());
        let epoch = 1;
        self.light = Some(FrameOwnership {
            token,
            epoch,
            members: members.clone(),
        });
        (epoch, members)
    }
    pub(in crate::xiaomi::adapter) fn current_light(&self, token: ActionToken, epoch: u64) -> bool {
        matches!(self.light, Some(ref frame) if frame.token == token && frame.epoch == epoch)
    }
    pub(in crate::xiaomi::adapter) fn cancel_light(&mut self, token: ActionToken, epoch: u64) {
        if self.current_light(token, epoch) {
            self.light = None;
        }
    }
    pub(in crate::xiaomi::adapter) fn cancel_dimension(&mut self, dimension: LightDimension) {
        if dimension == LightDimension::Color {
            self.light = None;
        }
    }

    /// A committed rebuild or runtime reset retires every queued frame.
    pub(in crate::xiaomi::adapter) fn cancel_all(&mut self) {
        self.light = None;
    }
}
