use crate::xiaomi::{
    adapter::{
        ChannelCommands, CommandStatus, DeviceStatus, GatewayStatus, ObservedPropertyStatus,
        PathKind, PathPriority, RecoveryStatus, RoutingStatus,
    },
    auth::{AuthReport, AuthenticationState, CertificateUpdate},
    certificate::{CertificateStatus, CertificateValidity},
};
use std::{fmt::Write as _, path::Path};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

pub fn format_report(report: &AuthReport, now: i64, executable: &Path, data_dir: &Path) -> String {
    let account = match &report.account {
        Some(account) => format!("Xiaomi account: {} ({}).", account.uid, account.region),
        None => "Xiaomi account: not signed in (cn).".to_owned(),
    };
    let mut output = format!(
        "{}\n{}\n{}\n{}",
        account,
        format_authentication("Mijia session", &report.mijia),
        format_authentication("HA OAuth", &report.ha),
        format_certificate(report.certificate, now)
    );
    if let Some(failure) = &report.login_failure {
        let _ = write!(
            output,
            "\nLogin failed during {}: {}.",
            failure.stage, failure.reason
        );
    }
    if let Some(stop) = report.recovery_stop {
        let _ = write!(
            output,
            "\nAccount recovery: stopped. {}.",
            crate::xiaomi::auth::FailureReason::RecoveryStopped(stop)
        );
    }
    if let CertificateUpdate::Failed(reason) = &report.certificate_update {
        let _ = write!(output, "\nGateway certificate update failed: {reason}.");
    }
    if matches!(
        report.mijia,
        AuthenticationState::NotSignedIn | AuthenticationState::SignInRequired(_)
    ) || report.login_failure.is_some()
        || report.recovery_stop.is_some()
        || matches!(
            report.ha,
            AuthenticationState::NotSignedIn | AuthenticationState::SignInRequired(_)
        )
    {
        let _ = write!(
            output,
            "\nSign in with:\n  {}",
            login_command(executable, data_dir)
        );
    }
    if matches!(report.mijia, AuthenticationState::Unavailable(_))
        || matches!(report.ha, AuthenticationState::Unavailable(_))
        || matches!(report.certificate_update, CertificateUpdate::Failed(_))
    {
        let _ = write!(
            output,
            "\nRetry maintenance with:\n  {}",
            check_command(executable, data_dir)
        );
    }
    let needs_verification = report.recovery_stop
        == Some(crate::storage::RecoveryStop::VerificationRequired)
        || report
            .login_failure
            .as_ref()
            .is_some_and(|failure| failure.reason.requires_verification())
        || [&report.mijia, &report.ha].iter().any(|state| match state {
            AuthenticationState::SignInRequired(reason)
            | AuthenticationState::Unavailable(reason) => reason.requires_verification(),
            _ => false,
        });
    if needs_verification {
        output.push_str("\nComplete the requested verification at https://www.mi.com/ on a phone or computer, preferably on the same network, then run auth login again. This may not prevent another verification request.");
    }
    output
}

pub(super) fn format_gateway(status: &GatewayStatus) -> String {
    match status {
        GatewayStatus::Discovering => "Gateway: discovering hub.".into(),
        GatewayStatus::Connecting(did) => format!("Gateway: connecting ({did})."),
        GatewayStatus::Connected(did) => format!("Gateway: connected ({did})."),
        GatewayStatus::Unavailable(reason) => format!("Gateway: unavailable. {reason}."),
    }
}

pub(super) fn path(path: PathKind) -> &'static str {
    match path {
        PathKind::Local => "hub",
        PathKind::Direct => "direct",
        PathKind::Cloud => "cloud",
    }
}

pub(super) fn format_routing(status: &RoutingStatus) -> String {
    let priority = match status.priority {
        PathPriority::LocalFirst => "hub-first",
        PathPriority::CloudFirst => "fallback-first",
    };
    let mut report = format!(
        "Routing: {priority}; hub {}.",
        if status.local_connected {
            "connected"
        } else {
            "disconnected"
        }
    );
    if let Some(reason) = &status.last_switch_reason {
        let _ = write!(report, "\nRouting last switch: {reason}.");
    }
    report
}

