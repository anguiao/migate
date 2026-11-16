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
use std::{cell::Ref, collections::BTreeMap};

mod choices;
mod notifications;
use choices::{Candidates, Operation, RouteChoice};
use notifications::{NotificationFacts, NotificationRoutes};

#[derive(Clone)]
pub(in crate::xiaomi::adapter) struct FunctionRoutes {
    pub(in crate::xiaomi::adapter) control: RouteChoice,
    pub(in crate::xiaomi::adapter) read: RouteChoice,
    pub(in crate::xiaomi::adapter) low_speed: Option<RouteChoice>,
    pub(in crate::xiaomi::adapter) notification: NotificationRoutes,
    pub(in crate::xiaomi::adapter) read_only: bool,
}

#[derive(Clone)]
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
    pub(in crate::xiaomi::adapter) priority: PathPriority,
    pub(in crate::xiaomi::adapter) local_connected: bool,
    pub(in crate::xiaomi::adapter) devices: BTreeMap<String, Rc<DeviceRoutes>>,
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

/// Borrows are confined to construction; consumers receive owned route results.
struct Facts<'a> {
    directory: Ref<'a, Directory>,
    hub: Ref<'a, Hub>,
    graph: Rc<DerivedConfiguration>,
    priority: PathPriority,
    hub_usable: bool,
    scope_invalid: bool,
    identity_matches: bool,
    mijia_usable: bool,
    http: Result<CloudPath, String>,
    authentication: Result<(), String>,
    notifications: Option<NotificationHandle>,
    devices: std::cell::RefCell<BTreeMap<String, Rc<DeviceRoutes>>>,
}

impl<'a> Facts<'a> {
    fn new(context: &'a Context, now: i64) -> Self {
        let directory = context.directory.borrow();
        let credentials = context.credentials.borrow();
        let hub = context.hub.borrow();
        let scope_invalid = context.lan_scope_invalid.get();
        let identity_matches = credentials.record().is_some_and(|record| {
            directory
                .scope
                .as_ref()
                .is_some_and(|scope| scope.uid == record.uid)
        });
        let path =
            credentials.mijia_path(directory.scope.as_ref().map(|scope| scope.home_id.clone()));
        let mijia_usable = path.is_some() && !credentials.http_paused();
        let http = if !directory.scope_valid() {
            Err("account/home scope not validated".into())
        } else if credentials.http_paused() {
            Err("Mijia HTTP unavailable".into())
        } else {
            path.ok_or_else(|| "Mijia HTTP unavailable".into())
        };
        let authentication = if scope_invalid {
            Err("account/home scope revoked".into())
        } else if !credentials.token_usable(now) {
            Err("credentials unavailable, expired or rejected".into())
        } else if !identity_matches {
            Err("account/home identity mismatch".into())
        } else {
            Ok(())
        };
        let notifications = authentication.as_ref().ok().and_then(|_| {
            let record = credentials.record()?;
            let key = credentials.ha_key()?;
            if !context.store.xiaomi().current_ha(&key.uid, key.generation) {
                return None;
            }
            let sessions = context.notifications.borrow();
            let session = sessions.as_ref()?;
            connection_matches(session, record, &key, &directory.scope.as_ref()?.home_id)
                .then(|| session.handle.clone())
        });
        Self {
            priority: hub.priority(),
            hub_usable: credentials.hub_usable(now),
            directory,
            hub,
            graph: context.graph.borrow().clone(),
            scope_invalid,
            identity_matches,
            mijia_usable,
            http,
            authentication,
            notifications,
            devices: std::cell::RefCell::new(BTreeMap::new()),
        }
    }

    fn virtual_group(&self, did: &str) -> bool {
        self.graph
            .rules
            .get(did)
            .is_some_and(|group| !group.is_physical)
    }

    fn local(
        &self,
        did: &str,
        slot: Option<&DeviceSlot>,
        retired: bool,
    ) -> Result<SessionHandle, String> {
        let slot = slot.ok_or("device description unavailable")?;
        let session = self.hub.session().ok_or("gateway disconnected")?;
        if !self.hub_usable {
            return Err("gateway certificate not currently valid".into());
        }
        if self.virtual_group(did) {
            return Err("virtual group has no hub route".into());
        }
        slot.admitted
            .as_ref()
            .ok_or("device description or binding unavailable")?;
        if retired {
            return Err("device source retired".into());
        }
        let access = self
            .hub
            .access(did)
            .ok_or("device access not available")?
            .as_ref()
            .map_err(ToString::to_string)?;
        if !access.online {
            return Err("device offline".into());
        }
        if !access.spec_v2_access {
            return Err("property access unavailable".into());
        }
        Ok(session.handle.clone())
    }

