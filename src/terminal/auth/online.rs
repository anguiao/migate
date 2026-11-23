use super::{LOGOUT_MESSAGE, run_check, run_login};
use crate::{
    RuntimeError,
    config::AuthCommand,
    terminal::management::{Output, Session},
    xiaomi::auth::AuthService,
};
use futures_lite::future;
use std::{cell::Cell, io, path::Path};

/// Share one executor across the bridge's foreground and socket sessions.
pub(crate) struct OnlineAuth<'a> {
    service: &'a AuthService,
    executable: &'a Path,
    data_dir: &'a Path,
    active: Cell<Option<AuthCommand>>,
}

impl<'a> OnlineAuth<'a> {
    pub(crate) fn new(service: &'a AuthService, executable: &'a Path, data_dir: &'a Path) -> Self {
        Self {
            service,
            executable,
            data_dir,
            active: Cell::new(None),
        }
    }

    /// Notify the bridge at login commit and after credential maintenance.
    /// After logout commits, wait for the bridge to stop serving the account.
    pub(crate) async fn execute(
        &self,
        command: AuthCommand,
        session: &Session,
        output: &mut Output<'_>,
        credentials_changed: impl Fn(),
        stop_account: impl AsyncFnOnce() -> Result<(), RuntimeError>,
    ) -> io::Result<bool> {
        if let Some(active) = self.active.get() {
            output
                .write(
                    session,
                    format!(
                        "The auth {} command is in progress. Please retry shortly.\n",
                        active.as_str()
                    ),
                )
                .await?;
            return Ok(false);
        }
        self.active.set(Some(command));
        let _active = ActiveOperation(&self.active);

        let result = match command {
            AuthCommand::Login => self.login(session, output, credentials_changed).await,
            AuthCommand::Check => run_check(
                self.service,
                self.executable,
                self.data_dir,
                async |text| output.write(session, text).await,
                credentials_changed,
            )
            .await
            .map(|_| ()),
            AuthCommand::Logout => {
                async {
                    self.service.logout()?;
                    stop_account().await?;
                    output.write(session, LOGOUT_MESSAGE.into()).await?;
                    Ok(())
                }
                .await
            }
        };
        if let Err(error) = result {
            output.write(session, format!("{error}\n")).await?;
            return Ok(false);
        }
        Ok(true)
    }

    async fn login(
        &self,
        session: &Session,
        output: &mut Output<'_>,
        credentials_changed: impl Fn(),
    ) -> Result<(), RuntimeError> {
        let committed = Cell::new(false);
        let disconnected = async {
            session.disconnected().await;
            if committed.get() {
                return future::pending().await;
            }
            Err(io::Error::from(io::ErrorKind::BrokenPipe).into())
        };
        let login = async {
            run_login(
                self.service,
                self.executable,
                self.data_dir,
                async |text| output.write(session, text).await,
                || {
                    committed.set(true);
                    credentials_changed();
                },
                || {
                    if committed.get() {
                        credentials_changed();
                    }
                },
            )
            .await
            .map(|_| ())
        };
        future::or(disconnected, login).await
    }
}

struct ActiveOperation<'a>(&'a Cell<Option<AuthCommand>>);

impl Drop for ActiveOperation<'_> {
    fn drop(&mut self) {
        self.0.set(None);
    }
}
