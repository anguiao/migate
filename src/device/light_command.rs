use super::{
    CommandError, Function, FunctionSnapshot, Rgb,
    command::{ActionToken, LightAction, LightDimension},
    state::RuntimeState,
};

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum LightColorTarget {
    Temperature(f64),
    Rgb(Rgb),
}

/// One non-empty light target with private fields: the target itself decides
/// its dimension, brightness and color can never be combined, and every
/// accepted numeric value has already been validated at this boundary.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LightTarget {
    power: Option<bool>,
    dimension: TargetDimension,
}
#[derive(Clone, Copy, Debug, PartialEq)]
enum TargetDimension {
    Brightness(f64),
    Color(LightColorTarget),
}
impl LightTarget {
    pub fn brightness(power: Option<bool>, percent: f64) -> Result<Self, CommandError> {
        super::model::validate_brightness(percent)?;
        Ok(Self {
            power,
            dimension: TargetDimension::Brightness(percent),
        })
    }
    pub fn temperature(power: Option<bool>, kelvin: f64) -> Result<Self, CommandError> {
        if !kelvin.is_finite() || kelvin <= 0.0 {
            return Err(CommandError::Rejected(
                "color temperature is not a positive finite value".into(),
            ));
        }
        Ok(Self {
            power,
            dimension: TargetDimension::Color(LightColorTarget::Temperature(kelvin)),
        })
    }
    pub fn rgb(power: Option<bool>, rgb: Rgb) -> Self {
        Self {
            power,
            dimension: TargetDimension::Color(LightColorTarget::Rgb(rgb)),
        }
    }
    pub fn power(self) -> Option<bool> {
        self.power
    }
    pub fn dimension(self) -> LightDimension {
        match self.dimension {
            TargetDimension::Brightness(_) => LightDimension::Brightness,
            TargetDimension::Color(_) => LightDimension::Color,
        }
    }
    pub fn brightness_percent(self) -> Option<f64> {
        match self.dimension {
            TargetDimension::Brightness(percent) => Some(percent),
            TargetDimension::Color(_) => None,
        }
    }
    pub fn color_target(self) -> Option<LightColorTarget> {
        match self.dimension {
            TargetDimension::Brightness(_) => None,
            TargetDimension::Color(color) => Some(color),
        }
    }
    /// A merged frame inherits an explicit earlier power target when omitted.
    pub(super) fn inherit_power(mut self, power: Option<bool>) -> Self {
        if self.power.is_none() {
            self.power = power;
        }
        self
    }
    /// Force the associated power of an otherwise valid target.
    pub(crate) fn with_power(mut self, power: Option<bool>) -> Self {
        self.power = power;
        self
    }
}

impl Function {
    /// Validate capability support at the command construction boundary. The
    /// target's numeric values and dimension were already validated by its
    /// constructor, so only the function's real kind is checked here.
    fn validate_light_target(&self, target: LightTarget) -> Result<(), CommandError> {
        let description = self.description();
        let Some(profile) = description.kind.light_profile() else {
            return Err(CommandError::Unsupported);
        };
        match target.dimension() {
            LightDimension::Brightness if !profile.has_brightness() => {
                return Err(CommandError::Unsupported);
            }
            LightDimension::Color => match target.color_target() {
                Some(LightColorTarget::Temperature(value)) => {
                    let Some(interval) = profile.temperature_interval() else {
                        return Err(CommandError::Unsupported);
                    };
                    if !((interval.min() as f64)..=(interval.max() as f64)).contains(&value) {
                        return Err(CommandError::Rejected(
                            "color temperature is outside its physical range".into(),
                        ));
                    }
                }
                Some(LightColorTarget::Rgb(_)) if !profile.has_rgb() => {
                    return Err(CommandError::Unsupported);
                }
                Some(LightColorTarget::Rgb(_)) => {}
                None => unreachable!("a color target always carries a color value"),
            },
            LightDimension::Brightness => {}
        }
        Ok(())
    }

    /// A physically constant trajectory can accept a source-confirmed no-write startup.
    pub(crate) fn begin_light_action_with_unchanged_startup(
        &self,
        target: LightTarget,
        final_frame: bool,
        allow_unchanged_startup: bool,
    ) -> Result<LightAction, CommandError> {
        self.validate_light_target(target)?;
        let mut state = self.0.state.lock().unwrap();
        self.validate_light_available(&state)?;
        let action = state
            .commands_mut()
            .expect("read-only functions have no light queue")
            .begin_light(target, final_frame, allow_unchanged_startup);
        self.notify_light_actions();
        Ok(action)
    }

