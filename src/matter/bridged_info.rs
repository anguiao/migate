use crate::storage::Store;
use rs_matter::{
    dm::{
        Cluster, Dataver, InvokeContext, ReadContext, WriteContext,
        clusters::decl::bridged_device_basic_information as bridged,
    },
    error::{Error, ErrorCode},
    persist::Persist,
    tlv::{TLVBuilderParent, TLVElement, Utf8Str, Utf8StrBuilder},
    with,
};
use std::cell::RefCell;

pub(super) const LABEL_KEY: u16 = rs_matter::persist::VENDOR_KEYS_START;
const DEFAULT_LABEL: &str = "MiGate 虚拟灯";
const MAX_LABEL_BYTES: usize = 32;

fn validate_label(label: &str) -> Result<(), Error> {
    if label.len() > MAX_LABEL_BYTES {
        return Err(ErrorCode::ConstraintError.into());
    }
    Ok(())
}

pub(super) fn load_label(store: &Store) -> Result<String, Error> {
    let Some(data) = store.load(LABEL_KEY) else {
        return Ok(DEFAULT_LABEL.into());
    };
    let label = TLVElement::new(data).utf8()?;
    validate_label(label)?;
    Ok(label.to_owned())
}

pub(super) struct BridgedHandler<'a> {
    dataver: Dataver,
    identity: &'a str,
    label: RefCell<String>,
}

impl<'a> BridgedHandler<'a> {
    pub(super) fn new(dataver: Dataver, identity: &'a str, label: String) -> Self {
        Self {
            dataver,
            identity,
            label: RefCell::new(label),
        }
    }
}

impl bridged::ClusterHandler for BridgedHandler<'_> {
    const CLUSTER: Cluster<'static> = bridged::FULL_CLUSTER
        .with_features(0)
        .with_attrs(
            with!(required; bridged::AttributeId::UniqueID | bridged::AttributeId::NodeLabel),
        )
        .with_cmds(with!());

    fn dataver(&self) -> u32 {
        self.dataver.get()
    }

    fn dataver_changed(&self) {
        self.dataver.changed();
    }

    fn reachable(&self, _ctx: impl ReadContext) -> Result<bool, Error> {
        Ok(true)
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

    #[test]
    fn bridged_label_defaults_and_detects_corruption() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path()).unwrap();
        assert_eq!(load_label(&store).unwrap(), "MiGate 虚拟灯");
        store.store(LABEL_KEY, &[0x15]).unwrap();
        assert!(load_label(&store).is_err());
    }
}
