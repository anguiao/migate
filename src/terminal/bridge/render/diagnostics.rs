use super::primitives::{known, observed_text};
use crate::{
    device::{FunctionDescription, PropertyObservation},
    terminal::status::{format_command, path, property_diagnostics, property_label},
    xiaomi::adapter::{DeviceStatus, FunctionStatus, ObservedPropertyStatus, PropertyRole},
};
use std::fmt::Write as _;

/// One function borrows all diagnostics from the command's captured input.
pub(super) struct FunctionDiagnostics<'a> {
    properties: &'a [ObservedPropertyStatus],
    pub(super) synchronization: String,
    commands: Vec<String>,
}

impl<'a> FunctionDiagnostics<'a> {
    pub(super) fn capture(device: Option<&'a DeviceStatus>, id: &str, show_commands: bool) -> Self {
        let function = device.and_then(|device| {
            device
                .functions
                .iter()
                .find(|function| function.function == id)
        });
        let commands = show_commands
            .then(|| {
                device
                    .and_then(|device| device.commands.iter().find(|channel| channel.channel == id))
            })
            .flatten();
        Self {
            properties: function.map_or(&[], |function| function.properties.as_slice()),
            synchronization: synchronization(function),
            commands: commands
                .into_iter()
                .flat_map(|channel| [("in-flight", &channel.inflight), ("last", &channel.last)])
                .filter_map(|(state, command)| {
                    command
                        .as_ref()
                        .map(|command| format_command(id, state, command))
                })
                .collect(),
        }
    }

    pub(super) fn property(&self, role: PropertyRole) -> Option<&ObservedPropertyStatus> {
        self.properties
            .iter()
            .find(|property| property.role == role)
    }

    pub(super) fn observation<T>(
        &self,
        role: PropertyRole,
        observation: PropertyObservation<T>,
        format: impl FnOnce(T) -> String,
    ) -> String {
        let label = match role {
            PropertyRole::TargetTemperature => "Target temperature",
            _ => property_label(role),
        };
        observed_line(
            label,
            self.property(role),
            &known(observation.value, format),
            observation.source_time,
            observation.received_at,
        )
    }

    pub(super) fn observed_lines(
        &self,
        description: &FunctionDescription,
        entries: impl IntoIterator<Item = String>,
    ) -> Vec<String> {
        let mut lines = vec![format!("  {} ({}):", description.id, description.label)];
        lines.extend(entries);
        lines.extend(self.commands.iter().map(|command| format!("    {command}")));
        lines
    }
}

pub(super) fn observed_line(
    label: &str,
    property: Option<&ObservedPropertyStatus>,
    value: &str,
    source_time: Option<i64>,
    received_at: Option<i64>,
) -> String {
    let value = observed_text(value, source_time, received_at);
    let diagnostic = property_diagnostics(property);
    format!("    {label}: {value}; {diagnostic}.")
}

pub(super) fn synchronization(function: Option<&FunctionStatus>) -> String {
    let Some(function) = function else {
        return "synchronization diagnostics unavailable".into();
    };
    let mut details = format!(
        "selected read {}; notification source {}; observation source {}",
        function.selected_read.map(path).unwrap_or("none"),
        function.notification_source.map(path).unwrap_or("none"),
        function.observation_source.map(path).unwrap_or("unknown")
    );
    if let Some(reason) = &function.read_problem {
        let _ = write!(details, "; read: {reason}");
    }
    if let Some(reason) = &function.sync_limited_reason {
        let _ = write!(details, "; synchronization limited: {reason}");
    }
    details
}
