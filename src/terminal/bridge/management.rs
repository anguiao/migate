use super::{AuthStatus, COMMAND_HELP, Context, Reply, is_command};
use crate::{
    RuntimeError,
    app::App,
    matter::PairingControl,
    terminal::{
        current_time,
        management::{self, Listener, Output, Session},
        pairing::write_status,
    },
};
use futures_lite::io::{AsyncBufRead, AsyncWrite};
use std::{cell::RefCell, io, rc::Rc};

enum BridgeState {
    Preparing(&'static str),
    Committed {
        app: Rc<App>,
        pairing: Option<PairingControl>,
    },
}

pub(crate) const WAITING_FOR_LOGIN: &str = "Waiting for Xiaomi sign-in.";
const PREPARING_DEVICES: &str = "Preparing Xiaomi devices.";
const WAITING_FOR_HOME: &str = "Waiting for Xiaomi home confirmation.";
const WAITING_FOR_READS: &str = "Waiting for initial Xiaomi device reads.";

pub(crate) struct Management<'a> {
    state: RefCell<BridgeState>,
    status: &'a AuthStatus,
}

impl<'a> Management<'a> {
    pub(crate) fn new(status: &'a AuthStatus) -> Self {
        Self {
            state: RefCell::new(BridgeState::Preparing(PREPARING_DEVICES)),
            status,
        }
    }

    pub(crate) fn wait_for_login(&self) {
        *self.state.borrow_mut() = BridgeState::Preparing(WAITING_FOR_LOGIN);
        self.status.signed_out();
    }

    pub(crate) fn prepare_devices(&self) {
        *self.state.borrow_mut() = BridgeState::Preparing(PREPARING_DEVICES);
    }

    pub(crate) fn wait_for_home(&self) {
        *self.state.borrow_mut() = BridgeState::Preparing(WAITING_FOR_HOME);
    }

    pub(crate) fn publish(&self, app: Rc<App>) {
        *self.state.borrow_mut() = BridgeState::Committed { app, pairing: None };
    }

    pub(crate) fn set_pairing(&self, control: PairingControl) {
        if let BridgeState::Committed { pairing, .. } = &mut *self.state.borrow_mut() {
            *pairing = Some(control);
        }
    }

    pub(crate) async fn serve(
        &self,
        input: impl AsyncBufRead + Unpin,
        output: impl AsyncWrite + Unpin,
        listener: Option<&Listener<'_>>,
    ) -> Result<(), RuntimeError> {
        management::serve(input, output, listener, async |words, session, output| {
            self.execute(words, session, output).await
        })
        .await
    }

    pub(crate) async fn execute(
        &self,
        words: &[&str],
        session: &Session,
        output: &mut Output<'_>,
    ) -> io::Result<bool> {
        if matches!(words, ["pairing"] | ["pairing", "open"]) {
            let (success, text) = match self.pairing(words.len() == 2).await {
                Ok(text) => (true, text),
                Err(error) => (false, format!("{error}\n")),
            };
            output.write(session, text).await?;
            return Ok(success);
        }
        let context = match &*self.state.borrow() {
            BridgeState::Preparing(phase) => Err(*phase),
            BridgeState::Committed { app, .. } => Ok(Context::committed(app.clone(), self.status)),
        };
        let reply = match context {
            Ok(context) => context
                .handle_words(words)
                .await
                .unwrap_or_else(|error| Some(Reply::failure(error.to_string()))),
            Err(phase) => self.preparing(phase, words),
        };
        let Some(reply) = reply else {
            return Ok(true);
        };
        output.write(session, format!("{}\n", reply.text)).await?;
        Ok(reply.success)
    }

    async fn pairing(&self, open: bool) -> Result<String, RuntimeError> {
        let control = match &*self.state.borrow() {
            BridgeState::Preparing(phase) => {
                return Err(self.status.preparation_reason(phase).into());
            }
            BridgeState::Committed { pairing, .. } => pairing.clone().ok_or(WAITING_FOR_READS)?,
        };
        let result = if open {
            control.open().await
        } else {
            control.status().await
        };
        if let BridgeState::Preparing(phase) = &*self.state.borrow() {
            return Err(self.status.preparation_reason(phase).into());
        }
        let mut rendered = Vec::new();
        let (executable, data_dir) = self.status.invocation_paths();
        write_status(&mut rendered, result?, executable, data_dir)?;
        Ok(String::from_utf8(rendered).expect("pairing status is UTF-8"))
    }

    fn preparing(&self, phase: &str, words: &[&str]) -> Option<Reply> {
        let info = || {
            format!(
                "{}\n{}",
                self.status.preparation_reason(phase),
                self.status.render(current_time())
            )
        };
        Some(match words {
            [] => return None,
            ["status"] | ["status", _] => Reply::success(info()),
            ["help"] => Reply::success(format!("{COMMAND_HELP}\n{}", info())),
            [verb, ..] if *verb != "pairing" && is_command(verb) => {
                Reply::failure(self.status.preparation_reason(phase))
            }
            _ => Reply::failure(format!(
                "Invalid command usage.\n{COMMAND_HELP}\n{}",
                info()
            )),
        })
    }
}
