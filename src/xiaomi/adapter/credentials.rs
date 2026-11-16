#[derive(Default)]
pub(super) enum HaSync {
    #[default]
    Idle,
    // Retained during refresh so concurrent failures coalesce into the same attempt.
    Rejected(u64),
    Refreshed(u64),
    Paused,
}
use super::{
    model::AdapterError,
    state::{Backoff, Context},
};
use crate::xiaomi::certificate::{CertificateStatus, CertificateValidity};
use crate::{
    storage::XiaomiRecord,
    xiaomi::auth::{
        AccountErrorKind, AuthError, AuthReport, AuthService, AuthenticationState,
        CertificateUpdate, FailureReason,
    },
};
use futures_lite::future;
use std::{rc::Rc, time::Duration};

/// Account/session identities and the outcomes of credential maintenance.
/// Reports and path eligibility both read these facts.
pub(super) struct Credentials {
    record: Option<XiaomiRecord>,
    mijia_generation: Option<u64>,
    ha_generation: Option<u64>,
    ha_sync: HaSync,
    mijia_rejected: Option<u64>,
    http_paused: bool,
    report: AuthReport,
}

struct Adopted {
    changed: bool,
    mijia_changed: bool,
    ha_changed: bool,
    report_changed: bool,
}

impl Credentials {
    pub(super) fn restored(
        record: Option<XiaomiRecord>,
        mijia_generation: Option<u64>,
        ha_generation: Option<u64>,
        report: AuthReport,
    ) -> Self {
        let ha_stopped = record.as_ref().is_some_and(|record| record.ha.stopped);
        Self {
            record,
            mijia_generation,
            ha_generation,
            ha_sync: if ha_stopped {
                HaSync::Paused
            } else {
                HaSync::Idle
            },
            mijia_rejected: None,
            http_paused: false,
            report,
        }
    }
    pub(super) fn record(&self) -> Option<&XiaomiRecord> {
        self.record.as_ref()
    }
    pub(super) fn report(&self, now: i64) -> AuthReport {
        let mut report = self.report.clone();
        if self.token_expired(now) && !self.refresh_paused() {
            report.ha = AuthenticationState::Unavailable(FailureReason::AccessTokenExpired);
        }
        report
    }
    pub(super) fn token_expired(&self, now: i64) -> bool {
        self.record
            .as_ref()
            .and_then(|record| record.ha_authorization())
            .is_some_and(|ha| ha.tokens.expires_at <= now)
    }
    pub(super) fn token_authorized(&self) -> bool {
        !matches!(self.ha_sync, HaSync::Rejected(_) | HaSync::Paused)
            && self
                .record
                .as_ref()
                .and_then(|record| record.ha_authorization())
                .is_some()
    }
    pub(super) fn token_usable(&self, now: i64) -> bool {
        self.token_authorized() && !self.token_expired(now)
    }
    pub(super) fn certificate_validity(&self) -> Option<CertificateValidity> {
        self.report.certificate
    }
    pub(super) fn certificate_status(&self, now: i64) -> Option<CertificateStatus> {
        self.certificate_validity()
            .map(|validity| validity.status(now))
    }
    pub(super) fn ha_key(&self) -> Option<super::routing::HaKey> {
        Some(super::routing::HaKey {
            uid: self.record.as_ref()?.uid.clone(),
            generation: self.ha_generation?,
        })
    }
    pub(super) fn current_ha(&self, key: &super::routing::HaKey) -> bool {
        self.ha_generation == Some(key.generation)
            && self
                .record
                .as_ref()
                .is_some_and(|record| record.uid == key.uid)
    }
    pub(super) fn mijia_path(&self, home_id: Option<String>) -> Option<super::routing::CloudPath> {
        let record = self.record.as_ref()?;
        let generation = self.mijia_generation?;
        if self.mijia_rejected == Some(generation) {
            return None;
        }
        Some(super::routing::CloudPath {
            uid: record.uid.clone(),
            generation,
            home_id,
            session: record.mijia.clone(),
        })
    }
    pub(super) fn current_mijia(&self, path: &super::routing::CloudPath) -> bool {
        self.mijia_generation == Some(path.generation)
            && self.mijia_rejected != Some(path.generation)
            && self
                .record
                .as_ref()
                .is_some_and(|record| record.uid == path.uid)
    }
    pub(super) fn mijia_rejected(&self) -> Option<u64> {
        self.mijia_rejected
    }
    pub(super) fn reject_mijia(&mut self, generation: u64, now: i64) {
        self.mijia_rejected = Some(generation);
        self.http_paused = true;
        self.mijia_authentication(AuthenticationState::Checking, now);
    }
    pub(super) fn http_paused(&self) -> bool {
        self.http_paused
    }
    pub(super) fn pause_http(&mut self) -> bool {
        let changed = !self.http_paused;
        self.http_paused = true;
        changed
    }
    pub(super) fn resume_http(&mut self) {
        self.http_paused = false;
    }
    fn adopt(
        &mut self,
        record: XiaomiRecord,
        mijia_generation: Option<u64>,
        ha_generation: Option<u64>,
    ) -> Adopted {
        let mijia_changed = self.mijia_generation != mijia_generation;
        let ha_changed = self.ha_generation != ha_generation;
        let changed = self.record.as_ref() != Some(&record) || mijia_changed || ha_changed;
        let report_changed = self.report.recovery_stop != record.recovery.stop || mijia_changed;
        self.report.recovery_stop = record.recovery.stop;
        if mijia_changed {
            self.report.mijia = AuthenticationState::Checking;
            self.mijia_rejected = None;
            self.http_paused = false;
        }
        if ha_changed {
            self.ha_sync = if record.ha.stopped {
                HaSync::Paused
            } else {
                HaSync::Idle
            };
            self.report.ha = if ha_generation.is_some() {
                AuthenticationState::Checking
            } else {
                AuthenticationState::SignInRequired(FailureReason::HaAuthorizationStopped)
            };
        }
        self.record = Some(record);
        self.mijia_generation = mijia_generation;
        self.ha_generation = ha_generation;
        Adopted {
            changed,
            mijia_changed,
            ha_changed,
            report_changed,
        }
    }
    pub(super) fn mijia_authentication(&mut self, authentication: AuthenticationState, now: i64) {
        if authentication_completed(&authentication) {
            self.report.set_completed_at(now);
        }
        self.report.mijia = authentication;
    }
    pub(super) fn ha_authentication(&mut self, authentication: AuthenticationState, now: i64) {
        if authentication_completed(&authentication) {
            self.report.set_completed_at(now);
        }
        self.report.ha = authentication;
    }
    fn certificate_result(
        &mut self,
        validity: Option<CertificateValidity>,
        update: CertificateUpdate,
        now: i64,
    ) -> bool {
        if validity.is_none()
            && self.report.certificate.is_none()
            && self.report.certificate_update == update
        {
            return false;
        }
        self.report.certificate = validity;
        self.report.certificate_update = update;
        self.report.set_completed_at(now);
        true
    }
    pub(super) fn refresh_paused(&self) -> bool {
        matches!(self.ha_sync, HaSync::Paused)
    }
    fn refresh_rejected(&self, generation: u64) -> bool {
        matches!(self.ha_sync, HaSync::Rejected(current) if current == generation)
    }
    fn begin_refresh(&mut self, generation: u64, now: i64) {
        if matches!(self.ha_sync, HaSync::Refreshed(current) if current == generation) {
            self.ha_sync = HaSync::Idle;
        }
        self.ha_authentication(AuthenticationState::Refreshing, now);
    }
    fn refreshed(&mut self, now: i64) {
        if let Some(generation) = self.ha_generation {
            self.ha_sync = HaSync::Refreshed(generation);
        }
        self.ha_authentication(AuthenticationState::Authenticated, now);
    }
    fn stop_refresh(&mut self, reason: FailureReason, now: i64) {
        self.ha_sync = HaSync::Paused;
        self.ha_authentication(AuthenticationState::SignInRequired(reason), now);
    }
    fn refreshed_token_rejected(&self, generation: u64) -> bool {
        matches!(self.ha_sync, HaSync::Refreshed(current) if current == generation)
    }
    fn reject_token(&mut self, generation: u64) {
        if !matches!(self.ha_sync, HaSync::Rejected(_) | HaSync::Paused) {
            self.ha_sync = HaSync::Rejected(generation);
        }
    }
}

