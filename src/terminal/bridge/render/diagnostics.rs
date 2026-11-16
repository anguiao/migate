use super::{
    AuthStatus,
    primitives::{Observation, observed_text},
};
use crate::{
    device::FunctionDescription, terminal::status::property_diagnostics,
    xiaomi::adapter::ObservedPropertyStatus,
};

/// Supplies the diagnostics belonging to one terminal command's input.
pub(in crate::terminal::bridge) trait DiagnosticSource {
    fn function_properties(&self, did: &str, id: &str) -> Vec<ObservedPropertyStatus>;
    fn function_sync(&self, did: &str, id: &str) -> String;
    fn function_commands(&self, did: &str, id: &str) -> Vec<String>;
}

impl DiagnosticSource for AuthStatus {
    fn function_properties(&self, did: &str, id: &str) -> Vec<ObservedPropertyStatus> {
        self.function_properties(did, id)
    }

    fn function_sync(&self, did: &str, id: &str) -> String {
        self.function_sync(did, id)
    }

    fn function_commands(&self, did: &str, id: &str) -> Vec<String> {
        self.function_commands(did, id)
    }
}

/// The formatters read captured values and never query the diagnostic source.
pub(super) struct FunctionDiagnostics {
    pub(super) properties: Vec<ObservedPropertyStatus>,
    pub(super) synchronization: String,
    commands: Vec<String>,
}

impl FunctionDiagnostics {
    pub(super) fn capture(
        source: &impl DiagnosticSource,
        did: &str,
        id: &str,
        show_commands: bool,
    ) -> Self {
        Self {
            properties: source.function_properties(did, id),
            synchronization: source.function_sync(did, id),
            commands: if show_commands {
                source.function_commands(did, id)
            } else {
                Vec::new()
            },
        }
    }

    pub(super) fn property(&self, name: &str) -> Option<&ObservedPropertyStatus> {
        self.properties
            .iter()
            .find(|property| property.name == name)
    }

    pub(super) fn property_text(&self, name: &str) -> String {
        property_diagnostics(self.property(name))
    }

    pub(super) fn observed_lines(
        &self,
        description: &FunctionDescription,
        entries: impl IntoIterator<Item = Observation>,
    ) -> Vec<String> {
        let mut lines = vec![format!("  {} ({}):", description.id, description.label)];
        for (name, value, source_time, received_at) in entries {
            let value = observed_text(&value, source_time, received_at);
            let diagnostic = self.property_text(if name == "Target temperature" {
                "Temperature"
            } else {
                name
            });
            let diagnostic = if diagnostic.is_empty() {
                String::new()
            } else {
                format!("; {diagnostic}")
            };
            lines.push(format!("    {name}: {value}{diagnostic}."));
        }
        lines.extend(self.commands.iter().map(|command| format!("    {command}")));
        lines
    }
}
