use crate::xiaomi::{
    adapter::{
        AdapterStatus, ChannelCommands, CommandStatus, DeviceStatus, GatewayStatus, PathKind,
        PathPriority, PendingDevice, RecoveryStatus, RoutingStatus,
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
        if let Some(adapter) = &*self.adapter.borrow() {
            for device in adapter
                .devices
                .iter()
                .filter(|device| did.is_none_or(|did| device.did == did))
            {
                let _ = write!(report, "\n{}", format_device(device));
            }
        }
        report
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
    let mut report = format!(
        "{} routing:\n  Usable control paths: {paths}.\n  Selected control: {}.\n  Notification source: {}.",
        status.did,
        status.selected_control.map(path).unwrap_or("none"),
        status.notification_source.map(path).unwrap_or("none")
    );
    if let Some(reason) = &status.unavailable_reason {
        let _ = write!(report, "\n  Control unavailable: {reason}.");
    }
    if let Some(reason) = &status.sync_limited_reason {
        let _ = write!(report, "\n  State synchronization limited: {reason}.");
    }
    if let Some(reason) = &status.last_switch_reason {
        let _ = write!(report, "\n  Device last switch: {reason}.");
    }
    for ChannelCommands {
        channel,
        inflight,
        last,
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
    }
    report
}

fn format_command(channel: &str, state: &str, command: &CommandStatus) -> String {
    let result = match &command.result {
        None => "awaiting result".into(),
        Some(Ok(())) => "accepted".into(),
        Some(Err(error)) => error.to_string(),
    };
    format!(
        "{channel} {state} command #{}: target {}; property {}.{}; actual path {}; {result}.",
        command.id,
        if command.target { "on" } else { "off" },
        command.binding.siid,
        command.binding.piid,
        command.actual_path.map(path).unwrap_or("not sent")
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

fn timestamp(value: i64) -> String {
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
mod tests {
    use super::*;

    use crate::terminal::bridge;
    use crate::xiaomi::auth::AuthReport;

    #[test]
    fn sign_in_required_preserves_the_login_hint() {
        let report = AuthReport::for_test(
            AuthenticationState::SignInRequired(
                crate::xiaomi::auth::FailureReason::AccountIdentityMismatch,
            ),
            None,
            CertificateUpdate::NotNeeded,
            0,
        );
        assert!(
            format_report(&report, 0, Path::new("/bin/migate"), Path::new("/data"))
                .contains("auth login")
        );
    }

    #[test]
    fn connected_gateway_keeps_authentication_and_catalog_diagnostics_visible() {
        use crate::xiaomi::adapter::{AdapterStatus, GatewayStatus};
        let status = AuthStatus::new(
            AuthReport::for_test(
                AuthenticationState::Checking,
                None,
                CertificateUpdate::NotNeeded,
                0,
            ),
            "/bin/migate".into(),
            "/data".into(),
        );
        status.update_adapter(AdapterStatus {
            authentication: AuthReport::for_test(
                AuthenticationState::SignInRequired(
                    crate::xiaomi::auth::FailureReason::AccountIdentityMismatch,
                ),
                None,
                CertificateUpdate::NotNeeded,
                0,
            ),
            gateway: GatewayStatus::Connected("gateway".into()),
            catalog_problem: Some("refresh failed".into()),
            pending: Vec::new(),
            routing: Default::default(),
            devices: Vec::new(),
        });
        let report = status.render(0);
        assert!(report.contains("Xiaomi: sign-in required"));
        assert!(report.contains("auth login"));
        assert!(report.contains("Gateway: connected (gateway)."));
        assert!(report.contains("Device description: refresh failed."));
    }

    #[test]
    fn certificate_dates_are_recomputed_for_each_render() {
        let report = AuthReport::for_test(
            AuthenticationState::Authenticated,
            Some(CertificateValidity {
                not_before: 1_700_000_000,
                not_after: 1_700_259_201,
            }),
            CertificateUpdate::NotNeeded,
            1_700_000_000,
        );
        assert_eq!(
            format_report(
                &report,
                1_700_000_000,
                Path::new("/bin/migate"),
                Path::new("/data")
            ),
            "Xiaomi: authenticated (cn).\nGateway certificate: valid until 2023-11-17T22:13:21Z."
        );
        assert_eq!(
            format_report(
                &report,
                1_700_000_001,
                Path::new("/bin/migate"),
                Path::new("/data")
            ),
            "Xiaomi: authenticated (cn).\nGateway certificate: valid until 2023-11-17T22:13:21Z; renewal due."
        );
    }

    #[test]
    fn bridge_help_reads_the_latest_report() {
        let directory = tempfile::tempdir().unwrap();
        let store = crate::storage::Store::open(directory.path()).unwrap();
        let registry = crate::device::DeviceRegistry::new();
        let checking = AuthReport::for_test(
            AuthenticationState::Checking,
            None,
            CertificateUpdate::NotNeeded,
            1_700_000_000,
        );
        let status = AuthStatus::new(checking, "/bin/migate".into(), "/data".into());
        assert!(
            futures_lite::future::block_on(bridge::handle_line(
                &registry,
                &store.devices(),
                &status,
                "help",
                1_700_000_000
            ))
            .unwrap()
            .unwrap()
            .contains("Xiaomi: checking authentication (cn)...")
        );

        let authenticated = AuthReport::for_test(
            AuthenticationState::Authenticated,
            Some(CertificateValidity {
                not_before: 1_699_000_000,
                not_after: 1_800_000_000,
            }),
            CertificateUpdate::NotNeeded,
            1_700_000_000,
        );
        assert!(status.replace(authenticated));
        let help = futures_lite::future::block_on(bridge::handle_line(
            &registry,
            &store.devices(),
            &status,
            "help",
            1_700_000_000,
        ))
        .unwrap()
        .unwrap();
        assert!(help.contains("Xiaomi: authenticated (cn)."), "{help}");
        assert!(!help.contains("checking authentication"), "{help}");

        let same_visible_status = AuthReport::for_test(
            AuthenticationState::Authenticated,
            Some(CertificateValidity {
                not_before: 1_699_000_000,
                not_after: 1_800_000_000,
            }),
            CertificateUpdate::NotNeeded,
            1_700_000_001,
        );
        assert!(!status.replace(same_visible_status));
    }

    #[test]
    fn routing_status_distinguishes_selected_control_from_the_actual_command_path() {
        use crate::{
            device::CommandError,
            xiaomi::{adapter::*, capability::PropertyBinding},
        };
        let report = AuthReport::for_test(
            AuthenticationState::Authenticated,
            None,
            CertificateUpdate::NotNeeded,
            0,
        );
        let status = AuthStatus::new(report.clone(), "/bin/migate".into(), "/data".into());
        status.update_adapter(AdapterStatus {
            authentication: report,
            gateway: GatewayStatus::Connected("gateway".into()),
            catalog_problem: None,
            pending: Vec::new(),
            routing: RoutingStatus {
                priority: PathPriority::CloudFirst,
                local_connected: true,
                business_degraded: true,
                recovery: RecoveryStatus::Scheduled(1_700_000_060),
                last_switch_reason: Some("local business response timed out".into()),
            },
            devices: vec![DeviceStatus {
                did: "one".into(),
                usable_control_paths: vec![PathKind::Local, PathKind::Cloud],
                selected_control: Some(PathKind::Cloud),
                notification_source: Some(PathKind::Local),
                unavailable_reason: None,
                sync_limited_reason: Some("cloud property subscriptions pending".into()),
                last_switch_reason: Some("cloud preferred after local timeout".into()),
                commands: vec![ChannelCommands {
                    channel: "main".into(),
                    inflight: Some(CommandStatus {
                        id: 42,
                        binding: PropertyBinding { siid: 2, piid: 1 },
                        target: true,
                        actual_path: Some(PathKind::Local),
                        result: None,
                    }),
                    last: Some(CommandStatus {
                        id: 41,
                        binding: PropertyBinding { siid: 2, piid: 1 },
                        target: false,
                        actual_path: Some(PathKind::Cloud),
                        result: Some(Err(CommandError::Timeout)),
                    }),
                }],
            }],
        });
        let output = status.render_devices(1_700_000_000, None);
        assert!(
            output.contains("Routing: cloud-first; local connected; business degraded."),
            "{output}"
        );
        assert!(
            output.contains("Local recovery: scheduled for 2023-11-14T22:14:20Z."),
            "{output}"
        );
        assert!(
            output.contains("Usable control paths: local, cloud."),
            "{output}"
        );
        assert!(output.contains("Selected control: cloud."), "{output}");
        assert!(output.contains("Notification source: local."), "{output}");
        assert!(output.contains("main in-flight command #42: target on; property 2.1; actual path local; awaiting result."), "{output}");
        assert!(output.contains("main last command #41: target off; property 2.1; actual path cloud; command acceptance timed out."), "{output}");
        assert!(
            output.contains("State synchronization limited: cloud property subscriptions pending."),
            "{output}"
        );
        assert!(
            output.contains("Device last switch: cloud preferred after local timeout."),
            "{output}"
        );
        let help = status.render(1_700_000_000);
        assert!(help.contains("Routing: cloud-first"));
        assert!(!help.contains("command #"));
        assert!(!help.contains("one routing:"));
    }

    #[test]
    fn routing_status_displays_running_recovery_unsent_work_and_previous_acceptance() {
        use crate::xiaomi::{adapter::*, capability::PropertyBinding};
        let report = AuthReport::for_test(
            AuthenticationState::Authenticated,
            None,
            CertificateUpdate::NotNeeded,
            0,
        );
        let status = AuthStatus::new(report.clone(), "/bin/migate".into(), "/data".into());
        status.update_adapter(AdapterStatus {
            authentication: report,
            gateway: GatewayStatus::Unavailable("transport disconnected".into()),
            catalog_problem: None,
            pending: Vec::new(),
            routing: RoutingStatus {
                recovery: RecoveryStatus::Running,
                ..Default::default()
            },
            devices: vec![DeviceStatus {
                did: "one".into(),
                usable_control_paths: Vec::new(),
                selected_control: None,
                notification_source: None,
                unavailable_reason: Some("token expired".into()),
                sync_limited_reason: Some("no notification source".into()),
                last_switch_reason: None,
                commands: vec![ChannelCommands {
                    channel: "main".into(),
                    inflight: Some(CommandStatus {
                        id: 8,
                        binding: PropertyBinding { siid: 3, piid: 1 },
                        target: false,
                        actual_path: None,
                        result: None,
                    }),
                    last: Some(CommandStatus {
                        id: 7,
                        binding: PropertyBinding { siid: 3, piid: 1 },
                        target: true,
                        actual_path: Some(PathKind::Local),
                        result: Some(Ok(())),
                    }),
                }],
            }],
        });
        let output = status.render_devices(0, Some("one"));
        assert!(output.contains("Local recovery: running."), "{output}");
        assert!(output.contains("Usable control paths: none."), "{output}");
        assert!(output.contains("Selected control: none."), "{output}");
        assert!(output.contains("Notification source: none."), "{output}");
        assert!(
            output.contains("Control unavailable: token expired."),
            "{output}"
        );
        assert!(output.contains("main in-flight command #8: target off; property 3.1; actual path not sent; awaiting result."), "{output}");
        assert!(
            output.contains(
                "main last command #7: target on; property 3.1; actual path local; accepted."
            ),
            "{output}"
        );
    }
}
