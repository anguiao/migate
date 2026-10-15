use super::BridgeOptions;
use crate::{
    RuntimeError,
    device::DeviceRegistry,
    matter::{Bridge, PairingEvent},
    publication::Publication,
    terminal,
    xiaomi::adapter::SourceHandle,
};
use futures_lite::{
    future,
    io::{AsyncBufRead, AsyncWrite},
};
use std::{
    future::Future,
    rc::Rc,
    time::{Duration, Instant},
};

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
    let mut source = Box::pin(async {
        source.await?;
        Err::<(), RuntimeError>("Device source ended unexpectedly".into())
    });
    let mut interrupt = Box::pin(interrupt);
    let preparation = async {
        let (handle, _) = ready.await?;
        loop {
            let changed = handle.listen();
            if handle.snapshot().scope.is_some() {
                break;
            }
            changed.await;
        }
        let publication = Rc::new(Publication::open(handle.clone(), options.store.clone())?);
        // Startup recovery applies revocation overlays before any old endpoint
        // can be served. Failure here leaves every control authority closed.
        if let Some(prepared) = publication.prepare()? {
            publication.apply(prepared)?;
        }
        Ok::<_, RuntimeError>((publication, handle))
    };
    let publication = future::or(
        async {
            interrupt.as_mut().await?;
            Ok(None)
        },
        future::or(
            async {
                source.as_mut().await?;
                Ok(None)
            },
            async { preparation.await.map(Some) },
        ),
    )
    .await?;
    let Some((publication, handle)) = publication else {
        return Ok(());
    };
    options.status.set_publication(publication.clone());
    let registry = handle.registry();
    let mut input = Box::pin(async {
        terminal::bridge::run_input(
            &registry,
            &options.store.devices(),
            options.status,
            input,
            output,
        )
        .await?;
        log::info!("Terminal input ended; bridge is still running");
        future::pending::<Result<(), RuntimeError>>().await
    });
    let mut retry_at = None;
    let mut retry_delay = Duration::from_secs(1);
    loop {
        let effective = publication.current();
        let bridge = Bridge::new(&effective.published, options.store, options.identity);
        let mut runner = Box::pin(bridge.run(options.port, &pairing));
        loop {
            let changed = publication.listen();
            let retry = async {
                if let Some(at) = retry_at {
                    async_io::Timer::at(at).await;
                } else {
                    future::pending::<()>().await;
                }
            };
            enum Wake {
                Stop,
                Configuration,
                Retry,
            }
            let wake = future::or(
                async {
                    interrupt.as_mut().await?;
                    Ok::<_, RuntimeError>(Wake::Stop)
                },
                future::or(
                    async {
                        source.as_mut().await?;
                        Ok(Wake::Stop)
                    },
                    future::or(
                        async {
                            input.as_mut().await?;
                            Ok(Wake::Stop)
                        },
                        future::or(
                            async {
                                runner.as_mut().await?;
                                Err("Matter instance ended unexpectedly".into())
                            },
                            future::or(
                                async {
                                    changed.await;
                                    Ok(Wake::Configuration)
                                },
                                async {
                                    retry.await;
                                    Ok(Wake::Retry)
                                },
                            ),
                        ),
                    ),
                ),
            )
            .await;
            let wake = match wake {
                Ok(wake) => wake,
                Err(error) => {
                    publication.stop_actions();
                    drop(runner);
                    bridge.check_failure()?;
                    return Err(error);
                }
            };
            if matches!(wake, Wake::Stop) {
                publication.stop_actions();
                drop(runner);
                bridge.check_failure()?;
                return Ok(());
            }
            if matches!(wake, Wake::Configuration) {
                retry_delay = Duration::from_secs(1);
            }
            let prepared = match publication.prepare() {
                Ok(prepared) => prepared,
                Err(error) => {
                    log::warn!(
                        "Configuration prepare failed; keeping committed configuration: {error}"
                    );
                    publication.fail(error.to_string());
                    retry_at = Some(Instant::now() + retry_delay);
                    retry_delay = (retry_delay * 2).min(Duration::from_secs(60));
                    continue;
                }
            };
            let Some(prepared) = prepared else {
                retry_at = None;
                continue;
            };
            if prepared.rebuild {
                publication.stop_actions();
                drop(runner);
                bridge.check_failure()?;
                // Dropping the actual runner releases transport, IM, responder,
                // handlers and socket. Only now reacquire the latest inputs.
                let latest = match publication.prepare() {
                    Ok(latest) => latest,
                    Err(error) => {
                        log::warn!(
                            "Late configuration prepare failed; restarting the committed configuration: {error}"
                        );
                        publication.fail(error.to_string());
                        retry_at = Some(Instant::now() + retry_delay);
                        retry_delay = (retry_delay * 2).min(Duration::from_secs(60));
                        break;
                    }
                };
                if let Some(latest) = latest {
                    match publication.apply(latest) {
                        Ok(()) => {
                            retry_at = None;
                            retry_delay = Duration::from_secs(1);
                        }
                        Err(error) => {
                            log::warn!(
                                "Configuration apply failed; keeping committed configuration: {error}"
                            );
                            retry_at = Some(Instant::now() + retry_delay);
                            retry_delay = (retry_delay * 2).min(Duration::from_secs(60));
                        }
                    }
                } else {
                    retry_at = None;
                }
                break;
            }
            match publication.apply(prepared) {
                Ok(()) => {
                    retry_at = None;
                    retry_delay = Duration::from_secs(1);
                }
                Err(error) => {
                    log::warn!(
                        "Configuration apply failed; keeping committed configuration: {error}"
                    );
                    retry_at = Some(Instant::now() + retry_delay);
                    retry_delay = (retry_delay * 2).min(Duration::from_secs(60));
                }
            }
        }
    }
}
