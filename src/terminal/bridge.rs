use super::{current_time, format_report};
use crate::{
    RuntimeError,
    device::{
        Capability, CommandOutcome, DeviceCommand, DeviceService, FeatureCapabilities, HvacMode,
        NumericRange, Percent, Property, PropertyState, PropertyValue, RgbColor, SwingMode,
        VacuumCleanMode,
    },
    storage::{DeviceStore, StorageError},
    xiaomi::runtime::{AdmissionStatus, OperationPaths, XiaomiCandidateState, XiaomiRuntime},
};
use std::{fmt::Write as _, path::Path};

pub const COMMAND_HELP: &str = "Commands:\n  devices\n  status <id>\n  on <id> | off <id>\n  set <id> <property> <value>\n  action <id> <action>\n  refresh\n  help\nProperties: brightness-percent, color-temperature-kelvin, color-rgb (R,G,B), target-temperature-celsius, position-percent (0 closed, 100 open), hvac-mode, fan-speed, swing-mode, oscillation, clean-mode.\nActions: curtain-stop, vacuum-start, vacuum-stop, vacuum-dock. Use status <id> to inspect supported values.";

#[derive(Debug)]
pub enum BridgeCommandError {
    User(String),
    Storage(StorageError),
}

impl std::fmt::Display for BridgeCommandError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::User(message) => formatter.write_str(message),
            Self::Storage(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for BridgeCommandError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::User(_) => None,
            Self::Storage(error) => Some(error),
        }
    }
}

impl From<StorageError> for BridgeCommandError {
    fn from(error: StorageError) -> Self {
        Self::Storage(error)
    }
}

pub async fn handle_line(
    service: &DeviceService,
    devices: &DeviceStore,
    runtime: &XiaomiRuntime,
    executable: &Path,
    data_dir: &Path,
    line: &str,
    now: i64,
) -> Result<Option<String>, BridgeCommandError> {
    let words = line.split_whitespace().collect::<Vec<_>>();
    let Some(name) = words.first().copied() else {
        return Ok(None);
    };
    let output = match (name, words.as_slice()) {
        ("help", [_]) => format!(
            "{COMMAND_HELP}\n{}",
            format_report(&runtime.auth_report(), now, executable, data_dir)
        ),
        ("devices", [_]) => render_devices(service, devices, runtime)?,
        ("status", [_, id]) => render_status(service, devices, runtime, id)?,
        ("on", [_, id]) => submit(service, devices, id, DeviceCommand::SetPower(true)).await?,
        ("off", [_, id]) => submit(service, devices, id, DeviceCommand::SetPower(false)).await?,
        ("set", [_, id, property, value]) => {
            let command =
                parse_set(property, value).ok_or_else(|| usage("set <id> <property> <value>"))?;
            submit(service, devices, id, command).await?
        }
        ("action", [_, id, action]) => {
            let command = parse_action(action).ok_or_else(|| usage("action <id> <action>"))?;
            submit(service, devices, id, command).await?
        }
        ("refresh", [_]) => {
            runtime.refresh();
            "Accepted: catalog discovery and state refresh requested.".to_owned()
        }
        ("status", _) => return Err(usage("status <id>")),
        ("on", _) => return Err(usage("on <id>")),
        ("off", _) => return Err(usage("off <id>")),
        ("set", _) => return Err(usage("set <id> <property> <value>")),
        ("action", _) => return Err(usage("action <id> <action>")),
        ("devices" | "refresh" | "help", _) => return Err(usage(name)),
        _ => return Err(usage("help")),
    };
    Ok(Some(output))
}

fn usage(syntax: &str) -> BridgeCommandError {
    BridgeCommandError::User(format!("Usage: {syntax}"))
}

async fn submit(
    service: &DeviceService,
    devices: &DeviceStore,
    public_id: &str,
    command: DeviceCommand,
) -> Result<String, BridgeCommandError> {
    let identity = resolve(devices, public_id)?
        .ok_or_else(|| BridgeCommandError::User(format!("Unknown feature id: {public_id}")))?;
    service
        .validate_command(&identity, &command)
        .map_err(|error| {
            BridgeCommandError::User(format!("Command rejected for {public_id}: {error}"))
        })?;
    Ok(match service.command(&identity, command).await {
        CommandOutcome::Accepted => {
            format!("Accepted: {public_id}. Await a device report for confirmed state.")
        }
        CommandOutcome::Ambiguous => format!(
            "Ambiguous: {public_id}. Delivery may have occurred; inspect status before retrying."
        ),
        CommandOutcome::Unavailable => format!(
            "Unavailable: {public_id}. The feature cannot accept this command now; inspect status and diagnostics."
        ),
        CommandOutcome::Unsupported => format!("Unsupported: {public_id}."),
        CommandOutcome::Expired => format!("Expired: {public_id}."),
        CommandOutcome::Cancelled => format!("Cancelled: {public_id}."),
        CommandOutcome::Superseded => format!("Superseded: {public_id}."),
        CommandOutcome::Rejected(code) => format!("Rejected: {public_id} (device code {code})."),
    })
}

