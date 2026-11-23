mod online;

pub(crate) use online::OnlineAuth;

use super::{format_report, qr};
use crate::{
    RuntimeError,
    storage::XiaomiStore,
    xiaomi::auth::{AuthError, AuthReport, AuthService, FailureReason, LoginFailure},
};
use futures_lite::io::{AsyncWrite, AsyncWriteExt};
use std::{io, path::Path};

const LOGOUT_MESSAGE: &str = "Xiaomi credentials removed.\n";

pub fn logout(store: &XiaomiStore, mut output: impl io::Write) -> Result<(), RuntimeError> {
    store.logout()?;
    output.write_all(LOGOUT_MESSAGE.as_bytes())?;
    Ok(())
}

pub async fn login(
    service: &AuthService,
    executable: &Path,
    data_dir: &Path,
    mut output: impl AsyncWrite + Unpin,
) -> Result<AuthReport, RuntimeError> {
    run_login(
        service,
        executable,
        data_dir,
        async |text| write_text(&mut output, text).await,
        || {},
        || {},
    )
    .await
}

async fn run_login(
    service: &AuthService,
    executable: &Path,
    data_dir: &Path,
    mut write: impl AsyncFnMut(String) -> io::Result<()>,
    committed: impl FnOnce(),
    maintained: impl FnOnce(),
) -> Result<AuthReport, RuntimeError> {
    write("This login obtains a new Mijia session for device HTTP and HA OAuth for cloud MQTT and the gateway certificate.\n".into()).await?;
    let attempt = match service.begin_login().await {
        Ok(attempt) => attempt,
        Err(error) => {
            report_error(
                &mut write,
                service,
                executable,
                data_dir,
                "QR request",
                &error,
            )
            .await?;
            return Err(error.into());
        }
    };
    let text = match qr::render(attempt.login_url()) {
        Ok(text) => text,
        Err(_) => {
            write("Login failed during QR display: payload is too long.\n".into()).await?;
            write_report(&mut write, &service.local_status()?, executable, data_dir).await?;
            return Err("Cannot render login QR code: payload is too long".into());
        }
    };
    let seconds = attempt.remaining().as_secs();
    write(format!("Scan this QR code with the Xiaomi app (valid for up to {seconds} seconds):\n{text}Waiting for scan confirmation...\n")).await?;
    let result = service.complete_login_with_commit(attempt, committed).await;
    maintained();
    let report = match result {
        Ok(report) => report,
        Err(error) => {
            let (stage, detail) = match &error {
                AuthError::LoginStep { stage, source } => (*stage, source.as_ref()),
                _ => ("credential or certificate preparation", &error),
            };
            report_error(&mut write, service, executable, data_dir, stage, detail).await?;
            return Err(error.into());
        }
    };
    write_report(&mut write, &report, executable, data_dir).await?;
    if !report.is_success() {
        return Err("Xiaomi login did not complete successfully".into());
    }
    Ok(report)
}

async fn report_error(
    write: &mut impl AsyncFnMut(String) -> io::Result<()>,
    service: &AuthService,
    executable: &Path,
    data_dir: &Path,
    stage: &'static str,
    error: &AuthError,
) -> Result<(), RuntimeError> {
    let reason = match error {
        AuthError::Account(error) => Some(FailureReason::Account(error.clone())),
        AuthError::Cloud(error) => Some(FailureReason::Cloud(error.clone())),
        AuthError::Failure(reason) => Some(reason.clone()),
        AuthError::Storage(_) | AuthError::LoginStep { .. } | AuthError::Missing(_) => None,
    };
    if reason.is_none() {
        write(format!("Login failed during {stage}: {error}.\n")).await?;
    }
    let mut report = service.local_status()?;
    report.login_failure = reason.map(|reason| LoginFailure { stage, reason });
    write_report(write, &report, executable, data_dir).await?;
    Ok(())
}

pub async fn check(
    service: &AuthService,
    executable: &Path,
    data_dir: &Path,
    mut output: impl AsyncWrite + Unpin,
) -> Result<AuthReport, RuntimeError> {
    run_check(
        service,
        executable,
        data_dir,
        async |text| write_text(&mut output, text).await,
        || {},
    )
    .await
}

async fn run_check(
    service: &AuthService,
    executable: &Path,
    data_dir: &Path,
    mut write: impl AsyncFnMut(String) -> io::Result<()>,
    checked: impl FnOnce(),
) -> Result<AuthReport, RuntimeError> {
    let report = service.check().await;
    checked();
    let report = report?;
    write_report(&mut write, &report, executable, data_dir).await?;
    if !report.is_success() {
        return Err("Xiaomi authentication check failed".into());
    }
    Ok(report)
}

pub async fn report_local_status(
    service: &AuthService,
    executable: &Path,
    data_dir: &Path,
    mut output: impl AsyncWrite + Unpin,
) -> Result<(), RuntimeError> {
    write_report(
        &mut async |text| write_text(&mut output, text).await,
        &service.local_status()?,
        executable,
        data_dir,
    )
    .await?;
    Ok(())
}

async fn write_report(
    write: &mut impl AsyncFnMut(String) -> io::Result<()>,
    report: &AuthReport,
    executable: &Path,
    data_dir: &Path,
) -> io::Result<()> {
    let rendered = format_report(report, report.completed_at(), executable, data_dir);
    write(format!("{rendered}\n")).await
}

async fn write_text(output: &mut (impl AsyncWrite + Unpin), text: String) -> io::Result<()> {
    output.write_all(text.as_bytes()).await?;
    output.flush().await
}
