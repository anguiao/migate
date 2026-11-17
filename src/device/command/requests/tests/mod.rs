use super::{CommandReceiver, CommandRequest, DeviceRequest};

impl CommandReceiver {
    pub(crate) async fn next_power(&mut self) -> Option<CommandRequest> {
        match self.next_operation().await? {
            DeviceRequest::Power(request) => Some(request),
            _ => panic!("expected a power request"),
        }
    }
}
