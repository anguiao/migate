use super::{ActiveCloudNotifications, DeviceSessions, SessionContext};
use crate::xiaomi::runtime::cloud_connection::{
    CloudConnectionConfig, CloudConnectionControl, CloudSelectionResult, cloud_connection_fact,
    cloud_connection_lifetime,
};
use crate::xiaomi::runtime::connection::mqtt_failure_code;
use crate::xiaomi::runtime::{
    XiaomiFailureStage, XiaomiRuntimeComponent, XiaomiRuntimeError, unix_time,
};
use crate::xiaomi::{
    cloud::CloudNotification,
    gateway::EventArguments,
    runtime::{PushSource, SessionAuthority},
};
use std::collections::{BTreeMap, BTreeSet};

impl DeviceSessions {
    pub(super) fn schedule_cloud_notifications(
        &mut self,
        ctx: &SessionContext<'_>,
    ) -> Result<(), XiaomiRuntimeError> {
        let snapshot = self.admission.snapshot()?;
        let devices = if snapshot.status == crate::xiaomi::runtime::AdmissionStatus::Active {
            snapshot
                .features
                .iter()
                .map(|feature| feature.identity.physical.clone())
                .collect::<BTreeSet<_>>()
        } else {
            BTreeSet::new()
        };
        let desired = devices
            .iter()
            .filter(|device| {
                !self
                    .gateways
                    .values()
                    .any(|gateway| gateway.tokens.contains_key(*device))
                    && !self
                        .lans
                        .get(*device)
                        .is_some_and(|lan| lan.control.push_active())
            })
            .map(|device| device.parent_did.as_str().to_owned())
            .collect::<BTreeSet<_>>();
        if let Some(cloud) = self.cloud_notifications.as_mut() {
            cloud.control.set_desired(desired);
            return Ok(());
        }
        if desired.is_empty() {
            return Ok(());
        }
        let (Some(network), Some(record)) =
            (ctx.discovery.network(), ctx.auth.snapshot().record.as_ref())
        else {
            return Ok(());
        };
        let interfaces = network.snapshot.interfaces().to_vec();
        if interfaces.is_empty() {
            return Ok(());
        }
        let authority = SessionAuthority::new();
        let config = CloudConnectionConfig {
            oauth_client_uuid: record.oauth_client_uuid.clone(),
            access_token: record.tokens.access_token.clone(),
        };
        let control = CloudConnectionControl::new(desired);
        let (fact_sender, fact_receiver) = flume::bounded(256);
        self.cloud_notifications = Some(ActiveCloudNotifications {
            authority: authority.clone(),
            task: cloud_connection_lifetime(config, authority, control.clone(), fact_sender),
            fact: cloud_connection_fact(fact_receiver),
            control,
            desired: BTreeSet::new(),
            generation: 0,
            session_id: 0,
            tokens: BTreeMap::new(),
            publication: None,
        });
        Ok(())
    }

    pub(super) fn finish_cloud_disconnected(
        &mut self,
        ctx: &SessionContext<'_>,
    ) -> Result<(), XiaomiRuntimeError> {
        let Some(cloud) = self.cloud_notifications.as_mut() else {
            return Ok(());
        };
        for (_, token) in std::mem::take(&mut cloud.tokens) {
            ctx.state.source_failed(&token);
        }
        cloud.publication = None;
        cloud.desired.clear();
        cloud.generation = 0;
        cloud.session_id = 0;
        self.select_fallback_push_sources(ctx)
    }

    pub(super) fn finish_cloud_selection(
        &mut self,
        ctx: &SessionContext<'_>,
        completed: CloudSelectionResult,
    ) -> Result<(), XiaomiRuntimeError> {
        let Some(cloud) = self.cloud_notifications.as_mut() else {
            return Ok(());
        };
        cloud.publication = None;
        let CloudSelectionResult { desired, result } = completed;
        let generation = match result {
            Ok(generation) => generation,
            Err(error) if error.kind() == &crate::xiaomi::mqtt::MqttErrorKind::Superseded => {
                return Ok(());
            }
            Err(error) => {
                ctx.record_boundary(
                    XiaomiRuntimeComponent::Cloud,
                    XiaomiFailureStage::Subscribe,
                    mqtt_failure_code(error.kind()),
                    None,
                );
                for (_, token) in std::mem::take(&mut cloud.tokens) {
                    ctx.state.source_failed(&token);
                }
                cloud.desired.clear();
                cloud.generation = 0;
                cloud.session_id = 0;
                return Ok(());
            }
        };
        for (_, token) in std::mem::take(&mut cloud.tokens) {
            ctx.state.source_failed(&token);
        }
        cloud.desired = desired.clone();
        cloud.generation = generation;
        cloud.session_id = generation;
        if !cloud.control.wants(&cloud.desired) {
            return Ok(());
        }
        let snapshot = self.admission.snapshot()?;
        for physical in snapshot
            .features
            .iter()
            .map(|feature| feature.identity.physical.clone())
            .collect::<BTreeSet<_>>()
        {
            if cloud.desired.contains(physical.parent_did.as_str())
                && let Some(token) = ctx.state.select_push_source(
                    &physical,
                    PushSource::Cloud,
                    cloud.session_id,
                    generation,
                )
            {
                ctx.state.acknowledge(&token, cloud.session_id, generation);
                cloud.tokens.insert(physical, token);
            }
        }
        Ok(())
    }

