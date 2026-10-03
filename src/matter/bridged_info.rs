use crate::{
    device::{Availability, Device, DeviceSnapshot},
    storage::{MatterStore, StorageError},
};
use rs_matter::{
    dm::{
        Cluster, Dataver, HandlerContext, InvokeContext, ReadContext, WriteContext,
        clusters::decl::bridged_device_basic_information as bridged,
    },
    error::{Error, ErrorCode},
    persist::Persist,
    tlv::{TLVBuilderParent, TLVElement, Utf8Str, Utf8StrBuilder},
    with,
};
use std::cell::RefCell;

pub(super) const LABEL_KEY: u16 = rs_matter::persist::VENDOR_KEYS_START;
const MAX_LABEL_BYTES: usize = 32;

fn validate_label(label: &str) -> Result<(), Error> {
    if label.len() > MAX_LABEL_BYTES {
        return Err(ErrorCode::ConstraintError.into());
    }
    Ok(())
}

pub(super) fn load_label(store: &MatterStore, default: &str) -> Result<String, StorageError> {
    let Some(data) = store.get(LABEL_KEY)? else {
        return Ok(default.into());
    };
    TLVElement::new(&data)
        .utf8()
        .and_then(|label| {
            validate_label(label)?;
            Ok(label.to_owned())
        })
        .map_err(|error| {
            StorageError::new(
                store.path(),
                format!("restore bridged device label for key {LABEL_KEY}"),
                error,
            )
        })
}

pub(super) struct BridgedHandler<'a> {
    device: &'a Device,
    dataver: Dataver,
    identity: &'a str,
    label: RefCell<String>,
    changes: RefCell<Option<crate::device::Changes>>,
    reported: std::cell::Cell<bool>,
}

fn reachable(snapshot: &DeviceSnapshot) -> bool {
    snapshot.availability == Availability::Available && snapshot.power.value().is_some()
}

impl<'a> BridgedHandler<'a> {
    pub(super) fn new(
        device: &'a Device,
        dataver: Dataver,
        identity: &'a str,
        label: String,
    ) -> Self {
        Self {
            device,
            dataver,
            identity,
            label: RefCell::new(label),
            changes: RefCell::new(Some(device.subscribe())),
            reported: std::cell::Cell::new(reachable(&device.snapshot())),
        }
    }
}

impl bridged::ClusterHandler for BridgedHandler<'_> {
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
        Ok(reachable(&self.device.snapshot()))
    }

    async fn run(&self, ctx: impl HandlerContext) -> Result<(), Error> {
        let mut changes = self.changes.borrow_mut().take().expect("handler runs once");
        loop {
            let value = reachable(&changes.changed().await);
            if self.reported.replace(value) != value {
                bridged::ReachableChanged::emit_for(
                    &ctx,
                    super::model::DEVICE_ENDPOINT,
                    |builder| builder.reachable_new_value(value)?.end(),
                )?;
                ctx.notify_attr_changed(
                    super::model::DEVICE_ENDPOINT,
                    Self::CLUSTER.id,
                    bridged::AttributeId::Reachable as _,
                );
            }
        }
    }

    fn unique_id<P: TLVBuilderParent>(
        &self,
        _ctx: impl ReadContext,
        builder: Utf8StrBuilder<P>,
    ) -> Result<P, Error> {
        builder.set(self.identity)
    }

    fn node_label<P: TLVBuilderParent>(
        &self,
        _ctx: impl ReadContext,
        builder: Utf8StrBuilder<P>,
    ) -> Result<P, Error> {
        builder.set(&self.label.borrow())
    }

    fn set_node_label(&self, ctx: impl WriteContext, value: Utf8Str<'_>) -> Result<(), Error> {
        validate_label(value)?;
        if *self.label.borrow() != value {
            Persist::new(ctx.kv()).store_tlv(LABEL_KEY, value)?;
            *self.label.borrow_mut() = value.to_owned();
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::Store;

    #[test]
    fn bridged_label_defaults_and_detects_corruption() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap().matter();
        assert_eq!(load_label(&store, "Test Outlet").unwrap(), "Test Outlet");
        store.put(LABEL_KEY, &[0x15]).unwrap();
        assert!(load_label(&store, "Test Outlet").is_err());
    }
}
