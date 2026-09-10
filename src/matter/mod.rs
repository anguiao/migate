mod bridged_info;
mod kv;
mod light;

use crate::{
    storage::{Identity, Store},
    virtual_device::VirtualLight,
};
use bridged_info::BridgedHandler;
use kv::ProtocolStore;
use light::{LightHandler, LightHooks};
use rs_matter::{
    MATTER_PORT, Matter, clusters,
    crypto::{Crypto, default_crypto},
    devices,
    dm::{
        Async, Dataver, Endpoint, Node,
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
            DEV_TYPE_AGGREGATOR, DEV_TYPE_BRIDGED_NODE, DEV_TYPE_ON_OFF_LIGHT,
            test::{DAC_PRIVKEY, TEST_DEV_ATT, TEST_DEV_COMM, TEST_DEV_DET},
        },
        endpoints,
        networks::{SysNetifs, eth::EthNetwork},
    },
    error::Error,
    im::{EthInteractionModelState, InteractionModel},
    pairing::{
        DiscoveryCapabilities,
        qr::{CommFlowType, Qr, QrPayload, QrTextType, no_optional_data},
    },
    respond::DefaultResponder,
    root_endpoint,
    transport::{
        MATTER_SOCKET_BIND_ADDR, exchange::MatterBuffers, network::mdns::astro::AstroMdns,
    },
};
use std::{future::Future, net::UdpSocket, time::Duration};

pub type RuntimeError = Box<dyn std::error::Error>;
const WINDOW_SECONDS: u16 = 900;
const AGGREGATOR_ENDPOINT: u16 = 1;
const LIGHT_ENDPOINT: u16 = 2;

fn basic_info(identity: &Identity) -> BasicInfoConfig<'_> {
    BasicInfoConfig {
        product_name: "MiGate",
        device_name: "MiGate",
        product_label: "MiGate",
        serial_no: &identity.bridge_id,
        unique_id: &identity.bridge_id,
        ..TEST_DEV_DET
    }
}
fn pairing_codes(info: &BasicInfoConfig<'_>) -> Result<(String, String), Error> {
    let payload = QrPayload::new_from_basic_info(
        DiscoveryCapabilities::IP,
        CommFlowType::Standard,
        TEST_DEV_COMM,
        info,
        no_optional_data,
    );
    let mut buffer = [0; 1024];
    let (text, _) = payload.as_str(&mut buffer)?;
    Ok((
        text.to_owned(),
        TEST_DEV_COMM.compute_pairing_code().to_string(),
    ))
}
fn print_pairing(info: &BasicInfoConfig<'_>) -> Result<(), RuntimeError> {
    use std::io::Write;
    let (text, manual) = pairing_codes(info)?;
    let mut output = std::io::stdout().lock();
    writeln!(
        output,
        "请在家庭 App 添加 MiGate，配对窗口为 15 分钟。\n手动配对码：{manual}\n{text}"
    )?;
    let mut scratch = [0; 4096];
    let mut qr_buf = [0; 4096];
    let qr = Qr::compute(&text, &mut scratch, &mut qr_buf)?;
    for y in qr.lines_range(QrTextType::Unicode, 4) {
        writeln!(
            output,
            "{}",
            qr.line_as_str(QrTextType::Unicode, 4, false, false, y, &mut scratch)?
                .0
        )?;
    }
    output.flush()?;
    Ok(())
}

