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
        clusters::decl::{
            bridged_device_basic_information::AttributeId as BridgedAttribute,
            color_control::AttributeId as ColorAttribute,
        },
        devices::test::{DAC_PRIVKEY, TEST_DEV_ATT, TEST_DEV_COMM},
        networks::eth::EthNetwork,
    },
    error::ErrorCode,
    im::{EthInteractionModelState, InteractionModel},
    transport::exchange::MatterBuffers,
};
use std::{future::Future, pin::Pin, rc::Rc};

pub(crate) struct PublishedLightModel {
    handler: model::Model,
    store: Store,
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
        Self {
            handler: model::handler_from(app.frozen(), rand::rng(), app.clone()),
            store: store.clone(),
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
        let state: EthInteractionModelState =
            EthInteractionModelState::new(EthNetwork::new_default());
        let crypto = default_crypto(rand::rng(), DAC_PRIVKEY);
        let kv = matter.kv(StoreAdapter::new(self.store.matter()));
        let im = InteractionModel::new(&matter, &crypto, &buffers, &self.handler, &kv, &state);
        let mut values = Vec::new();
        for (cluster, attribute) in [
            (0x300, ColorAttribute::ColorTempPhysicalMinMireds as u32),
            (0x300, ColorAttribute::ColorTempPhysicalMaxMireds as u32),
            (0x39, BridgedAttribute::ConfigurationVersion as u32),
        ] {
            values.push(
                read_bytes(
                    &self.handler,
                    &Context::at(&im, endpoint, cluster, attribute),
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
                endpoint,
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
