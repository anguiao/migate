use super::{BridgeOptions, BridgeTerminal, run_source_bridge};
use crate::{
    RuntimeError,
    config::AuthCommand,
    matter::{Advertisement, PairingEvent},
    storage::{StorageError, XiaomiStore},
    terminal::{auth::OnlineAuth, bridge::Management, management},
    xiaomi::adapter::SourceHandle,
};
use event_listener::Event;
use futures_lite::{
    future,
    io::{AsyncBufRead, AsyncWrite},
};
use std::{cell::RefCell, future::Future, pin::pin};

/// The live source belongs to the saved account while terminal sessions belong
/// to the process. Authentication commits notify this handoff synchronously.
pub(super) struct BridgeAccount<'a> {
    store: XiaomiStore,
    management: &'a Management<'a>,
    active: RefCell<Option<SourceHandle>>,
    changed: Event,
}

impl<'a> BridgeAccount<'a> {
    pub(super) fn new(
        store: XiaomiStore,
        management: &'a Management<'a>,
    ) -> Result<Self, StorageError> {
        store.snapshot()?;
        let account = Self {
            store,
            management,
            active: RefCell::new(None),
            changed: Event::new(),
        };
        account.credentials_changed();
        Ok(account)
    }

    pub(super) fn credentials_changed(&self) {
        let active = self.active.borrow();
        if self.signed_in() {
            if let Some(source) = active.as_ref() {
                source.reload_credentials();
            } else {
                self.management.prepare_devices();
            }
        } else {
            self.management.wait_for_login();
            if let Some(source) = active.as_ref() {
                source.reload_credentials();
                source.stop();
            }
        }
        self.changed.notify(usize::MAX);
    }

    fn signed_in(&self) -> bool {
        self.store.current_snapshot().record.is_some()
    }

    async fn wait_for_login(&self) {
        loop {
            let changed = self.changed.listen();
            if self.signed_in() {
                return;
            }
            changed.await;
        }
    }

    /// Return only after the old Matter runner, advertisement and source have
    /// been dropped. New device requests are revoked at the commit itself.
    async fn wait_for_logout(&self) {
        loop {
            let changed = self.changed.listen();
            if self.active.borrow().is_none() {
                return;
            }
            changed.await;
        }
    }

    pub(super) async fn serve(
        &self,
        online: OnlineAuth<'_>,
        terminal: BridgeTerminal<'_, impl AsyncBufRead + Unpin, impl AsyncWrite + Unpin>,
    ) -> Result<(), RuntimeError> {
        management::serve(
            terminal.input,
            terminal.output,
            terminal.listener,
            async |words, session, output| {
                if let Some(command) = AuthCommand::from_words(words) {
                    return online
                        .execute(
                            command,
                            session,
                            output,
                            || self.credentials_changed(),
                            async || {
                                self.credentials_changed();
                                self.wait_for_logout().await;
                                Ok(())
                            },
                        )
                        .await;
                }
                self.management.execute(words, session, output).await
            },
        )
        .await
    }

    pub(super) async fn run<Ready, Network>(
        &self,
        options: BridgeOptions<'_>,
        mut start: impl FnMut() -> Result<(SourceHandle, Ready, Network), RuntimeError>,
        process_end: impl Future<Output = Result<(), RuntimeError>>,
        pairing: impl Fn(PairingEvent) -> std::io::Result<()>,
    ) -> Result<(), RuntimeError>
    where
        Ready: Future<Output = Result<(), RuntimeError>>,
        Network: Future<Output = Result<(), RuntimeError>>,
    {
        let mut process_end = pin!(process_end);
        let mut advertisement = Advertisement::new(options.port);
        loop {
            let signed_in = future::or(
                async { process_end.as_mut().await.map(|()| false) },
                async {
                    self.wait_for_login().await;
                    Ok(true)
                },
            )
            .await?;
            if !signed_in {
                return Ok(());
            }

            let (handle, ready, network) = start()?;
            *self.active.borrow_mut() = Some(handle.clone());
            let mut shutdown = false;
            let stop = future::or(
                async {
                    let result = process_end.as_mut().await;
                    shutdown = true;
                    result
                },
                async {
                    // Sessions may commit a logout when polled above. Check
                    // retirement before polling any old networking again.
                    handle.stopped().await;
                    Ok(())
                },
            );
            run_source_bridge(
                options,
                async {
                    ready.await?;
                    Ok(handle.clone())
                },
                network,
                self.management,
                &mut advertisement,
                stop,
                &pairing,
            )
            .await?;
            if shutdown {
                return Ok(());
            }
            let shutdown = future::or(async { process_end.as_mut().await.map(|()| true) }, async {
                advertisement.stop().await;
                Ok(false)
            })
            .await?;
            self.active.borrow_mut().take();
            self.changed.notify(usize::MAX);
            if shutdown {
                return Ok(());
            }
        }
    }
}
