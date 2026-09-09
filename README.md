# MiGate

MiGate 使用 Rust 和 `rs-matter` 构建 Matter Bridge，目标是将米家（Xiaomi/MIoT）设备映射为标准 Matter 设备，接入 Apple Home 等生态。

当前已实现本机前台运行的一盏虚拟开关灯、Matter 桥接、终端控制和配对存储。真实 Apple Home 兼容性仍需真机验收；米家通信与真实设备接入属于后续阶段。

- [运行与 Apple Home 配对指南](docs/running.md)
- [起步方案](docs/specs/2026-09-08-virtual-light-bootstrap.md)

后续优先覆盖灯、开关、插座、传感器、窗帘和空调，仅暴露可自然映射为标准 Matter 能力的功能。
