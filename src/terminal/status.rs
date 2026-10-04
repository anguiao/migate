use crate::xiaomi::{
    auth::{AuthReport, AuthenticationState, CertificateUpdate},
    certificate::{CertificateStatus, CertificateValidity},
};
use std::{
    cell::RefCell,
    fmt::Write as _,
    path::{Path, PathBuf},
};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

type BridgeDiagnostics = (
    crate::xiaomi::source::GatewayStatus,
    Option<String>,
    Vec<crate::xiaomi::source::PendingDevice>,
);

pub struct AuthStatus {
    report: RefCell<AuthReport>,
    executable: PathBuf,
    data_dir: PathBuf,
    source: RefCell<Option<BridgeDiagnostics>>,
}

impl AuthStatus {
    pub fn new(report: AuthReport, executable: PathBuf, data_dir: PathBuf) -> Self {
        Self {
            report: RefCell::new(report),
            executable,
            data_dir,
            source: RefCell::new(None),
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
        if let Some((gateway, problem, _)) = &*self.source.borrow() {
            let _ = write!(report, "\n{}", format_gateway(gateway));
            if let Some(problem) = problem {
                let _ = write!(report, "\nDevice description: {problem}.");
            }
        }
        if self.source.borrow().is_some()
            && matches!(
                self.report.borrow().authentication,
                AuthenticationState::SignInRequired(_)
            )
        {
            report.push_str(
                "\nSign in independently, then restart MiGate to load the updated credentials.",
            );
        }
        report
    }

    pub fn update_source(&self, status: crate::xiaomi::source::SourceStatus) {
        if self.replace(status.authentication) {
            self.log(current_time());
        }
        let next = (status.gateway, status.catalog_problem, status.pending);
        let mut source = self.source.borrow_mut();
        if source.as_ref() != Some(&next) {
            log::info!("{}", format_gateway(&next.0));
            if let Some(problem) = &next.1 {
                log::warn!("Device description: {problem}.");
            }
            *source = Some(next);
        }
    }

    pub fn pending(&self) -> Vec<crate::xiaomi::source::PendingDevice> {
        self.source
            .borrow()
            .as_ref()
            .map(|source| source.2.clone())
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

fn format_gateway(status: &crate::xiaomi::source::GatewayStatus) -> String {
    use crate::xiaomi::source::GatewayStatus;
    match status {
        GatewayStatus::Discovering => "Gateway: discovering local gateway.".into(),
        GatewayStatus::Connecting(did) => format!("Gateway: connecting ({did})."),
        GatewayStatus::Connected(did) => format!("Gateway: connected ({did})."),
        GatewayStatus::Unavailable(reason) => format!("Gateway: unavailable. {reason}."),
    }
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
        .expect("validated certificate timestamp")
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
        use crate::xiaomi::source::{GatewayStatus, SourceStatus};
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
        status.update_source(SourceStatus {
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
}
