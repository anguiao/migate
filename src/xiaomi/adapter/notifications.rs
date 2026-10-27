use super::{
    model::{AdapterError, PathKind, SubscriptionBinding},
    observations::{Observation, Origin},
    routing::PathPriority,
    state::{Backoff, Context, Lifetime},
    tasks::TaskSet,
    work::{DeviceWork, OnlinePreparation},
};
use crate::{
    storage::XiaomiRecord,
    xiaomi::{
        auth::FailureReason,
        central::SessionHandle,
        cloud::{NotificationDriver, NotificationError, NotificationEvent, NotificationHandle},
    },
};
use event_listener::Event;
use futures_lite::future;
use std::{cell::Cell, rc::Rc, time::Duration};

mod cloud;
mod delivery;
mod selection;

pub(super) use cloud::CloudSession;
use cloud::{connection_key, connections, prepare_properties, reconcile_online};
use selection::selections;

#[cfg(test)]
mod tests;
#[cfg(test)]
pub(super) use tests::support::NotificationConnector;
#[derive(Clone)]
pub(super) enum NotificationSource {
    Local(SessionHandle),
    Cloud(NotificationHandle),
}
impl NotificationSource {
    pub(super) fn kind(&self) -> PathKind {
        match self {
            Self::Local(_) => PathKind::Local,
            Self::Cloud(_) => PathKind::Cloud,
        }
    }

    pub(super) fn same(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Local(a), Self::Local(b)) => a.same_connection(b),
            (Self::Cloud(a), Self::Cloud(b)) => a.same_connection(b),
            _ => false,
        }
    }
}
pub(super) async fn run(context: Rc<Context>) -> Result<(), AdapterError> {
    future::or(selections(context.clone()), connections(context)).await
}
