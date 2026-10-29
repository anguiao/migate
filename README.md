# MiGate

MiGate 使用 Rust 和 `rs-matter` 构建 Matter Bridge，目标是将米家（Xiaomi/MIoT）设备映射为标准 Matter 设备，接入 Apple Home 等生态。

当前在 macOS / Apple Silicon 本机前台运行，首次通过局域网小米主中枢确定米家自有家庭，随后从云端目录接入受支持的插座、灯光、米家原生灯组、窗帘、风扇、环境测量与状态功能，并按本地优先选择控制与通知路径。通用能力按实际服务和属性分别识别，不需要型号白名单；同一设备可有多个控制功能、多个同类功能实例，以及测量、门窗或运动等组合。人体存在只接入整机状态，`xiaomi.sensor_occupy.p1` 不展开分区。功能组合使用合成描述验证，实际型号的功能仍按设备描述判定。

能力描述样本包括插座 `cuco.plug.v3`、`xiaomi.plug.04`、`zimi.plug.zncz01`、`qmi.plug.psv3`，灯具 `devcea.light.ls2307`、`yeelink.light.light3`、`yeelink.light.ml9`、`yeelink.light.spot2`、`xiaomi.light.ceil04`、`xiaomi.light.bar2`、`lemesh.light.wy0c15`，窗帘 `xiaomi.curtain.acn010`，风扇 `dmaker.fan.p5c`，以及温湿度传感器 `miaomiaoce.sensor_ht.t6`、`xiaomi.sensor_ht.new3pr` 和门窗、运动、存在等设备的公开描述。内置补丁处理已确认的差异：`ls2307` 的跨服务 RGB 和颜色模式、`cuco.plug.cp7pd` 的开关 1、开关 2、USB 三个独立通道与总控刷新，以及已支持 `dmaker` 风扇的直吹模式与风速行为。

提供基础开关、灯具亮度与色温、设备支持的 RGB、软件渐变与连续控制、窗帘位置与停止、风扇调速与摇头、只读测量与状态、可选整机电量、主动通知、断线恢复及持久化身份、名称和配对。风扇普通风速可使用有效范围、步长或有序离散挡位，Matter 百分比映射到实际可达挡位，不代表原生连续调速。`dmaker` 的既有策略在自然风下显示记忆的直吹设定，非零调速会切回直吹；一般风扇不要求直吹模式。更多能力边界见[已知限制](docs/known-limitations.md)。

有效原生灯组在 Apple Home 中显示为一盏灯。组成员能唯一定位到灯光功能时，仅隐藏该灯光入口，同机其他控制、测量和状态仍可发布；多个灯光无法确定归属时不任意选择或隐藏，可独立成立的原生组功能仍可用。原生能力优先使用组接口；缺少 RGB 时，原生组需已具备开关、亮度和色温，且已确认成员均具备适配后的 RGB 契约，才能从成员功能派生 RGB 并构成扩展彩灯，不要求成员型号相同。派生 RGB 只向发送时已确认开启的成员调色，被跳过的成员不会在开灯后补发；一个成员受理不保证其他成员完成，状态以实际反馈为准。

终端使用 `list` 查询设备品类、功能标识、核心标记、能力、绑定、补丁与识别问题，再用 `on DID FUNCTION` 或 `off DID FUNCTION` 控制具体功能；单功能设备也须指定标识。核心标记只说明用途，可以有多项或没有，不决定控制目标或接入资格。`status DID` 还显示各功能的状态、路径和诊断，灯组会说明原生能力与成员派生的来源。中国大陆服务区的 OAuth 令牌与本地客户端证书独立维护。已准备账号可以先运行空网桥，再等待设备接入；云端可能仍依赖物理网关。

MiGate 在数据目录保存型号、实例 URN 与完整原始 MIoT 描述，以及已提交配置、身份和灯组关系；有效描述暂时没有可发布功能时也会保留，但不创建运行实例，可用 `list` 或 `status DID` 查看识别原因。每次启动会用当前内置规则和补丁重新识别缓存并重新派生灯组，在对外服务前尝试应用结果；正常缓存重算无需访问描述服务。结果等价时不增加配置版本，纯元数据变化不重建订阅或重启 Matter 服务，实际绑定或发布形状变化按需更新。

运行时米家目录中的型号或实例 URN 变化后，MiGate 获取匹配的完整描述并重新识别；新版描述按兼容扩展处理，假设已有功能、属性及其标识保持稳定。重新识别失败、描述获取失败或无效、配置保存失败时保留上次完整的配置、绑定、策略、组关系和订阅，显示原因并继续重试。已确认的设备移除与访问撤销仍生效，不会因缓存恢复；同一型号与 URN 下的静默修改不会主动检查，固件号变化本身也不触发获取。

每个可发布物理设备使用一个父端点和各可见功能的独立子端点，单功能设备也采用相同布局；有效电量由父端点共享。新增功能只分配新端点，已有功能的身份和端点保持不变。插座映射为 On/Off Plug-in Unit（`0x010A`，revision 3），窗帘映射为 Window Covering（`0x0202`，revision 6），风扇映射为 Fan（`0x002B`，revision 4）；灯光按能力采用 On/Off Light（`0x0100`）、Dimmable Light（`0x0101`）、Color Temperature Light（`0x010C`）或 Extended Color Light（`0x010D`），彩灯提供 CT + XY + HS（色相／饱和度）。测量和状态分别映射为 Temperature Sensor（`0x0302`）、Humidity Sensor（`0x0307`）、Air Quality Sensor（`0x002C`，提供 ppm 浓度，空气质量等级保持 Unknown）、Light Sensor（`0x0106`）、Contact Sensor（`0x0015`）或 Occupancy Sensor（`0x0107`）；共享电量使用 Power Source（`0x0011`）。OnOff/Level Lighting、场景与启动策略仍有标准必需项缺口，Level 保持 revision 6，与 Matter 1.6 的 revision 7 有差异；完整列表见[已知限制](docs/known-limitations.md)。Apple Home 的实际呈现、固件模式联动及基础兼容性在 `review` 阶段真机验收，不声称已通过 Matter 认证。

