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
