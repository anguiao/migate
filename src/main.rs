#![recursion_limit = "256"]

use std::{env, process::ExitCode};

use futures_lite::{future, io::BufReader};
use migate::{
    RuntimeError,
    config::{AuthCommand, Command, Config},
    runtime::{BridgeOptions, run_prepared_bridge, shutdown_signal},
    storage::{InstanceLock, Store, XiaomiStore},
    terminal::{self, AuthStatus},
    xiaomi::{
        auth::{AuthService, AuthenticationState},
        cloud::CloudClient,
        discovery::Discovery,
        source::XiaomiSource,
    },
};

fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp_millis()
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
    let _instance = InstanceLock::acquire(&config.data_dir)?;
    let store = Store::open(&config.data_dir)?;
    let identity = store.load_identity()?;
    log::info!("Data directory: {}", config.data_dir.display());
    let executable = env::current_exe()?;
    match config.command {
        Command::Auth(command) => run_auth(command, store.xiaomi(), &executable, &config.data_dir),
        Command::Bridge => run_bridge(
            store,
            identity,
            executable,
            config.data_dir,
            config.matter_port,
        ),
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
    future::block_on(async {
        let cancelled = async {
            let signal = shutdown_signal().await?;
            let message = match command {
                AuthCommand::Login => format!("Login cancelled by {signal}"),
                AuthCommand::Check => format!("Authentication check cancelled by {signal}"),
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
    executable: std::path::PathBuf,
    data_dir: std::path::PathBuf,
    matter_port: u16,
) -> Result<(), RuntimeError> {
    let cloud = CloudClient::new()?;
    let auth = AuthService::new(store.xiaomi(), cloud.clone());
    let report = auth.local_status()?;
    if matches!(report.authentication, AuthenticationState::NotSignedIn) {
        return Err(format!(
            "Xiaomi sign-in is required. Sign in with:\n  {}",
            terminal::login_command(&executable, &data_dir)
        )
        .into());
    }
    let status = AuthStatus::new(report, executable, data_dir);
    status.log(terminal::current_time());
    let (discovery, driver) = Discovery::start()?;
    let (source, ready) = XiaomiSource::new(store.clone(), cloud, discovery)?;
    future::block_on(async {
        let interrupt = async {
            let signal = shutdown_signal().await?;
            log::info!("Received {signal}; stopping MiGate");
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
            async { Ok(ready.wait().await?) },
            networking,
            BufReader::new(blocking::Unblock::new(std::io::stdin())),
            blocking::Unblock::new(std::io::stdout()),
            interrupt,
            |event| terminal::pairing::write_event(std::io::stdout().lock(), event),
        )
        .await
    })
}
