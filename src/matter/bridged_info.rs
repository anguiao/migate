use crate::{
    device::{DeviceRegistry, Function, Group, PhysicalDevice},
    storage::DeviceStore,
};
use futures_util::{StreamExt as _, stream::FuturesUnordered};
use rs_matter::{
    dm::{
        Cluster, Dataver, HandlerContext, InvokeContext, ReadContext, WriteContext,
        clusters::decl::bridged_device_basic_information as bridged,
    },
    error::{Error, ErrorCode},
    tlv::{TLVBuilderParent, Utf8Str, Utf8StrBuilder},
    with,
};
use std::cell::Cell;
const MAX_LABEL_BYTES: usize = 32;

/// Real reachability of a device-backed endpoint: at least one function is
/// both available and not closed. A revoked device keeps its registered
/// availability but closes every function, so a held applied view must not
/// keep reporting it as reachable. This reads state; it never mutates the
/// cached physical description.
fn device_reachable(device: &PhysicalDevice) -> bool {
    device
        .functions()
        .iter()
        .any(|function| function.fresh_snapshot().reachable)
}

fn validate_label(label: &str) -> Result<(), Error> {
    if label.len() > MAX_LABEL_BYTES {
        return Err(ErrorCode::ConstraintError.into());
    }
    Ok(())
}

pub(super) enum Target {
    Device(PhysicalDevice),
    Group {
        group: Group,
        registry: DeviceRegistry,
    },
}

impl Target {
    fn functions(&self) -> Vec<Function> {
        match self {
            Self::Device(device) => device.functions().to_vec(),
            Self::Group { group, .. } => group.functions(),
        }
    }

    fn reachable(&self) -> bool {
        match self {
            Self::Device(device) => device_reachable(device),
            Self::Group { group, .. } => group.snapshot().reachable,
        }
    }
}

pub(super) struct BridgedHandler {
    target: Target,
    endpoint: u16,
    identity: String,
    reported: Cell<bool>,
    dataver: Dataver,
    configuration_version: Cell<u32>,
    store: DeviceStore,
    protocol: super::storage::StoreAdapter,
}

impl BridgedHandler {
    pub(super) fn new(
        target: Target,
        dataver: Dataver,
        configuration_version: u32,
        endpoint: u16,
        identity: String,
        store: DeviceStore,
        protocol: super::storage::StoreAdapter,
    ) -> Self {
        let reported = Cell::new(target.reachable());
        Self {
            target,
            endpoint,
            identity,
            reported,
            dataver,
            configuration_version: Cell::new(configuration_version.max(1)),
            store,
            protocol,
        }
    }

    /// Update the fixed-shape ConfigurationVersion after a committed shape
    /// change. The caller notifies the attribute; this only tracks the value.
    pub(super) fn set_configuration_version(&self, version: u32) {
        self.configuration_version.set(version.max(1));
    }

    pub(super) fn configuration_version_value(&self) -> u32 {
        self.configuration_version.get().max(1)
    }
}

impl bridged::ClusterHandler for BridgedHandler {
    const CLUSTER: Cluster<'static> = bridged::FULL_CLUSTER
        .with_features(0)
        .with_attrs(
            with!(required; bridged::AttributeId::UniqueID | bridged::AttributeId::NodeLabel),
        )
        .with_cmds(with!())
        .with_events(with!(bridged::EventId::ReachableChanged));

    fn dataver(&self) -> u32 {
        self.dataver.get()
    }

    fn dataver_changed(&self) {
        self.dataver.changed();
    }

    fn configuration_version(&self, _ctx: impl ReadContext) -> Result<u32, Error> {
        Ok(self.configuration_version.get().max(1))
    }

    fn reachable(&self, _ctx: impl ReadContext) -> Result<bool, Error> {
        Ok(self.target.reachable())
    }

    async fn run(&self, ctx: impl HandlerContext) -> Result<(), Error> {
        loop {
            // FunctionChanges observes physical availability and closure too.
            // Subscribe before reading so intervening changes wake the loop.
            let mut changes = self
                .target
                .functions()
                .into_iter()
                .map(|function| {
                    let mut subscription = function.subscribe();
                    async move {
                        subscription.changed().await;
                    }
                })
                .collect::<FuturesUnordered<_>>();
            let value = self.target.reachable();
            if self.reported.replace(value) != value {
                bridged::ReachableChanged::emit_for(&ctx, self.endpoint, |builder| {
                    builder.reachable_new_value(value)?.end()
                })?;
                ctx.notify_attr_changed(
                    self.endpoint,
                    Self::CLUSTER.id,
                    bridged::AttributeId::Reachable as _,
                );
            }
            // Every published device or group has at least one function.
            changes.next().await;
        }
    }

    fn unique_id<P: TLVBuilderParent>(
        &self,
        _ctx: impl ReadContext,
        builder: Utf8StrBuilder<P>,
    ) -> Result<P, Error> {
        builder.set(&self.identity)
    }

    fn node_label<P: TLVBuilderParent>(
        &self,
        _ctx: impl ReadContext,
        builder: Utf8StrBuilder<P>,
    ) -> Result<P, Error> {
        match &self.target {
            Target::Device(device) => builder.set(&device.label()),
            Target::Group { group, .. } => builder.set(&group.description().label),
        }
    }

    fn set_node_label(&self, ctx: impl WriteContext, value: Utf8Str<'_>) -> Result<(), Error> {
        validate_label(value)?;
        match &self.target {
            Target::Device(device) => {
                if device.label() != value {
                    self.protocol
                        .check_failure()
                        .map_err(|_| ErrorCode::StdIoError)?;
                    self.protocol
                        .record(self.store.set_device_label(device.key(), value))?;
                    device.set_label(value.to_owned());
                    ctx.notify_changed();
                }
            }
            Target::Group { group, registry } => {
                if group.description().label != value {
                    self.protocol
                        .check_failure()
                        .map_err(|_| ErrorCode::StdIoError)?;
                    self.protocol
                        .record(self.store.set_group_label(group.key(), value))?;
                    // The running adapter and terminal read the registry
                    // snapshot, so a rename must be visible there too. The
                    // shared group handle carries the saved label.
                    registry.set_composition_label(group.key(), value);
                    ctx.notify_changed();
                }
            }
        }
        Ok(())
    }

    fn handle_keep_active(
        &self,
        _ctx: impl InvokeContext,
        _request: bridged::KeepActiveRequest<'_>,
    ) -> Result<(), Error> {
        Err(ErrorCode::CommandNotFound.into())
    }
}

/// Bridged Device Basic Information metadata for published single lights and
/// virtual groups. On top of the outlet surface it advertises the fixed-shape
/// `ConfigurationVersion` attribute; outlets deliberately keep the smaller
/// surface.
pub(super) const LIGHT_CLUSTER: Cluster<'static> = bridged::FULL_CLUSTER
    .with_features(0)
    .with_attrs(with!(required; bridged::AttributeId::UniqueID | bridged::AttributeId::NodeLabel | bridged::AttributeId::ConfigurationVersion))
    .with_cmds(with!());
