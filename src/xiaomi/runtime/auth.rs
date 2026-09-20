use super::{AUTH_MAINTENANCE_INTERVAL, observer_failure, unix_time};
use crate::{
    storage::{
        AuthRevision, AuthSnapshot, StorageError, XiaomiAuthObservation, XiaomiAuthObserver,
        XiaomiStore,
    },
    xiaomi::auth::{AuthError, AuthReport, AuthService},
};
use futures_util::{FutureExt, future::LocalBoxFuture};
use std::{
    rc::Rc,
    time::{Duration, Instant},
};

pub(super) type AuthTaskResult = (XiaomiAuthObservation, Result<AuthReport, AuthError>);

pub(super) enum AuthChange {
    Session(AuthSnapshot),
    Revision(AuthSnapshot),
    Unchanged,
}

/// Owns authentication observation, maintenance work, and its retry clock.
pub(super) struct AuthMaintenance {
    service: Rc<AuthService>,
    store: XiaomiStore,
    observer: XiaomiAuthObserver,
    task: Option<LocalBoxFuture<'static, AuthTaskResult>>,
    observation: XiaomiAuthObservation,
    snapshot: AuthSnapshot,
    recheck_revision: Option<AuthRevision>,
    next: Instant,
    backoff: Duration,
}

impl AuthMaintenance {
    pub(super) fn new(
        service: Rc<AuthService>,
        store: XiaomiStore,
        snapshot: AuthSnapshot,
    ) -> Self {
        Self {
            service,
            observer: store.auth_observer(),
            store,
            task: None,
            observation: observation(&snapshot),
            snapshot,
            recheck_revision: None,
            next: Instant::now(),
            backoff: AUTH_MAINTENANCE_INTERVAL,
        }
    }

    pub(super) fn snapshot(&self) -> &AuthSnapshot {
        &self.snapshot
    }

    pub(super) fn current_snapshot(&self) -> Result<AuthSnapshot, StorageError> {
        self.observer
            .snapshot()
            .map_err(|error| observer_failure(&self.store, error))
    }

    pub(super) fn observe(&self) -> Result<XiaomiAuthObservation, StorageError> {
        self.observer
            .observe()
            .map_err(|error| observer_failure(&self.store, error))
    }

    pub(super) fn changes(&mut self) -> Result<AuthChange, StorageError> {
        let observed = self.observe()?;
        if observed.session_generation != self.observation.session_generation
            || observed.uid != self.observation.uid
        {
            self.task = None;
            Ok(AuthChange::Session(self.current_snapshot()?))
        } else if observed.revision != self.observation.revision {
            if self.task.take().is_some() {
                self.recheck_revision = Some(observed.revision);
            }
            Ok(AuthChange::Revision(self.current_snapshot()?))
        } else {
            self.observation = observed;
            Ok(AuthChange::Unchanged)
        }
    }

    // The runtime commits observations only after dependent authorities are revoked.
    pub(super) fn accept_session(&mut self, snapshot: AuthSnapshot) {
        self.observation = observation(&snapshot);
        self.snapshot = snapshot;
        self.next = Instant::now();
    }

    pub(super) fn replace_snapshot(&mut self, snapshot: AuthSnapshot) -> AuthSnapshot {
        std::mem::replace(&mut self.snapshot, snapshot)
    }

    pub(super) fn commit_revision(&mut self) {
        self.observation = observation(&self.snapshot);
        self.next = if self.recheck_revision == Some(self.snapshot.revision) {
            Instant::now()
        } else {
            auth_maintenance_deadline(&self.snapshot)
        };
    }

    pub(super) fn schedule(&mut self, now: Instant, requested: bool) {
        if self.task.is_none() && (now >= self.next || requested) {
            let service = self.service.clone();
            let scope = self.observation.clone();
            self.task = Some(async move { (scope, service.check().await) }.boxed_local());
            self.next = now + AUTH_MAINTENANCE_INTERVAL;
        }
    }

    pub(super) fn running(&self) -> bool {
        self.task.is_some()
    }

    pub(super) fn deadline(&self) -> Option<Instant> {
        self.task.is_none().then_some(self.next)
    }

    pub(super) async fn next_event(&mut self) -> AuthTaskResult {
        match self.task.as_mut() {
            Some(task) => task.await,
            None => std::future::pending().await,
        }
    }

    pub(super) fn finish(
        &mut self,
        (scope, result): AuthTaskResult,
    ) -> Result<Option<AuthReport>, StorageError> {
        self.task = None;
        if let Err(AuthError::Storage(error)) = &result {
            return Err(error.clone());
        }
        let current = self.observe()?;
        if current.session_generation != scope.session_generation
            || current.uid != scope.uid
            || current.revision != scope.revision
        {
            if result.as_ref().is_ok_and(AuthReport::is_success)
                && current.session_generation == scope.session_generation
                && current.uid == scope.uid
            {
                self.recheck_revision = Some(current.revision);
            }
            self.next = Instant::now();
            return Ok(None);
        }
        match result {
            Ok(report) => {
                self.recheck_revision = None;
                if report.is_success() {
                    if let Ok(snapshot) = self.current_snapshot() {
                        self.next = auth_maintenance_deadline(&snapshot);
                        self.backoff = AUTH_MAINTENANCE_INTERVAL;
                    }
                } else {
                    self.retry();
                }
                Ok(Some(report))
            }
            Err(AuthError::Storage(error)) => Err(error),
            Err(_) => {
                self.retry();
                Ok(None)
            }
        }
    }

    fn retry(&mut self) {
        self.next = Instant::now() + self.backoff;
        self.backoff = (self.backoff * 2).min(Duration::from_secs(5 * 60));
    }

    #[cfg(test)]
    pub(super) fn set_service(&mut self, service: Rc<AuthService>) {
        self.service = service;
    }

    #[cfg(test)]
    pub(super) fn defer_until(&mut self, next: Instant) {
        self.next = next;
    }

    #[cfg(test)]
    pub(super) fn cancel(&mut self) {
        self.task = None;
    }
}

pub(super) fn observation(snapshot: &AuthSnapshot) -> XiaomiAuthObservation {
    XiaomiAuthObservation {
        uid: snapshot.record.as_ref().map(|record| record.uid.clone()),
        revision: snapshot.revision,
        session_generation: snapshot.session_generation,
    }
}

fn auth_maintenance_deadline(snapshot: &AuthSnapshot) -> Instant {
    let Some(record) = snapshot.record.as_ref() else {
        return Instant::now() + AUTH_MAINTENANCE_INTERVAL;
    };
    let now = unix_time();
    let certificate_due = crate::xiaomi::certificate::validate_certificate(
        &record.uid,
        &record.virtual_did,
        &record.private_key_pem,
        &record.certificate_pem,
    )
    .map(|validity| validity.not_after.saturating_sub(3 * 24 * 60 * 60))
    .unwrap_or(now);
    let due = record.tokens.refresh_at.min(certificate_due);
    let delay = due.saturating_sub(now).max(0) as u64;
    Instant::now() + Duration::from_secs(delay)
}