fn resolve(
    devices: &DeviceStore,
    public_id: &str,
) -> Result<Option<crate::device::FeatureIdentity>, crate::storage::StorageError> {
    Ok(devices
        .load_features(false)?
        .into_iter()
        .find(|item| item.public_id.as_str() == public_id)
        .map(|item| item.feature))
}

fn render_devices(
    service: &DeviceService,
    devices: &DeviceStore,
    runtime: &XiaomiRuntime,
) -> Result<String, crate::storage::StorageError> {
    let status = runtime.status();
    let mut output = match status.admission.status {
        AdmissionStatus::SuspendedConflict => "Home admission: SuspendedConflict. Control is paused because authenticated local gateways report conflicting homes.".to_owned(),
        other => format!("Home admission: {other:?}."),
    };
    for allocation in devices.load_features(false)? {
        let Some(feature) = service.feature(&allocation.feature) else {
            continue;
        };
        let paths = status
            .admission
            .features
            .iter()
            .find(|entry| entry.identity == allocation.feature)
            .map(|entry| entry.paths)
            .unwrap_or_default();
        let _ = write!(
            output,
            "\n{} | {} | role={} | admission={} | paths={}",
            allocation.public_id,
            feature.name,
            feature.identity.role.as_str(),
            if feature.admitted {
                "admitted"
            } else {
                "inactive"
            },
            format_paths(paths)
        );
    }
    for candidate in status.candidates {
        let outside_bound_home = status.admission.binding.as_ref().is_some_and(|binding| {
            candidate.account_id != binding.account.as_str()
                || candidate.home_id != binding.home.as_str()
        });
        let reason = if outside_bound_home {
            "outside bound home"
        } else {
            match candidate.state {
                XiaomiCandidateState::Unsupported => "unsupported type",
                XiaomiCandidateState::Unrecognized => "unrecognized capabilities",
                XiaomiCandidateState::WaitingForGateway => "waiting for local verification",
                XiaomiCandidateState::Ready => "admitted",
                XiaomiCandidateState::Conflict => "home ownership conflict; control paused",
            }
        };
        let _ = write!(
            output,
            "\ncandidate {} | {} | model={} | {}",
            candidate.did, candidate.name, candidate.model, reason
        );
    }
    if service.features().is_empty() && output.lines().count() == 1 {
        output.push_str("\nNo published features. Candidates may still be undiscovered, unverified, or unsupported.");
    }
    Ok(output)
}

fn format_paths(paths: OperationPaths) -> String {
    let mut values = Vec::new();
    if paths.gateway {
        values.push("gateway");
    }
    if paths.lan {
        values.push("lan");
    }
    if paths.cloud {
        values.push("cloud");
    }
    if values.is_empty() {
        "none".to_owned()
    } else {
        values.join(",")
    }
}

fn render_status(
    service: &DeviceService,
    devices: &DeviceStore,
    runtime: &XiaomiRuntime,
    public_id: &str,
) -> Result<String, BridgeCommandError> {
    let identity = resolve(devices, public_id)?
        .ok_or_else(|| BridgeCommandError::User(format!("Unknown feature id: {public_id}")))?;
    let feature = service
        .feature(&identity)
        .ok_or_else(|| BridgeCommandError::User(format!("Unknown feature id: {public_id}")))?;
    let mut output = format!(
        "{} | {} | role={} | {}",
        public_id,
        feature.name,
        feature.identity.role.as_str(),
        if service.can_control(&identity) {
            "available"
        } else {
            "unavailable"
        }
    );
    if let Some(snapshot) = service.snapshot(&identity) {
        for (property, state) in snapshot.properties() {
            let _ = write!(
                output,
                "\n{}: {}",
                property_name(*property),
                format_state(state)
            );
        }
    }
    if output.lines().count() == 1 {
        output.push_str("\nState: Unknown (no current or cached report).");
    }
    let admission = runtime.status().admission.status;
    if admission != AdmissionStatus::Active {
        let _ = write!(output, "\nControl gate: {:?}.", admission);
    }
    let _ = write!(
        output,
        "\nSupported commands: {}",
        format_capabilities(&feature.capabilities)
    );
    Ok(output)
}

