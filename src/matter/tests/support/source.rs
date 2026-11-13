//! Read a frozen source publication through the real Matter attribute handler.

use super::{Context, read_bytes, report_value};
use crate::{
    app::App,
    matter::{model, storage::StoreAdapter},
    storage::Store,
};
use rs_matter::{
    MATTER_PORT, Matter,
    crypto::default_crypto,
    dm::{
        AsyncHandler, InvokeReplyInstance, Metadata,
        clusters::decl::{
            bridged_device_basic_information::AttributeId as BridgedAttribute,
            color_control::AttributeId as ColorAttribute,
        },
        devices::test::{DAC_PRIVKEY, TEST_DEV_ATT, TEST_DEV_COMM},
    },
    error::ErrorCode,
    im::{InteractionModel, InteractionModelState},
    tlv::{TLVElement, TLVTag, TLVWrite},
    transport::exchange::MatterBuffers,
    utils::storage::WriteBuf,
};
use std::{future::Future, pin::Pin, rc::Rc};

pub(crate) struct PublishedLightModel {
    handler: model::Model,
    store: Store,
    children: std::collections::BTreeMap<u16, u16>,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct LightAttributes {
    pub(crate) minimum_mireds: u16,
    pub(crate) maximum_mireds: u16,
    pub(crate) current_mireds: Option<u16>,
    pub(crate) configuration_version: u32,
}

impl PublishedLightModel {
    pub(crate) fn new(app: &Rc<App>, store: &Store) -> Self {
        let entries = app.frozen();
        let children = entries
            .iter()
            .map(|entry| {
                let children = entry
                    .identity
                    .functions
                    .iter()
                    .filter(|function| {
                        entry
                            .device
                            .function(&function.id)
                            .is_some_and(|device| device.description().kind.is_light())
                    })
                    .collect::<Vec<_>>();
                assert_eq!(children.len(), 1);
                (entry.identity.endpoint, children[0].endpoint)
            })
            .collect();
        Self {
            handler: model::handler_from(entries, rand::rng(), app.clone()),
            store: store.clone(),
            children,
        }
    }

    pub(crate) fn read(
        &self,
        endpoint: u16,
    ) -> Pin<Box<dyn Future<Output = LightAttributes> + '_>> {
        Box::pin(self.read_attributes(endpoint))
    }

    async fn read_attributes(&self, endpoint: u16) -> LightAttributes {
        let identity = self.store.load_identity().unwrap();
        let info = model::basic_info(&identity);
        let matter = Matter::new(&info, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
        let buffers: MatterBuffers = MatterBuffers::new();
        let state: InteractionModelState = InteractionModelState::new();
        let crypto = default_crypto(rand::rng(), DAC_PRIVKEY);
        let kv = matter.kv(StoreAdapter::new(self.store.matter()));
        let im = InteractionModel::new(&matter, &crypto, &buffers, &self.handler, &kv, &state);
        let child = self.children[&endpoint];
        let mut values = Vec::new();
        for (cluster, attribute) in [
            (0x300, ColorAttribute::ColorTempPhysicalMinMireds as u32),
            (0x300, ColorAttribute::ColorTempPhysicalMaxMireds as u32),
            (0x39, BridgedAttribute::ConfigurationVersion as u32),
        ] {
            values.push(
                read_bytes(
                    &self.handler,
                    &Context::at(
                        &im,
                        if cluster == 0x39 { endpoint } else { child },
                        cluster,
                        attribute,
                    ),
                )
                .await
                .unwrap_or_else(|error| {
                    panic!("reading {endpoint}/{cluster:#x}/{attribute:#x}: {error}")
                }),
            );
        }
        let current_mireds = match read_bytes(
            &self.handler,
            &Context::at(
                &im,
                child,
                0x300,
                ColorAttribute::ColorTemperatureMireds as u32,
            ),
        )
        .await
        {
            Ok(value) => Some(report_value(&value).u16().unwrap()),
            Err(error) => {
                assert_eq!(
                    error.code(),
                    ErrorCode::Failure,
                    "unknown color temperature"
                );
                None
            }
        };
        LightAttributes {
            minimum_mireds: report_value(&values[0]).u16().unwrap(),
            maximum_mireds: report_value(&values[1]).u16().unwrap(),
            current_mireds,
            configuration_version: report_value(&values[2]).u32().unwrap(),
        }
    }
}

/// Exercise source-backed relay endpoints through the real Matter handler.
pub(crate) struct PublishedRelayModel {
    handler: model::Model,
    store: Store,
}

impl PublishedRelayModel {
    pub(crate) fn new(app: &Rc<App>, store: &Store) -> Self {
        Self {
            handler: model::handler_from(app.frozen(), rand::rng(), app.clone()),
            store: store.clone(),
        }
    }

