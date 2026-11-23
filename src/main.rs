#![recursion_limit = "256"]

use std::{
    env,
    fs::File,
    io::{self, IsTerminal},
    os::{
        fd::{AsFd, AsRawFd},
        unix::fs::FileTypeExt,
    },
    path::Path,
    process::ExitCode,
    time::Duration,
};

use async_io::Async;
use futures_lite::future;
use migate::{
    RuntimeError,
    app::{BridgeOptions, BridgeTerminal, run_managed_xiaomi_bridge, shutdown_signal},
    config::{AuthCommand, Command, Config},
    storage::{Identity, InstanceLock, Store, XiaomiStore},
    terminal::{
        self, AuthStatus,
        management::{Client, Listener, stdin_input},
    },
    xiaomi::{auth::AuthService, cloud::CloudClient},
};
use mio::{Events, Interest, Poll, Token, unix::SourceFd};

fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp_millis()
        .target(env_logger::Target::Stderr)
        .init();
    match run() {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(error) => {
            eprintln!("MiGate failed: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<bool, RuntimeError> {
    let config = Config::parse(
        env::args_os().skip(1),
        env::var_os("MIGATE_DATA_DIR"),
        env::var_os("MIGATE_MATTER_PORT"),
        env::var_os("HOME"),
        &env::current_dir()?,
    )?;
    let executable = env::current_exe()?;
    let data_dir = &config.data_dir;
    match config.command {
        Command::Bridge => run_bridge(&executable, data_dir, config.matter_port).map(|()| true),
        Command::Auth(command) => run_auth(command, &executable, data_dir),
        Command::Attach => run_client(future::or(
            async { shutdown_signal().await.map(|_| true) },
            async {
                let mut client = connect(&executable, data_dir).await?;
                let stdin = std::io::stdin();
                let interactive = stdin.is_terminal();
                let input = stdin_input(stdin);
                let output = blocking::Unblock::new(std::io::stdout());
                if interactive {
                    client.attach_interactive(input, output).await?;
                } else {
                    client.attach(input, output).await?;
                }
                Ok(true)
            },
        )),
        Command::Online(words) => run_client(async {
            let mut client = connect(&executable, data_dir).await?;
            let words = words.iter().map(String::as_str).collect::<Vec<_>>();
            client
                .execute(&words, blocking::Unblock::new(std::io::stdout()))
                .await
        }),
    }
}

fn run_client<T>(
    command: impl Future<Output = Result<T, RuntimeError>>,
) -> Result<T, RuntimeError> {
    let output_closed = async {
        let output = File::from(io::stdout().as_fd().try_clone_to_owned()?);
        let kind = output.metadata()?.file_type();
        if !kind.is_fifo() && !kind.is_socket() {
            return future::pending().await;
        }

        // Without a PTY, SSH can close its output reader while the client is
        // waiting for the bridge. Only a write-side failure ends the session.
        let mut poll = Poll::new()?;
        poll.registry().register(
            &mut SourceFd(&output.as_raw_fd()),
            Token(0),
            Interest::WRITABLE,
        )?;
        let ready = Async::new_nonblocking(poll.registry().try_clone()?)?;
        let mut events = Events::with_capacity(1);
        loop {
            ready.readable().await?;
            match poll.poll(&mut events, Some(Duration::ZERO)) {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                result => result?,
            }
            if events
                .iter()
                .any(|event| event.is_write_closed() || event.is_error())
            {
                return Err(io::Error::from(io::ErrorKind::BrokenPipe).into());
            }
        }
    };
    future::block_on(future::or(command, output_closed))
}

async fn connect(executable: &Path, data_dir: &Path) -> Result<Client, RuntimeError> {
    Client::connect(data_dir).await?.ok_or_else(|| {
        format!(
            "No MiGate bridge is running in this data directory. Run this command to start it.\n  {}",
            terminal::bridge_command(executable, data_dir)
        ).into()
    })
}

fn open_store(data_dir: &Path) -> Result<(Store, Identity), RuntimeError> {
    let store = Store::open(data_dir)?;
    let identity = store.load_identity()?;
    log::info!("Data directory: {}", data_dir.display());
    Ok((store, identity))
}

fn run_auth(
    command: AuthCommand,
    executable: &Path,
    data_dir: &Path,
) -> Result<bool, RuntimeError> {
    let interrupted = async {
        let signal = shutdown_signal().await?;
        Err(format!("Online authentication command interrupted by {signal}").into())
    };
    let online = async {
        let Some(mut client) = Client::connect(data_dir).await? else {
            return Ok(None);
        };
        client
            .execute(
                &["auth", command.as_str()],
                blocking::Unblock::new(std::io::stdout()),
            )
            .await
            .map(Some)
    };
    if let Some(success) = run_client(future::or(interrupted, online))? {
        return Ok(success);
    }

    let _instance = InstanceLock::try_acquire(data_dir)?
        .ok_or("Another MiGate process is using this data directory. Please retry shortly.")?;
    let (store, _identity) = open_store(data_dir)?;
    run_offline_auth(command, store.xiaomi(), executable, data_dir).map(|()| true)
}

fn run_offline_auth(
    command: AuthCommand,
    store: XiaomiStore,
    executable: &std::path::Path,
    data_dir: &std::path::Path,
) -> Result<(), RuntimeError> {
    if command == AuthCommand::Logout {
        return terminal::auth::logout(&store, std::io::stdout().lock());
    }
    let auth = AuthService::new(store, CloudClient::new()?)?;
    future::block_on(async {
        let cancelled = async { shutdown_signal().await.map(Some) };
        let operation = async {
            let output = blocking::Unblock::new(std::io::stdout());
            match command {
                AuthCommand::Login => terminal::auth::login(&auth, executable, data_dir, output)
                    .await
                    .map(|_| ()),
                AuthCommand::Check => terminal::auth::check(&auth, executable, data_dir, output)
                    .await
                    .map(|_| ()),
                AuthCommand::Logout => unreachable!(),
            }?;
            Ok::<Option<&'static str>, RuntimeError>(None)
        };
        let Some(signal) = future::or(cancelled, operation).await? else {
            return Ok(());
        };
        let message = match command {
            AuthCommand::Login => {
                terminal::auth::report_local_status(
                    &auth,
                    executable,
                    data_dir,
                    blocking::Unblock::new(std::io::stdout()),
                )
                .await?;
                format!("Login cancelled by {signal}")
            }
            AuthCommand::Check => format!("Authentication check cancelled by {signal}"),
            AuthCommand::Logout => unreachable!(),
        };
        Err(message.into())
    })
}

fn run_bridge(executable: &Path, data_dir: &Path, matter_port: u16) -> Result<(), RuntimeError> {
    let instance = InstanceLock::try_acquire(data_dir)?.ok_or_else(|| {
        format!(
            "Another MiGate process is using this data directory. To connect to a running bridge, run this command.\n  {} attach",
            terminal::bridge_command(executable, data_dir)
        )
    })?;
    let listener = Listener::bind(&instance)?;
    let (store, identity) = open_store(data_dir)?;
    let cloud = CloudClient::new()?;
    let auth = AuthService::new(store.xiaomi(), cloud.clone())?;
    let report = auth.local_status()?;
    let status = AuthStatus::new(report, executable.to_owned(), data_dir.to_owned());
    status.log(terminal::current_time());
    future::block_on(async {
        let interrupt = async {
            let signal = shutdown_signal().await?;
            log::info!("Received {signal}; stopping MiGate");
            Ok(())
        };
        run_managed_xiaomi_bridge(
            BridgeOptions {
                store: &store,
                identity: &identity,
                status: &status,
                port: matter_port,
            },
            cloud,
            &auth,
            BridgeTerminal {
                input: stdin_input(std::io::stdin()),
                output: blocking::Unblock::new(std::io::stdout()),
                listener: Some(&listener),
            },
            interrupt,
            |event| terminal::pairing::write_event(std::io::stdout().lock(), event),
        )
        .await
    })
}
