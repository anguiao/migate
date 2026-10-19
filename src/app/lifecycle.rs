use super::{App, BridgeOptions};
use crate::{
    RuntimeError,
    matter::{Advertisement, Bridge, PairingEvent},
    terminal,
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

/// Wait for the source topology and open the one committed configuration,
/// then drive it with the shared lifecycle.
pub async fn run_xiaomi_bridge(
    options: BridgeOptions<'_>,
    ready: impl Future<Output = Result<crate::xiaomi::adapter::SourceHandle, RuntimeError>>,
    source: impl Future<Output = Result<(), RuntimeError>>,
    input: impl AsyncBufRead + Unpin,
    output: impl AsyncWrite + Unpin,
    interrupt: impl Future<Output = Result<(), RuntimeError>>,
    pairing: impl Fn(PairingEvent) -> std::io::Result<()>,
) -> Result<(), RuntimeError> {
    let store = options.store.clone();
    let prepare = async {
        let handle = ready.await?;
        loop {
            let changed = handle.listen();
            if handle.snapshot().facts.scope.is_some() {
                break;
            }
            changed.await;
        }
        let app = Rc::new(App::open(handle, store)?);
        // Startup recovery applies revocation overlays before any old endpoint
        // can be served. Failure here leaves every control authority closed.
        if let Some(plan) = app.plan()? {
            let prepared = app.prepare(plan);
            app.apply(prepared)?;
        }
        Ok::<_, RuntimeError>(app)
    };
    run_lifecycle(options, prepare, source, input, output, interrupt, pairing).await
}

/// The one bridge lifecycle: one application configuration owner races the
/// source, terminal input, interrupt and the real Matter runner.
async fn run_lifecycle(
    options: BridgeOptions<'_>,
    prepare: impl Future<Output = Result<Rc<App>, RuntimeError>>,
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
    let app = future::or(
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
    let Some(app) = app else {
        return Ok(());
    };
    let terminal = terminal::bridge::Context::committed(app.clone(), options.status);
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
        let renamer: Rc<dyn crate::matter::LabelWriter> = app.clone();
        let bridge = Bridge::new(
            app.frozen(),
            app.root_version(),
            options.store,
            options.identity,
            renamer,
        );
        let mut runner = Box::pin(bridge.run_with_advertisement(&mut advertisement, &pairing));
        loop {
            let changed = app.listen();
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
                    app.stop_actions();
                    drop(runner);
                    drop(source);
                    bridge.check_failure()?;
                    return Err(error);
                }
            };
            if matches!(wake, Wake::Stop) {
                app.stop_actions();
                drop(runner);
                drop(source);
                bridge.check_failure()?;
                return Ok(());
            }
            if matches!(wake, Wake::Configuration) {
                retry_delay = Duration::from_secs(1);
            }
            let candidate = match app.plan() {
                Ok(candidate) => candidate,
                Err(error) => {
                    log::warn!(
                        "Configuration prepare failed; keeping committed configuration: {error}"
                    );
                    app.fail(error.to_string());
                    retry_at = Some(Instant::now() + retry_delay);
                    retry_delay = (retry_delay * 2).min(Duration::from_secs(60));
                    continue;
                }
            };
            let Some(candidate) = candidate else {
                retry_at = None;
                continue;
            };
            if candidate.rebuild {
                app.stop_actions();
                drop(runner);
                bridge.check_failure()?;
                // Dropping the actual runner releases transport, IM, responder,
                // handlers and socket. Only now reacquire the latest inputs.
                let latest = match app.plan() {
                    Ok(latest) => latest,
                    Err(error) => {
                        log::warn!(
                            "Late configuration prepare failed; restarting the committed configuration: {error}"
                        );
                        app.fail(error.to_string());
                        retry_at = Some(Instant::now() + retry_delay);
                        retry_delay = (retry_delay * 2).min(Duration::from_secs(60));
                        break;
                    }
                };
                if let Some(latest) = latest {
                    let prepared = app.prepare(latest);
                    match app.apply(prepared) {
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
            let prepared = app.prepare(candidate);
            match app.apply(prepared) {
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

#[cfg(test)]
#[path = "tests/mod.rs"]
mod tests;
