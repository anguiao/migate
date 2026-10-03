#![recursion_limit = "256"]

use std::{env, process::ExitCode};

use async_signal::{Signal, Signals};
use futures_lite::{StreamExt, future, io::BufReader};
use migate::{
    RuntimeError,
    config::{AuthCommand, Command, Config},
    device::DeviceKey,
    runtime::{BridgeOptions, DeviceSetup, default_device_label, run_prepared_bridge},
    storage::{Store, XiaomiStore},
    terminal::{self, AuthStatus},
    xiaomi::{
        auth::{AuthService, AuthenticationState},
        cloud::CloudClient,
        discovery::Discovery,
        source::XiaomiSource,
    },
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
            let auth = AuthService::new(store.xiaomi(), CloudClient::new()?);
            run_bridge(
                store,
                identity,
                auth,
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

// Temporary selection for this validation round; protocols receive an ordinary DeviceKey.
const VALIDATION_DEVICE_DID: &str = "571329355";

fn run_bridge(
    store: Store,
    identity: migate::storage::Identity,
    auth: AuthService,
    executable: std::path::PathBuf,
    data_dir: std::path::PathBuf,
    matter_port: u16,
) -> Result<(), RuntimeError> {
    let report = auth.local_status()?;
    let unsigned = matches!(report.authentication, AuthenticationState::NotSignedIn);
    let status = AuthStatus::new(report, executable, data_dir);
    status.log(terminal::current_time());
    if unsigned {
        return Err("Xiaomi sign-in is required".into());
    }
    let (discovery, driver) = Discovery::start()?;
    let (source, ready) = XiaomiSource::new(
        store.clone(),
        CloudClient::new()?,
        discovery,
        DeviceKey::new("xiaomi", VALIDATION_DEVICE_DID),
    )?;
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
        let networking = future::or(
            async {
                source.run(|report| status.update_source(report)).await?;
                Err::<(), RuntimeError>("Xiaomi source ended unexpectedly".into())
            },
            async {
                driver.run().await?;
                Err::<(), RuntimeError>("Gateway discovery ended unexpectedly".into())
            },
        );
        run_prepared_bridge(
            BridgeOptions {
                store: &store,
                identity: &identity,
                status: &status,
                port: matter_port,
            },
            async {
                let prepared = ready.wait().await?;
                let default_label = default_device_label(
                    &prepared.metadata.model,
                    &prepared.device.description().key.id,
                );
                Ok(DeviceSetup {
                    device: prepared.device,
                    default_label,
                })
            },
            networking,
            BufReader::new(blocking::Unblock::new(std::io::stdin())),
            blocking::Unblock::new(std::io::stdout()),
            interrupt,
            |event| terminal::pairing::write_event(std::io::stdout().lock(), event),
        )
        .await
    })
}
