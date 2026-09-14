use super::{
    bridged_info::BridgedHandler,
    light::{LightHandler, LightHooks},
};
use crate::{storage::Identity, virtual_device::VirtualLight};
use rs_matter::{
    Matter, clusters, devices,
    dm::{
        Async, AsyncHandler, Dataver, Endpoint, Node,
        clusters::{
            app::on_off::{self, OnOffHooks},
            basic_info::BasicInfoConfig,
            decl::bridged_device_basic_information::{self as bridged, ClusterHandler as _},
            desc::{self, ClusterHandler as _},
            groups::{self, ClusterHandler as _},
            identify::{self, ClusterHandler as _},
            scenes::{self, ClusterAsyncHandler as _},
        },
        devices::{
            DEV_TYPE_AGGREGATOR, DEV_TYPE_BRIDGED_NODE, DEV_TYPE_ON_OFF_LIGHT, test::TEST_DEV_DET,
        },
        endpoints,
        networks::SysNetifs,
    },
    error::Error,
    root_endpoint,
};

const AGGREGATOR_ENDPOINT: u16 = 1;
pub(super) const LIGHT_ENDPOINT: u16 = 2;

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

pub(super) fn on_off<'a>(
    light: &'a VirtualLight,
    scenes: &'a scenes::ScenesState<16>,
    dataver: Dataver,
) -> on_off::OnOffHandler<'a, LightHooks<'a>, on_off::NoLevelControl> {
    on_off::OnOffHandler::new_standalone(
        dataver,
        LIGHT_ENDPOINT,
        LightHooks::new(light).with_scenes(scenes),
    )
    .with_scene_invalidator(scenes)
}

pub(super) fn handler<'a>(
    light: &'a VirtualLight,
    light_id: &'a str,
    label: String,
    identify: &'a identify::IdentifyHandler,
    scenes: &'a scenes::ScenesState<16>,
    on_off: &'a on_off::OnOffHandler<'a, LightHooks<'a>, on_off::NoLevelControl>,
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
            |e, c| e == LIGHT_ENDPOINT && c == desc::DescHandler::CLUSTER.id,
            Async(desc::DescHandler::new(Dataver::new_rand(&mut random)).adapt()),
        )
        .chain(
            |e, c| e == LIGHT_ENDPOINT && c == groups::GroupsHandler::CLUSTER.id,
            Async(
                groups::GroupsHandler::new_with_identify(Dataver::new_rand(&mut random), identify)
                    .adapt(),
            ),
        )
        .chain(
            |e, c| e == LIGHT_ENDPOINT && c == identify::IdentifyHandler::<()>::CLUSTER.id,
            Async(identify::HandlerAdaptor(identify)),
        )
        .chain(
            |e, c| e == LIGHT_ENDPOINT && c == scenes::ScenesHandler::<16>::CLUSTER.id,
            scenes::ScenesHandler::new(Dataver::new_rand(&mut random), scenes, (on_off, ()))
                .adapt(),
        )
        .chain(
            |e, c| e == LIGHT_ENDPOINT && c == LightHooks::CLUSTER.id,
            LightHandler::new(on_off, light),
        )
        .chain(
            |e, c| e == LIGHT_ENDPOINT && c == BridgedHandler::CLUSTER.id,
            Async(bridged::HandlerAdaptor(BridgedHandler::new(
                Dataver::new_rand(&mut random),
                light_id,
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
            LIGHT_ENDPOINT,
            devices!(DEV_TYPE_ON_OFF_LIGHT, DEV_TYPE_BRIDGED_NODE),
            clusters!(
                desc::DescHandler::CLUSTER,
                groups::GroupsHandler::CLUSTER,
                identify::IdentifyHandler::<()>::CLUSTER,
                scenes::ScenesHandler::<16>::CLUSTER,
                BridgedHandler::CLUSTER,
                LightHooks::CLUSTER
            ),
        ),
    ],
};
