pub async fn next_power(
    receiver: &mut migate::device::CommandReceiver,
) -> Option<migate::device::CommandRequest> {
    match receiver.next_operation().await? {
        migate::device::DeviceRequest::Power(request) => Some(request),
        migate::device::DeviceRequest::Light(_) => panic!("unexpected lighting request"),
    }
}