    pub(crate) fn check_topology(&self, endpoints: &[u16]) {
        self.handler.access(|node| {
            assert_eq!(
                node.endpoints
                    .iter()
                    .filter(|endpoint| endpoint.id > 1)
                    .map(|endpoint| endpoint.id)
                    .collect::<Vec<_>>(),
                endpoints
            );
            for id in endpoints {
                let endpoint = node.endpoint(*id).unwrap();
                assert!(
                    endpoint
                        .device_types
                        .iter()
                        .any(|kind| kind.dtype == 0x0103)
                );
                assert!(
                    endpoint
                        .device_types
                        .iter()
                        .any(|kind| kind.dtype == 0x0013)
                );
                for cluster in [0x1d, 0x1e, 0x39, 6] {
                    assert!(endpoint.cluster(cluster).is_some());
                }
                assert!(endpoint.client_clusters.contains(&6));
            }
        });
    }

    async fn read(
        &self,
        endpoint: u16,
        cluster: u32,
        attribute: u32,
    ) -> Result<Vec<u8>, rs_matter::error::Error> {
        let identity = self.store.load_identity().unwrap();
        let info = model::basic_info(&identity);
        let matter = Matter::new(&info, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
        let buffers: MatterBuffers = MatterBuffers::new();
        let state: InteractionModelState = InteractionModelState::new();
        let crypto = default_crypto(rand::rng(), DAC_PRIVKEY);
        let kv = matter.kv(StoreAdapter::new(self.store.matter()));
        let im = InteractionModel::new(&matter, &crypto, &buffers, &self.handler, &kv, &state);
        read_bytes(
            &self.handler,
            &Context::at(&im, endpoint, cluster, attribute),
        )
        .await
    }

    pub(crate) async fn power(&self, endpoint: u16) -> Result<bool, rs_matter::error::Error> {
        let bytes = self.read(endpoint, 6, 0).await?;
        report_value(&bytes).bool()
    }

    pub(crate) async fn label(&self, endpoint: u16) -> String {
        let bytes = self
            .read(endpoint, 0x39, BridgedAttribute::NodeLabel as u32)
            .await
            .unwrap();
        report_value(&bytes).utf8().unwrap().to_owned()
    }

    pub(crate) async fn unique_id(&self, endpoint: u16) -> String {
        let bytes = self
            .read(endpoint, 0x39, BridgedAttribute::UniqueID as u32)
            .await
            .unwrap();
        report_value(&bytes).utf8().unwrap().to_owned()
    }

    pub(crate) async fn write_label(
        &self,
        endpoint: u16,
        label: &str,
    ) -> Result<(), rs_matter::error::Error> {
        let identity = self.store.load_identity().unwrap();
        let info = model::basic_info(&identity);
        let matter = Matter::new(&info, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
        let buffers: MatterBuffers = MatterBuffers::new();
        let state: InteractionModelState = InteractionModelState::new();
        let crypto = default_crypto(rand::rng(), DAC_PRIVKEY);
        let kv = matter.kv(StoreAdapter::new(self.store.matter()));
        let im = InteractionModel::new(&matter, &crypto, &buffers, &self.handler, &kv, &state);
        let mut bytes = [0; 128];
        let mut writer = WriteBuf::new(&mut bytes);
        writer.utf8(&TLVTag::Anonymous, label)?;
        let mut ctx = Context::at(&im, endpoint, 0x39, BridgedAttribute::NodeLabel as u32);
        ctx.data = TLVElement::new(writer.as_slice());
        self.handler.write(&ctx).await
    }

    pub(crate) async fn invoke(
        &self,
        endpoint: u16,
        command: u32,
    ) -> Result<(), rs_matter::error::Error> {
        let identity = self.store.load_identity().unwrap();
        let info = model::basic_info(&identity);
        let matter = Matter::new(&info, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
        let buffers: MatterBuffers = MatterBuffers::new();
        let state: InteractionModelState = InteractionModelState::new();
        let crypto = default_crypto(rand::rng(), DAC_PRIVKEY);
        let kv = matter.kv(StoreAdapter::new(self.store.matter()));
        let im = InteractionModel::new(&matter, &crypto, &buffers, &self.handler, &kv, &state);
        let mut ctx = Context::at(&im, endpoint, 6, 0);
        ctx.command.cmd_id = command;
        let mut bytes = [0; 128];
        self.handler
            .invoke(
                &ctx,
                InvokeReplyInstance::new(&ctx.command, WriteBuf::new(&mut bytes)),
            )
            .await
    }
}
