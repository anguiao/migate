# MiGate

MiGate 是一个使用 Rust 和 `rs-matter` 构建的 Matter Bridge，旨在将米家（Xiaomi/MIoT）设备映射为标准 Matter 设备，接入 Apple Home 等 Matter 生态。

> [!NOTE]
> 项目处于早期开发阶段，目前不可用。

## 目标

- 第一阶段以 Apple Home 为主要验证目标。
- 优先支持灯、开关、插座、传感器、窗帘和空调等标准设备。
- 仅暴露可自然映射到 Matter 的 MIoT 能力。
- 首个里程碑是跑通一盏米家灯到 Apple Home 的完整链路。