    fn direct(&self, slot: Option<&DeviceSlot>, retired: bool) -> Result<Client, String> {
        let slot = slot.ok_or("device description unavailable")?;
        slot.admitted
            .as_ref()
            .ok_or("device description or binding unavailable")?;
        if self.scope_invalid || retired {
            return Err("device source retired or scope revoked".into());
        }
        if slot
            .committed
            .as_ref()
            .and_then(|source| source.native.as_ref())
            .is_none_or(|native| super::super::lan::protocol(native).is_none())
        {
            return Err("device model not supported".into());
        }
        slot.lan
            .active
            .as_ref()
            .map(|(_, client)| client.clone())
            .ok_or_else(|| {
                slot.lan.status.as_ref().map_or_else(
                    || "not yet initialized".into(),
                    |status| {
                        status.problem.clone().unwrap_or_else(|| {
                            match status.state {
                                super::super::model::LanState::Ineligible => {
                                    "device is not an eligible IP device"
                                }
                                super::super::model::LanState::MissingCredentials => {
                                    "credentials missing"
                                }
                                super::super::model::LanState::InvalidCredentials => {
                                    "credentials invalid"
                                }
                                super::super::model::LanState::Authenticating => {
                                    "authentication in progress"
                                }
                                super::super::model::LanState::Ready => "session unavailable",
                                super::super::model::LanState::Failed => {
                                    "authentication or communication failed"
                                }
                            }
                            .into()
                        })
                    },
                )
            })
    }

    fn cloud_device_access(
        &self,
        slot: Option<&DeviceSlot>,
        retired: bool,
    ) -> Result<Option<bool>, String> {
        if !self.identity_matches {
            return Err("account/home identity mismatch".into());
        }
        let slot = slot.ok_or("device description unavailable")?;
        slot.admitted
            .as_ref()
            .ok_or("device description or binding unavailable")?;
        if retired {
            return Err("device source retired".into());
        }
        if slot.cloud_forbidden {
            return Err("device access forbidden".into());
        }
        Ok(slot.cloud_access.as_ref().map(|access| access.online))
    }

