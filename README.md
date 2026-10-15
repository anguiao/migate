# MiGate

MiGate 使用 Rust 和 `rs-matter` 构建 Matter Bridge，目标是将米家（Xiaomi/MIoT）设备映射为标准 Matter 设备，接入 Apple Home 等生态。

当前在 macOS / Apple Silicon 本机前台运行，首次通过局域网小米主中枢确定米家自有家庭，随后从云端目录接入受支持插座和单灯，并按本地优先选择控制与通知路径。支持 `cuco.plug.v3`、`xiaomi.plug.04`、`zimi.plug.zncz01`、`qmi.plug.psv3` 的单路主电源，以及 `cuco.plug.cp7pd` 的开关 1、开关 2、USB 三路独立控制。单灯包含 `devcea.light.ls2307`、`yeelink.light.light3`、`yeelink.light.ml9`、`yeelink.light.spot2`、`xiaomi.light.ceil04`、`xiaomi.light.bar2`、`lemesh.light.wy0c15` 七个代表描述样本，按实际开关、亮度、色温和 RGB 能力组合接入，不设型号白名单或样本数量上限；这些型号是解析样本，不构成兼容性保证。灯组按当前家庭内扁平且成员全部可作为单灯发布的关系接入，米家物理组和虚拟组采用同一规则；成员能力可以不同，同一成员可以属于多个组，并保留各自实际具备的灯光能力。

按实际能力提供开关、亮度、色温和 RGB 控制，以及软件渐变与连续控制、主动通知、统一选路、断线恢复、运行中自动加入和删除，以及持久化身份、名称和配对。单灯统一为 `main`；同时具备色温与色彩的灯在选择时自动切换模式。灯组按成员能力的交集与共同色温范围发布一个普通虚拟灯端点，复用成员读取、订阅和控制路径：至少一个成员完整受理即返回组成功，其余成员继续执行；实际状态只汇总成员的实际快照。成员关系、物理实例、发布集合或固定描述变化时自动重建 Matter 服务，允许整桥短时中断；未完成的灯光动作可能中断且不会自动续跑，米家连接、身份和配对保留。成员在 Apple Home 中隐藏，成员单控仍在米家进行。终端保留定向 on/off 控制，并对已发布的组 DID 执行整组 on/off（不带通道）；用 `list` / `status` 查看统一灯具快照、组派生能力与共同范围、实际聚合、成员当前状态、路径与已有请求诊断、关系未发布原因；无写入帧与动作受理分开显示。中国大陆服务区的 OAuth 令牌与本地客户端证书独立维护。已准备账号可以先运行空网桥，再等待设备接入。云端可能仍依赖物理网关，具体边界见已知限制。

单路插座在同一端点承载 Bridged Node 与 On/Off Plug-in Unit（`0x010A`，revision 3）；多路插座使用一个物理父节点和三个独立开关子节点，总控只触发内部状态刷新。具备开关的灯按实际能力选择 On/Off Light（`0x0100`，revision 3）、Dimmable Light（`0x0101`，revision 3）、Color Temperature Light（`0x010C`，revision 4）或 Extended Color Light（`0x010D`，revision 4）；有亮度才发布 LevelControl，有色温或 RGB 才发布 ColorControl，CT 与 XY／HS（色相／饱和度）按真实能力选择。纯开关灯仍是 On/Off Light，不套用插座类型；缺少开关或色温量程无法映射为标准灯的灯保留实际能力与身份，但不会出现在 Apple Home 中。灯组在单一虚拟灯端点上保留 Descriptor、Bridged Device Basic Information、Groups、Identify 与派生灯光控制能力；当前成员不发布为 Matter 端点，其端点编号与身份继续预留，退出最后一个已发布组后恢复原独立灯结构。OnOff/Level Lighting、场景与启动策略仍有标准必需项缺口，Level 保持 revision 6，与 Matter 1.6 的 revision 7 有差异；完整列表见[已知限制](docs/known-limitations.md)。Apple Home 中的组入口、组控件与状态汇总、固件模式联动及基础兼容性在 `review` 阶段真机验收，不声称已通过 Matter 认证。

