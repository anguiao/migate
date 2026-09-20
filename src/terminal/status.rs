use crate::xiaomi::auth::{AuthReport, AuthenticationState};
use std::{fmt::Write as _, path::Path};
use time::OffsetDateTime;

pub fn log_report(report: &AuthReport, now: i64, executable: &Path, data_dir: &Path) {
    report.log_status(now);
    if matches!(report.authentication, AuthenticationState::NotSignedIn) {
        log::info!("Sign in with: {}", login_command(executable, data_dir));
    }
}

pub fn format_report(report: &AuthReport, now: i64, executable: &Path, data_dir: &Path) -> String {
    let mut output = report.format_status(now);
    if matches!(report.authentication, AuthenticationState::NotSignedIn) {
        let _ = write!(
            output,
            "\nSign in with:\n  {}",
            login_command(executable, data_dir)
        );
    }
    output
}

fn login_command(executable: &Path, data_dir: &Path) -> String {
    format!(
        "{} --data-dir {} auth login",
        shell_quote(executable),
        shell_quote(data_dir)
    )
}

fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\"'\"'"))
}

pub fn current_time() -> i64 {
    OffsetDateTime::now_utc().unix_timestamp()
}

#[cfg(test)]
mod tests;
