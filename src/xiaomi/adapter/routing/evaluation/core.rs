//! Shared access and active-notification facts. Summaries never build details.

use super::{
    choices::{Operation, select_kind},
    notifications::{NotificationBindings, NotificationFacts},
    *,
};
use crate::xiaomi::certificate::CertificateStatus;
use std::{cell::RefCell, collections::BTreeSet};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::xiaomi::adapter) struct Selection {
    pub(super) control: Option<PathKind>,
    pub(super) read: Option<PathKind>,
    pub(super) notification: Option<PathKind>,
}

#[derive(Clone, Copy, Default, Eq, Ord, PartialEq, PartialOrd)]
pub(in crate::xiaomi::adapter) struct Deadlines {
    certificate: bool,
    token: bool,
}

impl Deadlines {
    pub(in crate::xiaomi::adapter) fn reason(self) -> &'static str {
        match (self.certificate, self.token) {
            (true, true) => "hub certificate expired; access token expired",
            (true, false) => "hub certificate expired",
            (false, true) => "access token expired",
            (false, false) => unreachable!("a deadline explanation relaxes at least one expiry"),
        }
    }
}

pub(in crate::xiaomi::adapter) struct DeviceSummary {
    pub(in crate::xiaomi::adapter) selection: Selection,
    pub(in crate::xiaomi::adapter) deadline: Option<Deadlines>,
}

pub(in crate::xiaomi::adapter) struct Summaries {
    pub(in crate::xiaomi::adapter) priority: PathPriority,
    pub(in crate::xiaomi::adapter) devices: BTreeMap<String, DeviceSummary>,
}

#[derive(Clone, Copy)]
pub(super) enum Unavailable {
    Fact(&'static str),
    Certificate,
    Authentication(AuthenticationFailure),
    HubAccess,
    DirectNotReady,
}

#[derive(Clone, Copy)]
pub(super) enum AuthenticationFailure {
    ScopeRevoked,
    Credentials,
    IdentityMismatch,
}

impl AuthenticationFailure {
    pub(super) fn reason(self) -> &'static str {
        match self {
            Self::ScopeRevoked => "account/home scope revoked",
            Self::Credentials => "credentials unavailable, expired or rejected",
            Self::IdentityMismatch => "account/home identity mismatch",
        }
    }
}

pub(super) struct Access {
    pub(super) read_only: bool,
    pub(super) has_sensors: bool,
    pub(super) virtual_group: bool,
    pub(super) hub: Result<SessionHandle, Unavailable>,
    pub(super) hub_push: Result<SessionHandle, Unavailable>,
    pub(super) direct: Result<Client, Unavailable>,
    pub(super) cloud_http: Result<CloudPath, Unavailable>,
    pub(super) cloud_sensor_http: Result<CloudPath, Unavailable>,
    pub(super) cloud_push: Result<(), Unavailable>,
    pub(super) cloud_sensor_push: Result<(), Unavailable>,
}

impl Access {
    fn control_operation(&self) -> Operation {
        if self.read_only {
            Operation::Disabled
        } else {
            Operation::Control
        }
    }
    pub(super) fn read_operation(&self) -> Operation {
        if self.virtual_group {
            Operation::Disabled
        } else {
            Operation::Read
        }
    }
    fn selected(&self, priority: PathPriority, control: bool) -> Option<PathKind> {
        let cloud = if !control && self.has_sensors {
            &self.cloud_sensor_http
        } else {
            &self.cloud_http
        };
        select_kind(
            priority,
            if control {
                self.control_operation()
            } else {
                self.read_operation()
            },
            self.hub.is_ok(),
            self.direct.is_ok(),
            cloud.is_ok(),
        )
    }
}

type AccessKey = (Deadlines, String);
type BindingKey = (Deadlines, String, String);

pub(super) struct Core<'a> {
    pub(super) directory: Ref<'a, Directory>,
    pub(super) hub: Ref<'a, Hub>,
    pub(super) graph: Rc<DerivedConfiguration>,
    pub(super) priority: PathPriority,
    ha_authorized: bool,
    token_expired: bool,
    certificate: Option<CertificateStatus>,
    scope_invalid: bool,
    identity_matches: bool,
    mijia_usable: bool,
    http: Result<CloudPath, Unavailable>,
    notifications: Option<NotificationHandle>,
    accesses: RefCell<BTreeMap<AccessKey, Rc<Access>>>,
    bindings: RefCell<BTreeMap<BindingKey, Rc<NotificationBindings>>>,
    selections: RefCell<BTreeMap<AccessKey, Selection>>,
}

