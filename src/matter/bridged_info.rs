use crate::{
    device::{Availability, PhysicalDevice},
    storage::{DeviceStore, StoredDevice},
};
use rs_matter::{
    dm::{
        Cluster, Dataver, HandlerContext, InvokeContext, ReadContext, WriteContext,
        clusters::decl::bridged_device_basic_information as bridged,
    },
    error::{Error, ErrorCode},
    tlv::{TLVBuilderParent, Utf8Str, Utf8StrBuilder},
    with,
};
const MAX_LABEL_BYTES: usize = 32;

fn validate_label(label: &str) -> Result<(), Error> {
    if label.len() > MAX_LABEL_BYTES {
        return Err(ErrorCode::ConstraintError.into());
    }
    Ok(())
}

pub(super) struct BridgedHandler {
    device: PhysicalDevice,
    endpoint: u16,
    store: DeviceStore,
    protocol: super::storage::StoreAdapter,
    dataver: Dataver,
    identity: String,
    reported: std::cell::Cell<bool>,
}

impl BridgedHandler {
    pub(super) fn new(
        device: PhysicalDevice,
        dataver: Dataver,
        stored: &StoredDevice,
        store: DeviceStore,
        protocol: super::storage::StoreAdapter,
    ) -> Self {
        let reported = std::cell::Cell::new(device.availability() == Availability::Available);
        Self {
            device,
            endpoint: stored.endpoint,
            store,
            protocol,
            dataver,
            identity: stored.unique_id.clone(),
            reported,
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
        Ok(self.device.availability() == Availability::Available)
    }

    async fn run(&self, ctx: impl HandlerContext) -> Result<(), Error> {
        let mut changes = self.device.subscribe();
        let mut value = self.device.availability() == Availability::Available;
        loop {
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
            value = changes.changed().await == Availability::Available;
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
        builder.set(&self.device.label())
    }

    fn set_node_label(&self, ctx: impl WriteContext, value: Utf8Str<'_>) -> Result<(), Error> {
        validate_label(value)?;
        if self.device.label() != value {
            self.protocol
                .check_failure()
                .map_err(|_| ErrorCode::StdIoError)?;
            self.protocol
                .record(self.store.set_device_label(self.device.key(), value))?;
            self.device.set_label(value.to_owned());
            ctx.notify_changed();
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