fn format_capabilities(capabilities: &FeatureCapabilities) -> String {
    let mut values = Vec::new();
    for capability in &capabilities.0 {
        match capability {
            Capability::Power { writable: true } => values.push("on, off".to_owned()),
            Capability::Brightness(range) => {
                values.push(format_range("set brightness-percent", *range))
            }
            Capability::ColorTemperature(range) => {
                values.push(format_range("set color-temperature-kelvin", *range))
            }
            Capability::Color => values.push("set color-rgb R,G,B (each 0..255)".to_owned()),
            Capability::TargetTemperature(range) => {
                values.push(format_range("set target-temperature-celsius", *range))
            }
            Capability::HvacModes(modes) => values.push(format!(
                "set hvac-mode {}",
                modes
                    .iter()
                    .map(|mode| match mode {
                        HvacMode::Off => "off",
                        HvacMode::Auto => "auto",
                        HvacMode::Cool => "cool",
                        HvacMode::Heat => "heat",
                        HvacMode::Dry => "dry",
                        HvacMode::FanOnly => "fan-only",
                    })
                    .collect::<Vec<_>>()
                    .join("|")
            )),
            Capability::FanSpeeds(speeds) => values.push(format!(
                "set fan-speed {}",
                speeds
                    .iter()
                    .map(u16::to_string)
                    .collect::<Vec<_>>()
                    .join("|")
            )),
            Capability::SwingModes(modes) => values.push(format!(
                "set swing-mode {}; set oscillation on|off",
                modes
                    .iter()
                    .map(|mode| match mode {
                        SwingMode::Off => "off",
                        SwingMode::Vertical => "vertical",
                        SwingMode::Horizontal => "horizontal",
                        SwingMode::Both => "both",
                    })
                    .collect::<Vec<_>>()
                    .join("|")
            )),
            Capability::CurtainPosition(range) => values.push(format_range(
                "set position-percent (0 closed, 100 open)",
                *range,
            )),
            Capability::CurtainStop => values.push("action curtain-stop".to_owned()),
            Capability::VacuumCleanModes(modes) => values.push(format!(
                "set clean-mode {}",
                modes
                    .iter()
                    .map(|mode| match mode {
                        VacuumCleanMode::Vacuum => "vacuum",
                        VacuumCleanMode::Mop => "mop",
                        VacuumCleanMode::VacuumAndMop => "vacuum-and-mop",
                    })
                    .collect::<Vec<_>>()
                    .join("|")
            )),
            Capability::VacuumControl => values.push("action vacuum-start|vacuum-stop".to_owned()),
            Capability::VacuumDock => values.push("action vacuum-dock".to_owned()),
            _ => {}
        }
    }
    if values.is_empty() {
        "none (read-only feature)".to_owned()
    } else {
        values.join("; ")
    }
}

fn format_range(command: &str, range: NumericRange) -> String {
    format!(
        "{command} {}..{} step {}",
        range.minimum, range.maximum, range.step
    )
}

fn format_state(state: &PropertyState) -> String {
    match state {
        PropertyState::Current {
            value,
            source,
            observed_at,
            ..
        } => format!(
            "{} (Current, source={source:?}, updated={observed_at})",
            format_value(value)
        ),
        PropertyState::LastKnown {
            value,
            source,
            observed_at,
            ..
        } => format!(
            "{} (LastKnown cached value, not current; source={source:?}, updated={observed_at})",
            format_value(value)
        ),
        PropertyState::Unknown {
            last_known: Some(value),
            ..
        } => format!(
            "Unknown (LastKnown cached value {}, not current; source={:?}, updated={})",
            format_value(&value.value),
            value.source,
            value.observed_at
        ),
        PropertyState::Unknown {
            last_known: None, ..
        } => "Unknown (no confirmed value).".to_owned(),
    }
}

fn format_value(value: &PropertyValue) -> String {
    match value {
        PropertyValue::Power(v) | PropertyValue::Motion(v) | PropertyValue::Oscillation(v) => {
            v.to_string()
        }
        PropertyValue::Percent(v) => format!("{}%", v.get()),
        PropertyValue::ColorTemperature(v) => format!("{v} K"),
        PropertyValue::Color(v) => format!("{},{},{} RGB", v.red, v.green, v.blue),
        PropertyValue::Temperature(v) => format!("{v} C"),
        PropertyValue::FanSpeed(v) => v.to_string(),
        PropertyValue::Illuminance(v) => format!("{v} lux"),
        PropertyValue::ContactOpen(v) => if *v { "open" } else { "closed" }.to_owned(),
        other => format!("{other:?}"),
    }
}

