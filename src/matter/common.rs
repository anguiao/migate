use super::storage::StoreAdapter;
use crate::{
    device::{DeviceService, FeatureIdentity},
    storage::{Identity, StorageError},
};
use rs_matter::dm::clusters::desc::ClusterHandler as _;
use rs_matter::{
    Matter, clusters, devices,
    dm::clusters::decl::bridged_device_basic_information as bridged,
    dm::{Cluster, Dataver, InvokeContext, ReadContext, WriteContext},
    dm::{
        Endpoint, Node,
        clusters::basic_info::BasicInfoConfig,
        devices::{DEV_TYPE_AGGREGATOR, test::TEST_DEV_DET},
    },
    error::{Error, ErrorCode},
    root_endpoint,
    tlv::{TLVBuilderParent, Utf8Str, Utf8StrBuilder},
    with,
};
use std::cell::RefCell;

pub(crate) fn basic_info(identity: &Identity) -> BasicInfoConfig<'_> {
    BasicInfoConfig {
        product_name: "MiGate",
        device_name: "MiGate",
        product_label: "MiGate",
        serial_no: &identity.bridge_id,
        unique_id: &identity.bridge_id,
        ..TEST_DEV_DET
    }
}

pub(crate) fn initialize_basic_info(
    matter: &Matter<'_>,
    kv: impl rs_matter::persist::KvBlobStoreAccess,
    missing: bool,
) -> Result<(), Error> {
    if missing {
        let mut settings = rs_matter::dm::clusters::basic_info::BasicInfoSettings::new();
        settings.node_label.push_str("MiGate").unwrap();
        rs_matter::persist::Persist::new(&kv)
            .store_tlv(rs_matter::persist::BASIC_INFO_KEY, &settings)?;
        matter.startup(&kv)?;
    }
    Ok(())
}

pub(super) const BASE_NODE: Node<'static> = Node {
    endpoints: &[
        root_endpoint!(eth),
        Endpoint::new(
            1,
            devices!(DEV_TYPE_AGGREGATOR),
            clusters!(rs_matter::dm::clusters::desc::DescHandler::CLUSTER),
        ),
    ],
};

pub(super) const BRIDGED_CLUSTER: Cluster<'static> = bridged::FULL_CLUSTER
    .with_features(0)
    .with_attrs(with!(required; bridged::AttributeId::UniqueID | bridged::AttributeId::NodeLabel))
    .with_cmds(with!())
    .with_events(with!(bridged::EventId::ReachableChanged));

pub(super) fn truncate_label(value: &str) -> String {
    if value.len() <= 32 {
        return value.to_owned();
    }
    let mut end = 32;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

pub(super) struct CommonHandler {
    dataver: Dataver,
    service: DeviceService,
    feature: FeatureIdentity,
    endpoint: u16,
    public_id: String,
    default_label: RefCell<String>,
    label_override: RefCell<Option<String>>,
    store: StoreAdapter,
}

impl CommonHandler {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        dataver: Dataver,
        service: DeviceService,
        feature: FeatureIdentity,
        endpoint: u16,
        public_id: String,
        default_label: String,
        label_override: Option<String>,
        store: StoreAdapter,
    ) -> Self {
        Self {
            dataver,
            service,
            feature,
            endpoint,
            public_id,
            default_label: RefCell::new(truncate_label(&default_label)),
            label_override: RefCell::new(label_override),
            store,
        }
    }

    pub(super) fn set_default_label(&self, value: &str) -> bool {
        let value = truncate_label(value);
        if *self.default_label.borrow() == value {
            return false;
        }
        *self.default_label.borrow_mut() = value;
        self.label_override.borrow().is_none()
    }

    pub(super) fn save_override(&self, value: &str) -> Result<(), StorageError> {
        self.store.capture(
            self.store
                .storage()
                .save_feature_label(self.endpoint, value),
        )
    }
}

impl bridged::ClusterHandler for CommonHandler {
    const CLUSTER: Cluster<'static> = BRIDGED_CLUSTER;
    fn dataver(&self) -> u32 {
        self.dataver.get()
    }
    fn dataver_changed(&self) {
        self.dataver.changed();
    }
    fn reachable(&self, _ctx: impl ReadContext) -> Result<bool, Error> {
        Ok(self.service.is_available(&self.feature))
    }
    fn unique_id<P: TLVBuilderParent>(
        &self,
        _ctx: impl ReadContext,
        builder: Utf8StrBuilder<P>,
    ) -> Result<P, Error> {
        builder.set(&self.public_id)
    }
    fn node_label<P: TLVBuilderParent>(
        &self,
        _ctx: impl ReadContext,
        builder: Utf8StrBuilder<P>,
    ) -> Result<P, Error> {
        if let Some(value) = self.label_override.borrow().as_deref() {
            builder.set(value)
        } else {
            builder.set(&self.default_label.borrow())
        }
    }
    fn set_node_label(&self, ctx: impl WriteContext, value: Utf8Str<'_>) -> Result<(), Error> {
        if value.len() > 32 {
            return Err(ErrorCode::ConstraintError.into());
        }
        let mut current = self.label_override.borrow_mut();
        if current.as_deref() != Some(value) {
            self.save_override(value).map_err(|_| ErrorCode::Failure)?;
            *current = Some(value.to_owned());
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
