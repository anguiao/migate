//! Pure Matter lighting command planning.
//!
//! Handlers decode protocol values and map errors; these functions own the
//! capability limits, Options/WithOnOff/OnLevel rules, target computation and
//! current-trajectory decisions. They consume plain values, return
//! Ignore/Stop/Execute and never touch TLV, rs-matter or the clock.

use crate::device::{Command, LightDimension, LightProfile};
use crate::lighting::{Task, color, hs, level};
use crate::matter::units::mired_bounds;

pub(super) const XY_MAX: u16 = color::XY_MAX;

/// Effective feedback projection relevant to planning.
#[derive(Clone, Copy, Default)]
pub(super) struct State {
    pub(super) power: Option<bool>,
    pub(super) level: Option<u8>,
    pub(super) temperature: Option<u16>,
    pub(super) xy: Option<(u16, u16)>,
    pub(super) hs: Option<hs::Hs>,
}

/// Current color trajectory sampled by the scheduler.
#[derive(Clone, Copy)]
pub(super) struct Trajectory {
    pub(super) task: color::Task,
    pub(super) elapsed_ms: u64,
    pub(super) current: bool,
}

/// Writable cluster settings owned by the report surface.
#[derive(Clone, Copy, Default)]
pub(super) struct Settings {
    pub(super) level_options: u8,
    pub(super) color_options: u8,
    pub(super) on_level: Option<u8>,
}

/// Common pure input shared by every command rule: the valid published
/// profile, writable settings, effective feedback and current trajectory.
#[derive(Clone, Copy)]
pub(super) struct Input {
    pub(super) profile: LightProfile,
    pub(super) settings: Settings,
    pub(super) state: State,
    pub(super) trajectory: Option<Trajectory>,
}

/// Matter Options/OptionsMask values decoded for one command.
#[derive(Clone, Copy)]
pub(super) struct Options {
    mask: u8,
    override_: u8,
}
impl Options {
    pub(super) fn new(mask: u8, override_: u8) -> Self {
        Self { mask, override_ }
    }
}

#[derive(Debug)]
pub(super) enum Plan {
    Ignore,
    Stop(LightDimension),
    Execute { task: Task },
}

