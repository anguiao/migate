mod format;

use format::{auth_report_lines, format_device, format_gateway, format_routing};
pub use format::{bridge_command, format_report, login_command};
pub(super) use format::{format_command, path, property_diagnostics, property_label, timestamp};

use crate::xiaomi::{
    adapter::{AdapterStatus, GatewayStatus},
    auth::AuthReport,
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

    pub(crate) fn signed_out(&self) {
        self.last_adapter.borrow_mut().take();
        let now = current_time();
        if self.replace(AuthReport::not_signed_in(now)) {
            self.log(now);
        }
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
        report
    }

    pub(super) fn preparation_reason(&self, phase: &str) -> String {
        if let Some(problem) = self
            .last_adapter
            .borrow()
            .as_ref()
            .and_then(|status| status.catalog_problem.as_deref())
        {
            format!("{phase}\nDevice description: {problem}.")
        } else {
            phase.to_owned()
        }
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
        for (level, line) in auth_report_lines(
            &report,
            report.certificate.map(|validity| validity.status(now)),
            &self.executable,
            &self.data_dir,
            " ",
        ) {
            log::log!(level, "{line}");
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
