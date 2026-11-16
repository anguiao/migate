mod format;

use format::{
    check_command, format_authentication, format_certificate, format_device, format_gateway,
    format_routing,
};
pub(super) use format::{format_command, path, property_diagnostics, timestamp};
pub use format::{format_report, login_command};

use crate::xiaomi::{
    adapter::{AdapterStatus, GatewayStatus},
    auth::{AuthReport, AuthenticationState, CertificateUpdate},
    certificate::CertificateStatus,
};
use std::{cell::RefCell, fmt::Write as _, path::PathBuf};
use time::OffsetDateTime;

struct AdapterLogState {
    gateway: GatewayStatus,
    catalog_problem: Option<String>,
}

pub struct AuthStatus {
    report: RefCell<AuthReport>,
    executable: PathBuf,
    data_dir: PathBuf,
    last_adapter: RefCell<Option<AdapterLogState>>,
}

impl AuthStatus {
    pub fn new(report: AuthReport, executable: PathBuf, data_dir: PathBuf) -> Self {
        Self {
            report: RefCell::new(report),
            executable,
            data_dir,
            last_adapter: RefCell::new(None),
        }
    }

    pub fn replace(&self, report: AuthReport) -> bool {
        let mut current = self.report.borrow_mut();
        let changed = current.account != report.account
            || current.recovery_stop != report.recovery_stop
            || current.mijia != report.mijia
            || current.ha != report.ha
            || current.certificate != report.certificate
            || current.certificate_update != report.certificate_update
            || current.login_failure != report.login_failure;
        *current = report;
        changed
    }

    pub fn render(&self, now: i64) -> String {
        format_report(&self.report.borrow(), now, &self.executable, &self.data_dir)
    }

    pub(super) fn render_adapter(&self, adapter: &AdapterStatus) -> String {
        let mut report = format::format_report_with_certificate(
            &adapter.auth,
            adapter.certificate_status,
            &self.executable,
            &self.data_dir,
        );
        let _ = write!(report, "\n{}", format_gateway(&adapter.gateway));
        if let Some(problem) = &adapter.catalog_problem {
            let _ = write!(report, "\nDevice description: {problem}.");
        }
        let _ = write!(report, "\n{}", format_routing(&adapter.routing));
        if matches!(adapter.auth.ha, AuthenticationState::SignInRequired(_))
            || matches!(adapter.auth.mijia, AuthenticationState::SignInRequired(_))
        {
            report.push_str("\nStop MiGate, sign in with the same data directory, then restart.");
        }
        report
    }

    pub fn update_adapter(&self, status: AdapterStatus) {
        if self.replace(status.auth) {
            self.log(current_time());
        }
        let next = AdapterLogState {
            gateway: status.gateway,
            catalog_problem: status.catalog_problem,
        };
        let mut adapter = self.last_adapter.borrow_mut();
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

    pub fn log(&self, now: i64) {
        let report = self.report.borrow();
        if let Some(account) = &report.account {
            log::info!("Xiaomi account: {} ({}).", account.uid, account.region);
        } else {
            log::info!("Xiaomi account: not signed in (cn).");
        }
        if let Some(stop) = report.recovery_stop {
            log::warn!(
                "Account recovery: stopped. {}.",
                crate::xiaomi::auth::FailureReason::RecoveryStopped(stop)
            );
        }
        for (label, state) in [("Mijia session", &report.mijia), ("HA OAuth", &report.ha)] {
            let level = if matches!(
                state,
                AuthenticationState::SignInRequired(_) | AuthenticationState::Unavailable(_)
            ) {
                log::Level::Warn
            } else {
                log::Level::Info
            };
            log::log!(level, "{}", format_authentication(label, state));
        }
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
            report.ha,
            AuthenticationState::NotSignedIn | AuthenticationState::SignInRequired(_)
        ) || report.recovery_stop.is_some()
            || matches!(
                report.mijia,
                AuthenticationState::NotSignedIn | AuthenticationState::SignInRequired(_)
            )
        {
            log::info!(
                "Sign in with: {}",
                login_command(&self.executable, &self.data_dir)
            );
        }
        if matches!(report.mijia, AuthenticationState::Unavailable(_))
            || matches!(report.ha, AuthenticationState::Unavailable(_))
            || matches!(report.certificate_update, CertificateUpdate::Failed(_))
        {
            log::info!(
                "Retry maintenance with: {}",
                check_command(&self.executable, &self.data_dir)
            );
        }
        if report.recovery_stop == Some(crate::storage::RecoveryStop::VerificationRequired)
            || [&report.mijia, &report.ha].iter().any(|state| match state {
                AuthenticationState::SignInRequired(reason)
                | AuthenticationState::Unavailable(reason) => reason.requires_verification(),
                _ => false,
            })
        {
            log::warn!(
                "Complete the requested verification at https://www.mi.com/ on a phone or computer, preferably on the same network, then run auth login again. This may not prevent another verification request."
            );
        }
    }
}

pub(super) fn append_devices(report: &mut String, adapter: &AdapterStatus, did: Option<&str>) {
    for device in adapter
        .devices
        .iter()
        .filter(|device| did.is_none_or(|did| device.did == did))
    {
        let _ = write!(report, "\n{}", format_device(device));
    }
}

pub fn current_time() -> i64 {
    OffsetDateTime::now_utc().unix_timestamp()
}

#[cfg(test)]
mod tests;
