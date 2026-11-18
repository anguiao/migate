mod air_conditioner;
mod heater;

pub(super) use air_conditioner::{AirConditionerControl, air_conditioner_lines};
pub(super) use heater::heater_lines;

use super::{
    diagnostics::FunctionDiagnostics,
    primitives::{capabilities, known, on_off, power_text},
};
use crate::device::{
    CurtainCommand, CurtainMotion, CurtainSnapshot, FanCommand, FanSnapshot, FunctionDescription,
    FunctionKind, LightMode, LightSnapshot, Power,
};
use crate::xiaomi::adapter::PropertyRole;

pub(super) fn power_line(description: &FunctionDescription, power: Power) -> String {
    format!(
        "  {} ({}): {}",
        description.id,
        description.label,
        power_text(power)
    )
}

pub(super) fn light_lines(
    description: &FunctionDescription,
    power: Power,
    lighting: LightSnapshot,
) -> Vec<String> {
    let mut lines = vec![
        power_line(description, power),
        format!("    Capabilities: {}.", capabilities(description)),
    ];
    let mut values = Vec::new();
    if description.supports_brightness() {
        values.push(format!(
            "Brightness: {}",
            known(lighting.brightness_percent, |value| format!("{value}%"))
        ));
    }
    if description.supports_color_temperature() {
        values.push(format!(
            "temperature: {}",
            known(lighting.temperature_kelvin, |value| format!("{value} K"))
        ));
    }
    if description.supports_rgb() {
        values.push(format!(
            "RGB: {}",
            known(lighting.color, |value| format!(
                "#{:02X}{:02X}{:02X}",
                value.red, value.green, value.blue
            ))
        ));
    }
    if description.supports_color_temperature() || description.supports_rgb() {
        values.push(format!(
            "mode: {}",
            match lighting.mode {
                None => "unknown",
                Some(LightMode::Temperature) => "temperature",
                Some(LightMode::Rgb) => "RGB",
            }
        ));
    }
    if !values.is_empty() {
        lines.push(format!("    {}.", values.join("; ")));
    }
    lines
}

pub(super) fn fan_lines(
    description: &FunctionDescription,
    fan: FanSnapshot,
    diagnostics: &FunctionDiagnostics,
    pending: Vec<FanCommand>,
) -> Vec<String> {
    let mut entries = vec![
        diagnostics.observation(PropertyRole::Power, fan.power, on_off),
        diagnostics.observation(PropertyRole::Speed, fan.speed, |value| {
            let FunctionKind::Fan { speed_count, .. } = description.kind else {
                unreachable!("fan snapshot has a fan description")
            };
            format!("level {value}/{speed_count}")
        }),
    ];
    if let Some(mode) = diagnostics
        .property(PropertyRole::NativeMode)
        .and_then(|property| property.native_mode)
    {
        entries.insert(
            1,
            diagnostics.observation(PropertyRole::NativeMode, mode, |value| value.to_string()),
        );
    }
    if matches!(
        description.kind,
        FunctionKind::Fan {
            horizontal_swing: true,
            ..
        }
    ) {
        entries.push(diagnostics.observation(PropertyRole::HorizontalSwing, fan.swing, on_off));
    }
    let mut lines = diagnostics.observed_lines(description, entries);
    if !pending.is_empty() {
        let pending = pending
            .into_iter()
            .map(|command| crate::xiaomi::adapter::CommandTarget::Fan(command).to_string())
            .collect::<Vec<_>>()
            .join(", ");
        lines.push(format!("    queued fan commands: {pending}."));
    }
    lines
}

pub(super) fn curtain_lines(
    description: &FunctionDescription,
    curtain: CurtainSnapshot,
    diagnostics: &FunctionDiagnostics,
    pending: Vec<CurtainCommand>,
) -> Vec<String> {
    let entries = [
        diagnostics.observation(
            PropertyRole::CurrentPosition,
            curtain.current_position,
            |value| format!("{value}%"),
        ),
        diagnostics.observation(
            PropertyRole::TargetPosition,
            curtain.target_position,
            |value| format!("{value}%"),
        ),
        diagnostics.observation(PropertyRole::CurtainStatus, curtain.status, |status| {
            match status {
                CurtainMotion::Opening => "opening",
                CurtainMotion::Closing => "closing",
                CurtainMotion::Stopped => "stopped",
            }
            .into()
        }),
    ];
    let mut lines = diagnostics.observed_lines(description, entries);
    if !pending.is_empty() {
        let pending = pending
            .into_iter()
            .map(|command| crate::xiaomi::adapter::CommandTarget::Curtain(command).to_string())
            .collect::<Vec<_>>()
            .join(", ");
        lines.push(format!("    queued curtain commands: {pending}."));
    }
    lines
}
