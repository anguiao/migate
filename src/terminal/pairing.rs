use super::{bridge_command, qr};
use crate::matter::{PairingEvent, PairingStatus};
use rs_matter::utils::timers::Instant;
use std::{
    fmt,
    io::{self, Write},
    path::Path,
};

pub fn write_event(
    mut output: impl Write,
    event: PairingEvent,
    executable: &Path,
    data_dir: &Path,
) -> io::Result<()> {
    match event {
        PairingEvent::Opened {
            window_seconds,
            manual_code,
            qr_payload,
        } => write_codes(
            &mut output,
            format_args!(
                "Add MiGate in the Home app. The pairing window is open for {} minutes.",
                window_seconds / 60,
            ),
            &manual_code,
            &qr_payload,
        )?,
        PairingEvent::Expired => {
            writeln!(output, "Pairing window expired.")?;
            write_reopen_hint(&mut output, executable, data_dir)?;
        }
    }
    output.flush()
}

pub(crate) fn write_status(
    mut output: impl Write,
    status: PairingStatus,
    executable: &Path,
    data_dir: &Path,
) -> io::Result<()> {
    match status {
        PairingStatus::Paired => writeln!(output, "MiGate is already paired.")?,
        PairingStatus::Closed => {
            writeln!(output, "Pairing window is closed.")?;
            write_reopen_hint(&mut output, executable, data_dir)?;
        }
        PairingStatus::Open {
            expires_at,
            manual_code,
            qr_payload,
        } => {
            let seconds = remaining_seconds(expires_at);
            write_codes(
                &mut output,
                format_args!(
                    "Add MiGate in the Home app. The pairing window has {}m {}s remaining.",
                    seconds / 60,
                    seconds % 60,
                ),
                &manual_code,
                &qr_payload,
            )?;
        }
        PairingStatus::OpenWithAdministratorCode { expires_at } => {
            let seconds = remaining_seconds(expires_at);
            writeln!(
                output,
                "The pairing window has {}m {}s remaining. Use the code from the Matter administrator.",
                seconds / 60,
                seconds % 60,
            )?;
        }
    }
    output.flush()
}

fn write_reopen_hint(mut output: impl Write, executable: &Path, data_dir: &Path) -> io::Result<()> {
    writeln!(
        output,
        "In the foreground bridge or an attach session, enter 'pairing open'.\nFrom the shell, run\n  {} pairing open",
        bridge_command(executable, data_dir)
    )
}

fn remaining_seconds(expires_at: Instant) -> u64 {
    expires_at
        .saturating_duration_since(Instant::now())
        .as_micros()
        .div_ceil(1_000_000)
}

fn write_codes(
    mut output: impl Write,
    message: fmt::Arguments<'_>,
    manual_code: &str,
    qr_payload: &str,
) -> io::Result<()> {
    let qr_text = qr::render(qr_payload).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "Cannot render pairing QR code: payload is too long",
        )
    })?;
    writeln!(
        output,
        "{message}\nManual pairing code: {manual_code}\n{qr_payload}"
    )?;
    output.write_all(qr_text.as_bytes())
}
