use std::{env, process::ExitCode};

use async_signal::{Signal, Signals};
use futures_lite::{StreamExt, future, io::BufReader};
use migate::{
    RuntimeError, config::Config, matter::Bridge, storage::Store, terminal,
    virtual_device::VirtualLight,
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
    log::info!(
        "MiGate bridge_id={} light_id={}",
        identity.bridge_id,
        identity.light_id
    );
    log::info!(
        "endpoint 0=root, 1=Aggregator, 2=MiGate Virtual Light (virtual-light-1); initial state: off"
    );
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
            terminal::run_input(
                &light,
                BufReader::new(blocking::Unblock::new(std::io::stdin())),
                blocking::Unblock::new(std::io::stdout()),
            )
            .await
            .map_err(|e| format!("Terminal I/O failed: {e}"))?;
            log::info!("Terminal input ended; bridge is still running");
            future::pending::<Result<(), RuntimeError>>().await
        };
        let result = future::or(bridge.run(), future::or(interrupt, input)).await;
        // Service futures have been dropped; a recorded storage failure still takes precedence.
        bridge.check_failure()?;
        result
    })
}
