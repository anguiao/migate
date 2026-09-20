use super::spec::urn_semantic;

pub fn supports_spec(model: &str, type_urn: &str) -> bool {
    urn_semantic(type_urn).is_some_and(|device_type| supports_device(model, device_type))
}

pub(super) fn supports_device(model: &str, device_type: &str) -> bool {
    !excluded(model, device_type) && supported_device(device_type, model)
}

fn excluded(model: &str, device: &str) -> bool {
    model == "yeelink.light.nl1"
        || matches!(
            device,
            "camera"
                | "speaker"
                | "router"
                | "lock"
                | "button"
                | "knob"
                | "push-window"
                | "window-opener"
                | "gateway"
        )
}
fn supported_device(device: &str, model: &str) -> bool {
    matches!(
        device,
        "light"
            | "switch"
            | "outlet"
            | "control-panel"
            | "air-conditioner"
            | "air-condition-outlet"
            | "curtain"
            | "fan"
            | "temperature-humidity-sensor"
            | "motion-sensor"
            | "occupancy-sensor"
            | "magnet-sensor"
            | "vacuum"
            | "bath-heater"
    ) || model == "lumi.acpartner.mcn04"
}
