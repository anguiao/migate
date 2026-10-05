# MiGate

MiGate 使用 Rust 和 `rs-matter` 构建 Matter Bridge，目标是将米家（Xiaomi/MIoT）设备映射为标准 Matter 设备，接入 Apple Home 等生态。

当前在 macOS / Apple Silicon 本机前台运行，通过一个局域网小米主中枢接入当前米家自有家庭的受支持插座，WiFi 与 BLE Mesh 设备共享连接。支持 `cuco.plug.v3`、`xiaomi.plug.04`、`zimi.plug.zncz01`、`qmi.plug.psv3` 的单路主电源，以及 `cuco.plug.cp7pd` 的开关 1、开关 2、USB 三路独立控制，不设置当前样本数量上限。

提供基础开关、主动通知、断线恢复、运行中自动加入和删除、定向终端控制，以及持久化身份、名称和配对。中国大陆服务区的 OAuth 登录、令牌管理与客户端证书准备沿用现有流程；云端成员同步与本地控制独立。已准备账号可以先运行空网桥，再等待设备接入。

单路设备在同一端点承载 Bridged Node 与 On/Off Plug-in Unit（`0x010A`，revision 3）；多路设备使用一个物理父节点和三个独立开关子节点，总控只触发内部状态刷新。当前保留基础 On/Off、Reachable 与 Identify/Groups，缺少 Lighting、计时关闭、全局场景及 Scenes Management，存在已知符合性限制。Apple Home 的实际呈现与基础兼容性在 `review` 阶段真机验收，不声称已通过 Matter 认证。

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

米家认证协议参考[小米官方 Home Assistant 集成](https://github.com/XiaoMi/ha_xiaomi_home)，使用该集成的 OAuth 应用配置。MiGate 是个人爱好项目，该授权归属于 HA 应用，不代表小米为 MiGate 提供独立授权或官方支持。参考来源与使用前提见[米家认证方案](docs/specs/2026-09-11-xiaomi-authentication/spec.md#认证方案与云端依赖)；注明来源不会扩大[上游许可证](https://github.com/XiaoMi/ha_xiaomi_home/blob/main/LICENSE.md)的授权范围。

后续优先覆盖灯、开关、插座、传感器、窗帘和空调，仅暴露可自然映射为标准 Matter 能力的功能。

## 代码组织

`mod.rs` 声明子模块并导出公共接口，具体实现按职责命名。模块内部实现保持私有，必要的共享成员仅对所在模块开放。

| 目录 | 职责 |
| --- | --- |
| `src/device/` | 通用设备模型、共享状态与通知、命令队列及其生命周期。 |
| `src/runtime/` | 网桥启动与任务生命周期、进程退出信号。 |
| `src/matter/` | Matter 服务、端点与处理器、配对及协议存储适配。 |
| `src/storage/` | SQLite 连接与迁移、目录和进程锁、各类持久化记录。 |
| `src/xiaomi/auth/` | 认证服务、登录、检查与刷新、状态报告。 |
| `src/xiaomi/cloud/` | HTTP 请求、响应解析、云端目录与 OAuth 协议。 |
| `src/xiaomi/capability/`、`src/xiaomi/catalog/` | MIoT 能力解析与当前本地家庭的设备描述加载。 |
| `src/xiaomi/discovery/` | mDNS 浏览、公告解析与合并、网关路由选择。 |
| `src/xiaomi/central/session/` | 中枢握手、传输驱动、MQTT 处理、请求分派与业务响应。 |
| `src/xiaomi/source/` | 启动准备、云端恢复、本地会话以及统一设备状态的协调。 |

模块测试随所属模块组织；跨模块、存储和进程集成测试位于 `tests/`。

## 开发检查

```sh
cargo fmt --check
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
```

`cargo test` 使用临时目录、可控协议对端与测试专用 source，复用生产的设备与运行组装入口，覆盖 Matter 命令、属性、订阅及退出和重启。进程回归使用系统分配端口、macOS Bonjour 和 `dns-sd` 检查多个实例及 mDNS 公告；普通检查不访问个人账号、真实中枢或插座。