impl Context {
    pub(super) fn adopt_credentials(&self, credentials: XiaomiRecord) {
        let snapshot = self.store.xiaomi().current_snapshot();
        let adopted = self.credentials.borrow_mut().adopt(
            credentials,
            snapshot.mijia_generation(),
            snapshot.ha_generation(),
        );
        if adopted.report_changed || adopted.ha_changed {
            self.status_changed.notify(usize::MAX);
        }
        if adopted.mijia_changed {
            self.directory.borrow_mut().reset_sync();
            self.mijia_changed.notify(usize::MAX);
            self.request_sync();
        }
        if adopted.ha_changed {
            self.ha_changed.notify(usize::MAX);
        }
        if adopted.changed {
            self.credentials_changed.notify(usize::MAX);
            self.changed.notify(usize::MAX);
            self.refresh_changed.notify(usize::MAX);
            self.update_all_availability();
            self.update_routing("cloud credentials updated");
        }
    }
    pub(super) fn reload_credentials(&self) {
        let record = self
            .store
            .xiaomi()
            .current_snapshot()
            .record
            .expect("active account");
        self.adopt_credentials(record);
    }
    pub(super) fn mijia_authentication(&self, authentication: AuthenticationState) {
        self.credentials
            .borrow_mut()
            .mijia_authentication(authentication, self.clock.unix_now());
        self.status_changed.notify(usize::MAX);
    }
    pub(super) fn confirm_ha(&self) {
        let checking = matches!(
            self.credentials.borrow().report.ha,
            AuthenticationState::Checking
        );
        if checking {
            self.ha_authentication(AuthenticationState::Authenticated);
        }
    }
    pub(super) fn ha_authentication(&self, authentication: AuthenticationState) {
        self.credentials
            .borrow_mut()
            .ha_authentication(authentication, self.clock.unix_now());
        self.status_changed.notify(usize::MAX);
    }
    pub(super) fn certificate(&self, validity: CertificateValidity, update: CertificateUpdate) {
        self.credentials.borrow_mut().certificate_result(
            Some(validity),
            update,
            self.clock.unix_now(),
        );
        self.status_changed.notify(usize::MAX);
        self.update_all_availability();
        self.update_routing("hub certificate updated");
        self.changed.notify(usize::MAX);
    }
    pub(super) fn certificate_unavailable(&self, update: CertificateUpdate) {
        if !self
            .credentials
            .borrow_mut()
            .certificate_result(None, update, self.clock.unix_now())
        {
            return;
        }
        self.status_changed.notify(usize::MAX);
        self.update_all_availability();
        self.update_routing("hub certificate unavailable");
        self.refresh_changed.notify(usize::MAX);
        self.changed.notify(usize::MAX);
    }
    pub(super) fn token_rejected(
        &self,
        key: &super::routing::HaKey,
        reason: FailureReason,
    ) -> Result<(), AdapterError> {
        if !self.current_ha(key) {
            return Ok(());
        }
        if self
            .credentials
            .borrow()
            .refreshed_token_rejected(key.generation)
        {
            self.store.xiaomi().stop_ha_refresh()?;
            self.credentials
                .borrow_mut()
                .stop_refresh(reason, self.clock.unix_now());
            self.status_changed.notify(usize::MAX);
        } else {
            self.credentials.borrow_mut().reject_token(key.generation);
        }
        self.update_all_availability();
        self.update_routing("cloud access token rejected");
        self.changed.notify(usize::MAX);
        self.ha_changed.notify(usize::MAX);
        self.refresh_changed.notify(usize::MAX);
        Ok(())
    }
}

