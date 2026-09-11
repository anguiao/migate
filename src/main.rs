#![recursion_limit = "256"]

use std::{env, future::Future, io::Write, process::ExitCode};

use async_signal::{Signal, Signals};
use futures_lite::{StreamExt, future, io::BufReader};
use migate::{
    RuntimeError,
    config::{AuthCommand, Command, Config},
    matter::Bridge,
    storage::Store,
    terminal::{self, AuthStatus},
    virtual_device::VirtualLight,
    xiaomi::{auth::AuthService, cloud::CloudClient},
};

fn main() -> ExitCode {
    env_logger::Builder::new()
        .filter_level(log::LevelFilter::Info)
        .target(env_logger::Target::Stderr)
        .init();
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("MiGate failed: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), RuntimeError> {
    let config = Config::parse(
        env::args_os().skip(1),
        env::var_os("MIGATE_DATA_DIR"),
        env::var_os("HOME"),
        &env::current_dir()?,
    )?;
    let store = Store::open(&config.data_dir)?;
    let identity = store.load_identity()?;
    log::info!("Data directory: {}", config.data_dir.display());
    let executable = env::current_exe()?;
    let auth = AuthService::new(store.xiaomi(), CloudClient::new()?);
    match config.command {
        Command::Auth(command) => run_auth(command, &auth, &executable, &config.data_dir),
        Command::Bridge => run_bridge(store, identity, auth, executable, config.data_dir),
    }
}

fn run_auth(
    command: AuthCommand,
    auth: &AuthService,
    executable: &std::path::Path,
    data_dir: &std::path::Path,
) -> Result<(), RuntimeError> {
    if command == AuthCommand::Logout {
        auth.logout()?;
        writeln!(std::io::stdout().lock(), "Xiaomi credentials removed.")?;
        return Ok(());
    }
    let mut signals = Signals::new([Signal::Int])?;
    future::block_on(async {
        let cancelled = async {
            signals
                .next()
                .await
                .ok_or("Ctrl-C signal stream ended unexpectedly")??;
            let message = match command {
                AuthCommand::Login => "Login cancelled by Ctrl-C",
                AuthCommand::Check => "Authentication check cancelled by Ctrl-C",
                AuthCommand::Logout => unreachable!(),
            };
            Err::<(), RuntimeError>(message.into())
        };
        let operation = async {
            let output = blocking::Unblock::new(std::io::stdout());
            match command {
                AuthCommand::Login => terminal::auth::login(
                    auth,
                    executable,
                    data_dir,
                    BufReader::new(blocking::Unblock::new(std::io::stdin())),
                    output,
                )
                .await
                .map(|_| ()),
                AuthCommand::Check => terminal::auth::check(auth, executable, data_dir, output)
                    .await
                    .map(|_| ()),
                AuthCommand::Logout => unreachable!(),
            }
        };
        future::or(cancelled, operation).await
    })
}

fn run_bridge(
    store: Store,
    identity: migate::storage::Identity,
    auth: AuthService,
    executable: std::path::PathBuf,
    data_dir: std::path::PathBuf,
) -> Result<(), RuntimeError> {
    log::info!(
        "MiGate bridge_id={} light_id={}",
        identity.bridge_id,
        identity.light_id
    );
    log::info!(
        "endpoint 0=root, 1=Aggregator, 2=MiGate Virtual Light (virtual-light-1); initial state: off"
    );
    let local_report = auth.local_status()?;
    let status = AuthStatus::new(local_report, executable, data_dir);
    writeln!(
        std::io::stdout().lock(),
        "{}",
        status.render(terminal::current_time())
    )?;
    let light = VirtualLight::new();
    let bridge = Bridge::new(&light, &identity, store.matter());
    let mut signals = Signals::new([Signal::Int])?;
    future::block_on(async {
        let interrupt = async {
            signals
                .next()
                .await
                .ok_or("Ctrl-C signal stream ended unexpectedly")??;
            log::info!("Received Ctrl-C; stopping MiGate");
            Ok(())
        };
        let input = async {
            terminal::bridge::run_input(
                &light,
                &status,
                BufReader::new(blocking::Unblock::new(std::io::stdin())),
                blocking::Unblock::new(std::io::stdout()),
            )
            .await
            .map_err(|e| format!("Terminal I/O failed: {e}"))?;
            log::info!("Terminal input ended; bridge is still running");
            future::pending::<Result<(), RuntimeError>>().await
        };
        let auth_check =
            run_once_then_pending(async { auth.check().await.map_err(Into::into) }, |report| {
                if status.replace(report) {
                    writeln!(
                        std::io::stdout().lock(),
                        "{}",
                        status.render(terminal::current_time())
                    )?;
                }
                Ok(())
            });
        let result = future::or(
            interrupt,
            future::or(bridge.run(), future::or(auth_check, input)),
        )
        .await;
        // Service futures have been dropped; a recorded storage failure still takes precedence.
        bridge.check_failure()?;
        result
    })
}

async fn run_once_then_pending<T>(
    task: impl Future<Output = Result<T, RuntimeError>>,
    completed: impl FnOnce(T) -> Result<(), RuntimeError>,
) -> Result<(), RuntimeError> {
    completed(task.await?)?;
    future::pending().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::Cell, rc::Rc};

    #[test]
    fn completed_background_task_stays_pending_but_errors_end_it() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let auth = AuthService::new(store.xiaomi(), CloudClient::new().unwrap());
        let callback_called = Rc::new(Cell::new(false));
        let completed = callback_called.clone();
        let ordinary_failure = run_once_then_pending(
            async { auth.check().await.map_err(Into::into) },
            move |report| {
                assert!(!report.is_success());
                completed.set(true);
                Ok(())
            },
        );
        let mut ordinary_failure = std::pin::pin!(ordinary_failure);
        assert!(future::block_on(future::poll_once(ordinary_failure.as_mut())).is_none());
        assert!(callback_called.get());

        let callback_called = Rc::new(Cell::new(false));
        let completed = callback_called.clone();
        let network_pending =
            run_once_then_pending(future::pending::<Result<(), RuntimeError>>(), move |_| {
                completed.set(true);
                Ok(())
            });
        let mut network_pending = std::pin::pin!(network_pending);
        assert!(future::block_on(future::poll_once(network_pending.as_mut())).is_none());
        assert!(!callback_called.get());

        let storage_error = run_once_then_pending(
            async { Err::<(), RuntimeError>("storage failed".into()) },
            |_| Ok(()),
        );
        let mut storage_error = std::pin::pin!(storage_error);
        let result = future::block_on(future::poll_once(storage_error.as_mut()));
        assert_eq!(result.unwrap().unwrap_err().to_string(), "storage failed");
    }

    #[test]
    fn winning_interrupt_drops_background_task() {
        struct Dropped(Rc<Cell<bool>>);
        impl Drop for Dropped {
            fn drop(&mut self) {
                self.0.set(true);
            }
        }

        let dropped = Rc::new(Cell::new(false));
        let wrote = Rc::new(Cell::new(false));
        let marker = Dropped(dropped.clone());
        let completion_write = wrote.clone();
        future::block_on(async {
            let background = run_once_then_pending(
                async move {
                    let _marker = marker;
                    future::pending::<Result<(), RuntimeError>>().await
                },
                move |_| {
                    completion_write.set(true);
                    Ok(())
                },
            );
            let result = future::or(async { Ok::<_, RuntimeError>(()) }, background).await;
            assert!(result.is_ok());
        });
        assert!(dropped.get());
        assert!(!wrote.get());
    }
}