    fn device(&self, did: &str) -> Rc<DeviceRoutes> {
        if let Some(routes) = self.devices.borrow().get(did) {
            return routes.clone();
        }
        let slot = self.directory.devices.get(did);
        let work = slot.and_then(|slot| slot.admitted.as_ref());
        let native = work.map(|work| &work.native).or_else(|| {
            slot.and_then(|slot| slot.candidate.as_ref())
                .and_then(|source| source.native.as_ref())
        });
        let read_only = native.is_some_and(|native| native.device().is_read_only());
        let has_sensors = work.is_some_and(|work| {
            work.native
                .device()
                .functions()
                .iter()
                .any(|function| function.kind().is_read_only())
        });
        let virtual_group = self.virtual_group(did);
        let retired = work.is_some_and(|work| work.life.is_closed());
        let hub = self.local(did, slot, retired);
        let hub_push = hub.clone().and_then(|handle| {
            if self
                .hub
                .access(did)
                .unwrap()
                .as_ref()
                .unwrap()
                .push_available
            {
                Ok(handle)
            } else {
                Err("notification access unavailable".into())
            }
        });
        let direct = self.direct(slot, retired);
        let cloud_device = self.cloud_device_access(slot, retired);
        let http_access = cloud_device
            .clone()
            .and_then(|online| online.ok_or_else(|| "device information not available".into()));
        let push_access = cloud_device.and_then(|online| match online {
            Some(online) => Ok(online),
            None if !self.mijia_usable => Ok(true),
            None => Err("device information not available".into()),
        });
        let online = |result: Result<bool, String>| {
            result.and_then(|online| {
                if online {
                    Ok(())
                } else {
                    Err("device offline".into())
                }
            })
        };
        let cloud_http = self
            .http
            .clone()
            .and_then(|path| online(http_access.clone()).map(|_| path));
        let cloud_push = self
            .authentication
            .clone()
            .and_then(|_| online(push_access.clone()));
        let (cloud_sensor_http, cloud_sensor_push) = if unreliable_cloud_online(did) {
            (
                self.http.clone().and_then(|path| http_access.map(|_| path)),
                self.authentication
                    .clone()
                    .and_then(|_| push_access.map(|_| ())),
            )
        } else {
            (cloud_http.clone(), cloud_push.clone())
        };
        let candidates = |cloud: Result<CloudPath, String>| Candidates {
            hub: hub.clone(),
            direct: direct.clone(),
            cloud,
        };
        let control = RouteChoice::new(
            if read_only {
                Candidates::unavailable("device has no control function")
            } else {
                candidates(cloud_http.clone())
            },
            self.priority,
            if read_only {
                Operation::Disabled
            } else {
                Operation::Control
            },
        );
        let read = RouteChoice::new(
            candidates(if has_sensors {
                cloud_sensor_http.clone()
            } else {
                cloud_http.clone()
            }),
            self.priority,
            if virtual_group {
                Operation::Disabled
            } else {
                Operation::Read
            },
        );
        let notifications = NotificationFacts::new(
            work,
            &hub_push,
            self.notifications.as_ref(),
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
                let direct = if profile.is_some_and(|profile| profile.target() != id) {
                    Err("committed direct protocol does not support this function".into())
                } else {
                    direct.clone()
                };
                let control = RouteChoice::new(
                    if sensor || read_only {
                        Candidates::unavailable("function is read-only")
                    } else {
                        Candidates {
                            direct: direct.clone(),
                            ..candidates(cloud_http.clone())
                        }
                    },
                    self.priority,
                    if sensor || read_only {
                        Operation::Disabled
                    } else {
                        Operation::Control
                    },
                );
                let read = RouteChoice::new(
                    Candidates {
                        direct,
                        ..candidates(if sensor {
                            cloud_sensor_http.clone()
                        } else {
                            cloud_http.clone()
                        })
                    },
                    self.priority,
                    if virtual_group {
                        Operation::Disabled
                    } else {
                        Operation::Read
                    },
                );
                let low_speed = function
                    .and_then(|function| function.fan())
                    .and_then(|fan| fan.shared_speed())
                    .map(|speed| {
                        RouteChoice::new(
                            Candidates {
                                hub: Err("low speed has no hub operation".into()),
                                direct: candidates(cloud_http.clone()).direct,
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
                let notification = notifications.function(&id, cloud_eligibility);
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
        let notification_source = if virtual_group {
            self.graph
                .rules
                .get(did)
                .filter(|group| group.members.len() == group.confirmed_members)
                .filter(|_| control.selected.is_some())
                .and_then(|group| {
                    group
                        .members
                        .iter()
                        .try_fold(PathKind::Local, |path, member| {
                            let routes = self.device(&member.device.id);
                            let kind = if routes.virtual_group {
                                routes.notification_source?
                            } else {
                                routes
                                    .functions
                                    .get(&member.id)?
                                    .notification
                                    .active
                                    .as_ref()?
                                    .kind()
                            };
                            Some(if kind == PathKind::Cloud { kind } else { path })
                        })
                })
        } else {
            work.and_then(|work| {
                work.native
                    .device()
                    .functions()
                    .iter()
                    .find_map(|function| {
                        functions[function.id()]
                            .notification
                            .active
                            .as_ref()
                            .map(NotificationSource::kind)
                    })
            })
        };
        let available = control.selected.is_some()
            || functions.iter().any(|(id, function)| {
                id != "battery"
                    && function.read_only
                    && (function.read.selected.is_some() || function.notification.active.is_some())
            });
        let unavailable_reason = (!available).then(|| read.candidates.unavailable_reason());
        let sync_limited_reason = notification_source.is_none().then(|| {
            notifications.reason(if has_sensors {
                &cloud_sensor_push
            } else {
                &cloud_push
            })
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
        let routes = Rc::new(DeviceRoutes {
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
        });
        self.devices.borrow_mut().insert(did.into(), routes.clone());
        routes
    }
}

pub(super) fn evaluate(context: &Context, now: i64) -> RoutingEvaluation {
    let facts = Facts::new(context, now);
    let devices = facts
        .directory
        .devices
        .iter()
        .filter(|(_, slot)| slot.admitted.is_some())
        .map(|(did, _)| (did.clone(), facts.device(did)))
        .collect();
    RoutingEvaluation {
        priority: facts.priority,
        local_connected: facts.hub.session().is_some(),
        devices,
    }
}

pub(super) fn device(context: &Context, did: &str, now: i64) -> Rc<DeviceRoutes> {
    Facts::new(context, now).device(did)
}

impl Context {
    pub(in crate::xiaomi::adapter) fn cloud_authentication_at(
        &self,
        now: i64,
    ) -> Result<(), String> {
        Facts::new(self, now).authentication
    }

    pub(in crate::xiaomi::adapter) fn notification_handle_at(
        &self,
        now: i64,
    ) -> Option<NotificationHandle> {
        Facts::new(self, now).notifications
    }
}