    pub(super) fn select_fallback_push_sources(
        &mut self,
        ctx: &SessionContext<'_>,
    ) -> Result<(), XiaomiRuntimeError> {
        let devices = self
            .admission
            .snapshot()?
            .features
            .into_iter()
            .map(|feature| feature.identity.physical)
            .collect::<BTreeSet<_>>();
        for device in devices {
            let gateway_selected = self
                .gateways
                .values()
                .any(|gateway| gateway.tokens.contains_key(&device));
            if let Some(lan) = self.lans.get(&device) {
                lan.control.set_desired_push(!gateway_selected);
            }
            let lan_selected = !gateway_selected
                && self
                    .lans
                    .get(&device)
                    .is_some_and(|lan| lan.control.push_active());
            if gateway_selected || lan_selected {
                if let Some(cloud) = self.cloud_notifications.as_mut()
                    && let Some(token) = cloud.tokens.remove(&device)
                {
                    ctx.state.source_failed(&token);
                }
                continue;
            }
            if let Some(cloud) = self.cloud_notifications.as_mut()
                && cloud.desired.contains(device.parent_did.as_str())
                && !cloud.tokens.contains_key(&device)
                && let Some(token) = ctx.state.select_push_source(
                    &device,
                    PushSource::Cloud,
                    cloud.session_id,
                    cloud.generation,
                )
            {
                ctx.state
                    .acknowledge(&token, cloud.session_id, cloud.generation);
                cloud.tokens.insert(device, token);
            }
        }
        Ok(())
    }

    pub(super) fn apply_cloud_notification(
        &mut self,
        ctx: &SessionContext<'_>,
        notification: CloudNotification,
    ) -> Result<(), XiaomiRuntimeError> {
        let Some(cloud) = self.cloud_notifications.as_ref() else {
            return Ok(());
        };
        let did = match &notification {
            CloudNotification::Property { did, .. }
            | CloudNotification::Event { did, .. }
            | CloudNotification::State { did, .. } => did,
        };
        let Some((device, token)) = cloud
            .tokens
            .iter()
            .find(|(device, _)| device.parent_did.as_str() == did)
        else {
            return Ok(());
        };
        match notification {
            CloudNotification::Property {
                siid,
                piid,
                value,
                generation,
                ..
            } => {
                ctx.state.apply_property(
                    token,
                    cloud.session_id,
                    generation,
                    siid,
                    piid,
                    value.as_ref(),
                    unix_time(),
                    false,
                );
            }
            CloudNotification::Event {
                siid,
                eiid,
                arguments,
                generation,
                ..
            } => match arguments {
                EventArguments::Keyed(arguments) => {
                    ctx.state.apply_keyed_event(
                        token,
                        cloud.session_id,
                        generation,
                        siid,
                        eiid,
                        &arguments,
                        unix_time(),
                        false,
                    );
                }
                EventArguments::Positional(arguments) => {
                    ctx.state.apply_positional_event(
                        token,
                        cloud.session_id,
                        generation,
                        siid,
                        eiid,
                        &arguments,
                        unix_time(),
                        false,
                    );
                }
            },
            CloudNotification::State {
                online, generation, ..
            } => {
                if ctx
                    .state
                    .apply_cloud_online(token, cloud.session_id, generation, online)
                    .is_some()
                    && self.admission.observe_cloud_online(device, online)
                {
                    let snapshot = self.admission.snapshot()?;
                    self.sync_cloud_routes(ctx)?;
                    ctx.state.reconcile(&snapshot);
                    ctx.status.borrow_mut().admission = snapshot;
                }
            }
        }
        Ok(())
    }

    pub(super) fn drop_cloud_notifications(&mut self, ctx: &SessionContext<'_>) {
        if let Some(cloud) = self.cloud_notifications.take() {
            cloud.control.stop();
            cloud.authority.revoke();
            for (_, token) in cloud.tokens {
                ctx.state.source_failed(&token);
            }
        }
    }

    pub(super) fn sync_cloud_routes(
        &mut self,
        ctx: &SessionContext<'_>,
    ) -> Result<(), XiaomiRuntimeError> {
        let Some(record) = ctx.auth.snapshot().record.as_ref() else {
            return Ok(());
        };
        if self.cloud_authority.is_none() {
            self.cloud_authority = Some(SessionAuthority::new());
        }
        let authority = self
            .cloud_authority
            .as_ref()
            .expect("cloud authority was initialized")
            .clone();
        let snapshot = self.admission.snapshot()?;
        let enabled = snapshot
            .features
            .iter()
            .filter(|feature| feature.paths.cloud)
            .map(|feature| feature.identity.physical.clone())
            .collect::<BTreeSet<_>>();
        for device in self.cloud_routes.difference(&enabled) {
            ctx.registry.revoke_cloud(device);
        }
        for feature in &snapshot.features {
            if enabled.contains(&feature.identity.physical) {
                ctx.registry.install_cloud_if_changed(
                    feature.identity.physical.clone(),
                    ctx.cloud.clone(),
                    record.tokens.access_token.clone(),
                    record.tokens.expires_at,
                    authority.clone(),
                );
            } else {
                ctx.registry.revoke_cloud(&feature.identity.physical);
            }
        }
        self.cloud_routes = enabled;
        Ok(())
    }
}
