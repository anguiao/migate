use crate::xiaomi::{
    adapter::{
        AdapterStatus, ChannelCommands, CommandStatus, DeviceStatus, GatewayStatus,
        ObservedPropertyStatus, PathKind, PathPriority, PendingDevice, RecoveryStatus,
        RoutingStatus,
    },
    auth::{AuthReport, AuthenticationState, CertificateUpdate},
    certificate::{CertificateStatus, CertificateValidity},
};
use std::{
    cell::RefCell,
    fmt::Write as _,
    path::{Path, PathBuf},
};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

struct BridgeDiagnostics {
    gateway: GatewayStatus,
    catalog_problem: Option<String>,
    pending: Vec<PendingDevice>,
    routing: RoutingStatus,
    devices: Vec<DeviceStatus>,
}

pub struct AuthStatus {
    report: RefCell<AuthReport>,
    executable: PathBuf,
    data_dir: PathBuf,
    adapter: RefCell<Option<BridgeDiagnostics>>,
}

impl AuthStatus {
    pub fn new(report: AuthReport, executable: PathBuf, data_dir: PathBuf) -> Self {
        Self {
            report: RefCell::new(report),
            executable,
            data_dir,
            adapter: RefCell::new(None),
        }
    }

    pub fn replace(&self, report: AuthReport) -> bool {
        let mut current = self.report.borrow_mut();
        let changed = current.authentication != report.authentication
            || current.certificate != report.certificate
            || current.certificate_update != report.certificate_update;
        *current = report;
        changed
    }

    pub fn render(&self, now: i64) -> String {
        let mut report =
            format_report(&self.report.borrow(), now, &self.executable, &self.data_dir);
        if let Some(adapter) = &*self.adapter.borrow() {
            let _ = write!(report, "\n{}", format_gateway(&adapter.gateway));
            if let Some(problem) = &adapter.catalog_problem {
                let _ = write!(report, "\nDevice description: {problem}.");
            }
            let _ = write!(report, "\n{}", format_routing(&adapter.routing));
        }
        if self.adapter.borrow().is_some()
            && matches!(
                self.report.borrow().authentication,
                AuthenticationState::SignInRequired(_)
            )
        {
            report.push_str("\nStop MiGate, sign in with the same data directory, then restart.");
        }
        report
    }

    pub fn render_devices(&self, now: i64, did: Option<&str>) -> String {
        let mut report = self.render(now);
        self.append_devices(&mut report, did);
        report
    }

    pub(super) fn append_devices(&self, report: &mut String, did: Option<&str>) {
        if let Some(adapter) = &*self.adapter.borrow() {
            for device in adapter
                .devices
                .iter()
                .filter(|device| did.is_none_or(|did| device.did == did))
            {
                let _ = write!(report, "\n{}", format_device(device));
            }
        }
    }

    pub(super) fn function_properties(&self, did: &str, id: &str) -> Vec<ObservedPropertyStatus> {
        self.adapter
            .borrow()
            .as_ref()
            .and_then(|adapter| adapter.devices.iter().find(|device| device.did == did))
            .and_then(|device| {
                device
                    .functions
                    .iter()
                    .find(|function| function.function == id)
            })
            .map(|function| function.properties.clone())
            .unwrap_or_default()
    }

