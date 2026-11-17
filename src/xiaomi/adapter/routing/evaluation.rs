//! Read-only routing decisions from one sample of the access and binding facts.

use super::*;
use crate::{
    configuration::DerivedConfiguration,
    xiaomi::{
        adapter::{
            directory::{DeviceSlot, Directory},
            hub::Hub,
            model::PathKind,
            notifications::{NotificationSource, connection_matches},
        },
        cloud::NotificationHandle,
    },
};
use std::{
    cell::Ref,
    collections::{BTreeMap, BTreeSet},
};

mod choices;
mod core;
mod details;
use core::{Core, Deadlines};
pub(in crate::xiaomi::adapter) use core::{Selection, Summaries};
mod notifications;
use choices::{Candidates, Operation, RouteChoice};
use notifications::{NotificationFacts, NotificationRoutes};

pub(in crate::xiaomi::adapter) struct FunctionRoutes {
    pub(in crate::xiaomi::adapter) control: RouteChoice,
    pub(in crate::xiaomi::adapter) read: RouteChoice,
    pub(in crate::xiaomi::adapter) low_speed: Option<RouteChoice>,
    pub(in crate::xiaomi::adapter) notification: NotificationRoutes,
    pub(in crate::xiaomi::adapter) read_only: bool,
}

pub(in crate::xiaomi::adapter) struct DeviceRoutes {
    pub(in crate::xiaomi::adapter) read_only: bool,
    pub(in crate::xiaomi::adapter) virtual_group: bool,
    pub(in crate::xiaomi::adapter) control: RouteChoice,
    pub(in crate::xiaomi::adapter) read: RouteChoice,
    pub(in crate::xiaomi::adapter) shared_feedback: Option<RouteChoice>,
    pub(in crate::xiaomi::adapter) functions: BTreeMap<String, FunctionRoutes>,
    pub(in crate::xiaomi::adapter) notification_source: Option<PathKind>,
    pub(in crate::xiaomi::adapter) sync_limited_reason: Option<String>,
    pub(in crate::xiaomi::adapter) physical_online: Option<bool>,
    pub(in crate::xiaomi::adapter) available: bool,
    pub(in crate::xiaomi::adapter) unavailable_reason: Option<String>,
    pub(in crate::xiaomi::adapter) hub_push: Result<SessionHandle, String>,
    pub(in crate::xiaomi::adapter) cloud_http: Result<CloudPath, String>,
    pub(in crate::xiaomi::adapter) cloud_sensor_http: Result<CloudPath, String>,
    pub(in crate::xiaomi::adapter) cloud_push: Result<(), String>,
    pub(in crate::xiaomi::adapter) cloud_sensor_push: Result<(), String>,
}

pub(in crate::xiaomi::adapter) struct RoutingEvaluation {
    pub(in crate::xiaomi::adapter) local_connected: bool,
    pub(in crate::xiaomi::adapter) devices: BTreeMap<String, DeviceRoutes>,
    pub(in crate::xiaomi::adapter) summaries: Summaries,
}

impl DeviceRoutes {
    pub(in crate::xiaomi::adapter) fn cloud_candidate(
        &self,
        http: bool,
        sensor: bool,
    ) -> Result<(), String> {
        match (http, sensor) {
            (true, true) => self
                .cloud_sensor_http
                .as_ref()
                .map(|_| ())
                .map_err(Clone::clone),
            (true, false) => self.cloud_http.as_ref().map(|_| ()).map_err(Clone::clone),
            (false, true) => self.cloud_sensor_push.clone(),
            (false, false) => self.cloud_push.clone(),
        }
    }
}

