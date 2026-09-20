use super::{
    AUTH_OBSERVE_INTERVAL, Inner, Runner, XiaomiBoundaryDiagnostic, XiaomiFailureStage,
    XiaomiRuntime, XiaomiRuntimeComponent, XiaomiRuntimeDiagnostic, XiaomiRuntimeError,
    XiaomiSafeFailureCode,
    auth::{AuthChange, AuthMaintenance, AuthTaskResult},
    catalog_refresh::{CatalogRefresh, CatalogTaskResult},
    discovery::{DiscoveryEvent, NetworkDiscovery},
    sessions::{SessionContext, SessionEvent},
    status::record_diagnostic,
    unix_time,
};
use crate::{
    storage::{AuthSnapshot, StorageError},
    xiaomi::{auth::AuthReport, discovery::NetworkUpdate},
};
use async_io::Timer;
use futures_lite::future;
use futures_util::{FutureExt, future::LocalBoxFuture, future::select_all};
use std::{rc::Rc, time::Instant};

struct RuntimeRunGuard {
    inner: Rc<Inner>,
}

impl Drop for RuntimeRunGuard {
    fn drop(&mut self) {
        self.inner.stopped.set(true);
        self.inner.registry.revoke_all();
        self.inner.commands.stop();
        self.inner.state.stop();
        self.inner.running.set(false);
        self.inner.wake.notify(usize::MAX);
    }
}

impl XiaomiRuntime {
    pub(super) fn replace_auth_report(&self, report: AuthReport) {
        let changed = {
            let previous = self.inner.auth_report.borrow();
            previous.authentication != report.authentication
                || previous.certificate != report.certificate
                || previous.certificate_update != report.certificate_update
        };
        if changed {
            crate::terminal::log_status(&report, unix_time());
        }
        *self.inner.auth_report.borrow_mut() = report;
    }

    pub fn stop(&self) {
        self.inner.stopped.set(true);
        self.inner.registry.revoke_all();
        self.inner.commands.stop();
        self.inner.state.stop();
        self.inner.wake.notify(usize::MAX);
    }

    pub async fn run(&self) -> Result<(), XiaomiRuntimeError> {
        self.check_failure()?;
        if self.inner.running.replace(true) {
            return Err(XiaomiRuntimeError::Storage(StorageError::new(
                self.inner.runner_path(),
                "start Xiaomi runtime",
                "Xiaomi runtime is already running",
            )));
        }
        self.inner.stopped.set(false);
        let mut runner = self.inner.runner.borrow_mut().take().ok_or_else(|| {
            XiaomiRuntimeError::Storage(StorageError::new(
                self.inner.runner_path(),
                "start Xiaomi runtime",
                "Xiaomi runtime cannot be restarted",
            ))
        })?;
        let _run_guard = RuntimeRunGuard {
            inner: self.inner.clone(),
        };
        let result = future::or(self.run_events(&mut runner), async {
            future::or(self.inner.commands.run(), self.inner.state.run()).await;
            Ok(())
        })
        .await;
        if let Err(XiaomiRuntimeError::Storage(error)) = &result {
            self.record_failure(error.clone());
        }
        result
    }

    fn record_failure(&self, error: StorageError) {
        self.inner.failure.borrow_mut().get_or_insert(error);
        self.inner.failed.notify(usize::MAX);
    }

    fn record_diagnostic(&self, diagnostic: XiaomiRuntimeDiagnostic) {
        record_diagnostic(&self.inner.diagnostics, diagnostic);
    }

    fn record_boundary(
        &self,
        component: XiaomiRuntimeComponent,
        stage: XiaomiFailureStage,
        code: XiaomiSafeFailureCode,
        subject: Option<String>,
    ) {
        self.record_diagnostic(XiaomiRuntimeDiagnostic::Boundary(
            XiaomiBoundaryDiagnostic {
                component,
                stage,
                code,
                subject,
            },
        ));
    }

