use crate::device::model::PropertyObservation;
use crate::device::{
    Availability, Command, CommandError, CurtainCommand, CurtainMotion, CurtainSnapshot,
    CurtainValue, DeviceKey, Function, FunctionDescription, FunctionKind, FunctionSnapshot,
    PhysicalDevice, PhysicalDeviceDescription,
};
use futures_lite::future::{block_on, poll_once};

impl FunctionSnapshot {
    pub(crate) fn curtain(&self) -> Option<CurtainSnapshot> {
        match self.state {
            crate::device::FunctionState::Curtain { curtain } => Some(curtain),
            _ => None,
        }
    }
}

fn curtain(id: &str) -> (PhysicalDevice, crate::device::CommandReceiver) {
    let description = PhysicalDeviceDescription::new(
        DeviceKey::new("xiaomi", id),
        "Curtain",
        vec![FunctionDescription {
            id: "main".into(),
            label: "Curtain".into(),
            kind: FunctionKind::Curtain,
        }],
    )
    .unwrap();
    let (device, mut receivers) = PhysicalDevice::new(description);
    (device, receivers.remove(0))
}

fn ready_curtain(id: &str) -> (PhysicalDevice, crate::device::CommandReceiver) {
    let (device, receiver) = curtain(id);
    device.set_availability(Availability::Available);
    (device, receiver)
}

async fn next_curtain(
    receiver: &mut crate::device::CommandReceiver,
) -> crate::device::CurtainRequest {
    match receiver.next_operation().await.unwrap() {
        crate::device::DeviceRequest::Curtain(request) => request,
        _ => panic!("expected curtain request"),
    }
}

mod commands;
mod observations;
mod stop_projection;