fn deadline(context: &Context, unix: i64) -> Duration {
    context.clock.now()
        + Duration::from_secs(unix.saturating_sub(context.clock.unix_now()).max(0) as u64)
}

fn saved(auth: &AuthService, context: &Context) -> Result<XiaomiRecord, AdapterError> {
    let record = auth
        .saved_bridge_credentials()?
        .ok_or_else(|| AdapterError::Unavailable("Xiaomi sign-in is required".into()))?;
    context.adopt_credentials(record.clone());
    Ok(record)
}

fn eligibility_changed(context: &Context, reason: &str) {
    context.update_all_availability();
    context.update_routing(reason);
    context.credentials_changed.notify(usize::MAX);
    context.changed.notify(usize::MAX);
    context.refresh_changed.notify(usize::MAX);
    context.status_changed.notify(usize::MAX);
}

async fn ha_retired(context: &Context, key: &super::routing::HaKey) {
    loop {
        let changed = context.ha_changed.listen();
        if !context.current_ha(key) {
            return;
        }
        changed.await;
    }
}

fn token_expired(context: &Context) {
    eligibility_changed(context, "access token expired");
}

fn authentication_completed(authentication: &AuthenticationState) -> bool {
    matches!(
        authentication,
        AuthenticationState::Authenticated
            | AuthenticationState::Unavailable(_)
            | AuthenticationState::SignInRequired(_)
    )
}