    pub(super) fn session_context<'a>(
        &'a self,
        auth: &'a AuthMaintenance,
        discovery: &'a NetworkDiscovery,
        catalog: &'a CatalogRefresh,
    ) -> SessionContext<'a> {
        SessionContext {
            auth,
            discovery,
            cloud: catalog.client(),
            state: &self.inner.state,
            registry: &self.inner.registry,
            status: &self.inner.status,
            diagnostics: &self.inner.diagnostics,
            wake: &self.inner.wake,
            refresh_requested: &self.inner.refresh_requested,
            devices: &self.inner.devices,
            certificate_expires: self
                .inner
                .auth_report
                .borrow()
                .certificate
                .map_or(0, |validity| validity.not_after),
        }
    }

    async fn run_events(&self, runner: &mut Runner) -> Result<(), XiaomiRuntimeError> {
        loop {
            if self.inner.stopped.get() {
                return Ok(());
            }
            runner
                .sessions
                .drain_transport_failures(&self.session_context(
                    &runner.auth,
                    &runner.discovery,
                    &runner.catalog_refresh,
                ))?;
            for diagnostic in self.inner.commands.drain_diagnostics() {
                self.record_diagnostic(XiaomiRuntimeDiagnostic::Command(diagnostic));
            }
            for diagnostic in self.inner.state.drain_diagnostics() {
                self.record_diagnostic(XiaomiRuntimeDiagnostic::State(diagnostic));
            }
            let requested = self.inner.refresh_requested.get();
            if runner
                .discovery
                .schedule(Instant::now(), requested)
                .is_err()
            {
                self.discovery_failed();
            }
            match runner.auth.changes()? {
                AuthChange::Session(snapshot) => {
                    runner.sessions.reset_auth(
                        &self.session_context(
                            &runner.auth,
                            &runner.discovery,
                            &runner.catalog_refresh,
                        ),
                        &snapshot,
                    )?;
                    runner.catalog_refresh.reset();
                    runner.auth.accept_session(snapshot);
                }
                AuthChange::Revision(snapshot) => self.apply_auth_revision(runner, snapshot)?,
                AuthChange::Unchanged => {}
            }
            runner.auth.schedule(Instant::now(), requested);
            if runner.catalog_refresh.due(Instant::now(), requested) {
                runner
                    .catalog_refresh
                    .start(runner.auth.current_snapshot()?);
            }
            if requested
                && runner.discovery.refresh_scheduled()
                && runner.auth.running()
                && runner.catalog_refresh.running()
            {
                self.inner.refresh_requested.set(false);
            }
            runner.sessions.schedule(&self.session_context(
                &runner.auth,
                &runner.discovery,
                &runner.catalog_refresh,
            ))?;
            let deadline = [
                runner.auth.deadline(),
                runner.catalog_refresh.deadline(),
                runner.discovery.deadline(),
            ]
            .into_iter()
            .flatten()
            .fold(Instant::now() + AUTH_OBSERVE_INTERVAL, Instant::min);
            let listener = self.inner.wake.listen();
            if self.inner.stopped.get() {
                continue;
            }
            enum Event {
                Wake,
                Discovery(DiscoveryEvent),
                Session(SessionEvent),
                Auth(AuthTaskResult),
                Catalog(CatalogTaskResult),
            }
            let event = {
                let waits: Vec<LocalBoxFuture<'_, Event>> = vec![
                    future::or(listener, async {
                        Timer::at(deadline).await;
                    })
                    .map(|_| Event::Wake)
                    .boxed_local(),
                    runner
                        .discovery
                        .next_event()
                        .map(Event::Discovery)
                        .boxed_local(),
                    runner
                        .sessions
                        .next_event()
                        .map(Event::Session)
                        .boxed_local(),
                    runner.auth.next_event().map(Event::Auth).boxed_local(),
                    runner
                        .catalog_refresh
                        .next_event()
                        .map(Event::Catalog)
                        .boxed_local(),
                ];
                select_all(waits).await.0
            };
            match event {
                Event::Wake => {}
                Event::Discovery(DiscoveryEvent::Network(result)) => {
                    let result = runner.discovery.finish_network(result);
                    self.finish_network_refresh(runner, result)?;
                }
                Event::Discovery(DiscoveryEvent::Mdns(result)) => {
                    match runner.discovery.finish_browser(result) {
                        Ok(true) => {
                            let ctx = self.session_context(
                                &runner.auth,
                                &runner.discovery,
                                &runner.catalog_refresh,
                            );
                            runner.sessions.reconcile_gateway_candidates(&ctx)?;
                            runner.sessions.update_status(&ctx);
                        }
                        Ok(false) => {}
                        Err(_) => self.discovery_failed(),
                    }
                }
                Event::Session(event) => {
                    runner.sessions.handle_event(
                        &self.session_context(
                            &runner.auth,
                            &runner.discovery,
                            &runner.catalog_refresh,
                        ),
                        event,
                    )?;
                }
                Event::Auth(result) => {
                    if let Some(report) = runner.auth.finish(result)? {
                        runner.sessions.apply_auth_report(
                            &self.session_context(
                                &runner.auth,
                                &runner.discovery,
                                &runner.catalog_refresh,
                            ),
                            &report,
                        )?;
                        self.replace_auth_report(report);
                    }
                }
                Event::Catalog(result) => {
                    runner.catalog_refresh.cancel();
                    self.finish_catalog_refresh(runner, result)?;
                }
            }
        }
    }

    fn apply_auth_revision(
        &self,
        runner: &mut Runner,
        snapshot: AuthSnapshot,
    ) -> Result<(), XiaomiRuntimeError> {
        let previous = runner.auth.replace_snapshot(snapshot);
        match runner.sessions.apply_auth_revision(
            &self.session_context(&runner.auth, &runner.discovery, &runner.catalog_refresh),
            &previous,
        ) {
            Ok(()) => {
                runner.auth.commit_revision();
                runner.catalog_refresh.cancel();
                Ok(())
            }
            Err(error) => {
                runner.auth.replace_snapshot(previous);
                Err(error)
            }
        }
    }

    pub(super) fn finish_catalog_refresh(
        &self,
        runner: &mut Runner,
        result: CatalogTaskResult,
    ) -> Result<(), XiaomiRuntimeError> {
        let action = runner.sessions.finish_catalog_refresh(
            &self.session_context(&runner.auth, &runner.discovery, &runner.catalog_refresh),
            result,
        )?;
        runner.catalog_refresh.apply(action);
        Ok(())
    }

    pub(super) fn finish_network_refresh(
        &self,
        runner: &mut Runner,
        result: Result<NetworkUpdate, crate::xiaomi::discovery::DiscoveryError>,
    ) -> Result<(), XiaomiRuntimeError> {
        match result {
            Ok(update) if runner.discovery.network() == Some(&update) => {}
            Ok(update) => {
                runner.sessions.network_changed(
                    &self.session_context(&runner.auth, &runner.discovery, &runner.catalog_refresh),
                    update.epoch,
                    true,
                )?;
                if runner.discovery.install(update).is_err() {
                    self.discovery_failed();
                }
            }
            Err(_) => {
                self.discovery_failed();
                if runner.discovery.network().is_some() {
                    let epoch = runner.discovery.next_epoch();
                    runner.sessions.network_changed(
                        &self.session_context(
                            &runner.auth,
                            &runner.discovery,
                            &runner.catalog_refresh,
                        ),
                        epoch,
                        false,
                    )?;
                    runner.discovery.clear();
                }
            }
        }
        Ok(())
    }

    fn discovery_failed(&self) {
        self.record_boundary(
            XiaomiRuntimeComponent::Network,
            XiaomiFailureStage::Discover,
            XiaomiSafeFailureCode::Unavailable,
            None,
        );
    }
}