    pub fn submit_light_frame(
        &self,
        token: ActionToken,
        target: LightTarget,
        final_frame: bool,
    ) -> Result<(), CommandError> {
        if target.dimension() != token.dimension() {
            return Err(CommandError::Rejected(
                "target dimension does not match the action".into(),
            ));
        }
        self.validate_light_target(target)?;
        let power = target.power();
        let mut state = self.0.state.lock().unwrap();
        self.validate_light_available(&state)?;
        state
            .commands_mut()
            .expect("read-only functions have no light queue")
            .submit_light_frame(token, target, final_frame)?;
        if power == Some(false) {
            self.notify_light_actions();
        }
        self.0.submitted.notify(usize::MAX);
        Ok(())
    }

    fn validate_light_available(&self, state: &RuntimeState) -> Result<(), CommandError> {
        let commands = state.commands().ok_or(CommandError::Unsupported)?;
        if commands.is_closed() {
            return Err(CommandError::Cancelled);
        }
        self.snapshot_locked(state).availability().command_result()
    }
    pub fn light_action_current(&self, token: ActionToken) -> bool {
        self.0
            .state
            .lock()
            .unwrap()
            .commands()
            .expect("read-only functions have no light queue")
            .light_action_current(token)
    }
    /// A completed final frame can finish its remaining writes until takeover or Stop.
    pub(crate) fn light_action_owns_writes(&self, token: ActionToken) -> bool {
        self.0
            .state
            .lock()
            .unwrap()
            .commands()
            .expect("read-only functions have no light queue")
            .light_action_owns_writes(token)
    }
    pub fn stop_light(&self, dimension: LightDimension) -> Result<FunctionSnapshot, CommandError> {
        self.cancel_light_dimension(dimension, CommandError::Stopped)
    }
    /// The adapter invokes takeover before applying a validated external report.
    pub fn take_over_light(
        &self,
        dimension: LightDimension,
    ) -> Result<FunctionSnapshot, CommandError> {
        self.cancel_light_dimension(dimension, CommandError::Superseded)
    }
    fn cancel_light_dimension(
        &self,
        dimension: LightDimension,
        error: CommandError,
    ) -> Result<FunctionSnapshot, CommandError> {
        let mut state = self.0.state.lock().unwrap();
        if !self.description().kind.is_light() {
            return Err(CommandError::Unsupported);
        }
        state
            .commands_mut()
            .expect("read-only functions have no light queue")
            .cancel_light(dimension, error);
        self.notify_light_actions();
        Ok(self.snapshot_locked(&state))
    }
    /// External off invalidates both dimensions and queued power operations.
    pub fn take_over_light_all(&self) -> Result<FunctionSnapshot, CommandError> {
        let mut state = self.0.state.lock().unwrap();
        if !self.description().kind.is_light() {
            return Err(CommandError::Unsupported);
        }
        state
            .commands_mut()
            .expect("read-only functions have no light queue")
            .cancel_pending_light(CommandError::Superseded);
        self.notify_light_actions();
        Ok(self.snapshot_locked(&state))
    }
    pub fn light_action_diagnostics(&self) -> Option<super::LightActionDiagnostics> {
        if !self.description().kind.is_light() {
            return None;
        }
        Some(
            self.0
                .state
                .lock()
                .unwrap()
                .commands()
                .expect("read-only functions have no light queue")
                .light_diagnostics(),
        )
    }
    pub fn subscribe_light_actions(&self) -> Option<LightActionChanges> {
        self.light_action_diagnostics()
            .map(|snapshot| LightActionChanges {
                function: self.clone(),
                revision: snapshot.revision,
            })
    }
    pub(super) fn notify_light_actions(&self) {
        self.0.light_changed.notify(usize::MAX);
        self.0.submitted.notify(usize::MAX);
    }
}

pub struct LightActionChanges {
    function: Function,
    revision: u64,
}
impl LightActionChanges {
    pub async fn changed(&mut self) -> super::LightActionDiagnostics {
        loop {
            let listener = self.function.0.light_changed.listen();
            let snapshot = self.function.light_action_diagnostics().unwrap();
            if snapshot.revision != self.revision {
                self.revision = snapshot.revision;
                return snapshot;
            }
            listener.await;
        }
    }
}