impl Core<'_> {
    fn device(&self, did: &str) -> DeviceRoutes {
        let slot = self.directory.devices.get(did);
        let work = slot.and_then(|slot| slot.admitted.as_ref());
        let access = self.access(did, Deadlines::default());
        let read_only = access.read_only;
        let has_sensors = access.has_sensors;
        let virtual_group = access.virtual_group;
        let hub = self.describe(did, &access.hub);
        let hub_push = self.describe(did, &access.hub_push);
        let direct = self.describe(did, &access.direct);
        let cloud_http = self.describe(did, &access.cloud_http);
        let cloud_sensor_http = self.describe(did, &access.cloud_sensor_http);
        let cloud_push = self.describe(did, &access.cloud_push);
        let cloud_sensor_push = self.describe(did, &access.cloud_sensor_push);
        let candidates = |direct, sensor| Candidates {
            hub: hub.clone(),
            direct,
            cloud: if sensor {
                cloud_sensor_http.clone()
            } else {
                cloud_http.clone()
            },
        };
        let selection = self.selection(did, Deadlines::default());
        let control = RouteChoice::from_selection(
            if read_only {
                Candidates::unavailable("device has no control function")
            } else {
                candidates(direct.clone(), false)
            },
            selection.control,
        );
        let read =
            RouteChoice::from_selection(candidates(direct.clone(), has_sensors), selection.read);
        let notifications = NotificationFacts::new(
            work,
            hub_push.as_ref().ok(),
            self.notification_handle(Deadlines::default()).as_ref(),
            virtual_group,
            self.priority,
        );
        let mut functions = BTreeMap::new();
        if let Some(work) = work {
            let profile = slot
                .unwrap()
                .committed
                .as_ref()
                .and_then(|source| source.native.as_ref())
                .and_then(|native| native.legacy_profile());
            for id in work.channels() {
                let function = work
                    .native
                    .device()
                    .functions()
                    .iter()
                    .find(|function| function.id() == id);
                let sensor = function.is_some_and(|function| function.kind().is_read_only())
                    || id == "battery" && work.native.device().battery().is_some();
                let function_direct = if profile.is_some_and(|profile| profile.target() != id) {
                    Err("committed direct protocol does not support this function".into())
                } else {
                    direct.clone()
                };
                let control = RouteChoice::new(
                    if sensor || read_only {
                        Candidates::unavailable("function is read-only")
                    } else {
                        candidates(function_direct.clone(), false)
                    },
                    self.priority,
                    Operation::Control,
                );
                let read = RouteChoice::new(
                    candidates(function_direct, sensor),
                    self.priority,
                    access.read_operation(),
                );
                let low_speed = function
                    .and_then(|function| function.fan())
                    .and_then(|fan| fan.shared_speed())
                    .map(|speed| {
                        RouteChoice::new(
                            Candidates {
                                hub: Err("low speed has no hub operation".into()),
                                direct: direct.clone(),
                                cloud: if speed.cloud_action.is_some() {
                                    cloud_http.clone()
                                } else {
                                    Err("low speed has no cloud action".into())
                                },
                            },
                            self.priority,
                            Operation::Control,
                        )
                    });
                let cloud_eligibility = if sensor {
                    &cloud_sensor_push
                } else {
                    &cloud_push
                };
                let notification = notifications.function(
                    &id,
                    cloud_eligibility,
                    &self.bindings(did, &id, Deadlines::default()),
                    hub_push.as_ref().err().map(String::as_str),
                );
                functions.insert(
                    id,
                    FunctionRoutes {
                        control,
                        read,
                        low_speed,
                        read_only: sensor,
                        notification,
                    },
                );
            }
        }
        let shared_feedback = work
            .filter(|work| !virtual_group && work.native.plan().shared_feedback().is_some())
            .map(|_| {
                RouteChoice::new(
                    Candidates {
                        hub: Err("shared feedback has no hub operation".into()),
                        direct: direct.clone(),
                        cloud: cloud_http.clone(),
                    },
                    self.priority,
                    Operation::Read,
                )
            });
        let notification_source = selection.notification;
        let available = control.selected.is_some()
            || functions.iter().any(|(id, function)| {
                id != "battery"
                    && function.read_only
                    && (function.read.selected.is_some() || function.notification.active.is_some())
            });
        let unavailable_reason = (!available).then(|| read.candidates.unavailable_reason());
        let sync_limited_reason = notification_source.is_none().then(|| {
            notifications.reason(
                if has_sensors {
                    &cloud_sensor_push
                } else {
                    &cloud_push
                },
                hub_push.as_ref().err().map(String::as_str),
            )
        });
        let physical_online = if has_sensors && unreliable_cloud_online(did) {
            None
        } else {
            slot.and_then(|slot| slot.cloud_access.as_ref())
                .map(|access| access.online)
                .or_else(|| {
                    self.hub
                        .access(did)
                        .and_then(|access| access.as_ref().ok())
                        .map(|access| access.online)
                })
        };
        DeviceRoutes {
            read_only,
            virtual_group,
            control,
            read,
            shared_feedback,
            functions,
            notification_source,
            sync_limited_reason,
            physical_online,
            available,
            unavailable_reason,
            hub_push,
            cloud_http,
            cloud_sensor_http,
            cloud_push,
            cloud_sensor_push,
        }
    }
}

impl Context {
    pub(in crate::xiaomi::adapter) fn routing_evaluation_at(&self, now: i64) -> RoutingEvaluation {
        let facts = Core::new(self, now);
        let dids = facts.serving();
        let summaries = facts.summaries(&dids);
        let devices = dids
            .into_iter()
            .map(|did| {
                let routes = facts.device(&did);
                (did, routes)
            })
            .collect();
        RoutingEvaluation {
            local_connected: facts.hub.session().is_some(),
            devices,
            summaries,
        }
    }

    pub(in crate::xiaomi::adapter) fn device_routes(&self, did: &str) -> DeviceRoutes {
        Core::new(self, self.clock.unix_now()).device(did)
    }

    /// Share immutable global inputs within this batch, then release all owner
    /// borrows before callers apply transitions or notify consumers.
    pub(in crate::xiaomi::adapter) fn device_routes_for<'a>(
        &self,
        dids: impl IntoIterator<Item = &'a str>,
    ) -> BTreeMap<String, DeviceRoutes> {
        let facts = Core::new(self, self.clock.unix_now());
        dids.into_iter()
            .map(|did| (did.to_owned(), facts.device(did)))
            .collect()
    }

    pub(in crate::xiaomi::adapter) fn routing_summaries_at(
        &self,
        now: i64,
        dids: Option<&BTreeSet<String>>,
    ) -> Summaries {
        let facts = Core::new(self, now);
        match dids {
            Some(affected) => facts.summaries(affected),
            None => facts.summaries(&facts.serving()),
        }
    }

    pub(in crate::xiaomi::adapter) fn cloud_authentication_at(
        &self,
        now: i64,
    ) -> Result<(), String> {
        let core = Core::new(self, now);
        core.authentication(Deadlines::default())
            .map_err(|failure| failure.reason().into())
    }
    pub(in crate::xiaomi::adapter) fn notification_handle_at(
        &self,
        now: i64,
    ) -> Option<NotificationHandle> {
        Core::new(self, now).notification_handle(Deadlines::default())
    }
}
