use super::qr;
use crate::matter::PairingEvent;
use std::io::{self, Write};

pub fn write_event(mut output: impl Write, event: PairingEvent) -> io::Result<()> {
    match event {
        PairingEvent::Opened {
            window_seconds,
            manual_code,
            qr_payload,
        } => {
            let qr_text = qr::render(&qr_payload).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "Cannot render pairing QR code: payload is too long",
                )
            })?;
            writeln!(
                output,
                "Add MiGate in the Home app. The pairing window is open for {} minutes.\nManual pairing code: {manual_code}\n{qr_payload}",
                window_seconds / 60,
            )?;
            output.write_all(qr_text.as_bytes())?;
        }
        PairingEvent::Expired => {
            writeln!(
                output,
                "Pairing window expired. Restart MiGate to reopen it."
            )?;
        }
    }
    output.flush()
}
