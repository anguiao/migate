# MiGate

MiGate 使用 Rust 和 `rs-matter` 构建 Matter Bridge，目标是将米家（Xiaomi/MIoT）设备映射为标准 Matter 设备，接入 Apple Home 等生态。

当前支持本机前台运行的一盏虚拟开关灯、Matter 桥接、终端控制和配对存储，以及中国大陆服务区的米家 OAuth 登录、令牌管理和中枢客户端证书准备。中枢连接、设备控制和真实设备的 Matter 映射属于后续阶段。

- [运行与 Apple Home 配对指南](docs/running.md)
- [起步方案](docs/specs/2026-09-08-virtual-light-bootstrap.md)
- [米家认证方案](docs/specs/2026-09-11-xiaomi-authentication.md)

米家认证协议参考[小米官方 Home Assistant 集成](https://github.com/XiaoMi/ha_xiaomi_home)，使用该集成的 OAuth 应用配置。MiGate 是个人爱好项目，该授权归属于 HA 应用，不代表小米为 MiGate 提供独立授权或官方支持。参考来源与使用前提见[米家认证方案](docs/specs/2026-09-11-xiaomi-authentication.md#认证方案与云端依赖)；注明来源不会扩大[上游许可证](https://github.com/XiaoMi/ha_xiaomi_home/blob/main/LICENSE.md)的授权范围。

后续优先覆盖灯、开关、插座、传感器、窗帘和空调，仅暴露可自然映射为标准 Matter 能力的功能。
