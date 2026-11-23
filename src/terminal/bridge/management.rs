use super::{AuthStatus, COMMAND_HELP, Context, Reply, is_command};
use crate::{
    RuntimeError,
    app::App,
    terminal::{
        current_time,
        management::{Listener, Output, Session, read_terminal},
    },
};
use futures_lite::{
    future,
    io::{AsyncBufRead, AsyncWrite},
};
use std::{cell::RefCell, io, rc::Rc};

enum BridgeState {
    Preparing(&'static str),
    Committed(Rc<App>),
}

pub(crate) struct Management<'a> {
    state: RefCell<BridgeState>,
    status: &'a AuthStatus,
}

impl<'a> Management<'a> {
    pub(crate) fn new(status: &'a AuthStatus) -> Self {
        Self {
            state: RefCell::new(BridgeState::Preparing("Preparing Xiaomi devices.")),
            status,
        }
    }

    pub(crate) fn wait_for_home(&self) {
        *self.state.borrow_mut() = BridgeState::Preparing("Waiting for Xiaomi home confirmation.");
    }

    pub(crate) fn publish(&self, app: Rc<App>) {
        *self.state.borrow_mut() = BridgeState::Committed(app);
    }

    pub(crate) async fn serve(
        &self,
        input: impl AsyncBufRead + Unpin,
        output: impl AsyncWrite + Unpin,
        listener: Option<&Listener<'_>>,
    ) -> Result<(), RuntimeError> {
        let foreground = async {
            let session = Session::default();
            let mut output = Output::terminal(output);
            read_terminal(input, async |words| {
                self.execute(words, &session, &mut output).await?;
                Ok(())
            })
            .await
            .map_err(|error| format!("Terminal I/O failed: {error}"))?;
            log::info!("Terminal input ended; bridge is still running");
            future::pending().await
        };
        let sockets = async {
            match listener {
                Some(listener) => listener.serve(self).await,
                None => future::pending().await,
            }
        };
        future::or(foreground, sockets).await
    }

    pub(crate) async fn execute(
        &self,
        words: &[&str],
        session: &Session,
        output: &mut Output<impl AsyncWrite + Unpin>,
    ) -> io::Result<bool> {
        let context = match &*self.state.borrow() {
            BridgeState::Preparing(phase) => Err(*phase),
            BridgeState::Committed(app) => Ok(Context::committed(app.clone(), self.status)),
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
            [verb, ..] if is_command(verb) => Reply::failure(self.status.preparation_reason(phase)),
            _ => Reply::failure(format!(
                "Invalid command usage.\n{COMMAND_HELP}\n{}",
                info()
            )),
        })
    }
}
