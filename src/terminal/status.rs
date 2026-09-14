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

pub struct AuthStatus {
    report: RefCell<AuthReport>,
    executable: PathBuf,
    data_dir: PathBuf,
}

impl AuthStatus {
    pub fn new(report: AuthReport, executable: PathBuf, data_dir: PathBuf) -> Self {
        Self {
            report: RefCell::new(report),
            executable,
            data_dir,
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
        format_report(&self.report.borrow(), now, &self.executable, &self.data_dir)
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
        if matches!(report.authentication, AuthenticationState::NotSignedIn) {
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
    if matches!(report.authentication, AuthenticationState::NotSignedIn) {
        let _ = write!(
            output,
            "\nSign in with:\n  {}",
            login_command(executable, data_dir)
        );
    }
    output
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

fn login_command(executable: &Path, data_dir: &Path) -> String {
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
    use crate::virtual_device::VirtualLight;
    use crate::xiaomi::auth::AuthReport;

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
        let checking = AuthReport::for_test(
            AuthenticationState::Checking,
            None,
            CertificateUpdate::NotNeeded,
            1_700_000_000,
        );
        let status = AuthStatus::new(checking, "/bin/migate".into(), "/data".into());
        assert!(
            bridge::handle_line(&VirtualLight::new(), &status, "help", 1_700_000_000)
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
        let help =
            bridge::handle_line(&VirtualLight::new(), &status, "help", 1_700_000_000).unwrap();
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