```sh
cargo run --locked -- auth login
cargo run --locked
```

仓库 Cargo 配置默认使用 `.migate/`；直接运行构建产物时目录规则不同，详见运行指南。

旧格式的数据目录不再受支持。请按[运行指南](docs/running.md#数据目录与重新配对)使用新目录并重新登录、配对；新目录会建立新的网桥配对身份，原目录保留。

- [运行与 Apple Home 配对指南](docs/running.md)
- [已知限制与恢复经验](docs/known-limitations.md)
- [起步方案](docs/specs/2026-09-08-virtual-light-bootstrap/spec.md)
- [米家认证方案](docs/specs/2026-09-11-xiaomi-authentication/spec.md)
- [最小设备模型方案](docs/specs/2026-10-03-minimal-device-model/spec.md)
- [多设备与生命周期方案](docs/specs/2026-10-04-multi-device-channel-bridge/spec.md)
- [云端接入与统一选路方案](docs/specs/2026-10-06-cloud-access-routing/spec.md)
- [单灯照明方案](docs/specs/2026-10-08-single-light-control/spec.md)
- [原生灯组与能力修复方案](docs/specs/2026-10-10-native-light-groups-and-capability-repair/spec.md)
- [结构整理方案](docs/specs/2026-10-19-architecture-restructuring/spec.md)
- [独立环境测量、CO₂ 与照度方案](docs/specs/2026-10-24-independent-measurements/spec.md)
- [门窗、运动与人体存在方案](docs/specs/2026-10-26-state-sensors/spec.md)
- [窗帘控制与状态同步方案](docs/specs/2026-10-28-curtain-control/spec.md)
- [风扇控制与状态同步方案](docs/specs/2026-10-29-fan-control/spec.md)
- [设备功能组合与内置适配补丁方案](docs/specs/2026-10-29-device-capability-composition/spec.md)

米家认证协议参考[小米官方 Home Assistant 集成](https://github.com/XiaoMi/ha_xiaomi_home)，使用该集成的 OAuth 应用配置。MiGate 是个人爱好项目，该授权归属于 HA 应用，不代表小米为 MiGate 提供独立授权或官方支持。参考来源与使用前提见[米家认证方案](docs/specs/2026-09-11-xiaomi-authentication/spec.md#认证方案与云端依赖)；注明来源不会扩大[上游许可证](https://github.com/XiaoMi/ha_xiaomi_home/blob/main/LICENSE.md)的授权范围。

后续继续覆盖开关、插座、其他传感器类别和空调，仅暴露可自然映射为标准 Matter 能力的功能。

## 代码组织

`mod.rs` 声明子模块并导出公共接口，具体实现按职责命名。模块内部实现保持私有，必要的共享成员仅对所在模块开放。

| 目录 | 职责 |
| --- | --- |
| `src/device/` | 有效逻辑模型、状态与通知、每个功能的命令顺序、动作与回执生命周期。 |
| `src/configuration/` | 纯配置计划、身份与端点分配、配置比较及已提交群组规则的派生。 |
| `src/app/` | 应用组装、生效配置、事务提交与激活、来源和 Matter 的运行生命周期。 |
| `src/lighting/` | 灯光轨迹、颜色数学、时间推进与原始动作进度。 |
| `src/matter/` | 显式发布形状、协议命令规划、单位换算、属性投影、版本与报告、配对。 |
| `src/storage/` | 完整配置的私有编码与事务、独立撤权记录、认证、桥接身份及协议数据存储。 |
| `src/xiaomi/auth/` | 认证服务、登录、检查与刷新、状态报告。 |
| `src/xiaomi/cloud/` | HTTP 请求、响应解析、云端目录、OAuth 与 MQTT 通知。 |
| `src/xiaomi/capability/`、`src/xiaomi/catalog/` | MIoT 能力解析与当前本地家庭的设备描述加载。 |
| `src/xiaomi/discovery/` | mDNS 浏览、公告解析与合并、网关路由选择。 |
| `src/xiaomi/central/session/` | 中枢握手、传输驱动、MQTT 处理、请求分派与业务响应。 |
| `src/xiaomi/mqtt/` | 两套驱动共用的帧、握手、发送进度、保活和入站 QoS2 机制。 |
| `src/xiaomi/adapter/` | 最新来源事实、不可变原生运行对象、路径与订阅、已提交群组的控制和反馈。 |

模块测试及辅助代码放在所属模块的 `tests/` 目录；公开接口、存储和进程集成测试位于根目录 `tests/`。

## 开发检查

```sh
cargo fmt --check
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
```

`cargo test` 使用临时目录、可控协议对端与可推进时钟，复用生产的设备与运行组装入口，覆盖本地和云端选路、灯具量化与接管、Matter 命令、属性、订阅及退出和重启。进程回归使用系统分配端口、macOS Bonjour 和 `dns-sd` 检查多个实例及 mDNS 公告；普通检查不访问个人账号、真实中枢或设备。
