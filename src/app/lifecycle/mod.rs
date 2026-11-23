use super::{
    App, BridgeOptions,
    startup::{INITIAL_READ_LIMIT, StartupDeadline, StartupGate},
};
use crate::{
    RuntimeError,
    matter::{Advertisement, Bridge, PairingEvent},
    terminal::{bridge::Management, management::Listener},
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
    run_xiaomi_bridge_with_terminal(
        options,
        ready,
        source,
        BridgeTerminal {
            input,
            output,
            listener: None,
        },
        interrupt,
        pairing,
    )
    .await
}

pub struct BridgeTerminal<'a, I, O> {
    pub input: I,
    pub output: O,
    pub listener: Option<&'a Listener<'a>>,
}

/// Run the foreground session and management clients throughout bridge startup and serving.
pub async fn run_xiaomi_bridge_with_terminal(
    options: BridgeOptions<'_>,
    ready: impl Future<Output = Result<crate::xiaomi::adapter::SourceHandle, RuntimeError>>,
    source: impl Future<Output = Result<(), RuntimeError>>,
    terminal: BridgeTerminal<'_, impl AsyncBufRead + Unpin, impl AsyncWrite + Unpin>,
    interrupt: impl Future<Output = Result<(), RuntimeError>>,
    pairing: impl Fn(PairingEvent) -> std::io::Result<()>,
) -> Result<(), RuntimeError> {
    let management = Management::new(options.status);
    let sessions = management.serve(terminal.input, terminal.output, terminal.listener);
    let store = options.store.clone();
    let prepare = async {
        let handle = ready.await?;
        management.wait_for_home();
        loop {
            let changed = handle.listen();
            if handle.has_scope() {
                break;
            }
            changed.await;
        }
        let app = Rc::new(App::open(handle, store));
        // Cached recognition failures retain their source's committed facts.
        // Apply other valid changes before serving, including real revocations.
        match app.plan() {
            Ok(Some(plan)) => {
                let prepared = app.prepare(plan);
                if let Err(error) = app.apply(prepared) {
                    log::warn!(
                        "Startup configuration apply failed; keeping committed configuration: {error}"
                    );
                }
            }
            Ok(None) => {}
            Err(error) => {
                log::warn!(
                    "Startup configuration prepare failed; keeping committed configuration: {error}"
                );
                app.fail(error.to_string());
            }
        }
        Ok::<_, RuntimeError>((app, StartupGate::new(INITIAL_READ_LIMIT)))
    };
    run_lifecycle(
        options,
        prepare,
        source,
        &management,
        sessions,
        interrupt,
        pairing,
    )
    .await
}

/// The one bridge lifecycle: one application configuration owner races the
/// source, terminal input, interrupt and the real Matter runner.
async fn run_lifecycle<D: StartupDeadline>(
    options: BridgeOptions<'_>,
    prepare: impl Future<Output = Result<(Rc<App>, StartupGate<D>), RuntimeError>>,
    source: impl Future<Output = Result<(), RuntimeError>>,
    management: &Management<'_>,
    sessions: impl Future<Output = Result<(), RuntimeError>>,
    interrupt: impl Future<Output = Result<(), RuntimeError>>,
    pairing: impl Fn(PairingEvent) -> std::io::Result<()>,
) -> Result<(), RuntimeError> {
    let mut source = Box::pin(async {
        source.await?;
        Err::<(), RuntimeError>("Device source ended unexpectedly".into())
    });
    let mut interrupt = Box::pin(interrupt);
    let mut sessions = Box::pin(sessions);
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
            future::or(async { prepare.await.map(Some) }, async {
                sessions.as_mut().await?;
                Ok(None)
            }),
        ),
    )
    .await?;
    let Some((app, startup)) = app else {
        return Ok(());
    };
    management.publish(app.clone());
    log::info!("MiGate bridge_id={}", options.identity.bridge_id);
    let mut retry_at = app
        .pending()
        .then(|| Instant::now() + Duration::from_secs(1));
    let mut retry_delay = Duration::from_secs(1);
    // Keep the port and unchanged mDNS registrations across Matter rebuilds.
    let mut advertisement = Advertisement::new(options.port);
    loop {
        let renamer: Rc<dyn crate::matter::LabelWriter> = app.clone();
        let bridge = Bridge::new(options.store, options.identity, renamer);
        let mut runner = Box::pin(async {
            startup.wait(&app).await;
            bridge
                .run_with_advertisement(app.matter_configuration(), &mut advertisement, &pairing)
                .await
        });
        let mut finishing_advertisement = false;
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
                Rebuild,
            }
            let prefer_runner = startup.prefer_runner();
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
                            sessions.as_mut().await?;
                            Ok(Wake::Stop)
                        },
                        async {
                            let protocol = async {
                                runner.as_mut().await?;
                                Ok(Wake::Rebuild)
                            };
                            if finishing_advertisement {
                                return protocol.await;
                            }
                            let configuration = async {
                                changed.await;
                                Ok(Wake::Configuration)
                            };
                            let retry = async {
                                retry.await;
                                Ok(Wake::Retry)
                            };
                            if prefer_runner {
                                future::or(protocol, future::or(configuration, retry)).await
                            } else {
                                future::or(configuration, future::or(protocol, retry)).await
                            }
                        },
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
            if matches!(wake, Wake::Rebuild) {
                break;
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
                finishing_advertisement = bridge.request_rebuild().is_pending();
                if finishing_advertisement {
                    continue;
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
        app.stop_actions();
        drop(runner);
        bridge.check_failure()?;
        // Reacquire the latest inputs after releasing the old protocol and socket.
        let latest = match app.plan() {
            Ok(latest) => latest,
            Err(error) => {
                log::warn!(
                    "Late configuration prepare failed; restarting the committed configuration: {error}"
                );
                app.fail(error.to_string());
                retry_at = Some(Instant::now() + retry_delay);
                retry_delay = (retry_delay * 2).min(Duration::from_secs(60));
                continue;
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
    }
}

#[cfg(test)]
mod tests;
