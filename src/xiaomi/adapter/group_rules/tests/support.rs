use super::super::Member;
use crate::device::{
    FunctionDescription, FunctionKind, LightMode, LightProfile, LightSnapshot, Power, Rgb,
    TemperatureInterval,
};

pub(super) fn interval() -> TemperatureInterval {
    TemperatureInterval::new(2700, 6500).expect("valid test interval")
}

pub(super) fn description(profile: LightProfile) -> FunctionDescription {
    FunctionDescription {
        id: "main".into(),
        label: "Group".into(),
        kind: FunctionKind::Light(profile),
    }
}

pub(super) fn dimmable() -> FunctionDescription {
    description(LightProfile::Dimmable)
}

pub(super) fn on_off() -> FunctionDescription {
    description(LightProfile::OnOff)
}

pub(super) fn color_temperature() -> FunctionDescription {
    description(LightProfile::ColorTemperature(interval()))
}

pub(super) fn extended() -> FunctionDescription {
    description(LightProfile::ExtendedColor(interval()))
}

pub(super) fn light(
    brightness: Option<f64>,
    temperature: Option<u32>,
    color: Option<u32>,
    mode: Option<LightMode>,
) -> LightSnapshot {
    LightSnapshot {
        brightness_percent: brightness,
        temperature_kelvin: temperature,
        color: color.map(|value| Rgb::from_packed(value).expect("packed test color")),
        mode,
    }
}

pub(super) fn member<'a>(did: &'a str, power: Power) -> Member<'a> {
    Member {
        did,
        power,
        lighting: light(None, None, None, None),
        supports_color: true,
    }
}

pub(super) fn with_light<'a>(mut member: Member<'a>, lighting: LightSnapshot) -> Member<'a> {
    member.lighting = lighting;
    member
}

pub(super) fn members<'a>(items: &[(&'a str, Power)]) -> Vec<Member<'a>> {
    items
        .iter()
        .map(|(did, power)| member(did, *power))
        .collect()
}
