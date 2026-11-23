use super::{AuthError, AuthService, tokens};
use crate::storage::{HaAuthorization, TokenSet, XiaomiStore};
use futures_util::{
    FutureExt,
    future::{LocalBoxFuture, Shared, WeakShared},
};
use std::{cell::RefCell, future::Future};

pub(super) const HA_CHANGED: AuthError =
    AuthError::Missing("HA OAuth credentials changed during maintenance");

#[derive(Clone)]
pub(super) struct RefreshedTokens {
    pub(super) tokens: TokenSet,
    pub(super) generation: u64,
}

type RefreshTask = LocalBoxFuture<'static, Result<RefreshedTokens, AuthError>>;
pub(super) type HaRefresh = Shared<RefreshTask>;

/// Store clones share an in-flight refresh for the current HA generation.
#[derive(Default)]
pub(crate) struct HaRefreshes(RefCell<Option<(u64, WeakShared<RefreshTask>)>>);

impl HaRefreshes {
    pub(super) fn in_flight(&self, generation: u64) -> Option<HaRefresh> {
        self.0
            .borrow()
            .as_ref()
            .filter(|(active, _)| *active == generation)
            .and_then(|(_, refresh)| refresh.upgrade())
            .filter(|refresh| refresh.peek().is_none())
    }

    fn start(
        &self,
        generation: u64,
        task: impl Future<Output = Result<RefreshedTokens, AuthError>> + 'static,
    ) -> HaRefresh {
        let refresh = task.boxed_local().shared();
        // The refresh owns a Store clone while it is pending.
        *self.0.borrow_mut() = Some((generation, refresh.downgrade().expect("unpolled refresh")));
        refresh
    }
}

impl AuthService {
    pub(super) async fn refresh_tokens_once(
        &self,
        uid: &str,
        generation: u64,
        authorization: &HaAuthorization,
        now: impl FnOnce() -> i64 + 'static,
    ) -> Result<RefreshedTokens, AuthError> {
        require_ha(&self.store, uid, generation)?;
        let refreshes = self.store.ha_refreshes();
        let refresh = refreshes.in_flight(generation).unwrap_or_else(|| {
            let uid = uid.to_owned();
            let authorization = authorization.clone();
            let store = self.store.clone();
            let cloud = self.cloud.clone();
            refreshes.start(generation, async move {
                let response = cloud
                    .refresh_token(
                        &authorization.redirect_uri,
                        &authorization.tokens.refresh_token,
                    )
                    .await;
                require_ha(&store, &uid, generation)?;
                let response = match response {
                    Ok(response) => response,
                    Err(error) => {
                        if error.is_unauthorized() {
                            store.stop_ha_refresh()?;
                        }
                        return Err(AuthError::Cloud(error));
                    }
                };
                let tokens = tokens::from_response(response, now())?;
                store.update_tokens(&tokens)?;
                Ok(RefreshedTokens {
                    tokens,
                    generation: store.current_snapshot().ha_generation,
                })
            })
        });
        let result = refresh.await;
        match &result {
            Ok(refreshed) => require_ha(&self.store, uid, refreshed.generation)?,
            Err(_) if self.store.current_snapshot().ha_generation != generation => {
                return Err(HA_CHANGED);
            }
            Err(_) => {}
        }
        result
    }
}

pub(super) fn require_ha(store: &XiaomiStore, uid: &str, generation: u64) -> Result<(), AuthError> {
    if store.current_ha(uid, generation) {
        Ok(())
    } else {
        Err(HA_CHANGED)
    }
}
