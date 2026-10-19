use crate::{
    RuntimeError,
    device::DeviceRegistry,
    matter::{Advertisement, Bridge, PairingEvent},
    publication::Publication,
    storage::{Identity, Store},
    terminal::{self, AuthStatus},
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

pub struct BridgeOptions<'a> {
    pub store: &'a Store,
    pub identity: &'a Identity,
    pub status: &'a AuthStatus,
    pub port: u16,
}

/// A ready runtime: the served registry plus the optional committed
/// configuration that production monitors for rebuilds.
pub(super) struct PreparedRuntime {
    pub(super) registry: DeviceRegistry,
    pub(super) publication: Option<Rc<Publication>>,
}

/// The one bridge lifecycle shared by production and isolated tests.
///
/// The startup race here lets ready, source failure and interrupt settle
/// before serving. Once ready wins, this function races the source, terminal
/// input, interrupt and the real Matter runner; a committed publication
/// additionally drives configuration rebuilds and bounded retries.
pub(super) async fn run_lifecycle(
    options: BridgeOptions<'_>,
    prepare: impl Future<Output = Result<PreparedRuntime, RuntimeError>>,
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
        future::or(
            async {
                source.as_mut().await?;
                Ok(None)
            },
            async { prepare.await.map(Some) },
        ),
    )
    .await?;
    let Some(prepared) = prepared else {
        return Ok(());
    };
    let PreparedRuntime {
        registry,
        publication,
    } = prepared;
    let terminal = match &publication {
        Some(publication) => terminal::bridge::Context::configured(
            publication.clone(),
            &options.store.devices(),
            options.status,
        ),
        None => {
            terminal::bridge::Context::fixed(&registry, &options.store.devices(), options.status)
        }
    };
    log::info!("MiGate bridge_id={}", options.identity.bridge_id);
    let mut input = Box::pin(async {
        terminal
            .run_input(input, output)
            .await
            .map_err(|error| format!("Terminal I/O failed: {error}"))?;
        log::info!("Terminal input ended; bridge is still running");
        future::pending::<Result<(), RuntimeError>>().await
    });
    let mut retry_at = None;
    let mut retry_delay = Duration::from_secs(1);
    // Keep unchanged mDNS registrations while replacing the protocol instance.
    // astro-dnssd unregisters asynchronously, so immediate re-registration of
    // the same name can otherwise overlap its previous registration.
    let mut advertisement = Advertisement::new(options.port);
    loop {
        let effective = publication
            .as_ref()
            .map(|publication| publication.current());
        let bridge = Bridge::new(
            match &effective {
                Some(effective) => &effective.published,
                None => &registry,
            },
            options.store,
            options.identity,
        );
        let mut runner = Box::pin(bridge.run_with_advertisement(&mut advertisement, &pairing));
        loop {
            let changed = async {
                match &publication {
                    Some(publication) => publication.listen().await,
                    None => future::pending().await,
                }
            };
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
                    if let Some(publication) = &publication {
                        publication.stop_actions();
                    }
                    drop(runner);
                    drop(source);
                    bridge.check_failure()?;
                    return Err(error);
                }
            };
            if matches!(wake, Wake::Stop) {
                if let Some(publication) = &publication {
                    publication.stop_actions();
                }
                drop(runner);
                drop(source);
                bridge.check_failure()?;
                return Ok(());
            }
            if matches!(wake, Wake::Configuration) {
                retry_delay = Duration::from_secs(1);
            }
            let publication = publication
                .as_ref()
                .expect("static runtime cannot wake without a committed publication");
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

/// Isolated tests assemble a plain registry; it shares the lifecycle above.
pub async fn run_prepared_bridge(
    options: BridgeOptions<'_>,
    ready: impl Future<Output = Result<DeviceRegistry, RuntimeError>>,
    source: impl Future<Output = Result<(), RuntimeError>>,
    input: impl AsyncBufRead + Unpin,
    output: impl AsyncWrite + Unpin,
    interrupt: impl Future<Output = Result<(), RuntimeError>>,
    pairing: impl Fn(PairingEvent) -> std::io::Result<()>,
) -> Result<(), RuntimeError> {
    run_lifecycle(
        options,
        async {
            ready.await.map(|registry| PreparedRuntime {
                registry,
                publication: None,
            })
        },
        source,
        input,
        output,
        interrupt,
        pairing,
    )
    .await
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