async fn tokens(auth: &AuthService, context: Rc<Context>) -> Result<(), AdapterError> {
    let mut backoff = Backoff::new(1, 30);
    let mut retry = None;
    let mut observed_expiry = None;
    loop {
        let changed = context.ha_changed.listen();
        let record = saved(auth, &context)?;
        let Some(key) = context.ha_key() else {
            if !matches!(
                context.credentials.borrow().report.ha,
                AuthenticationState::SignInRequired(_)
            ) {
                context.ha_authentication(AuthenticationState::SignInRequired(
                    FailureReason::HaAuthorizationStopped,
                ));
            }
            changed.await;
            continue;
        };
        let tokens = &record.ha_authorization().unwrap().tokens;
        let is_expired = tokens.expires_at <= context.clock.unix_now();
        if is_expired && observed_expiry != Some(tokens.expires_at) {
            observed_expiry = Some(tokens.expires_at);
            token_expired(&context);
        }
        let paused = context.credentials.borrow().refresh_paused();
        let rejected = context
            .credentials
            .borrow()
            .refresh_rejected(key.generation);
        let refresh_due = retry.unwrap_or_else(|| {
            if rejected {
                context.clock.now()
            } else {
                deadline(&context, tokens.refresh_at)
            }
        });
        if !paused
            && (rejected || context.clock.unix_now() >= tokens.refresh_at)
            && context.clock.now() >= refresh_due
        {
            context
                .credentials
                .borrow_mut()
                .begin_refresh(key.generation, context.clock.unix_now());
            context.status_changed.notify(usize::MAX);
            let refreshing = auth.refresh_bridge_tokens(&record, || context.clock.unix_now());
            let mut refreshing = std::pin::pin!(refreshing);
            let result = if !is_expired {
                let expiry = deadline(&context, tokens.expires_at);
                match future::or(async { Some(refreshing.as_mut().await) }, async {
                    context.clock.wait_until(expiry).await;
                    None
                })
                .await
                {
                    Some(result) => result,
                    None => {
                        observed_expiry = Some(tokens.expires_at);
                        token_expired(&context);
                        refreshing.await
                    }
                }
            } else {
                refreshing.await
            };
            match result {
                Ok(replacement) => {
                    let already_due = replacement
                        .ha_authorization()
                        .is_some_and(|ha| ha.tokens.refresh_at <= context.clock.unix_now());
                    context.adopt_credentials(replacement);
                    context
                        .credentials
                        .borrow_mut()
                        .refreshed(context.clock.unix_now());
                    context.status_changed.notify(usize::MAX);
                    context.ha_changed.notify(usize::MAX);
                    retry = already_due.then(|| context.clock.now() + Duration::from_secs(1));
                    backoff = Backoff::new(1, 30);
                }
                Err(AuthError::Cloud(error)) => {
                    if error.is_unauthorized() {
                        saved(auth, &context)?;
                        context
                            .credentials
                            .borrow_mut()
                            .stop_refresh(FailureReason::Cloud(error), context.clock.unix_now());
                        context.status_changed.notify(usize::MAX);
                    } else {
                        let delay = if error.is_temporary() {
                            backoff.failed()
                        } else {
                            super::cloud::MEMBER_INTERVAL
                        };
                        let reason = FailureReason::Cloud(error);
                        context.ha_authentication(AuthenticationState::Unavailable(reason));
                        retry = Some(context.clock.now() + delay);
                    }
                    eligibility_changed(&context, "cloud token maintenance failed");
                }
                Err(error) => return Err(error.into()),
            }
            continue;
        }
        let next = (!paused)
            .then_some(refresh_due)
            .into_iter()
            .chain((!is_expired).then(|| deadline(&context, tokens.expires_at)))
            .min();
        future::or(changed, async {
            if let Some(next) = next {
                context.clock.wait_until(next).await;
            } else {
                future::pending::<()>().await;
            }
        })
        .await;
    }
}

