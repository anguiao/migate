use super::{bridged_info::BridgedHandler, on_off::OnOffHandler};
use crate::{device::Device, storage::Identity};
use rs_matter::{
    Matter, clusters, devices,
    dm::{
        Async, AsyncHandler, Dataver, DeviceType, Endpoint, Node,
        clusters::{
            app::on_off::{self, ClusterAsyncHandler as _},
            basic_info::BasicInfoConfig,
            decl::bridged_device_basic_information::{self as bridged, ClusterHandler as _},
            desc::{self, ClusterHandler as _},
            groups::{self, ClusterHandler as _},
            identify::{self, ClusterHandler as _},
        },
        devices::{DEV_TYPE_AGGREGATOR, DEV_TYPE_BRIDGED_NODE, test::TEST_DEV_DET},
        endpoints,
        networks::SysNetifs,
    },
    error::Error,
    root_endpoint,
};
const AGGREGATOR_ENDPOINT: u16 = 1;
pub(super) const DEVICE_ENDPOINT: u16 = 2;
// Basic interoperability intentionally omits the standard's Lighting/Scenes requirements.
const ON_OFF_PLUG: DeviceType = DeviceType {
    dtype: 0x010a,
    drev: 3,
};
pub(super) fn basic_info(identity: &Identity) -> BasicInfoConfig<'_> {
    BasicInfoConfig {
        product_name: "MiGate",
        device_name: "MiGate",
        product_label: "MiGate",
        serial_no: &identity.bridge_id,
        unique_id: &identity.bridge_id,
        ..TEST_DEV_DET
    }
}
pub(super) fn initialize_basic_info(
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
pub(super) fn on_off(device: &Device, dataver: Dataver) -> OnOffHandler<'_> {
    OnOffHandler::new(device, dataver)
}
pub(super) fn handler<'a>(
    device: &'a Device,
    device_id: &'a str,
    label: String,
    identify: &'a identify::IdentifyHandler,
    on_off: &'a OnOffHandler<'a>,
    mut random: impl rand::Rng + 'a,
) -> impl AsyncHandler + 'a {
    endpoints::EthSysHandlerBuilder::new()
        .netif_diag(&SysNetifs)
        .build(&mut random)
        .chain(
            |e, c| e == AGGREGATOR_ENDPOINT && c == desc::DescHandler::CLUSTER.id,
            Async(desc::DescHandler::new_aggregator(Dataver::new_rand(&mut random)).adapt()),
        )
        .chain(
            |e, c| e == DEVICE_ENDPOINT && c == desc::DescHandler::CLUSTER.id,
            Async(desc::DescHandler::new(Dataver::new_rand(&mut random)).adapt()),
        )
        .chain(
            |e, c| e == DEVICE_ENDPOINT && c == groups::GroupsHandler::CLUSTER.id,
            Async(
                groups::GroupsHandler::new_with_identify(Dataver::new_rand(&mut random), identify)
                    .adapt(),
            ),
        )
        .chain(
            |e, c| e == DEVICE_ENDPOINT && c == identify::IdentifyHandler::<()>::CLUSTER.id,
            Async(identify::HandlerAdaptor(identify)),
        )
        .chain(
            |e, c| e == DEVICE_ENDPOINT && c == OnOffHandler::CLUSTER.id,
            on_off::HandlerAsyncAdaptor(on_off),
        )
        .chain(
            |e, c| e == DEVICE_ENDPOINT && c == BridgedHandler::CLUSTER.id,
            Async(bridged::HandlerAdaptor(BridgedHandler::new(
                device,
                Dataver::new_rand(&mut random),
                device_id,
                label,
            ))),
        )
}
pub(super) const NODE: Node<'static> = Node {
    endpoints: &[
        root_endpoint!(eth),
        Endpoint::new(
            AGGREGATOR_ENDPOINT,
            devices!(DEV_TYPE_AGGREGATOR),
            clusters!(desc::DescHandler::CLUSTER),
        ),
        Endpoint::new(
            DEVICE_ENDPOINT,
            devices!(ON_OFF_PLUG, DEV_TYPE_BRIDGED_NODE),
            clusters!(
                desc::DescHandler::CLUSTER,
                groups::GroupsHandler::CLUSTER,
                identify::IdentifyHandler::<()>::CLUSTER,
                BridgedHandler::CLUSTER,
                OnOffHandler::CLUSTER
            ),
        ),
    ],
};
