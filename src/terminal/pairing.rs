use crate::matter::PairingEvent;
use std::io::{self, Write};

pub fn write_event(mut output: impl Write, event: PairingEvent) -> io::Result<()> {
    match event {
        PairingEvent::Opened {
            window_seconds,
            manual_code,
            qr_payload,
            qr_text,
        } => {
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