async fn certificates(auth: &AuthService, context: Rc<Context>) -> Result<(), AdapterError> {
    let mut backoff = Backoff::new(1, 30);
    let mut due = context.clock.now();
    let mut last_expiry = None;
    let mut last_hub = None;
    let mut validity = None;
    let mut maintained = None;
    loop {
        let changed = context.changed.listen();
        let record = saved(auth, &context)?;
        if last_hub.as_ref() != Some(&record.hub) {
            let report = auth.local_status()?;
            validity = report.certificate;
            last_hub = Some(record.hub.clone());
            if let Some(validity) = validity {
                context.certificate(validity, CertificateUpdate::NotNeeded);
            } else {
                context.certificate_unavailable(report.certificate_update);
            }
        }
        let key = context.ha_key();
        if maintained != key {
            maintained = key.clone();
            due = context.clock.now();
            backoff = Backoff::new(1, 30);
        }
        let now = context.clock.unix_now();
        if let Some(validity) = validity
            && now >= validity.not_after
            && last_expiry != Some(validity.not_after)
        {
            last_expiry = Some(validity.not_after);
            eligibility_changed(&context, "hub certificate expired");
        }
        let needs_renewal = validity.is_none_or(|validity| validity.renewal_due(now));
        if needs_renewal && context.token_usable() && context.clock.now() >= due {
            let key = key.unwrap();
            let attempt = async {
                let renewal = auth
                    .renew_bridge_certificate(&record, key.generation, || context.clock.unix_now());
                let mut renewal = std::pin::pin!(renewal);
                let result = if let Some(validity) =
                    validity.filter(|validity| validity.currently_valid(now))
                {
                    match future::or(async { Some(renewal.as_mut().await) }, async {
                        context
                            .clock
                            .wait_until(deadline(&context, validity.not_after))
                            .await;
                        None
                    })
                    .await
                    {
                        Some(result) => result,
                        None => {
                            let current = saved(auth, &context)?;
                            if current.uid == record.uid && current.hub == record.hub {
                                last_expiry = Some(validity.not_after);
                                eligibility_changed(&context, "hub certificate expired");
                            }
                            renewal.await
                        }
                    }
                } else {
                    renewal.await
                };
                Ok::<_, AdapterError>(Some(result))
            };
            let result = future::or(attempt, async {
                ha_retired(&context, &key).await;
                Ok(None)
            })
            .await?;
            let Some(result) = result else {
                saved(auth, &context)?;
                continue;
            };
            match result {
                Ok(renewed) => {
                    if !context.current_ha(&key) {
                        saved(auth, &context)?;
                        continue;
                    }
                    context.adopt_credentials(renewed.credentials);
                    last_hub = Some(context.credentials.borrow().record().unwrap().hub.clone());
                    let failed = matches!(renewed.update, CertificateUpdate::Failed(_));
                    validity = renewed.validity;
                    if let Some(validity) = renewed.validity {
                        context.certificate(validity, renewed.update);
                    } else {
                        context.certificate_unavailable(renewed.update);
                    }
                    due = context.clock.now() + super::cloud::MEMBER_INTERVAL;
                    if !failed {
                        context.confirm_ha();
                        backoff = Backoff::new(1, 30);
                    }
                }
                Err(AuthError::Missing(_)) => {
                    saved(auth, &context)?;
                }
                Err(AuthError::Cloud(error)) => {
                    let current = saved(auth, &context)?;
                    if !context.current_ha(&key) {
                        continue;
                    }
                    last_hub = Some(current.hub);
                    let delay = if error.is_temporary() {
                        backoff.failed()
                    } else {
                        super::cloud::MEMBER_INTERVAL
                    };
                    if error.is_unauthorized() {
                        context.token_rejected(&key, FailureReason::Cloud(error.clone()))?;
                    }
                    let update = CertificateUpdate::Failed(FailureReason::Cloud(error));
                    if let Some(validity) = validity {
                        context.certificate(validity, update);
                    } else {
                        context.certificate_unavailable(update);
                    }
                    due = context.clock.now() + delay;
                }
                Err(AuthError::Failure(reason)) => {
                    if !context.current_ha(&key) {
                        continue;
                    }
                    let update = CertificateUpdate::Failed(reason);
                    if let Some(validity) = validity {
                        context.certificate(validity, update);
                    } else {
                        context.certificate_unavailable(update);
                    }
                    due = context.clock.now() + super::cloud::MEMBER_INTERVAL;
                }
                Err(error) => return Err(error.into()),
            }
            continue;
        }
        let expiry = validity
            .filter(|validity| now < validity.not_after)
            .map(|validity| deadline(&context, validity.not_after));
        let renewal = context.token_usable().then(|| match validity {
            Some(validity) => due.max(deadline(&context, validity.renewal_at())),
            None => due,
        });
        let next = expiry.into_iter().chain(renewal).min();
        future::or(changed, async {
            if let Some(next) = next {
                context.clock.wait_until(next).await;
            } else {
                future::pending::<()>().await;
            }
        })
        .await;
    }
}

