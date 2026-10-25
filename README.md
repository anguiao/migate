# MiGate

MiGate 使用 Rust 和 `rs-matter` 构建 Matter Bridge，目标是将米家（Xiaomi/MIoT）设备映射为标准 Matter 设备，接入 Apple Home 等生态。

当前在 macOS / Apple Silicon 本机前台运行，首次通过局域网小米主中枢确定米家自有家庭，随后从云端目录接入受支持插座、单灯、米家原生灯组及环境测量能力，并按本地优先选择控制与通知路径。支持 `cuco.plug.v3`、`xiaomi.plug.04`、`zimi.plug.zncz01`、`qmi.plug.psv3` 的单路主电源，以及 `cuco.plug.cp7pd` 的开关 1、开关 2、USB 三路独立控制。灯具包含 `devcea.light.ls2307`、`yeelink.light.light3`、`yeelink.light.ml9`、`yeelink.light.spot2`、`xiaomi.light.ceil04`、`xiaomi.light.bar2`、`lemesh.light.wy0c15` 七个描述样本；测量以 `miaomiaoce.sensor_ht.t6`、`xiaomi.sensor_ht.new3pr` 及运动、存在、夜灯设备的公开描述为解析样例，分别识别环境温度、相对湿度、CO₂ 和照度，可单项接入，也可与同机已有控制功能组合。设备按真实能力准入，不按样本型号建立通用白名单。

提供基础开关、灯具亮度与色温、设备支持的 RGB、软件渐变与连续控制、只读环境温度、湿度、CO₂、照度与可选整机电量、主动通知、统一选路、断线恢复、运行中自动加入和删除，以及持久化身份、名称和配对。灯具统一为 `main`，选择色温或颜色时自动切换模式。终端保留定向 on/off 控制，用 `list` / `status` 查看统一灯具快照、动作结果、实际批属性与路径；无写入帧与动作受理分开显示。中国大陆服务区的 OAuth 令牌与本地客户端证书独立维护。已准备账号可以先运行空网桥，再等待设备接入。云端可能仍依赖物理网关，具体边界见已知限制。

每个有效原生灯组在 Apple Home 中显示为一盏普通灯，成员保留米家单控，其灯光控制入口在 Home 中隐藏；成员的测量及共享电量仍由原设备发布。原生功能使用组 DID，`devcea.light.ls2307` 成员的 RGB 与模式缺口由来源适配补齐；状态采用实际反馈。关系和能力变化自动应用，必要时短暂重启 Matter 服务，保留身份、端点与配对。

目录发现设备描述的新 URN／版本时，MiGate 自动加载受支持的新描述。加载或保存失败时继续使用上次完整配置；只有元数据变化时保留当前实例，实际功能或发布形状变化时整体更新。

单路插座在同一端点承载 Bridged Node 与 On/Off Plug-in Unit（`0x010A`，revision 3）；多路插座使用一个物理父节点和三个独立开关子节点，总控只触发内部状态刷新。灯具按真实能力采用 On/Off Light（`0x0100`）、Dimmable Light（`0x0101`）、Color Temperature Light（`0x010C`）或 Extended Color Light（`0x010D`）。含测量的设备统一使用一个物理父节点和各可见功能子节点：Temperature Sensor（`0x0302`）、Humidity Sensor（`0x0307`）、Air Quality Sensor（`0x002C`，提供 ppm 浓度，空气质量等级保持 Unknown）或 Light Sensor（`0x0106`，提供 lux 照度），单项测量也保留父节点；设备提供有效电量能力时，父节点额外发布 Power Source（`0x0011`）与电池 Power Source cluster。彩灯提供 CT + XY + HS（色相／饱和度）。OnOff/Level Lighting、场景与启动策略仍有标准必需项缺口，Level 保持 revision 6，与 Matter 1.6 的 revision 7 有差异；完整列表见[已知限制](docs/known-limitations.md)。Apple Home 的实际呈现、固件模式联动及基础兼容性在 `review` 阶段真机验收，不声称已通过 Matter 认证。

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
- [原生灯组与能力修复方案](docs/specs/2026-10-10-native-light-groups-and-capability-repair/spec.md)
- [结构整理方案](docs/specs/2026-10-19-architecture-restructuring/spec.md)
- [独立环境测量、CO₂ 与照度方案](docs/specs/2026-10-24-independent-measurements/spec.md)

米家认证协议参考[小米官方 Home Assistant 集成](https://github.com/XiaoMi/ha_xiaomi_home)，使用该集成的 OAuth 应用配置。MiGate 是个人爱好项目，该授权归属于 HA 应用，不代表小米为 MiGate 提供独立授权或官方支持。参考来源与使用前提见[米家认证方案](docs/specs/2026-09-11-xiaomi-authentication/spec.md#认证方案与云端依赖)；注明来源不会扩大[上游许可证](https://github.com/XiaoMi/ha_xiaomi_home/blob/main/LICENSE.md)的授权范围。

后续继续覆盖开关、插座、其他传感器类别、窗帘和空调，仅暴露可自然映射为标准 Matter 能力的功能。

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
