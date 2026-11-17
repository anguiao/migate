mod coalescing;
mod receipts;
mod takeover;

use super::new_device;
use crate::device::{
    Availability, Command, CommandError, CommandReceiver, DeviceKey, DeviceRequest, Function,
    FunctionDescription, FunctionKind, LightAction, LightDimension, LightPatch, LightProfile,
    LightRequest, LightTarget, PhysicalDevice, PhysicalDeviceDescription, Rgb, TemperatureInterval,
};
use futures_lite::future::{block_on, poll_once};

fn ready_light(rgb: bool) -> (PhysicalDevice, CommandReceiver) {
    let interval = TemperatureInterval::new(2700, 6500).unwrap();
    let profile = if rgb {
        LightProfile::ExtendedColor(interval)
    } else {
        LightProfile::ColorTemperature(interval)
    };
    let description = PhysicalDeviceDescription::new(
        DeviceKey::new("xiaomi", "test-outlet"),
        "Test Outlet",
        vec![FunctionDescription {
            id: "main".into(),
            label: "Main power".into(),
            kind: FunctionKind::Light(profile),
        }],
    )
    .unwrap();
    let (physical, receiver) = new_device(description);
    physical.set_availability(Availability::Available);
    (physical, receiver)
}

fn brightness(value: f64) -> LightTarget {
    LightTarget::brightness(None, value).expect("valid test target")
}
fn temperature(value: f64) -> LightTarget {
    LightTarget::temperature(None, value).expect("valid test target")
}
async fn frame(receiver: &mut CommandReceiver) -> LightRequest {
    match receiver.next_operation().await.unwrap() {
        DeviceRequest::Light(request) => request,
        _ => panic!("expected a lighting frame"),
    }
}
fn begin(function: &Function, value: f64, final_frame: bool) -> LightAction {
    function
        .begin_light_action(brightness(value), final_frame)
        .unwrap()
}
