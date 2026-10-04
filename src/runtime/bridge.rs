use crate::{
    RuntimeError,
    device::{Device, DeviceRegistry, PhysicalDevice},
    matter::{Bridge, PairingEvent},
    storage::{Identity, Store},
    terminal::{self, AuthStatus},
};
use futures_lite::{
    future,
    io::{AsyncBufRead, AsyncWrite},
};
use std::future::Future;

pub struct DeviceSetup {
    pub device: Device,
    pub default_label: String,
}

pub struct BridgeOptions<'a> {
    pub store: &'a Store,
    pub identity: &'a Identity,
    pub status: &'a AuthStatus,
    pub port: u16,
}

/// Production and isolated tests assemble the same bridge and lifetime boundary.
pub async fn run_prepared_bridge(
    options: BridgeOptions<'_>,
    ready: impl Future<Output = Result<DeviceSetup, RuntimeError>>,
    source: impl Future<Output = Result<(), RuntimeError>>,
    input: impl AsyncBufRead + Unpin,
    output: impl AsyncWrite + Unpin,
    interrupt: impl Future<Output = Result<(), RuntimeError>>,
    pairing: impl Fn(PairingEvent) -> std::io::Result<()>,
) -> Result<(), RuntimeError> {
    let mut source = Box::pin(async {
        source.await?;
        Err::<(), RuntimeError>("Device source ended unexpectedly".into())
    });
    let mut interrupt = Box::pin(interrupt);
    let prepared = future::or(
        async {
            interrupt.as_mut().await?;
            Ok(None)
        },
        future::or(async { ready.await.map(Some) }, async {
            source.as_mut().await.map(|_| None)
        }),
    )
    .await?;
    let Some(prepared) = prepared else {
        return Ok(());
    };
    let physical = PhysicalDevice::from_single(prepared.device.clone(), prepared.default_label);
    let stored = options.store.devices().publish(physical.description())?;
    let registry = DeviceRegistry::new();
    registry.insert(physical);
    log::info!(
        "MiGate bridge_id={} device_id={} did={} endpoint={}",
        options.identity.bridge_id,
        stored.unique_id,
        stored.key.id,
        stored.endpoint
    );
    let bridge = Bridge::new(&registry, options.store, options.identity);
    let result = {
        let input = async {
            terminal::bridge::run_input(&prepared.device, options.status, input, output)
                .await
                .map_err(|error| format!("Terminal I/O failed: {error}"))?;
            log::info!("Terminal input ended; bridge is still running");
            future::pending::<Result<(), RuntimeError>>().await
        };
        future::or(
            interrupt.as_mut(),
            future::or(
                source.as_mut(),
                future::or(bridge.run(options.port, pairing), input),
            ),
        )
        .await
    };
    // Drop the actual runner before checking failures, including after Ctrl-C.
    drop(source);
    bridge.check_failure()?;
    result
}

pub fn default_device_label(model: &str, id: &str) -> String {
    let mut start = id.len().saturating_sub(16);
    while !id.is_char_boundary(start) {
        start += 1;
    }
    let suffix = &id[start..];
    let mut end = model.len().min(32 - suffix.len() - 3);
    while !model.is_char_boundary(end) {
        end -= 1;
    }
    format!("{} ({suffix})", &model[..end])
}
