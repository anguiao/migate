use super::{AuthStatus, current_time};
use crate::{device::Command, virtual_device::VirtualLight};

pub const COMMAND_HELP: &str = "Available commands: on, off, status, help";

pub fn handle_line(light: &VirtualLight, line: &str) -> Option<String> {
    let snapshot = match line.trim() {
        "" => return None,
        "on" => light.execute(Command::On),
        "off" => light.execute(Command::Off),
        "status" => light.snapshot(),
        _ => return Some(COMMAND_HELP.to_owned()),
    };
    Some(format!(
        "{}: {}",
        snapshot.id,
        if snapshot.power { "on" } else { "off" }
    ))
}

pub fn handle_line_with_status(
    light: &VirtualLight,
    status: &AuthStatus,
    line: &str,
    now: i64,
) -> Option<String> {
    match line.trim() {
        "help" => Some(format!("{COMMAND_HELP}\n{}", status.render(now))),
        "" | "on" | "off" | "status" => handle_line(light, line),
        _ => Some(format!("{COMMAND_HELP}\n{}", status.render(now))),
    }
}

/// Read commands until EOF. The caller keeps the bridge alive after this returns.
pub async fn run_input(
    light: &VirtualLight,
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
        if let Some(result) = handle_line_with_status(light, status, &line, current_time()) {
            if matches!(line.trim(), "on" | "off") {
                log::info!("terminal {result}");
            }
            output.write_all(result.as_bytes()).await?;
            output.write_all(b"\n").await?;
            output.flush().await?;
        }
    }
}
