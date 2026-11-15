mod format;

use format::{
    check_command, format_authentication, format_certificate, format_command, format_device,
    format_gateway, format_routing, path,
};
pub use format::{format_report, login_command};
pub(super) use format::{property_diagnostics, timestamp};

use crate::xiaomi::{
    adapter::{
        AdapterStatus, DeviceStatus, GatewayStatus, ObservedPropertyStatus, PendingDevice,
        RoutingStatus,
    },
    auth::{AuthReport, AuthenticationState, CertificateUpdate},
    certificate::CertificateStatus,
};
use std::{cell::RefCell, fmt::Write as _, path::PathBuf};
use time::OffsetDateTime;

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
            && (matches!(
                self.report.borrow().ha,
                AuthenticationState::SignInRequired(_)
            ) || matches!(
                self.report.borrow().mijia,
                AuthenticationState::SignInRequired(_)
            ))
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

    pub(super) fn function_sync(&self, did: &str, id: &str) -> String {
        let adapter = self.adapter.borrow();
        let Some(function) = adapter
            .as_ref()
            .and_then(|adapter| adapter.devices.iter().find(|device| device.did == did))
            .and_then(|device| {
                device
                    .functions
                    .iter()
                    .find(|function| function.function == id)
            })
        else {
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
        if self.replace(status.auth) {
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

pub fn current_time() -> i64 {
    OffsetDateTime::now_utc().unix_timestamp()
}

#[cfg(test)]
mod tests;
