use std::{env, process::ExitCode};

use async_signal::{Signal, Signals};
use futures_lite::{StreamExt, future, io::BufReader};
use migate::{config::Config, matter, storage::Store, terminal, virtual_device::VirtualLight};

fn main() -> ExitCode {
    env_logger::Builder::new()
        .filter_level(log::LevelFilter::Info)
        .target(env_logger::Target::Stderr)
        .init();
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("MiGate 失败：{error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), matter::RuntimeError> {
    let config = Config::parse(
        env::args_os().skip(1),
        env::var_os("MIGATE_DATA_DIR"),
        env::var_os("HOME"),
        &env::current_dir()?,
    )?;
    let store = Store::open(&config.data_dir)?;
    log::info!("数据目录：{}", store.directory().display());
    log::info!(
        "MiGate bridge_id={} light_id={}",
        store.identity().bridge_id,
        store.identity().light_id
    );
    log::info!("endpoint 0=root, 1=Aggregator, 2=MiGate 虚拟灯 virtual-light-1；初始状态 off");
    let light = VirtualLight::new();
    let mut signals = Signals::new([Signal::Int])?;
    future::block_on(async {
        let interrupt = async {
            signals.next().await.ok_or("Ctrl-C 信号流意外结束")??;
            log::info!("收到 Ctrl-C，正在停止 MiGate");
            Ok(())
        };
        let input = async {
            terminal::run_input(
                &light,
                BufReader::new(blocking::Unblock::new(std::io::stdin())),
                blocking::Unblock::new(std::io::stdout()),
            )
            .await
            .map_err(|e| format!("终端输入输出失败：{e}"))?;
            future::pending::<Result<(), matter::RuntimeError>>().await
        };
        matter::run(&light, store, future::or(interrupt, input)).await
    })
}