pub(super) fn format_device(status: &DeviceStatus) -> String {
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
    if status.usable_control_paths.contains(&PathKind::Cloud)
        || status.usable_read_paths.contains(&PathKind::Cloud)
    {
        report.push_str("\n  Cloud HTTP backend: mijia.");
    }
    if let Some(lan) = &status.lan {
        let _ = write!(
            report,
            "\n  Direct LAN ({}): {}; address {}; identity {}.",
            match lan.protocol {
                crate::xiaomi::adapter::LanProtocol::Miot => "MIoT properties",
                crate::xiaomi::adapter::LanProtocol::LegacyMcn02 =>
                    "legacy miIO mapping lumi.acpartner.mcn02",
            },
            match lan.state {
                crate::xiaomi::adapter::LanState::Ineligible =>
                    "ineligible (not a direct IP device)",
                crate::xiaomi::adapter::LanState::MissingCredentials => "credentials missing",
                crate::xiaomi::adapter::LanState::InvalidCredentials => "credentials invalid",
                crate::xiaomi::adapter::LanState::Authenticating => "authenticating",
                crate::xiaomi::adapter::LanState::Ready => "ready",
                crate::xiaomi::adapter::LanState::Failed => "failed",
            },
            lan.address
                .map(|ip| ip.to_string())
                .unwrap_or_else(|| "unknown".into()),
            if lan.state == crate::xiaomi::adapter::LanState::Ready {
                "confirmed"
            } else {
                "unconfirmed"
            }
        );
        if let Some(problem) = &lan.problem {
            let _ = write!(report, "\n  Direct LAN: {problem}.");
        }
        match lan.recovery {
            RecoveryStatus::None => {}
            RecoveryStatus::Running => report.push_str("\n  Direct LAN recovery: running."),
            RecoveryStatus::Scheduled(at) => {
                let _ = write!(
                    report,
                    "\n  Direct LAN recovery: scheduled for {}.",
                    timestamp(at)
                );
            }
        }
    } else if let Some(reason) = &status.direct_unavailable_reason {
        let _ = write!(report, "\n  Direct LAN: unavailable ({reason}).");
    }
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
    if let Some(mode) = &status.standard_mode {
        let _ = write!(
            report,
            "\n  Standard 3.1 mode: {}; {}; observed {}.",
            native_diagnostic(mode),
            mode.source.map(path).unwrap_or("source unknown"),
            mode.received_at
                .map(timestamp)
                .unwrap_or_else(|| "unknown".into())
        );
    }
    if let Some(shared) = &status.shared_feedback {
        let _ = write!(
            report,
            "\n  Legacy feedback group: {}; {}; pending {}.",
            shared.source.map(path).unwrap_or("source unknown"),
            if shared.pending {
                "waiting for readback"
            } else {
                "settled"
            },
            shared.pending
        );
        if let Some(problem) = &shared.read_problem {
            let _ = write!(report, " {problem}.");
        }
        for (name, field) in [
            ("bh_mode", &shared.native_mode),
            ("fan_speed_idx", &shared.fan_speed_index),
            ("temperature", &shared.temperature),
        ] {
            if let Some(field) = field {
                let _ = write!(report, "\n    {name}: {}.", native_diagnostic(field));
            }
        }
        for fan in &shared.fans {
            let _ = write!(
                report,
                "\n    {} {}: {}; observed {}.",
                fan.function,
                match fan.channel {
                    crate::xiaomi::capability::SharedFanChannel::Blow => "吹风",
                    crate::xiaomi::capability::SharedFanChannel::Ventilation => "换气",
                },
                match fan.level {
                    Some(1) => "low (50%)".to_string(),
                    Some(2) => "high (100%)".to_string(),
                    Some(level) => format!("level {level}"),
                    None => "unknown".into(),
                },
                fan.received_at
                    .map(timestamp)
                    .unwrap_or_else(|| "unknown".into())
            );
            if let Some(reason) = &fan.reason {
                let _ = write!(report, " {reason}.");
            }
        }
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

pub(in crate::terminal) fn property_diagnostics(
    property: Option<&ObservedPropertyStatus>,
) -> String {
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
    if let Some(detail) = &property.source_detail {
        fields.push(format!("source details {detail}"));
    }
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

fn native_diagnostic(value: &crate::xiaomi::adapter::DiagnosticObservation) -> String {
    let mut text = format!("raw {}", value.value);
    if let Some(source_time) = value.source_time {
        let _ = write!(text, "; source time {}", timestamp(source_time));
    }
    if let Some(reason) = &value.reason {
        let _ = write!(text, "; {reason}");
    }
    text
}

pub(super) fn format_command(channel: &str, state: &str, command: &CommandStatus) -> String {
    let mut text = format!(
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
    );
    if !command.steps.is_empty() {
        let steps = command
            .steps
            .iter()
            .map(|step| {
                let receipt = match &step.receipt {
                    crate::xiaomi::adapter::StepReceipt::Awaiting => "awaiting receipt".into(),
                    crate::xiaomi::adapter::StepReceipt::Accepted => "accepted".into(),
                    crate::xiaomi::adapter::StepReceipt::Rejected(reason) => {
                        format!("rejected: {reason}")
                    }
                    crate::xiaomi::adapter::StepReceipt::Uncertain(reason) => {
                        format!("result uncertain: {reason}")
                    }
                };
                match &step.native {
                    Some(native) => format!("{native} ({}, {receipt})", path(step.path)),
                    None => format!(
                        "{}.{} ({}, {})",
                        step.binding.siid,
                        step.binding.piid,
                        path(step.path),
                        receipt
                    ),
                }
            })
            .collect::<Vec<_>>()
            .join(", ");
        text.push_str(&format!(" Steps: {steps}."));
    }
    text
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

pub(super) fn format_authentication(label: &str, authentication: &AuthenticationState) -> String {
    match authentication {
        AuthenticationState::NotSignedIn => format!("{label}: not signed in."),
        AuthenticationState::Checking => format!("{label}: checking."),
        AuthenticationState::Restoring => format!("{label}: restoring."),
        AuthenticationState::Refreshing => format!("{label}: refreshing."),
        AuthenticationState::Authenticated => format!("{label}: available."),
        AuthenticationState::SignInRequired(reason) => {
            format!("{label}: sign-in required. {reason}.")
        }
        AuthenticationState::Unavailable(reason) => {
            format!("{label}: unavailable. {reason}.")
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

pub(super) fn check_command(executable: &Path, data_dir: &Path) -> String {
    format!(
        "{} --data-dir {} auth check",
        shell_quote(executable),
        shell_quote(data_dir)
    )
}

pub(super) fn format_certificate(validity: Option<CertificateValidity>, now: i64) -> String {
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

pub(in crate::terminal) fn timestamp(value: i64) -> String {
    OffsetDateTime::from_unix_timestamp(value)
        .expect("validated diagnostic timestamp")
        .format(&Rfc3339)
        .expect("RFC 3339 timestamp formatting")
}

fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\"'\"'"))
}
