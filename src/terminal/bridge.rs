use super::{AuthStatus, current_time};
use crate::device::{Availability, Command, Device, Power};
pub const COMMAND_HELP: &str = "Available commands: on, off, status, help";

pub async fn handle_line(
    device: &Device,
    status: &AuthStatus,
    line: &str,
    now: i64,
) -> Option<String> {
    let key = &device.description().key;
    let command = match line.trim() {
        "" => return None,
        "on" => Some(Command::On),
        "off" => Some(Command::Off),
        "status" => None,
        _ => return Some(format!("{COMMAND_HELP}\n{}", status.render(now))),
    };
    if let Some(command) = command {
        return Some(match device.execute(command).await {
            Ok(_) => {
                log::info!("terminal {} {command:?}: accepted", key.id);
                format!("{}: command accepted", key.id)
            }
            Err(error) => format!("{}: {error}", key.id),
        });
    }
    let snapshot = device.snapshot();
    let power = match snapshot.power {
        Power::Unknown => "unknown",
        Power::On => "on",
        Power::Off => "off",
    };
    let availability = match snapshot.availability {
        Availability::Initializing => "initializing".into(),
        Availability::Available => "available".into(),
        Availability::Unavailable(reason) => format!("unavailable ({reason})"),
    };
    Some(format!(
        "{}: {power}\nDevice: {availability}.\n{}",
        key.id,
        status.render(now)
    ))
}
/// Read commands until EOF. The caller keeps the bridge alive after this returns.
pub async fn run_input(
    device: &Device,
    status: &AuthStatus,
    mut input: impl futures_lite::io::AsyncBufRead + Unpin,
    mut output: impl futures_lite::io::AsyncWrite + Unpin,
) -> std::io::Result<()> {
    use futures_lite::io::{AsyncBufReadExt, AsyncWriteExt};
    let mut line = String::new();
    loop {
        line.clear();
        if input.read_line(&mut line).await? == 0 {
            return Ok(());
        }
        if let Some(result) = handle_line(device, status, &line, current_time()).await {
            output.write_all(result.as_bytes()).await?;
            output.write_all(b"\n").await?;
            output.flush().await?;
        }
    }
}