impl<'a> Core<'a> {
    pub(super) fn new(context: &'a Context, now: i64) -> Self {
        let directory = context.directory.borrow();
        let credentials = context.credentials.borrow();
        let hub = context.hub.borrow();
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
            Err(Unavailable::Fact("account/home scope not validated"))
        } else if credentials.http_paused() {
            Err(Unavailable::Fact("Mijia HTTP unavailable"))
        } else {
            path.ok_or(Unavailable::Fact("Mijia HTTP unavailable"))
        };
        let mut core = Self {
            ha_authorized: credentials.token_authorized(),
            token_expired: credentials.token_expired(now),
            certificate: credentials.certificate_status(now),
            directory,
            priority: hub.priority(),
            hub,
            identity_matches,
            mijia_usable,
            http,
            graph: context.graph.borrow().clone(),
            scope_invalid: context.lan_scope_invalid.get(),
            notifications: None,
            accesses: RefCell::new(BTreeMap::new()),
            bindings: RefCell::new(BTreeMap::new()),
            selections: RefCell::new(BTreeMap::new()),
        };
        // Capture the real session identity once. A counterfactual may relax
        // expiry, but can never restore a rejected or replaced authorization.
        if core
            .authentication(Deadlines {
                token: true,
                certificate: false,
            })
            .is_ok()
        {
            core.notifications = (|| {
                let record = credentials.record()?;
                let key = credentials.ha_key()?;
                if !context.store.xiaomi().current_ha(&key.uid, key.generation) {
                    return None;
                }
                let sessions = context.notifications.borrow();
                let session = sessions.as_ref()?;
                connection_matches(
                    session,
                    record,
                    &key,
                    &core.directory.scope.as_ref()?.home_id,
                )
                .then(|| session.handle.clone())
            })();
        }
        core
    }

    pub(super) fn authentication(&self, relaxed: Deadlines) -> Result<(), AuthenticationFailure> {
        if self.scope_invalid {
            Err(AuthenticationFailure::ScopeRevoked)
        } else if !self.ha_authorized || self.token_expired && !relaxed.token {
            Err(AuthenticationFailure::Credentials)
        } else if !self.identity_matches {
            Err(AuthenticationFailure::IdentityMismatch)
        } else {
            Ok(())
        }
    }

    pub(super) fn notification_handle(&self, relaxed: Deadlines) -> Option<NotificationHandle> {
        self.authentication(relaxed)
            .ok()
            .and_then(|()| self.notifications.clone())
    }

    fn local(
        &self,
        did: &str,
        slot: Option<&DeviceSlot>,
        retired: bool,
        relaxed: Deadlines,
    ) -> Result<SessionHandle, Unavailable> {
        let slot = slot.ok_or(Unavailable::Fact("device description unavailable"))?;
        let session = self
            .hub
            .session()
            .ok_or(Unavailable::Fact("gateway disconnected"))?;
        match self.certificate {
            Some(CertificateStatus::Valid | CertificateStatus::RenewalDue) => {}
            Some(CertificateStatus::Expired) if relaxed.certificate => {}
            _ => return Err(Unavailable::Certificate),
        }
        if self
            .graph
            .rules
            .get(did)
            .is_some_and(|group| !group.is_physical)
        {
            return Err(Unavailable::Fact("virtual group has no hub route"));
        }
        slot.work.as_ref().ok_or(Unavailable::Fact(
            "device description or binding unavailable",
        ))?;
        if retired {
            return Err(Unavailable::Fact("device source retired"));
        }
        let access = self
            .hub
            .access(did)
            .ok_or(Unavailable::Fact("device access not available"))?
            .as_ref()
            .map_err(|_| Unavailable::HubAccess)?;
        if !access.online {
            return Err(Unavailable::Fact("device offline"));
        }
        if !access.spec_v2_access {
            return Err(Unavailable::Fact("property access unavailable"));
        }
        Ok(session.handle.clone())
    }

    fn direct(&self, slot: Option<&DeviceSlot>, retired: bool) -> Result<Client, Unavailable> {
        let slot = slot.ok_or(Unavailable::Fact("device description unavailable"))?;
        let work = slot.work.as_ref().ok_or(Unavailable::Fact(
            "device description or binding unavailable",
        ))?;
        if self.scope_invalid || retired {
            return Err(Unavailable::Fact("device source retired or scope revoked"));
        }
        if work.native.direct_protocol().is_none() {
            return Err(Unavailable::Fact("device model not supported"));
        }
        work.lan
            .borrow()
            .active
            .as_ref()
            .map(|(_, client)| client.clone())
            .ok_or(Unavailable::DirectNotReady)
    }

    fn cloud_device(
        &self,
        slot: Option<&DeviceSlot>,
        retired: bool,
    ) -> Result<Option<bool>, Unavailable> {
        if !self.identity_matches {
            return Err(Unavailable::Fact("account/home identity mismatch"));
        }
        let slot = slot.ok_or(Unavailable::Fact("device description unavailable"))?;
        slot.work.as_ref().ok_or(Unavailable::Fact(
            "device description or binding unavailable",
        ))?;
        if retired {
            return Err(Unavailable::Fact("device source retired"));
        }
        if slot.cloud_forbidden {
            return Err(Unavailable::Fact("device access forbidden"));
        }
        Ok(slot.cloud_access.as_ref().map(|access| access.online))
    }

    pub(super) fn access(&self, did: &str, relaxed: Deadlines) -> Rc<Access> {
        let key = (relaxed, did.to_owned());
        if let Some(access) = self.accesses.borrow().get(&key) {
            return access.clone();
        }
        let slot = self.directory.devices.get(did);
        let work = slot.and_then(|slot| slot.work.as_ref());
        let native = work.map(|work| &work.native).or_else(|| {
            slot.and_then(|slot| slot.latest_description.as_ref())
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
        let virtual_group = self
            .graph
            .rules
            .get(did)
            .is_some_and(|group| !group.is_physical);
        let retired = work.is_some_and(|work| work.life.is_closed());
        let hub = self.local(did, slot, retired, relaxed);
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
                Err(Unavailable::Fact("notification access unavailable"))
            }
        });
        let direct = self.direct(slot, retired);
        let cloud_device = self.cloud_device(slot, retired);
        let http_access = cloud_device
            .and_then(|online| online.ok_or(Unavailable::Fact("device information not available")));
        let push_access = cloud_device.and_then(|online| match online {
            Some(online) => Ok(online),
            None if !self.mijia_usable => Ok(true),
            None => Err(Unavailable::Fact("device information not available")),
        });
        let online = |access: Result<bool, Unavailable>| {
            access.and_then(|online| {
                if online {
                    Ok(())
                } else {
                    Err(Unavailable::Fact("device offline"))
                }
            })
        };
        let cloud_http = self
            .http
            .clone()
            .and_then(|path| online(http_access).map(|()| path));
        let cloud_push = self
            .authentication(relaxed)
            .map_err(Unavailable::Authentication)
            .and_then(|()| online(push_access));
        let (cloud_sensor_http, cloud_sensor_push) = if unreliable_cloud_online(did) {
            (
                self.http.clone().and_then(|path| http_access.map(|_| path)),
                self.authentication(relaxed)
                    .map_err(Unavailable::Authentication)
                    .and_then(|()| push_access.map(|_| ())),
            )
        } else {
            (cloud_http.clone(), cloud_push)
        };
        let access = Rc::new(Access {
            read_only,
            has_sensors,
            virtual_group,
            hub,
            hub_push,
            direct,
            cloud_http,
            cloud_sensor_http,
            cloud_push,
            cloud_sensor_push,
        });
        self.accesses.borrow_mut().insert(key, access.clone());
        access
    }

    pub(super) fn bindings(
        &self,
        did: &str,
        id: &str,
        relaxed: Deadlines,
    ) -> Rc<NotificationBindings> {
        let key = (relaxed, did.to_owned(), id.to_owned());
        if let Some(bindings) = self.bindings.borrow().get(&key) {
            return bindings.clone();
        }
        let access = self.access(did, relaxed);
        let work = self
            .directory
            .devices
            .get(did)
            .and_then(|slot| slot.work.as_ref());
        let sensor = work.is_some_and(|work| {
            work.native
                .device()
                .functions()
                .iter()
                .find(|function| function.id() == id)
                .is_some_and(|function| function.kind().is_read_only())
                || id == "battery" && work.native.device().battery().is_some()
        });
        let facts = NotificationFacts::new(
            work,
            access.hub_push.as_ref().ok(),
            self.notification_handle(relaxed).as_ref(),
            access.virtual_group,
            self.priority,
        );
        let bindings = Rc::new(facts.bindings(
            id,
            if sensor {
                access.cloud_sensor_push.is_ok()
            } else {
                access.cloud_push.is_ok()
            },
        ));
        self.bindings.borrow_mut().insert(key, bindings.clone());
        bindings
    }

    fn active(&self, did: &str, id: &str, relaxed: Deadlines) -> Option<PathKind> {
        self.directory.devices.get(did)?.work.as_ref()?;
        self.bindings(did, id, relaxed)
            .active
            .as_ref()
            .map(NotificationSource::kind)
    }

    pub(super) fn selection(&self, did: &str, relaxed: Deadlines) -> Selection {
        let key = (relaxed, did.to_owned());
        if let Some(selected) = self.selections.borrow().get(&key) {
            return *selected;
        }
        let access = self.access(did, relaxed);
        let control = access.selected(self.priority, true);
        let read = access.selected(self.priority, false);
        let notification = if access.virtual_group {
            let group = &self.graph.rules[did];
            (group.members.len() == group.confirmed_members && control.is_some())
                .then(|| {
                    group
                        .members
                        .iter()
                        .try_fold(PathKind::Local, |path, member| {
                            let member_access = self.access(&member.device.id, relaxed);
                            let kind = if member_access.virtual_group {
                                self.selection(&member.device.id, relaxed).notification?
                            } else {
                                self.active(&member.device.id, &member.id, relaxed)?
                            };
                            Some(if kind == PathKind::Cloud { kind } else { path })
                        })
                })
                .flatten()
        } else {
            self.directory
                .devices
                .get(did)
                .and_then(|slot| slot.work.as_ref())
                .and_then(|work| {
                    work.native
                        .device()
                        .functions()
                        .iter()
                        .find_map(|function| self.active(did, function.id(), relaxed))
                })
        };
        let selected = Selection {
            control,
            read,
            notification,
        };
        self.selections.borrow_mut().insert(key, selected);
        selected
    }

    fn deadline(&self, did: &str, old: Selection, selected: Selection) -> Option<Deadlines> {
        if old == selected {
            return None;
        }
        let certificate = self.certificate == Some(CertificateStatus::Expired);
        let token = self.token_expired;
        // Try minimal explanations first, preserving every non-temporal fact.
        [
            Deadlines {
                certificate: true,
                token: false,
            },
            Deadlines {
                certificate: false,
                token: true,
            },
            Deadlines {
                certificate: true,
                token: true,
            },
        ]
        .into_iter()
        .filter(|relaxed| (!relaxed.certificate || certificate) && (!relaxed.token || token))
        .find(|relaxed| self.selection(did, *relaxed) == old)
    }

    pub(super) fn summaries(&self, dids: &BTreeSet<String>) -> Summaries {
        let devices = dids
            .iter()
            .filter_map(|did| {
                let slot = self.directory.devices.get(did)?;
                slot.work.as_ref()?;
                let selection = self.selection(did, Deadlines::default());
                let deadline = slot
                    .routing_history
                    .as_ref()
                    .and_then(|history| self.deadline(did, history.selection, selection));
                Some((
                    did.clone(),
                    DeviceSummary {
                        selection,
                        deadline,
                    },
                ))
            })
            .collect();
        Summaries {
            priority: self.priority,
            devices,
        }
    }

    pub(super) fn serving(&self) -> BTreeSet<String> {
        self.directory
            .devices
            .iter()
            .filter(|(_, slot)| slot.work.is_some())
            .map(|(did, _)| did.clone())
            .collect()
    }
}