    pub(super) fn function_commands(&self, did: &str, id: &str) -> Vec<String> {
        self.adapter
            .borrow()
            .as_ref()
            .and_then(|adapter| adapter.devices.iter().find(|device| device.did == did))
            .and_then(|device| device.commands.iter().find(|channel| channel.channel == id))
            .map(|channel| {
                [("in-flight", &channel.inflight), ("last", &channel.last)]
                    .into_iter()
                    .filter_map(|(state, command)| {
                        command
                            .as_ref()
                            .map(|command| format_command(id, state, command))
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn update_adapter(&self, status: AdapterStatus) {
        if self.replace(status.authentication) {
            self.log(current_time());
        }
        let next = BridgeDiagnostics {
            gateway: status.gateway,
            catalog_problem: status.catalog_problem,
            pending: status.pending,
            routing: status.routing,
            devices: status.devices,
        };
        let mut adapter = self.adapter.borrow_mut();
        if adapter
            .as_ref()
            .is_none_or(|old| old.gateway != next.gateway)
        {
            log::info!("{}", format_gateway(&next.gateway));
        }
        if adapter
            .as_ref()
            .is_none_or(|old| old.catalog_problem != next.catalog_problem)
            && let Some(problem) = &next.catalog_problem
        {
            log::warn!("Device description: {problem}.");
        }
        *adapter = Some(next);
    }

    pub fn pending(&self) -> Vec<PendingDevice> {
        self.adapter
            .borrow()
            .as_ref()
            .map(|adapter| adapter.pending.clone())
            .unwrap_or_default()
    }

    pub fn log(&self, now: i64) {
        let report = self.report.borrow();
        let authentication_level = match &report.authentication {
            AuthenticationState::SignInRequired(_) | AuthenticationState::Unavailable(_) => {
                log::Level::Warn
            }
            _ => log::Level::Info,
        };
        log::log!(
            authentication_level,
            "{}",
            format_authentication(&report.authentication)
        );
        let certificate_level = match report.certificate.map(|validity| validity.status(now)) {
            Some(CertificateStatus::NotYetValid | CertificateStatus::Expired) => log::Level::Warn,
            _ => log::Level::Info,
        };
        log::log!(
            certificate_level,
            "{}",
            format_certificate(report.certificate, now)
        );
        if let CertificateUpdate::Failed(reason) = &report.certificate_update {
            log::warn!("Gateway certificate update failed: {reason}.");
        }
        if matches!(
            report.authentication,
            AuthenticationState::NotSignedIn | AuthenticationState::SignInRequired(_)
        ) {
            log::info!(
                "Sign in with: {}",
                login_command(&self.executable, &self.data_dir)
            );
        }
    }
}

pub fn format_report(report: &AuthReport, now: i64, executable: &Path, data_dir: &Path) -> String {
    let mut output = format!(
        "{}\n{}",
        format_authentication(&report.authentication),
        format_certificate(report.certificate, now)
    );
    if let CertificateUpdate::Failed(reason) = &report.certificate_update {
        let _ = write!(output, "\nGateway certificate update failed: {reason}.");
    }
    if matches!(
        report.authentication,
        AuthenticationState::NotSignedIn | AuthenticationState::SignInRequired(_)
    ) {
        let _ = write!(
            output,
            "\nSign in with:\n  {}",
            login_command(executable, data_dir)
        );
    }
    output
}

fn format_gateway(status: &GatewayStatus) -> String {
    match status {
        GatewayStatus::Discovering => "Gateway: discovering local gateway.".into(),
        GatewayStatus::Connecting(did) => format!("Gateway: connecting ({did})."),
        GatewayStatus::Connected(did) => format!("Gateway: connected ({did})."),
        GatewayStatus::Unavailable(reason) => format!("Gateway: unavailable. {reason}."),
    }
}

fn path(path: PathKind) -> &'static str {
    match path {
        PathKind::Local => "local",
        PathKind::Cloud => "cloud",
    }
}

fn format_routing(status: &RoutingStatus) -> String {
    let priority = match status.priority {
        PathPriority::LocalFirst => "local-first",
        PathPriority::CloudFirst => "cloud-first",
    };
    let mut report = format!(
        "Routing: {priority}; local {}; business {}.",
        if status.local_connected {
            "connected"
        } else {
            "disconnected"
        },
        if status.business_degraded {
            "degraded"
        } else {
            "not degraded"
        }
    );
    match status.recovery {
        RecoveryStatus::None => report.push_str("\nLocal recovery: none."),
        RecoveryStatus::Scheduled(deadline) => {
            let _ = write!(
                report,
                "\nLocal recovery: scheduled for {}.",
                timestamp(deadline)
            );
        }
        RecoveryStatus::Running => report.push_str("\nLocal recovery: running."),
    }
    if let Some(reason) = &status.last_switch_reason {
        let _ = write!(report, "\nRouting last switch: {reason}.");
    }
    report
}

fn format_device(status: &DeviceStatus) -> String {
    let paths = status
        .usable_control_paths
        .iter()
        .copied()
        .map(path)
        .collect::<Vec<_>>();
    let paths = if paths.is_empty() {
        "none".into()
    } else {
        paths.join(", ")
    };
    let read_paths = status
        .usable_read_paths
        .iter()
        .copied()
        .map(path)
        .collect::<Vec<_>>();
    let read_paths = if read_paths.is_empty() {
        "none".into()
    } else {
        read_paths.join(", ")
    };
    let physical_online = match status.physical_online {
        None => "unknown",
        Some(true) => "online",
        Some(false) => "offline",
    };
    let mut report = format!(
        "{} routing:\n  Usable control paths: {paths}.\n  Selected control: {}.\n  Usable read paths: {read_paths}.\n  Selected read: {}.\n  Physical online: {physical_online}.\n  Notification source: {}.",
        status.did,
        status.selected_control.map(path).unwrap_or("none"),
        status.selected_read.map(path).unwrap_or("none"),
        status.notification_source.map(path).unwrap_or("none")
    );
    if status.read_only {
        report.push_str("\n  Control: read-only.");
    }
    if let Some(reason) = &status.unavailable_reason {
        let label = if status.read_only {
            "Read unavailable"
        } else {
            "Control unavailable"
        };
        let _ = write!(report, "\n  {label}: {reason}.");
    }
    if let Some(reason) = &status.sync_limited_reason {
        let _ = write!(report, "\n  State synchronization limited: {reason}.");
    }
    if let Some(reason) = &status.battery_problem {
        let _ = write!(report, "\n  Battery: {reason}.");
    }
    if let Some(reason) = &status.last_switch_reason {
        let _ = write!(report, "\n  Device last switch: {reason}.");
    }
    for function in &status.functions {
        let _ = write!(
            report,
            "\n  {}: selected read {}; notification source {}.",
            function.function,
            function.selected_read.map(path).unwrap_or("none"),
            function.notification_source.map(path).unwrap_or("none")
        );
        if !function.subscriptions.is_empty() {
            let _ = write!(
                report,
                "\n    Observation source: {}.",
                function.observation_source.map(path).unwrap_or("unknown")
            );
            for subscription in &function.subscriptions {
                let _ = write!(
                    report,
                    "\n    {} {}: {}.",
                    path(subscription.source),
                    subscription.binding,
                    if subscription.ready {
                        "ready"
                    } else {
                        "not ready"
                    }
                );
                if let Some(problem) = &subscription.problem {
                    let _ = write!(report, " {problem}.");
                }
            }
        }
        if let Some(reason) = &function.sync_limited_reason {
            let _ = write!(report, "\n    State synchronization limited: {reason}.");
        }
        if let Some(reason) = &function.read_problem {
            let _ = write!(report, "\n    Read: {reason}.");
        }
        for property in &function.properties {
            let _ = write!(
                report,
                "\n    {}: {}.",
                property.name,
                property_diagnostics(Some(property))
            );
        }
    }
    for ChannelCommands {
        channel,
        inflight,
        last,
        light_actions,
    } in &status.commands
    {
        if let Some(command) = inflight {
            let _ = write!(
                report,
                "\n  {}",
                format_command(channel, "in-flight", command)
            );
        }
        if let Some(command) = last {
            let _ = write!(report, "\n  {}", format_command(channel, "last", command));
        }
        if let Some(actions) = light_actions {
            let _ = write!(report, "\n  {}", format_actions(channel, actions));
        }
    }
    report
}

pub(super) fn property_diagnostics(property: Option<&ObservedPropertyStatus>) -> String {
    let Some(property) = property else {
        return "observation source unknown; read unknown; subscription unknown".into();
    };
    let mut fields = vec![
        format!(
            "property {}.{}",
            property.binding.siid, property.binding.piid
        ),
        format!(
            "observation source {}",
            property.observation_source.map(path).unwrap_or("unknown")
        ),
        property.read_problem.as_ref().map_or_else(
            || "read: no reported problem".into(),
            |problem| format!("read: {problem}"),
        ),
    ];
    fields.extend(property.subscriptions.iter().map(|subscription| {
        let mut text = format!(
            "{} subscription {}",
            path(subscription.source),
            if subscription.ready {
                "ready"
            } else {
                "not ready"
            }
        );
        if let Some(problem) = &subscription.problem {
            let _ = write!(text, " ({problem})");
        }
        text
    }));
    fields.join("; ")
}

fn format_command(channel: &str, state: &str, command: &CommandStatus) -> String {
    format!(
        "{channel} {state} command #{}: target {}; {} {}; actual path {}; {}.",
        command.id,
        command.target,
        if matches!(
            command.target,
            crate::xiaomi::adapter::CommandTarget::Power(_)
        ) {
            "property"
        } else {
            "properties"
        },
        command.properties_text(),
        command.actual_path.map(path).unwrap_or("not sent"),
        command.result_text(),
    )
}

fn dimension(dimension: crate::device::LightDimension) -> &'static str {
    match dimension {
        crate::device::LightDimension::Brightness => "brightness",
        crate::device::LightDimension::Color => "color",
    }
}
fn format_actions(channel: &str, actions: &crate::device::LightActionDiagnostics) -> String {
    use crate::device::CommandError;
    let awaiting = actions.awaiting_startup.map_or_else(
        || "none".into(),
        |token| {
            format!(
                "{}{}",
                dimension(token.dimension()),
                if actions.brightness == Some(token) || actions.color == Some(token) {
                    ""
                } else {
                    " (previous action)"
                }
            )
        },
    );
    let last = actions.last.as_ref().map_or_else(
        || "none".into(),
        |last| {
            let result = match &last.result {
                Ok(()) => "accepted".into(),
                Err(CommandError::Superseded) => "superseded".into(),
                Err(CommandError::Stopped) => "stopped".into(),
                Err(CommandError::Cancelled) => "cancelled".into(),
                Err(error) => format!("failed: {error}"),
            };
            format!("{} {result}", dimension(last.token.dimension()))
        },
    );
    format!(
        "{channel} actions: brightness {}; color {}; awaiting startup receipt {awaiting}; last {last}.",
        if actions.brightness.is_some() {
            "active"
        } else {
            "idle"
        },
        if actions.color.is_some() {
            "active"
        } else {
            "idle"
        },
    )
}

fn format_authentication(authentication: &AuthenticationState) -> String {
    match authentication {
        AuthenticationState::NotSignedIn => "Xiaomi: not signed in (cn).".to_owned(),
        AuthenticationState::Checking => "Xiaomi: checking authentication (cn)...".to_owned(),
        AuthenticationState::Authenticated => "Xiaomi: authenticated (cn).".to_owned(),
        AuthenticationState::SignInRequired(reason) => {
            format!("Xiaomi: sign-in required (cn). {reason}.")
        }
        AuthenticationState::Unavailable(reason) => {
            format!("Xiaomi: authentication unavailable (cn). {reason}.")
        }
    }
}

pub fn login_command(executable: &Path, data_dir: &Path) -> String {
    format!(
        "{} --data-dir {} auth login",
        shell_quote(executable),
        shell_quote(data_dir)
    )
}

fn format_certificate(validity: Option<CertificateValidity>, now: i64) -> String {
    let Some(validity) = validity else {
        return "Gateway certificate: not prepared.".to_owned();
    };
    match validity.status(now) {
        CertificateStatus::NotYetValid => format!(
            "Gateway certificate: not valid before {}.",
            timestamp(validity.not_before)
        ),
        CertificateStatus::Valid => format!(
            "Gateway certificate: valid until {}.",
            timestamp(validity.not_after)
        ),
        CertificateStatus::RenewalDue => format!(
            "Gateway certificate: valid until {}; renewal due.",
            timestamp(validity.not_after)
        ),
        CertificateStatus::Expired => format!(
            "Gateway certificate: expired at {}.",
            timestamp(validity.not_after)
        ),
    }
}

pub(super) fn timestamp(value: i64) -> String {
    OffsetDateTime::from_unix_timestamp(value)
        .expect("validated diagnostic timestamp")
        .format(&Rfc3339)
        .expect("RFC 3339 timestamp formatting")
}

fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\"'\"'"))
}

pub fn current_time() -> i64 {
    OffsetDateTime::now_utc().unix_timestamp()
}

#[cfg(test)]
mod tests;
