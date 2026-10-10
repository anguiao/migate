use crate::{
    device::{Availability, DeviceKey, DeviceRegistry, PhysicalDevice},
    storage::{DeviceStore, StoredDevice},
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
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};
const MAX_LABEL_BYTES: usize = 32;

fn validate_label(label: &str) -> Result<(), Error> {
    if label.len() > MAX_LABEL_BYTES {
        return Err(ErrorCode::ConstraintError.into());
    }
    Ok(())
}

/// The shared runtime state of one composition parent. The bridge is retained
/// across membership updates so the saved name and the last reported
/// reachability survive reconciliation; only the run loop is restarted when
/// the member set changes.
pub(super) struct GroupBridge {
    key: DeviceKey,
    identity: String,
    endpoint: u16,
    label: RefCell<String>,
    members: RefCell<Vec<PhysicalDevice>>,
    reported: Cell<bool>,
    registry: DeviceRegistry,
}

impl GroupBridge {
    pub(super) fn new(
        key: DeviceKey,
        identity: String,
        endpoint: u16,
        label: String,
        members: Vec<PhysicalDevice>,
        registry: DeviceRegistry,
    ) -> Self {
        let available = any_available(&members);
        Self {
            key,
            identity,
            endpoint,
            label: RefCell::new(label),
            members: RefCell::new(members),
            reported: Cell::new(available),
            registry,
        }
    }

    pub(super) fn set_members(&self, members: Vec<PhysicalDevice>) {
        *self.members.borrow_mut() = members;
    }

    /// Whether every member key is bound to the same running device instance.
    /// A replacement keeps the key and endpoint but retires the old instance.
    pub(super) fn same_members(&self, members: &[PhysicalDevice]) -> bool {
        let current = self.members.borrow();
        current.len() == members.len()
            && current.iter().zip(members).all(|(current, member)| {
                current.key() == member.key() && current.same_instance(member)
            })
    }

    fn available(&self) -> bool {
        any_available(&self.members.borrow())
    }
}

fn any_available(members: &[PhysicalDevice]) -> bool {
    members
        .iter()
        .any(|member| member.availability() == Availability::Available)
}

enum Target {
    Device {
        device: PhysicalDevice,
        endpoint: u16,
        identity: String,
        reported: Cell<bool>,
    },
    Group(Rc<GroupBridge>),
}

pub(super) struct BridgedHandler {
    target: Target,
    dataver: Dataver,
    store: DeviceStore,
    protocol: super::storage::StoreAdapter,
}

impl BridgedHandler {
    pub(super) fn new(
        device: PhysicalDevice,
        dataver: Dataver,
        stored: &StoredDevice,
        store: DeviceStore,
        protocol: super::storage::StoreAdapter,
    ) -> Self {
        let reported = Cell::new(device.availability() == Availability::Available);
        Self {
            target: Target::Device {
                device,
                endpoint: stored.endpoint,
                identity: stored.unique_id.clone(),
                reported,
            },
            dataver,
            store,
            protocol,
        }
    }

    pub(super) fn new_group(
        bridge: Rc<GroupBridge>,
        dataver: Dataver,
        store: DeviceStore,
        protocol: super::storage::StoreAdapter,
    ) -> Self {
        Self {
            target: Target::Group(bridge),
            dataver,
            store,
            protocol,
        }
    }

    /// The stable composition key of a group parent, or `None` for a
    /// device-backed bridge.
    pub(super) fn group_key(&self) -> Option<&DeviceKey> {
        match &self.target {
            Target::Device { .. } => None,
            Target::Group(bridge) => Some(&bridge.key),
        }
    }

    /// The shared state retained across reconciles, for group parents only.
    pub(super) fn group_state(&self) -> Option<Rc<GroupBridge>> {
        match &self.target {
            Target::Device { .. } => None,
            Target::Group(bridge) => Some(bridge.clone()),
        }
    }

    fn endpoint(&self) -> u16 {
        match &self.target {
            Target::Device { endpoint, .. } => *endpoint,
            Target::Group(bridge) => bridge.endpoint,
        }
    }

    fn reachable_value(&self) -> bool {
        match &self.target {
            Target::Device { device, .. } => device.availability() == Availability::Available,
            Target::Group(bridge) => bridge.available(),
        }
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

    fn reachable(&self, _ctx: impl ReadContext) -> Result<bool, Error> {
        Ok(self.reachable_value())
    }

    async fn run(&self, ctx: impl HandlerContext) -> Result<(), Error> {
        match &self.target {
            Target::Device {
                device, reported, ..
            } => {
                let mut changes = device.subscribe();
                let mut value = device.availability() == Availability::Available;
                loop {
                    if reported.replace(value) != value {
                        bridged::ReachableChanged::emit_for(&ctx, self.endpoint(), |builder| {
                            builder.reachable_new_value(value)?.end()
                        })?;
                        ctx.notify_attr_changed(
                            self.endpoint(),
                            Self::CLUSTER.id,
                            bridged::AttributeId::Reachable as _,
                        );
                    }
                    value = changes.changed().await == Availability::Available;
                }
            }
            Target::Group(bridge) => loop {
                // Subscribe before reading the aggregate so a change between
                // the two steps is either included in the value or wakes us.
                let mut changes = bridge
                    .members
                    .borrow()
                    .iter()
                    .map(|member| {
                        let mut subscription = member.subscribe();
                        async move {
                            subscription.changed().await;
                        }
                    })
                    .collect::<FuturesUnordered<_>>();
                let value = bridge.available();
                if bridge.reported.replace(value) != value {
                    bridged::ReachableChanged::emit_for(&ctx, bridge.endpoint, |builder| {
                        builder.reachable_new_value(value)?.end()
                    })?;
                    ctx.notify_attr_changed(
                        bridge.endpoint,
                        Self::CLUSTER.id,
                        bridged::AttributeId::Reachable as _,
                    );
                }
                if changes.is_empty() {
                    std::future::pending::<()>().await;
                } else {
                    changes.next().await;
                }
            },
        }
    }

    fn unique_id<P: TLVBuilderParent>(
        &self,
        _ctx: impl ReadContext,
        builder: Utf8StrBuilder<P>,
    ) -> Result<P, Error> {
        match &self.target {
            Target::Device { identity, .. } => builder.set(identity),
            Target::Group(bridge) => builder.set(&bridge.identity),
        }
    }

    fn node_label<P: TLVBuilderParent>(
        &self,
        _ctx: impl ReadContext,
        builder: Utf8StrBuilder<P>,
    ) -> Result<P, Error> {
        match &self.target {
            Target::Device { device, .. } => builder.set(&device.label()),
            Target::Group(bridge) => builder.set(&bridge.label.borrow()),
        }
    }

    fn set_node_label(&self, ctx: impl WriteContext, value: Utf8Str<'_>) -> Result<(), Error> {
        validate_label(value)?;
        match &self.target {
            Target::Device { device, .. } => {
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
            Target::Group(bridge) => {
                if *bridge.label.borrow() != value {
                    self.protocol
                        .check_failure()
                        .map_err(|_| ErrorCode::StdIoError)?;
                    self.protocol
                        .record(self.store.set_group_label(&bridge.key, value))?;
                    bridge.label.replace(value.to_owned());
                    // The running adapter and terminal read the registry
                    // snapshot, so a rename must be visible there too.
                    bridge.registry.set_composition_label(&bridge.key, value);
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