fn property_name(property: Property) -> &'static str {
    match property {
        Property::Power => "power",
        Property::Brightness => "brightness-percent",
        Property::ColorTemperature => "color-temperature-kelvin",
        Property::Color => "color-rgb",
        Property::CurrentTemperature => "current-temperature-celsius",
        Property::TargetTemperature => "target-temperature-celsius",
        Property::HvacMode => "hvac-mode",
        Property::FanSpeed => "fan-speed",
        Property::SwingMode => "swing-mode",
        Property::CurtainPosition => "position-percent",
        Property::CurtainTargetPosition => "target-position-percent",
        Property::CurtainMovement => "curtain-movement",
        Property::Oscillation => "oscillation",
        Property::Temperature => "temperature-celsius",
        Property::Humidity => "humidity-percent",
        Property::Illuminance => "illuminance-lux",
        Property::Motion => "motion",
        Property::Occupancy => "occupancy",
        Property::Contact => "contact",
        Property::Battery => "battery-percent",
        Property::VacuumCleanMode => "clean-mode",
        Property::VacuumOperationalState => "vacuum-state",
        Property::VacuumFault => "vacuum-fault",
    }
}

fn parse_set(property: &str, value: &str) -> Option<DeviceCommand> {
    Some(match property {
        "brightness-percent" => {
            DeviceCommand::SetBrightness(Percent::new(value.parse::<f64>().ok()?).ok()?)
        }
        "color-temperature-kelvin" => DeviceCommand::SetColorTemperature(value.parse().ok()?),
        "color-rgb" => {
            let v = value
                .split(',')
                .map(str::parse)
                .collect::<Result<Vec<u8>, _>>()
                .ok()?;
            let [r, g, b] = v.as_slice() else { return None };
            DeviceCommand::SetColor(RgbColor {
                red: *r,
                green: *g,
                blue: *b,
            })
        }
        "target-temperature-celsius" => DeviceCommand::SetTargetTemperature(value.parse().ok()?),
        "position-percent" => {
            DeviceCommand::SetCurtainPosition(Percent::new(value.parse::<f64>().ok()?).ok()?)
        }
        "hvac-mode" => DeviceCommand::SetHvacMode(match value {
            "off" => HvacMode::Off,
            "auto" => HvacMode::Auto,
            "cool" => HvacMode::Cool,
            "heat" => HvacMode::Heat,
            "dry" => HvacMode::Dry,
            "fan-only" => HvacMode::FanOnly,
            _ => return None,
        }),
        "fan-speed" => DeviceCommand::SetFanSpeed(value.parse().ok()?),
        "swing-mode" => DeviceCommand::SetSwingMode(match value {
            "off" => SwingMode::Off,
            "vertical" => SwingMode::Vertical,
            "horizontal" => SwingMode::Horizontal,
            "both" => SwingMode::Both,
            _ => return None,
        }),
        "oscillation" => DeviceCommand::SetOscillation(match value {
            "on" => true,
            "off" => false,
            _ => return None,
        }),
        "clean-mode" => DeviceCommand::SetVacuumCleanMode(match value {
            "vacuum" => VacuumCleanMode::Vacuum,
            "mop" => VacuumCleanMode::Mop,
            "vacuum-and-mop" => VacuumCleanMode::VacuumAndMop,
            _ => return None,
        }),
        _ => return None,
    })
}

fn parse_action(action: &str) -> Option<DeviceCommand> {
    Some(match action {
        "curtain-stop" => DeviceCommand::StopCurtain,
        "vacuum-start" => DeviceCommand::StartVacuum,
        "vacuum-stop" => DeviceCommand::StopVacuum,
        "vacuum-dock" => DeviceCommand::ReturnVacuumToDock,
        _ => return None,
    })
}

pub async fn run_input(
    service: &DeviceService,
    devices: &DeviceStore,
    runtime: &XiaomiRuntime,
    executable: &Path,
    data_dir: &Path,
    mut input: impl futures_lite::io::AsyncBufRead + Unpin,
    mut output: impl futures_lite::io::AsyncWrite + Unpin,
) -> Result<(), RuntimeError> {
    use futures_lite::io::{AsyncBufReadExt, AsyncWriteExt};
    let mut line = String::new();
    loop {
        line.clear();
        if input.read_line(&mut line).await? == 0 {
            return Ok(());
        }
        let rendered = match handle_line(
            service,
            devices,
            runtime,
            executable,
            data_dir,
            &line,
            current_time(),
        )
        .await
        {
            Ok(value) => value,
            Err(BridgeCommandError::User(message)) => Some(message),
            Err(BridgeCommandError::Storage(error)) => return Err(error.into()),
        };
        if let Some(rendered) = rendered {
            output.write_all(rendered.as_bytes()).await?;
            output.write_all(b"\n").await?;
            output.flush().await?;
        }
    }
}
