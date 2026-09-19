#![recursion_limit = "256"]

use std::{env, process::ExitCode};

use async_signal::{Signal, Signals};
use futures_lite::{StreamExt, future, io::BufReader};
use migate::{
    RuntimeError,
    config::{AuthCommand, Command, Config},
    matter::DeviceBridge,
    storage::{Store, XiaomiStore},
    terminal,
    xiaomi::{auth::AuthService, cloud::CloudClient, runtime::XiaomiRuntime},
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
        env::var_os("MIGATE_MATTER_PORT"),
        env::var_os("HOME"),
        &env::current_dir()?,
    )?;
    let store = Store::open(&config.data_dir)?;
    let identity = store.load_identity()?;
    log::info!("Data directory: {}", config.data_dir.display());
    let executable = env::current_exe()?;
    match config.command {
        Command::Auth(command) => run_auth(command, store.xiaomi(), &executable, &config.data_dir),
        Command::Bridge => {
            let runtime = XiaomiRuntime::new(store.clone())?;
            run_bridge(
                store,
                identity,
                runtime,
                executable,
                config.data_dir,
                config.matter_port,
            )
        }
    }
}

fn run_auth(
    command: AuthCommand,
    store: XiaomiStore,
    executable: &std::path::Path,
    data_dir: &std::path::Path,
) -> Result<(), RuntimeError> {
    if command == AuthCommand::Logout {
        return terminal::auth::logout(&store, std::io::stdout().lock());
    }
    let auth = AuthService::new(store, CloudClient::new()?);
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
                    &auth,
                    executable,
                    data_dir,
                    BufReader::new(blocking::Unblock::new(std::io::stdin())),
                    output,
                )
                .await
                .map(|_| ()),
                AuthCommand::Check => terminal::auth::check(&auth, executable, data_dir, output)
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
    runtime: XiaomiRuntime,
    executable: std::path::PathBuf,
    data_dir: std::path::PathBuf,
    matter_port: u16,
) -> Result<(), RuntimeError> {
    log::info!("MiGate bridge_id={}", identity.bridge_id);
    log::info!("endpoint 0=root, 1=Aggregator; real features use persisted endpoints from 2");
    terminal::log_report(
        &runtime.auth_report(),
        terminal::current_time(),
        &executable,
        &data_dir,
    );
    let service = runtime.service();
    let devices = store.devices();
    let bridge = DeviceBridge::new(&service, &identity, store);
    let mut signals = Signals::new([Signal::Int])?;
    future::block_on(async {
        let interrupt = async {
            signals
                .next()
                .await
                .ok_or("Ctrl-C signal stream ended unexpectedly")??;
            log::info!("Received Ctrl-C; stopping MiGate");
            runtime.stop();
            Ok(())
        };
        let input = async {
            terminal::bridge::run_input(
                &service,
                &devices,
                &runtime,
                &executable,
                &data_dir,
                BufReader::new(blocking::Unblock::new(std::io::stdin())),
                blocking::Unblock::new(std::io::stdout()),
            )
            .await?;
            log::info!("Terminal input ended; bridge is still running");
            future::pending::<Result<(), RuntimeError>>().await
        };
        let result = future::or(
            interrupt,
            future::or(
                bridge.run(matter_port, |event| {
                    terminal::pairing::write_event(std::io::stdout().lock(), event)
                }),
                future::or(async { runtime.run().await.map_err(Into::into) }, input),
            ),
        )
        .await;
        // Service futures have been dropped; recorded storage failures still take precedence.
        runtime.check_failure()?;
        bridge.check_failure()?;
        result
    })
}
