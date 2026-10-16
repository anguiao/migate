use super::{
    BridgeOptions,
    bridge::{PreparedRuntime, run_lifecycle},
};
use crate::{
    RuntimeError, device::DeviceRegistry, matter::PairingEvent, publication::Publication,
    xiaomi::adapter::SourceHandle,
};
use futures_lite::io::{AsyncBufRead, AsyncWrite};
use std::{future::Future, rc::Rc};

/// Drive committed Xiaomi configurations while the source connections continue.
pub async fn run_xiaomi_bridge(
    options: BridgeOptions<'_>,
    ready: impl Future<Output = Result<(SourceHandle, DeviceRegistry), RuntimeError>>,
    source: impl Future<Output = Result<(), RuntimeError>>,
    input: impl AsyncBufRead + Unpin,
    output: impl AsyncWrite + Unpin,
    interrupt: impl Future<Output = Result<(), RuntimeError>>,
    pairing: impl Fn(PairingEvent) -> std::io::Result<()>,
) -> Result<(), RuntimeError> {
    let store = options.store.clone();
    let prepare = async {
        let (handle, _) = ready.await?;
        loop {
            let changed = handle.listen();
            if handle.snapshot().scope.is_some() {
                break;
            }
            changed.await;
        }
        let publication = Rc::new(Publication::open(handle.clone(), store)?);
        // Startup recovery applies revocation overlays before any old endpoint
        // can be served. Failure here leaves every control authority closed.
        if let Some(prepared) = publication.prepare()? {
            publication.apply(prepared)?;
        }
        Ok::<_, RuntimeError>(PreparedRuntime {
            registry: handle.registry(),
            publication: Some(publication),
        })
    };
    run_lifecycle(options, prepare, source, input, output, interrupt, pairing).await
}
