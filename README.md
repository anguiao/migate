# MiGate

MiGate 是用 Rust 和 `rs-matter` 构建的米家 Matter Bridge。它读取中国大陆服务区的米家目录，在当前局域网中自动确认账号自有家庭和设备归属，将已经本地确认的真实设备能力映射到 Matter，并让 Apple Home 与终端共用同一份状态和控制队列。

当前实现覆盖灯与灯组、开关／插座／面板负载、空调伴侣、窗帘、风扇、温湿度／运动／存在／照度／门窗传感器、扫地机器人和浴霸的核心功能。目标型号和能力边界见[运行与配对指南](docs/running.md#实现范围与验证边界)。型号列表示本轮实现范围，不表示每个型号已经完成实机验证。

- [运行与 Apple Home 配对指南](docs/running.md)
- [项目结构与生命周期](docs/architecture.md)
- [真实设备桥接设计](docs/specs/2026-09-14-xiaomi-device-bridge.md)
- [米家认证设计](docs/specs/2026-09-11-xiaomi-authentication.md)

米家认证协议参考[小米官方 Home Assistant 集成](https://github.com/XiaoMi/ha_xiaomi_home)，使用该集成的 OAuth 应用配置。MiGate 是个人爱好项目，该授权归属于 HA 应用，不代表小米为 MiGate 提供独立授权或官方支持。参考来源与使用前提见[米家认证设计](docs/specs/2026-09-11-xiaomi-authentication.md#认证方案与云端依赖)；注明来源不会扩大[上游许可证](https://github.com/XiaoMi/ha_xiaomi_home/blob/main/LICENSE.md)的授权范围。

## 开发检查

```sh
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
cargo fmt --check
```

当前平台范围为 macOS 和 Linux，两边使用同一套构建、Clippy 和测试命令，CI 分别运行这些检查。系统依赖见[运行环境](docs/running.md#运行环境)。进程测试使用临时数据目录、回环网络和系统分配的端口，并通过本机 mDNS 浏览核对公告端口，不操作真实家庭设备。
