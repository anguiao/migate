use crate::{device::Command, virtual_device::VirtualLight};

pub const COMMAND_HELP: &str = "可用命令：on, off, status";

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

/// Read commands until EOF. The caller keeps the bridge alive after this returns.
pub async fn run_input(
    light: &VirtualLight,
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
        if let Some(result) = handle_line(light, &line) {
            if matches!(line.trim(), "on" | "off") {
                log::info!("terminal {result}");
            }
            output.write_all(result.as_bytes()).await?;
            output.write_all(b"\n").await?;
            output.flush().await?;
        }
    }
}
