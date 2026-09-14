use super::format_report;
use crate::{
    RuntimeError,
    storage::XiaomiStore,
    xiaomi::auth::{AuthReport, AuthService},
};
use futures_lite::io::{AsyncBufReadExt, AsyncWriteExt};
use std::path::Path;

pub fn logout(store: &XiaomiStore, mut output: impl std::io::Write) -> Result<(), RuntimeError> {
    store.logout()?;
    writeln!(output, "Xiaomi credentials removed.")?;
    Ok(())
}

pub async fn login(
    service: &AuthService,
    executable: &Path,
    data_dir: &Path,
    mut input: impl futures_lite::io::AsyncBufRead + Unpin,
    mut output: impl futures_lite::io::AsyncWrite + Unpin,
) -> Result<AuthReport, RuntimeError> {
    let attempt = service.begin_login()?;
    let prompt = format!(
        "Authorization URL:\n{}\nOpen this URL in a browser and authorize the HA application.\nThe final homeassistant.local page may be unreachable.\nPaste the complete address-bar URL containing code and state:\n",
        attempt.authorization().authorization_url()
    );
    output.write_all(prompt.as_bytes()).await?;
    output.flush().await?;
    let mut callback = String::new();
    if input.read_line(&mut callback).await? == 0 {
        return Err("Login cancelled before a callback URL was received".into());
    }
    let report = service.complete_login(attempt, callback.trim()).await?;
    write_report(&mut output, &report, executable, data_dir).await?;
    if !report.is_success() {
        return Err("Xiaomi login did not complete successfully".into());
    }
    Ok(report)
}

pub async fn check(
    service: &AuthService,
    executable: &Path,
    data_dir: &Path,
    mut output: impl futures_lite::io::AsyncWrite + Unpin,
) -> Result<AuthReport, RuntimeError> {
    let report = service.check().await?;
    write_report(&mut output, &report, executable, data_dir).await?;
    if !report.is_success() {
        return Err("Xiaomi authentication check failed".into());
    }
    Ok(report)
}

async fn write_report(
    output: &mut (impl futures_lite::io::AsyncWrite + Unpin),
    report: &AuthReport,
    executable: &Path,
    data_dir: &Path,
) -> std::io::Result<()> {
    let rendered = format_report(report, report.completed_at(), executable, data_dir);
    output.write_all(rendered.as_bytes()).await?;
    output.write_all(b"\n").await?;
    output.flush().await
}
