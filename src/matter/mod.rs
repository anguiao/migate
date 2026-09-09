mod kv;
mod light;
use crate::{
    storage::{Identity, Store},
    virtual_device::VirtualLight,
};
use kv::ProtocolStore;
#[cfg(test)]
use light::basic_command;
use light::{LightHandler, LightHooks};
use rs_matter::{
    MATTER_PORT, Matter, clusters,
    crypto::{Crypto, default_crypto},
    devices,
    dm::{
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
        networks::{SysNetifs, eth::EthNetwork},
        *,
    },
    error::Error,
    im::{EthInteractionModelState, InteractionModel},
    pairing::{
        DiscoveryCapabilities,
        qr::{CommFlowType, Qr, QrPayload, QrTextType, no_optional_data},
    },
    respond::DefaultResponder,
    root_endpoint,
    tlv::{TLVBuilderParent, Utf8StrBuilder},
    transport::{
        MATTER_SOCKET_BIND_ADDR, exchange::MatterBuffers, network::mdns::astro::AstroMdns,
    },
    with,
};
use std::{cell::RefCell, future::Future, net::UdpSocket, time::Duration};

pub type RuntimeError = Box<dyn std::error::Error>;
const WINDOW_SECONDS: u16 = 900;
fn should_open_window(has_fabrics: bool) -> bool {
    !has_fabrics
}
const LIGHT_LABEL_KEY: u16 = rs_matter::persist::VENDOR_KEYS_START;
fn load_light_label(store: &Store) -> Result<String, Error> {
    let Some(data) = store.load(LIGHT_LABEL_KEY) else {
        return Ok("MiGate 虚拟灯".into());
    };
    let label = rs_matter::tlv::TLVElement::new(data).utf8()?;
    if label.len() > 32 {
        return Err(rs_matter::error::ErrorCode::ConstraintError.into());
    }
    Ok(label.to_owned())
}
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
    let label = load_light_label(&store).map_err(|e| {
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
        2,
        LightHooks::new(light).with_scenes(&scenes),
    )
    .with_scene_invalidator(&scenes);
    let model = (
        NODE,
        endpoints::EthSysHandlerBuilder::new()
            .netif_diag(&SysNetifs)
            .build(random)
            .chain(
                |e, c| e == 1 && c == desc::DescHandler::CLUSTER.id,
                Async(desc::DescHandler::new_aggregator(Dataver::new_rand(&mut random)).adapt()),
            )
            .chain(
                |e, c| e == 2 && c == desc::DescHandler::CLUSTER.id,
                Async(desc::DescHandler::new(Dataver::new_rand(&mut random)).adapt()),
            )
            .chain(
                |e, c| e == 2 && c == groups::GroupsHandler::CLUSTER.id,
                Async(
                    groups::GroupsHandler::new_with_identify(
                        Dataver::new_rand(&mut random),
                        &identify,
                    )
                    .adapt(),
                ),
            )
            .chain(
                |e, c| e == 2 && c == identify::IdentifyHandler::<()>::CLUSTER.id,
                Async(identify::HandlerAdaptor(&identify)),
            )
            .chain(
                |e, c| e == 2 && c == scenes::ScenesHandler::<16>::CLUSTER.id,
                scenes::ScenesHandler::new(Dataver::new_rand(&mut random), &scenes, (&on_off, ()))
                    .adapt(),
            )
            .chain(
                |e, c| e == 2 && c == LightHooks::CLUSTER.id,
                LightHandler::new(&on_off, light),
            )
            .chain(
                |e, c| e == 2 && c == BridgedHandler::CLUSTER.id,
                Async(bridged::HandlerAdaptor(BridgedHandler {
                    dataver: Dataver::new_rand(&mut random),
                    identity: &identity.light_id,
                    label: RefCell::new(label),
                })),
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
    let opened = should_open_window(matter.has_fabrics());
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
            1,
            devices!(DEV_TYPE_AGGREGATOR),
            clusters!(desc::DescHandler::CLUSTER),
        ),
        Endpoint::new(
            2,
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
struct BridgedHandler<'a> {
    dataver: Dataver,
    identity: &'a str,
    label: RefCell<String>,
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
    fn set_node_label(
        &self,
        ctx: impl WriteContext,
        value: rs_matter::tlv::Utf8Str<'_>,
    ) -> Result<(), Error> {
        if value.len() > 32 {
            return Err(rs_matter::error::ErrorCode::ConstraintError.into());
        }
        if *self.label.borrow() != value {
            rs_matter::persist::Persist::new(ctx.kv()).store_tlv(LIGHT_LABEL_KEY, value)?;
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
        Err(rs_matter::error::ErrorCode::CommandNotFound.into())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::virtual_device::VirtualLight;
    #[test]
    fn commands_confirm_device_state() {
        let light = VirtualLight::new();
        for (id, power) in [(1, true), (0, false), (2, true), (2, false)] {
            basic_command(&light, id, &rs_matter::tlv::TLVElement::new(&[0x15, 0x18])).unwrap();
            assert_eq!(light.snapshot().power, power);
        }
    }
}
#[cfg(test)]
mod storage_tests {
    use super::kv::ProtocolStore;
    use crate::storage::Store;
    use rs_matter::persist::KvBlobStore;
    #[test]
    fn raw_blob_roundtrip_and_removal() {
        let dir = tempfile::tempdir().unwrap();
        let mut kv = ProtocolStore::new(Store::open(dir.path()).unwrap());
        kv.store(123, &[1, 2, 3], &mut []).unwrap();
        let mut restored = ProtocolStore::new(Store::open(dir.path()).unwrap());
        let mut buf = [0; 10];
        assert_eq!(restored.load(123, &mut buf).unwrap(), Some(&[1, 2, 3][..]));
        restored.remove(123, &mut []).unwrap();
        assert!(Store::open(dir.path()).unwrap().load(123).is_none());
    }
    #[test]
    fn write_failure_wakes_fatal_watcher() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data");
        let mut kv = ProtocolStore::new(Store::open(&path).unwrap());
        std::fs::remove_dir_all(&path).unwrap();
        assert!(kv.store(1, &[1], &mut []).is_err());
        let message = futures_lite::future::block_on(kv.failed());
        assert!(message.to_string().contains(path.to_str().unwrap()));
    }
}
#[cfg(test)]
mod topology_tests {
    use super::*;
    #[test]
    fn topology_and_identity_are_fixed() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::storage::Store::open(dir.path()).unwrap();
        let info = basic_info(store.identity());
        assert_eq!(info.serial_no, store.identity().bridge_id);
        assert_eq!(info.unique_id, store.identity().bridge_id);
        assert_eq!(info.product_name, "MiGate");
        assert_eq!(
            NODE.endpoints.iter().map(|e| e.id).collect::<Vec<_>>(),
            [0, 1, 2]
        );
        assert!(should_open_window(false));
        assert!(!should_open_window(true));
        let (qr, manual) = pairing_codes(&info).unwrap();
        let mut buf = [0; 1024];
        let qr = rs_matter::pairing::qr::QrPayload::parse(&qr, &mut buf).unwrap();
        let manual = rs_matter::pairing::qr::QrPayload::parse_pairing_code(&manual).unwrap();
        assert_eq!(qr.passcode(), manual.passcode());
    }
}
#[cfg(test)]
mod recovery_tests {
    use super::*;
    #[test]
    fn protocol_corruption_exits_with_path_without_overwriting() {
        std::thread::Builder::new()
            .stack_size(16 * 1024 * 1024)
            .spawn(|| {
                for key in [
                    rs_matter::persist::BASIC_INFO_KEY,
                    rs_matter::persist::SCENES_KEY,
                    LIGHT_LABEL_KEY,
                ] {
                    let dir = tempfile::tempdir().unwrap();
                    let mut store = Store::open(dir.path()).unwrap();
                    store.store(key, &[0xff, 0x11]).unwrap();
                    let before = std::fs::read(dir.path().join("state.json")).unwrap();
                    let light = VirtualLight::new();
                    let error =
                        futures_lite::future::block_on(run(&light, store, std::future::pending()))
                            .unwrap_err();
                    assert!(error.to_string().contains(dir.path().to_str().unwrap()));
                    assert_eq!(
                        std::fs::read(dir.path().join("state.json")).unwrap(),
                        before
                    );
                }
            })
            .unwrap()
            .join()
            .unwrap();
    }
}
#[cfg(test)]
mod defaults_tests {
    use super::*;
    use rs_matter::{
        persist::{BASIC_INFO_KEY, KvBlobStore},
        tlv::{FromTLV, TLVElement},
    };
    #[test]
    fn default_node_label_uses_upstream_format_and_preserves_existing_settings() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let identity = store.identity().clone();
        let info = basic_info(&identity);
        let matter = Matter::new(&info, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
        let mut protocol = ProtocolStore::new(store);
        let kv = matter.kv(protocol.clone());
        initialize_basic_info(&matter, &kv, true).unwrap();
        let mut buf = [0; 1024];
        let before = protocol
            .load(BASIC_INFO_KEY, &mut buf)
            .unwrap()
            .unwrap()
            .to_vec();
        let settings = rs_matter::dm::clusters::basic_info::BasicInfoSettings::from_tlv(
            &TLVElement::new(&before),
        )
        .unwrap();
        assert_eq!(settings.node_label.as_str(), "MiGate");
        initialize_basic_info(&matter, &kv, false).unwrap();
        assert_eq!(
            protocol.load(BASIC_INFO_KEY, &mut buf).unwrap().unwrap(),
            before
        );
    }
}
#[cfg(test)]
mod label_tests {
    use super::*;
    #[test]
    fn bridged_label_defaults_and_detects_corruption() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path()).unwrap();
        assert_eq!(load_light_label(&store).unwrap(), "MiGate 虚拟灯");
        store.store(LIGHT_LABEL_KEY, &[0x15]).unwrap();
        assert!(load_light_label(&store).is_err());
    }
}
#[cfg(test)]
mod integration_tests;
