---
status: implemented
date: 2026-11-19
---

# Linux 原生运行与网桥开机自启

本次将 MiGate 的运行平台扩展到 64 位 Linux，面向树莓派和采用 Intel/AMD 处理器的小主机。首版以使用 systemd 和 Avahi 的 Debian/Ubuntu 系统为基准，ARM64 和 x86_64 分别在对应的 Linux 环境中从源码构建。Linux 主机和现有的 Apple Silicon Mac 应能在系统启动后自动运行网桥，让 Apple Home 通过原配对恢复设备控制。

## 局域网发现

MiGate 需要在局域网里完成两个方向的发现。

| 用途 | macOS | Linux |
| --- | --- | --- |
| 让 Apple Home 找到 MiGate | 通过 Bonjour 发布 Matter 服务，使用 `AstroMdns`。 | 通过系统 D-Bus 将 Matter 服务注册到 Avahi，使用 `AvahiMdns`。 |
| 让 MiGate 找到小米主中枢 | 通过 `mdns-sd` 浏览 `_miot-central._tcp.local.`。 | 使用相同的浏览和解析流程。 |

平台选择放在 Matter 公告的创建入口和 Cargo 目标依赖中。macOS 启用 `rs-matter` 的 `astro-dnssd`，Linux 启用 `zbus`。两个后端都发布协议栈根据配对窗口和已保存的 fabric 生成的记录，包括配对时的 `_matterc._udp` 和已配对网桥的 `_matter._tcp`。小米认证命令独立于 Matter 公告后端。[项目锁定版本的 mDNS 后端说明](https://github.com/project-chip/rs-matter/blob/c86233d310902503995738b3137884a3a7dc1764/README.md#build)

公告使用 Matter 套接字实际绑定的端口，包括 `MIGATE_MATTER_PORT=0` 时由系统分配的端口。同一进程因设备配置变化重建 Matter 实例时，复用公告对象和端口，按新实例的服务集合更新记录。端口绑定、系统 D-Bus 连接或 Avahi 注册失败时，网桥以非零状态退出，日志指出失败的操作。

## 从前台交给系统服务

在目标主机执行 `cargo build --locked --release`。首次部署时，推荐用普通用户和明确的数据目录在终端执行 `auth login`，前台启动网桥，在 iPhone「家庭」完成配对。已有配对的 Mac 可以直接选用原网桥的数据目录。

仓库提供一份 systemd 系统服务模板和一份 macOS LaunchDaemon 模板。用户在模板中指定运行网桥的普通用户、已构建程序的绝对路径，以及通过 `--data-dir` 传入的数据目录绝对路径。程序和数据目录应在系统启动时可访问。先停止前台进程，再启用服务，后台网桥由同一目录恢复米家凭据、设备端点和 Matter 配对。

- **Linux** 由系统级 systemd 在开机时拉起 MiGate，并安排 Avahi 先启动。网桥运行期间若 Avahi 单独重启，Avahi 就绪后应自动重新发布当前的 Matter 服务。允许 systemd 连带重启 MiGate，已配对的 Apple Home 用原配对恢复控制。通过 `systemctl` 查看服务状态，通过 `journalctl` 查看运行日志。
- **macOS** 由系统域的 LaunchDaemon 在桌面用户登录前启动 MiGate。通过 `launchctl` 查看服务状态，运行日志写到模板指定的路径。若冷启动停在 FileVault 解锁界面，需先解锁启动卷。[Apple 后台服务说明](https://developer.apple.com/library/archive/documentation/MacOSX/Conceptual/BPSystemStartup/Chapters/CreatingLaunchdJobs.html) · [FileVault 启动说明](https://support.apple.com/guide/security/volume-encryption-with-filevault-sec4c6dc1b6e/1/web/1)

服务在没有交互终端时持续运行，响应 `SIGTERM` 正常退出。后台网桥异常退出后，两端均由系统服务管理器自动重试，连续启动之间保留间隔，直至网桥恢复或管理员停服。

需要执行 `auth` 命令或在终端用 `list`、`status DID` 排障时，在 Linux 用 `systemctl stop` 停服，在 macOS 用 `launchctl bootout` 从系统域卸载服务。停服后以原用户和原目录操作，结束后分别用 `systemctl start` 或 `launchctl bootstrap` 恢复。临时停服期间网桥保持停止，下次开机仍按自启配置启动。前后台切换期间允许 Apple Home 短暂离线。

运行指南需给出两端从源码构建、填写模板、启停、查看日志和取消自启的步骤，并说明如何检查 Linux 的 Avahi 与局域网发现。项目简介和已知限制同步标明支持的平台与网络条件。[运行指南](../../running.md) · [项目简介](../../../README.md) · [已知限制](../../known-limitations.md)

## 验收条件

| 环境 | 应验证的行为 |
| --- | --- |
| Lima 的 Ubuntu ARM64（`ubuntu`） | 本机构建并运行自动化测试。在独立数据目录完成真实米家登录，发现小米主中枢，在 Apple Home 配对并控制一台真实设备，核对设备动作及状态回报。交给 systemd 后重启整台虚拟机，在登录 Linux 前用原配对再次控制同一台设备。 |
| Lima 的 Ubuntu x86_64（`ubuntu-amd64`） | 在虚拟机内完成 release 构建并运行同一套自动化测试。进程测试用受控设备源启动网桥，验证 Matter UDP 监听和 Avahi 公告。 |
| macOS / Apple Silicon | 在本机运行构建与自动化测试。从前台切换到 LaunchDaemon，验证系统启动后网桥自动使用原数据目录，Apple Home 可沿用原配对控制设备。 |

ARM64 网桥的数据目录位于虚拟机自己的磁盘，x86_64 的进程测试使用该机的临时目录。ARM64 家庭联调使用桥接到家庭局域网的网卡，供 iPhone 发现网桥、网桥发现小米主中枢，并实际传输 Matter UDP 数据。[Lima 网络说明](https://lima-vm.io/docs/config/network/)

macOS 和两种 Linux 架构的进程测试都要实际解析本次测试实例发布的服务名与端口，并和监听端口核对。现有测试对 macOS 工具 `/usr/bin/dns-sd` 及其输出格式的依赖需要适配 Linux。回归还覆盖关闭终端输入后继续服务、收到停止信号后释放数据目录，以及用原目录重启后恢复身份。

在 Linux ARM64 和 macOS 分别验证 systemd 与 LaunchDaemon 模板。后台网桥异常退出后应自动恢复服务。通过系统管理器手动停服后，经过重试间隔仍应能以原用户独占原数据目录。

在已配对的 ARM64 网桥运行期间单独重启 Avahi，验证能再次从局域网解析该网桥的 `_matter._tcp` 记录并用原配对控制设备。

## 后续计划

- **多终端管理。** 支持本机终端及 SSH 会话连接到运行中的网桥，让用户在故障发生时直接用 `list`、`status DID` 查看设备和路径。终端提交命令并接收结果，设备连接、实时状态和持久化写入由同一个网桥进程负责。通信方式与命令范围另行设计。
- **发布方式。** 准备对外发布时再决定交叉编译及预构建二进制的分发方式。
