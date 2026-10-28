use std::{
    collections::VecDeque,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

use super::{
    CommandError, CommandReceiver, DeviceRequest, Function, FunctionSnapshot, command::Completion,
};

/// The logical request is kept separately from its quantized native write.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CurtainCommand {
    Target(f64),
    Stop,
}

struct Entry {
    command: CurtainCommand,
    completion: Completion,
    sent: AtomicBool,
    queued_at: Instant,
}

pub struct CurtainAction(Arc<Entry>);
impl CurtainAction {
    /// Acceptance is a transport receipt, not a physical observation.
    pub async fn accepted(&self) -> Result<FunctionSnapshot, CommandError> {
        self.0.completion.wait().await
    }
}

#[derive(Default)]
pub(super) struct CurtainCoordinator {
    pending: VecDeque<Arc<Entry>>,
    flight: Option<Arc<Entry>>,
    closed: bool,
}
impl CurtainCoordinator {
    fn submit(&mut self, command: CurtainCommand) -> CurtainAction {
        // A Stop removes all earlier unsent targets; a target replaces only
        // the adjacent target, never crossing an intervening Stop.
        if matches!(command, CurtainCommand::Stop) {
            self.pending.retain(|entry| {
                if matches!(entry.command, CurtainCommand::Target(_)) {
                    entry.completion.finish(Err(CommandError::Superseded));
                    false
                } else {
                    true
                }
            });
        } else if let Some(entry) = self
            .pending
            .pop_back_if(|entry| matches!(entry.command, CurtainCommand::Target(_)))
        {
            entry.completion.finish(Err(CommandError::Superseded));
        }
        if self.pending.is_empty()
            && let Some(flight) = &self.flight
            && matches!(flight.command, CurtainCommand::Target(_))
            && !flight.sent.load(Ordering::Relaxed)
        {
            flight.completion.finish(Err(CommandError::Superseded));
        }
        let entry = Arc::new(Entry {
            command,
            completion: Completion::default(),
            sent: AtomicBool::new(false),
            queued_at: Instant::now(),
        });
        self.pending.push_back(entry.clone());
        CurtainAction(entry)
    }

    pub(super) fn unavailable(&mut self, error: CommandError) {
        for entry in self.pending.drain(..) {
            entry.completion.finish(Err(error.clone()));
        }
        if let Some(flight) = &self.flight
            && !flight.sent.load(Ordering::Relaxed)
        {
            flight.completion.finish(Err(error));
        }
    }

    pub(super) fn close(&mut self, error: CommandError) {
        self.closed = true;
        self.unavailable(error);
    }
}

impl Function {
    pub(crate) fn pending_curtain_commands(&self) -> Vec<CurtainCommand> {
        let state = self.0.state.lock().unwrap();
        let Some(commands) = state.curtain_commands() else {
            return Vec::new();
        };
        commands
            .flight
            .iter()
            .filter(|entry| !entry.sent.load(Ordering::Relaxed) && !entry.completion.is_finished())
            .chain(commands.pending.iter())
            .map(|entry| entry.command)
            .collect()
    }

    /// Submit a logical target or explicit stop, in this function's order.
    pub fn begin_curtain(&self, command: CurtainCommand) -> Result<CurtainAction, CommandError> {
        if let CurtainCommand::Target(value) = command
            && (!value.is_finite() || !(0.0..=100.0).contains(&value))
        {
            return Err(CommandError::Rejected(
                "curtain target is outside 0..100 percent".into(),
            ));
        }
        let mut state = self.0.state.lock().unwrap();
        let commands = state.curtain_commands().ok_or(CommandError::Unsupported)?;
        if commands.closed {
            return Err(CommandError::Cancelled);
        }
        if matches!(command, CurtainCommand::Target(_)) {
            let changed = state.update_curtain_projection(|p| p.target_command());
            self.notify_curtain_projection(changed);
        }
        self.snapshot_locked(&state).availability.command_result()?;
        let action = state.curtain_commands_mut().unwrap().submit(command);
        if matches!(command, CurtainCommand::Stop) {
            let changed = state.update_curtain_projection(|p| p.clear_pending_target());
            self.notify_curtain_projection(changed);
        }
        self.0.submitted.notify(usize::MAX);
        Ok(action)
    }
}

impl CommandReceiver {
    pub(super) async fn next_curtain_operation(&mut self) -> Option<DeviceRequest> {
        let flight = self
            .function
            .0
            .state
            .lock()
            .unwrap()
            .curtain_commands()
            .unwrap()
            .flight
            .clone();
        if let Some(flight) = flight {
            let _ = flight.completion.wait().await;
            let mut state = self.function.0.state.lock().unwrap();
            let commands = state.curtain_commands_mut().unwrap();
            if commands
                .flight
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, &flight))
            {
                commands.flight = None;
            }
        }
        loop {
            let listener = self.function.0.submitted.listen();
            {
                let mut state = self.function.0.state.lock().unwrap();
                let commands = state.curtain_commands_mut().unwrap();
                if let Some(entry) = commands.pending.pop_front() {
                    commands.flight = Some(entry.clone());
                    return Some(DeviceRequest::Curtain(CurtainRequest {
                        command: entry.command,
                        function: self.function.clone(),
                        entry,
                    }));
                }
                if commands.closed {
                    return None;
                }
            }
            listener.await;
        }
    }
}

/// The original receipt retains the gate once transmission is possible.
pub struct CurtainRequest {
    pub command: CurtainCommand,
    function: Function,
    entry: Arc<Entry>,
}
impl CurtainRequest {
    pub(crate) fn queued_at(&self) -> Instant {
        self.entry.queued_at
    }

    pub fn current_result(&self) -> Result<(), CommandError> {
        match self.entry.completion.result() {
            Some(Err(error)) => Err(error),
            Some(Ok(_)) => Err(CommandError::Cancelled),
            None => Ok(()),
        }
    }

    /// Mark at the actual transport boundary, under the same lock as takeover.
    pub fn mark_sent(&self) -> Result<(), CommandError> {
        let _state = self.function.0.state.lock().unwrap();
        self.current_result()?;
        self.entry.sent.store(true, Ordering::Relaxed);
        Ok(())
    }

    pub fn accept(self) -> Result<FunctionSnapshot, CommandError> {
        let state = self.function.0.state.lock().unwrap();
        if let Some(result) = self.entry.completion.result() {
            return result;
        }
        if !self.entry.sent.load(Ordering::Relaxed) {
            let error = CommandError::SendFailed("curtain request was not submitted".into());
            self.entry.completion.finish(Err(error.clone()));
            return Err(error);
        }
        let result = Ok(self.function.snapshot_locked(&state));
        self.entry.completion.finish(result.clone());
        result
    }

    pub fn reject(self, error: CommandError) {
        let _state = self.function.0.state.lock().unwrap();
        self.entry.completion.finish(Err(error));
    }
}
impl Drop for CurtainRequest {
    fn drop(&mut self) {
        if !self.entry.completion.is_finished() {
            let _state = self.function.0.state.lock().unwrap();
            self.entry.completion.finish(Err(CommandError::Cancelled));
        }
    }
}
