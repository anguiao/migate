# MiGate

MiGate 使用 Rust 和 `rs-matter` 构建 Matter Bridge，目标是将米家（Xiaomi/MIoT）设备映射为标准 Matter 设备，接入 Apple Home 等生态。

当前在 macOS / Apple Silicon 本机前台运行，首次通过局域网小米主中枢确定米家自有家庭，随后从云端目录接入受支持插座和单灯，并按本地优先选择控制与通知路径。支持 `cuco.plug.v3`、`xiaomi.plug.04`、`zimi.plug.zncz01`、`qmi.plug.psv3` 的单路主电源，以及 `cuco.plug.cp7pd` 的开关 1、开关 2、USB 三路独立控制。单灯包含 `devcea.light.ls2307`、`yeelink.light.light3`、`yeelink.light.ml9`、`yeelink.light.spot2`、`xiaomi.light.ceil04`、`xiaomi.light.bar2`、`lemesh.light.wy0c15` 七个描述样本；按完整能力准入，不设置当前样本数量上限。灯组按当前家庭内扁平、互不重叠且成员全部为已支持单灯的关系接入，米家物理组和虚拟组采用同一规则；成员保留各自的完整灯光能力。

提供基础开关、灯具亮度与色温、设备支持的 RGB、软件渐变与连续控制、主动通知、统一选路、断线恢复、运行中自动加入和删除，以及持久化身份、名称和配对。单灯统一为 `main`，选择色温或颜色时自动切换模式。灯组只发布组合关系：MiGate 发布仅承载组身份的父端点与保留各自能力的成员子端点，不建立可控制的总灯端点，也不聚合整组状态；Apple Home 中每组一个入口并单独控制成员是本轮交付要求，实际界面在 `review` 阶段真机验收。终端保留定向 on/off 控制，用 `list` / `status` 查看统一灯具快照、动作结果、实际批属性与路径，同时显示组与成员关系、关系未发布的原因；无写入帧与动作受理分开显示。中国大陆服务区的 OAuth 令牌与本地客户端证书独立维护。已准备账号可以先运行空网桥，再等待设备接入。云端可能仍依赖物理网关，具体边界见已知限制。

单路插座在同一端点承载 Bridged Node 与 On/Off Plug-in Unit（`0x010A`，revision 3）；多路插座使用一个物理父节点和三个独立开关子节点，总控只触发内部状态刷新。每盏灯使用一个 Color Temperature Light（`0x010C`）或 Extended Color Light（`0x010D`）端点，revision 4，声明 CT 或 CT + XY + HS（色相／饱和度）。灯组使用一个仅含 Descriptor 与 Bridged Device Basic Information 的 Bridged Node 父端点承载组身份，成员作为子端点保留各自灯型与完整灯光能力，不重复声明桥接身份。OnOff/Level Lighting、场景与启动策略仍有标准必需项缺口，Level 保持 revision 6，与 Matter 1.6 的 revision 7 有差异；完整列表见[已知限制](docs/known-limitations.md)。Apple Home 的实际呈现、组入口自动形成、成员单控与整组／成员颜色控件、固件模式联动及基础兼容性在 `review` 阶段真机验收，不声称已通过 Matter 认证。

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
- [灯具能力自由组合与灯组接入方案](docs/specs/2026-10-10-light-capabilities-and-groups/spec.md)

米家认证协议参考[小米官方 Home Assistant 集成](https://github.com/XiaoMi/ha_xiaomi_home)，使用该集成的 OAuth 应用配置。MiGate 是个人爱好项目，该授权归属于 HA 应用，不代表小米为 MiGate 提供独立授权或官方支持。参考来源与使用前提见[米家认证方案](docs/specs/2026-09-11-xiaomi-authentication/spec.md#认证方案与云端依赖)；注明来源不会扩大[上游许可证](https://github.com/XiaoMi/ha_xiaomi_home/blob/main/LICENSE.md)的授权范围。

后续继续覆盖开关、插座、传感器、窗帘和空调，仅暴露可自然映射为标准 Matter 能力的功能。

## 代码组织

`mod.rs` 声明子模块并导出公共接口，具体实现按职责命名。模块内部实现保持私有，必要的共享成员仅对所在模块开放。

| 目录 | 职责 |
| --- | --- |
| `src/device/` | 通用设备模型、共享状态与通知、命令队列及其生命周期。 |
| `src/runtime/` | 网桥启动与任务生命周期、进程退出信号。 |
| `src/matter/` | Matter 服务、端点与处理器、配对及协议存储适配。 |
| `src/storage/` | SQLite 连接与迁移、目录和进程锁、各类持久化记录。 |
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