async fn mijia(auth: &AuthService, context: Rc<Context>) -> Result<(), AdapterError> {
    let mut backoff = Backoff::new(1, 30);
    let mut retry_due = None;
    let mut blocked = false;
    loop {
        let record = saved(auth, &context)?;
        let generation = context
            .credentials
            .borrow()
            .mijia_generation
            .expect("active account");
        let changed = context.mijia_changed.listen();
        let credentials_changed = context.credentials_changed.listen();
        let rejected = context.credentials.borrow().mijia_rejected() == Some(generation);
        if !rejected {
            future::or(changed, credentials_changed).await;
            continue;
        }
        if let Some(stop) = record.recovery.stop {
            context.mijia_authentication(AuthenticationState::SignInRequired(
                FailureReason::RecoveryStopped(stop),
            ));
            future::or(changed, credentials_changed).await;
            continue;
        }
        if blocked {
            future::or(changed, credentials_changed).await;
            continue;
        }
        if let Some(due) = retry_due
            && context.clock.now() < due
        {
            future::or(
                future::or(changed, credentials_changed),
                context.clock.wait_until(due),
            )
            .await;
            continue;
        }
        retry_due = None;
        context.mijia_authentication(AuthenticationState::Restoring);
        match auth.recover_mijia(&record).await {
            Ok(_) => {
                saved(auth, &context)?;
                context.mijia_authentication(AuthenticationState::Authenticated);
                context.request_sync();
                context.refresh_changed.notify(usize::MAX);
                backoff = Backoff::new(1, 30);
            }
            Err(AuthError::Storage(error)) => return Err(error.into()),
            Err(error) => {
                let current = saved(auth, &context)?;
                let stop = current.recovery.stop;
                let (reason, retry) = match error {
                    AuthError::Account(error) => {
                        let retry = matches!(
                            error.kind(),
                            AccountErrorKind::Network | AccountErrorKind::Timeout
                        );
                        (FailureReason::Account(error), retry)
                    }
                    AuthError::Cloud(error) => {
                        let retry = error.is_temporary();
                        (FailureReason::Cloud(error), retry)
                    }
                    AuthError::Failure(reason) => (reason, false),
                    other => return Err(other.into()),
                };
                if let Some(stop) = stop {
                    context.mijia_authentication(AuthenticationState::SignInRequired(
                        FailureReason::RecoveryStopped(stop),
                    ));
                } else {
                    context.mijia_authentication(AuthenticationState::Unavailable(reason));
                }
                if retry && stop.is_none() {
                    retry_due = Some(context.clock.now() + backoff.failed());
                } else {
                    blocked = true;
                }
            }
        }
    }
}

pub(super) async fn run(auth: &AuthService, context: Rc<Context>) -> Result<(), AdapterError> {
    future::or(
        mijia(auth, context.clone()),
        future::or(tokens(auth, context.clone()), certificates(auth, context)),
    )
    .await
}
