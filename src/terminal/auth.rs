use super::format_report;
use crate::{
    RuntimeError,
    storage::XiaomiStore,
    xiaomi::auth::{AccountErrorKind, AuthError, AuthReport, AuthService},
};
use futures_lite::io::AsyncWriteExt;
use qrcodegen_no_heap::{QrCode, QrCodeEcc, Version};
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
    mut output: impl futures_lite::io::AsyncWrite + Unpin,
) -> Result<AuthReport, RuntimeError> {
    output.write_all(b"This login obtains a new Mijia session for device HTTP and HA OAuth for cloud MQTT and the gateway certificate.\n").await?;
    output.flush().await?;
    let attempt = match service.begin_login().await {
        Ok(attempt) => attempt,
        Err(error) => {
            report_error(
                &mut output,
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
    let mut scratch = [0; 16384];
    let mut buffer = [0; 16384];
    let qr = match QrCode::encode_text(
        attempt.login_url(),
        &mut scratch,
        &mut buffer,
        QrCodeEcc::Medium,
        Version::MIN,
        Version::MAX,
        None,
        false,
    ) {
        Ok(qr) => qr,
        Err(_) => {
            output
                .write_all(b"Login failed during QR display: payload is too long.\n")
                .await?;
            write_report(&mut output, &service.local_status()?, executable, data_dir).await?;
            return Err("Cannot render login QR code: payload is too long".into());
        }
    };
    let mut text = String::from("\x1b[97;40m");
    for y in (-4..qr.size() + 4).step_by(2) {
        for x in -4..qr.size() + 4 {
            let top = !qr.get_module(x, y);
            let bottom = !qr.get_module(x, y + 1);
            text.push(match (top, bottom) {
                (true, true) => '█',
                (true, false) => '▀',
                (false, true) => '▄',
                (false, false) => ' ',
            });
        }
        text.push('\n');
    }
    text.push_str("\x1b[0m");
    let seconds = attempt.remaining().as_secs();
    output.write_all(format!("Scan this QR code with the Xiaomi app (valid for up to {seconds} seconds):\n{text}Waiting for scan confirmation...\n").as_bytes()).await?;
    output.flush().await?;
    let report = match service.complete_login(attempt).await {
        Ok(report) => report,
        Err(error) => {
            let (stage, detail) = match &error {
                AuthError::LoginStep { stage, source } => (*stage, source.as_ref()),
                _ => ("credential or certificate preparation", &error),
            };
            report_error(&mut output, service, executable, data_dir, stage, detail).await?;
            return Err(error.into());
        }
    };
    write_report(&mut output, &report, executable, data_dir).await?;
    if !report.is_success() {
        return Err("Xiaomi login did not complete successfully".into());
    }
    Ok(report)
}

async fn report_error(
    output: &mut (impl futures_lite::io::AsyncWrite + Unpin),
    service: &AuthService,
    executable: &Path,
    data_dir: &Path,
    stage: &str,
    error: &AuthError,
) -> Result<(), RuntimeError> {
    output
        .write_all(format!("Login failed during {stage}: {error}.\n").as_bytes())
        .await?;
    if matches!(error, AuthError::Account(error) if error.kind() == AccountErrorKind::VerificationRequired)
    {
        output.write_all(b"Complete the requested verification at https://www.mi.com/ on a phone or computer, preferably on the same network, then run auth login again. This may not prevent another verification request.\n").await?;
    }
    write_report(output, &service.local_status()?, executable, data_dir).await?;
    Ok(())
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

pub async fn report_local_status(
    service: &AuthService,
    executable: &Path,
    data_dir: &Path,
    mut output: impl futures_lite::io::AsyncWrite + Unpin,
) -> Result<(), RuntimeError> {
    write_report(&mut output, &service.local_status()?, executable, data_dir).await?;
    Ok(())
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