#[derive(Debug)]
pub(super) enum PowerPlan {
    Execute(Command),
    Begin { task: level::Task, power: bool },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum PlanError {
    InvalidCommand,
    Constraint,
    InvalidState,
    Unsupported,
}

#[derive(Clone, Copy)]
pub(super) enum MoveMode {
    Up,
    Down,
    Stop,
}

#[derive(Clone, Copy)]
pub(super) enum StepMode {
    Up,
    Down,
}

fn validate_options(request: Options) -> Result<(), PlanError> {
    if (request.mask | request.override_) & !1 != 0 {
        return Err(PlanError::Constraint);
    }
    Ok(())
}

/// ExecuteIfOff processing. The profile implements ExecuteIfOff only; an
/// ordinary command cannot enable omitted behavior through an override.
fn should_continue(
    power: Option<bool>,
    execute_if_off: u8,
    request: Options,
) -> Result<bool, PlanError> {
    validate_options(request)?;
    let execute_if_off = if request.mask & 1 != 0 {
        request.override_ & 1 != 0
    } else {
        execute_if_off & 1 != 0
    };
    match power {
        Some(true) => Ok(true),
        Some(false) => Ok(execute_if_off),
        None if execute_if_off => Ok(true),
        None => Err(PlanError::InvalidState),
    }
}

fn level_gate(with_on_off: bool, request: Options, input: &Input) -> Result<bool, PlanError> {
    if !input.profile.has_brightness() {
        return Err(PlanError::Unsupported);
    }
    if with_on_off {
        validate_options(request)?;
        Ok(true)
    } else {
        should_continue(input.state.power, input.settings.level_options, request)
    }
}

fn color_gate(request: Options, input: &Input) -> Result<bool, PlanError> {
    should_continue(input.state.power, input.settings.color_options, request)
}

pub(super) fn move_to_level(
    mut level: u8,
    with_on_off: bool,
    transition_ds: Option<u16>,
    request: Options,
    input: &Input,
) -> Result<Plan, PlanError> {
    if level > 254 {
        return Err(PlanError::InvalidCommand);
    }
    if !level_gate(with_on_off, request, input)? {
        return Ok(Plan::Ignore);
    }
    level = level.clamp(1, 254);
    let transition_ms = transition_ds.unwrap_or(0) as u64 * 100;
    let start = if transition_ms == 0 {
        input.state.level.unwrap_or(level)
    } else {
        input.state.level.ok_or(PlanError::InvalidState)?
    };
    Ok(Plan::Execute {
        task: Task::Level(level::Task::MoveToLevel {
            with_on_off,
            start,
            target: level,
            transition_ms,
        }),
    })
}

pub(super) fn move_level(
    with_on_off: bool,
    mode: level::Direction,
    rate: Option<u8>,
    request: Options,
    input: &Input,
) -> Result<Plan, PlanError> {
    // Manufacturer-selected fastest planning rate, not DefaultMoveRate.
    const FASTEST_RATE: u8 = 50;
    let rate = rate.unwrap_or(FASTEST_RATE);
    if rate == 0 {
        return Err(PlanError::InvalidCommand);
    }
    if !level_gate(with_on_off, request, input)? {
        return Ok(Plan::Ignore);
    }
    Ok(Plan::Execute {
        task: Task::Level(level::Task::Move {
            with_on_off,
            start: input.state.level.ok_or(PlanError::InvalidState)?,
            move_mode: mode,
            rate,
        }),
    })
}

pub(super) fn step(
    with_on_off: bool,
    mode: StepMode,
    size: u8,
    transition: Option<u16>,
    request: Options,
    input: &Input,
) -> Result<Plan, PlanError> {
    if size == 0 {
        return Err(PlanError::InvalidCommand);
    }
    if !level_gate(with_on_off, request, input)? {
        return Ok(Plan::Ignore);
    }
    let current = input.state.level.ok_or(PlanError::InvalidState)?;
    let target = match mode {
        StepMode::Up => current.saturating_add(size).min(254),
        StepMode::Down => current.saturating_sub(size).max(1),
    };
    let duration = transition.unwrap_or(0) as u64 * current.abs_diff(target) as u64 / size as u64;
    move_to_level(target, with_on_off, Some(duration as u16), request, input)
}

pub(super) fn stop_level(
    with_on_off: bool,
    request: Options,
    input: &Input,
) -> Result<Plan, PlanError> {
    if !level_gate(with_on_off, request, input)? {
        return Ok(Plan::Ignore);
    }
    Ok(Plan::Stop(LightDimension::Brightness))
}

fn hs_base(input: &Input) -> Result<(hs::HsTask, hs::Hs), PlanError> {
    let start = input.state.hs.ok_or(PlanError::InvalidState)?;
    let task = match input.trajectory {
        Some(Trajectory {
            task: color::Task::Hs(task),
            elapsed_ms,
            current: true,
        }) => task.resume(elapsed_ms),
        _ => hs::HsTask::stationary(start),
    };
    Ok((task, start))
}

/// A Hue/Saturation Stop only cancels its own HS trajectory, never an XY or
/// color-temperature move that shares the color dimension.
fn stop_hs(input: &Input) -> Plan {
    if input
        .trajectory
        .is_some_and(|value| matches!(value.task, color::Task::Hs(_)))
    {
        Plan::Stop(LightDimension::Color)
    } else {
        Plan::Ignore
    }
}

pub(super) fn move_to_hue(
    hue: u8,
    direction: hs::Direction,
    duration_ds: u16,
    request: Options,
    input: &Input,
) -> Result<Plan, PlanError> {
    if !input.profile.has_rgb() {
        return Err(PlanError::Unsupported);
    }
    if hue == u8::MAX || duration_ds == u16::MAX {
        return Err(PlanError::Constraint);
    }
    if !color_gate(request, input)? {
        return Ok(Plan::Ignore);
    }
    let (mut task, start) = hs_base(input)?;
    task.hue = hs::HueAxis::move_to(start.hue, hue as f64, direction, duration_ds as u64 * 100);
    Ok(Plan::Execute {
        task: Task::Color(color::Task::Hs(task)),
    })
}

pub(super) fn move_hue(
    mode: MoveMode,
    rate: u8,
    request: Options,
    input: &Input,
) -> Result<Plan, PlanError> {
    if !input.profile.has_rgb() {
        return Err(PlanError::Unsupported);
    }
    if !matches!(mode, MoveMode::Stop) && rate == 0 {
        return Err(PlanError::InvalidCommand);
    }
    if !color_gate(request, input)? {
        return Ok(Plan::Ignore);
    }
    if matches!(mode, MoveMode::Stop) {
        return Ok(stop_hs(input));
    }
    let (mut task, start) = hs_base(input)?;
    let rate = if matches!(mode, MoveMode::Up) {
        rate as f64
    } else {
        -(rate as f64)
    };
    task.hue = hs::HueAxis::move_at(start.hue, rate);
    Ok(Plan::Execute {
        task: Task::Color(color::Task::Hs(task)),
    })
}

pub(super) fn step_hue(
    mode: StepMode,
    size: u8,
    duration_ds: u8,
    request: Options,
    input: &Input,
) -> Result<Plan, PlanError> {
    if !input.profile.has_rgb() {
        return Err(PlanError::Unsupported);
    }
    if size == 0 {
        return Err(PlanError::InvalidCommand);
    }
    if !color_gate(request, input)? {
        return Ok(Plan::Ignore);
    }
    let (mut task, start) = hs_base(input)?;
    let delta = if matches!(mode, StepMode::Up) {
        size as f64
    } else {
        -(size as f64)
    };
    task.hue = hs::HueAxis::step(start.hue, delta, duration_ds as u64 * 100);
    Ok(Plan::Execute {
        task: Task::Color(color::Task::Hs(task)),
    })
}

pub(super) fn move_to_saturation(
    saturation: u8,
    duration_ds: u16,
    request: Options,
    input: &Input,
) -> Result<Plan, PlanError> {
    if !input.profile.has_rgb() {
        return Err(PlanError::Unsupported);
    }
    if saturation == u8::MAX || duration_ds == u16::MAX {
        return Err(PlanError::Constraint);
    }
    if !color_gate(request, input)? {
        return Ok(Plan::Ignore);
    }
    let (mut task, start) = hs_base(input)?;
    task.saturation = hs::SaturationAxis::move_to(
        start.saturation,
        saturation as f64,
        duration_ds as u64 * 100,
    );
    Ok(Plan::Execute {
        task: Task::Color(color::Task::Hs(task)),
    })
}

pub(super) fn move_saturation(
    mode: MoveMode,
    rate: u8,
    request: Options,
    input: &Input,
) -> Result<Plan, PlanError> {
    if !input.profile.has_rgb() {
        return Err(PlanError::Unsupported);
    }
    if !matches!(mode, MoveMode::Stop) && rate == 0 {
        return Err(PlanError::InvalidCommand);
    }
    if !color_gate(request, input)? {
        return Ok(Plan::Ignore);
    }
    if matches!(mode, MoveMode::Stop) {
        return Ok(stop_hs(input));
    }
    let (mut task, start) = hs_base(input)?;
    let rate = if matches!(mode, MoveMode::Up) {
        rate as f64
    } else {
        -(rate as f64)
    };
    task.saturation = hs::SaturationAxis::move_at(start.saturation, rate);
    Ok(Plan::Execute {
        task: Task::Color(color::Task::Hs(task)),
    })
}

pub(super) fn step_saturation(
    mode: StepMode,
    size: u8,
    duration_ds: u8,
    request: Options,
    input: &Input,
) -> Result<Plan, PlanError> {
    if !input.profile.has_rgb() {
        return Err(PlanError::Unsupported);
    }
    if size == 0 {
        return Err(PlanError::InvalidCommand);
    }
    if !color_gate(request, input)? {
        return Ok(Plan::Ignore);
    }
    let (mut task, start) = hs_base(input)?;
    let delta = if matches!(mode, StepMode::Up) {
        size as f64
    } else {
        -(size as f64)
    };
    task.saturation = hs::SaturationAxis::move_to(
        start.saturation,
        (start.saturation + delta).clamp(0.0, 254.0),
        duration_ds as u64 * 100,
    );
    Ok(Plan::Execute {
        task: Task::Color(color::Task::Hs(task)),
    })
}

pub(super) fn move_to_hue_and_saturation(
    hue: u8,
    saturation: u8,
    duration_ds: u16,
    request: Options,
    input: &Input,
) -> Result<Plan, PlanError> {
    if !input.profile.has_rgb() {
        return Err(PlanError::Unsupported);
    }
    if hue == u8::MAX || saturation == u8::MAX || duration_ds == u16::MAX {
        return Err(PlanError::Constraint);
    }
    if !color_gate(request, input)? {
        return Ok(Plan::Ignore);
    }
    let target = hs::Hs {
        hue: hue as f64,
        saturation: saturation as f64,
    };
    let task = if duration_ds == 0 {
        hs::HsTask::stationary(target)
    } else {
        let start = input.state.hs.ok_or(PlanError::InvalidState)?;
        hs::HsTask {
            hue: hs::HueAxis::move_to(
                start.hue,
                target.hue,
                hs::Direction::Shortest,
                duration_ds as u64 * 100,
            ),
            saturation: hs::SaturationAxis::move_to(
                start.saturation,
                target.saturation,
                duration_ds as u64 * 100,
            ),
        }
    };
    Ok(Plan::Execute {
        task: Task::Color(color::Task::Hs(task)),
    })
}

pub(super) fn move_to_color(
    x: u16,
    y: u16,
    duration_ds: u16,
    request: Options,
    input: &Input,
) -> Result<Plan, PlanError> {
    if !input.profile.has_rgb() {
        return Err(PlanError::Unsupported);
    }
    if x > XY_MAX || y > XY_MAX || duration_ds == u16::MAX {
        return Err(PlanError::Constraint);
    }
    if !color_gate(request, input)? {
        return Ok(Plan::Ignore);
    }
    let (start_x, start_y) = if duration_ds == 0 {
        (x, y)
    } else {
        input.state.xy.ok_or(PlanError::InvalidState)?
    };
    Ok(Plan::Execute {
        task: Task::Color(color::Task::XyMoveTo {
            start_x,
            start_y,
            target_x: x,
            target_y: y,
            transition_ms: duration_ds as u64 * 100,
        }),
    })
}

pub(super) fn move_color(
    rate_x: i16,
    rate_y: i16,
    request: Options,
    input: &Input,
) -> Result<Plan, PlanError> {
    if !input.profile.has_rgb() {
        return Err(PlanError::Unsupported);
    }
    if !color_gate(request, input)? {
        return Ok(Plan::Ignore);
    }
    if rate_x == 0 && rate_y == 0 {
        return Ok(Plan::Stop(LightDimension::Color));
    }
    let (start_x, start_y) = input.state.xy.ok_or(PlanError::InvalidState)?;
    Ok(Plan::Execute {
        task: Task::Color(color::Task::XyMove {
            start_x,
            start_y,
            rate_x,
            rate_y,
        }),
    })
}

pub(super) fn step_color(
    x: i16,
    y: i16,
    duration_ds: u16,
    request: Options,
    input: &Input,
) -> Result<Plan, PlanError> {
    if !input.profile.has_rgb() {
        return Err(PlanError::Unsupported);
    }
    if x == 0 && y == 0 {
        return Err(PlanError::InvalidCommand);
    }
    if duration_ds == u16::MAX {
        return Err(PlanError::Constraint);
    }
    if !color_gate(request, input)? {
        return Ok(Plan::Ignore);
    }
    let (start_x, start_y) = input.state.xy.ok_or(PlanError::InvalidState)?;
    let target_x = (start_x as i32 + x as i32).clamp(0, XY_MAX as i32) as u16;
    let target_y = (start_y as i32 + y as i32).clamp(0, XY_MAX as i32) as u16;
    Ok(Plan::Execute {
        task: Task::Color(color::Task::XyMoveTo {
            start_x,
            start_y,
            target_x,
            target_y,
            transition_ms: duration_ds as u64 * 100,
        }),
    })
}

pub(super) fn stop_move_step(request: Options, input: &Input) -> Result<Plan, PlanError> {
    if !color_gate(request, input)? {
        return Ok(Plan::Ignore);
    }
    Ok(Plan::Stop(LightDimension::Color))
}

pub(super) fn move_to_color_temperature(
    target: u16,
    duration_ds: u16,
    request: Options,
    input: &Input,
) -> Result<Plan, PlanError> {
    if target > XY_MAX || duration_ds == u16::MAX {
        return Err(PlanError::Constraint);
    }
    if !color_gate(request, input)? {
        return Ok(Plan::Ignore);
    }
    let (lo, hi) = input
        .profile
        .temperature_interval()
        .map(mired_bounds)
        .ok_or(PlanError::InvalidState)?;
    let target = target.clamp(lo, hi);
    let start = if duration_ds == 0 {
        input.state.temperature.unwrap_or(target)
    } else {
        input.state.temperature.ok_or(PlanError::InvalidState)?
    };
    Ok(Plan::Execute {
        task: Task::Color(color::Task::ColorTemperatureMoveTo {
            start,
            target_mireds: target,
            transition_ms: duration_ds as u64 * 100,
        }),
    })
}

fn temperature_bounds(profile: LightProfile, min: u16, max: u16) -> Result<(u16, u16), PlanError> {
    let (lo, hi) = profile
        .temperature_interval()
        .map(mired_bounds)
        .ok_or(PlanError::InvalidState)?;
    let lo = if min == 0 { lo } else { min.max(lo) };
    let hi = if max == 0 { hi } else { max.min(hi) };
    if lo > hi {
        return Err(PlanError::InvalidCommand);
    }
    Ok((lo, hi))
}

pub(super) fn move_color_temperature(
    mode: MoveMode,
    rate: u16,
    min: u16,
    max: u16,
    request: Options,
    input: &Input,
) -> Result<Plan, PlanError> {
    if min > XY_MAX || max > XY_MAX {
        return Err(PlanError::Constraint);
    }
    if !matches!(mode, MoveMode::Stop) && rate == 0 {
        return Err(PlanError::InvalidCommand);
    }
    if !color_gate(request, input)? {
        return Ok(Plan::Ignore);
    }
    if matches!(mode, MoveMode::Stop) {
        return Ok(Plan::Stop(LightDimension::Color));
    }
    let (lo, hi) = temperature_bounds(input.profile, min, max)?;
    let start = input.state.temperature.ok_or(PlanError::InvalidState)?;
    let rate_mireds_per_sec = if matches!(mode, MoveMode::Up) {
        rate as i32
    } else {
        -(rate as i32)
    };
    Ok(Plan::Execute {
        task: Task::Color(color::Task::ColorTemperatureMove {
            start,
            rate_mireds_per_sec,
            min_mireds: lo,
            max_mireds: hi,
        }),
    })
}

pub(super) fn step_color_temperature(
    mode: StepMode,
    size: u16,
    duration_ds: u16,
    min: u16,
    max: u16,
    request: Options,
    input: &Input,
) -> Result<Plan, PlanError> {
    if min > XY_MAX || max > XY_MAX {
        return Err(PlanError::Constraint);
    }
    if size == 0 {
        return Err(PlanError::InvalidCommand);
    }
    if duration_ds == u16::MAX {
        return Err(PlanError::Constraint);
    }
    if !color_gate(request, input)? {
        return Ok(Plan::Ignore);
    }
    let (lo, hi) = temperature_bounds(input.profile, min, max)?;
    let start = input.state.temperature.ok_or(PlanError::InvalidState)?;
    let target = if matches!(mode, StepMode::Up) {
        start.saturating_add(size.min(hi.saturating_sub(start)))
    } else {
        start.saturating_sub(size.min(start.saturating_sub(lo)))
    };
    // Matter 1.6 preserves the requested Step rate when a bound clips the
    // distance. XY Step intentionally retains its whole transition.
    let transition_ms =
        (duration_ds as u64 * 100 * start.abs_diff(target) as u64).div_ceil(size as u64);
    Ok(Plan::Execute {
        task: Task::Color(color::Task::ColorTemperatureMoveTo {
            start,
            target_mireds: target,
            transition_ms,
        }),
    })
}

/// On/Off planning including the OnLevel preset. Repeated On keeps the
/// existing level; repeated Off still cancels light actions through the
/// function command path.
pub(super) fn power(command: Command, input: &Input) -> Result<PowerPlan, PlanError> {
    let on = match command {
        Command::On => true,
        Command::Off => false,
        Command::Toggle => !input.state.power.ok_or(PlanError::InvalidState)?,
    };
    if let Some(preset) = input
        .settings
        .on_level
        .filter(|_| input.state.power != Some(on))
    {
        let target = if on { preset } else { 1 };
        return Ok(PowerPlan::Begin {
            task: level::Task::MoveToLevel {
                with_on_off: false,
                start: target,
                target,
                transition_ms: 0,
            },
            power: on,
        });
    }
    Ok(PowerPlan::Execute(if on {
        Command::On
    } else {
        Command::Off
    }))
}