```sh
cargo run --locked -- auth login
cargo run --locked
```

仓库 Cargo 配置默认使用 `.migate/`；直接运行构建产物时目录规则不同，详见运行指南。

- [运行与 Apple Home 配对指南](docs/running.md)
- [已知限制与恢复经验](docs/known-limitations.md)
- [起步方案](docs/specs/2026-09-08-virtual-light-bootstrap/spec.md)
- [米家认证方案](docs/specs/2026-09-11-xiaomi-authentication/spec.md)
- [最小设备模型方案](docs/specs/2026-10-03-minimal-device-model/spec.md)
- [多设备与生命周期方案](docs/specs/2026-10-04-multi-device-channel-bridge/spec.md)
- [云端接入与统一选路方案](docs/specs/2026-10-06-cloud-access-routing/spec.md)
- [单灯照明方案](docs/specs/2026-10-08-single-light-control/spec.md)
- [米家原生灯组接入与能力修复方案](docs/specs/2026-10-10-native-light-groups-and-capability-repair/spec.md)

米家认证协议参考[小米官方 Home Assistant 集成](https://github.com/XiaoMi/ha_xiaomi_home)，使用该集成的 OAuth 应用配置。MiGate 是个人爱好项目，该授权归属于 HA 应用，不代表小米为 MiGate 提供独立授权或官方支持。参考来源与使用前提见[米家认证方案](docs/specs/2026-09-11-xiaomi-authentication/spec.md#认证方案与云端依赖)；注明来源不会扩大[上游许可证](https://github.com/XiaoMi/ha_xiaomi_home/blob/main/LICENSE.md)的授权范围。

后续继续覆盖开关、插座、传感器、窗帘和空调，仅暴露可自然映射为标准 Matter 能力的功能。

## 代码组织

`mod.rs` 声明子模块并导出公共接口，具体实现按职责命名。模块内部实现保持私有，必要的共享成员仅对所在模块开放。

| 目录 | 职责 |
| --- | --- |
| `src/device/` | 通用设备模型、共享状态与通知、命令队列及其生命周期。 |
| `src/lighting/` | 单灯与灯组共用的照明执行器：目标与渐变、连续动作、接管及动作生命周期。 |
| `src/runtime/` | 网桥启动与任务生命周期、进程退出信号。 |
| `src/matter/` | Matter 服务、端点与处理器、配对及协议存储适配。 |
| `src/storage/` | SQLite 连接与当前格式初始化、目录和进程锁、各类持久化记录。 |
| `src/xiaomi/auth/` | 认证服务、登录、检查与刷新、状态报告。 |
| `src/xiaomi/cloud/` | HTTP 请求、响应解析、云端目录、OAuth 与 MQTT 通知。 |
| `src/xiaomi/capability/`、`src/xiaomi/catalog/` | MIoT 能力解析与当前本地家庭的设备描述加载。 |
| `src/xiaomi/discovery/` | mDNS 浏览、公告解析与合并、网关路由选择。 |
| `src/xiaomi/central/session/` | 中枢握手、传输驱动、MQTT 处理、请求分派与业务响应。 |
| `src/xiaomi/adapter/` | 设备成员与生命周期、路径选择与恢复、通知来源、状态观察及凭据协调。 |

模块测试及辅助代码放在所属模块的 `tests/` 目录；公开接口、存储和进程集成测试位于根目录 `tests/`。

## 开发检查

```sh
cargo fmt --check
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
```

`cargo test` 使用临时目录、可控协议对端与可推进时钟，复用生产的设备与运行组装入口，覆盖本地和云端选路、灯具量化与接管、Matter 命令、属性、订阅及退出和重启。进程回归使用系统分配端口、macOS Bonjour 和 `dns-sd` 检查多个实例及 mDNS 公告；普通检查不访问个人账号、真实中枢或设备。