fn initialize_basic_info(
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

/// Run the bridge on the caller's local executor until shutdown or a fatal service error.
/// The shutdown future may also carry fatal errors from other process tasks.
pub async fn run(
    light: &VirtualLight,
    store: Store,
    shutdown: impl Future<Output = Result<(), RuntimeError>>,
) -> Result<(), RuntimeError> {
    let label = bridged_info::load_label(&store).map_err(|e| {
        format!(
            "桥接设备名称恢复失败（{}）：{e}",
            store.directory().display()
        )
    })?;
    let missing_basic_info = store.load(rs_matter::persist::BASIC_INFO_KEY).is_none();
    let identity = store.identity().clone();
    let directory = store.directory().display().to_string();
    let info = basic_info(&identity);
    let matter = Matter::new(&info, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
    let store = ProtocolStore::new(store);
    let kv = matter.kv(store.clone());
    matter
        .startup(&kv)
        .map_err(|e| format!("Matter 数据恢复失败（{directory}）：{e}"))?;
    let buffers: MatterBuffers = MatterBuffers::new();
    let state: EthInteractionModelState = EthInteractionModelState::new(EthNetwork::new_default());
    let crypto = default_crypto(rand::rng(), DAC_PRIVKEY);
    let mut random = crypto.rand()?;
    let scenes = scenes::ScenesState::<16>::new();
    let identify = identify::IdentifyHandler::new(Dataver::new_rand(&mut random));
    let on_off = on_off::OnOffHandler::new_standalone(
        Dataver::new_rand(&mut random),
        LIGHT_ENDPOINT,
        LightHooks::new(light).with_scenes(&scenes),
    )
    .with_scene_invalidator(&scenes);
    let model = (
        NODE,
        endpoints::EthSysHandlerBuilder::new()
            .netif_diag(&SysNetifs)
            .build(random)
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
                    groups::GroupsHandler::new_with_identify(
                        Dataver::new_rand(&mut random),
                        &identify,
                    )
                    .adapt(),
                ),
            )
            .chain(
                |e, c| e == LIGHT_ENDPOINT && c == identify::IdentifyHandler::<()>::CLUSTER.id,
                Async(identify::HandlerAdaptor(&identify)),
            )
            .chain(
                |e, c| e == LIGHT_ENDPOINT && c == scenes::ScenesHandler::<16>::CLUSTER.id,
                scenes::ScenesHandler::new(Dataver::new_rand(&mut random), &scenes, (&on_off, ()))
                    .adapt(),
            )
            .chain(
                |e, c| e == LIGHT_ENDPOINT && c == LightHooks::CLUSTER.id,
                LightHandler::new(&on_off, light),
            )
            .chain(
                |e, c| e == LIGHT_ENDPOINT && c == BridgedHandler::CLUSTER.id,
                Async(bridged::HandlerAdaptor(BridgedHandler::new(
                    Dataver::new_rand(&mut random),
                    &identity.light_id,
                    label,
                ))),
            ),
    );
    let im = InteractionModel::new(&matter, &crypto, &buffers, model, &kv, &state);
    im.startup()
        .await
        .map_err(|e| format!("Matter 模型恢复失败（{directory}）：{e}"))?;
    // Initialize only after every existing blob has been restored successfully.
    initialize_basic_info(&matter, &kv, missing_basic_info)
        .map_err(|e| format!("Matter 默认名称初始化失败（{directory}）：{e}"))?;
    let socket = async_io::Async::<UdpSocket>::bind(MATTER_SOCKET_BIND_ADDR)
        .map_err(|e| format!("无法绑定 Matter UDP 5540：{e}"))?;
    let responder = DefaultResponder::new(&im);
    let opened = !matter.has_fabrics();
    if opened {
        matter.open_basic_comm_window(WINDOW_SECONDS, &crypto, &())?;
        print_pairing(&info)?;
    }
    let timeout = async {
        if opened {
            async_io::Timer::after(Duration::from_secs(WINDOW_SECONDS.into())).await;
            if !matter.has_fabrics() {
                use std::io::Write;
                writeln!(
                    std::io::stdout().lock(),
                    "配对窗口已超时；重启 MiGate 可重新打开配对窗口。"
                )?;
            }
        }
        std::future::pending::<Result<(), RuntimeError>>().await
    };
    let fatal = async { Err::<(), RuntimeError>(store.failed().await.to_string().into()) };
    let transport = async {
        matter
            .run(&crypto, &socket, &socket, &socket)
            .await
            .map_err(|e| format!("Matter 传输任务失败：{e}").into())
    };
    let mdns = async {
        AstroMdns::new()
            .run(&matter)
            .await
            .map_err(|e| format!("mDNS 服务失败：{e}").into())
    };
    let respond = async {
        responder
            .run::<4, 4>()
            .await
            .map_err(|e| format!("Matter 响应任务失败：{e}").into())
    };
    let job = async {
        im.run()
            .await
            .map_err(|e| format!("Matter 模型任务失败：{e}").into())
    };
    use futures_lite::future::or;
    let result = or(
        shutdown,
        or(
            fatal,
            or(timeout, or(transport, or(mdns, or(respond, job)))),
        ),
    )
    .await;
    // All service futures are dropped before this final synchronous durability check.
    finish(&store, result)
}

fn finish(store: &ProtocolStore, result: Result<(), RuntimeError>) -> Result<(), RuntimeError> {
    // A sticky storage failure takes precedence even when shutdown won the race.
    store.flush().map_err(|error| error.to_string())?;
    result
}

const NODE: Node<'static> = Node {
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
#[cfg(test)]
mod tests;
