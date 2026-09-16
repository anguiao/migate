# 运行 MiGate

当前支持 macOS / Apple Silicon，使用 Rust 1.96 和 Cargo 构建。MiGate 目前提供虚拟灯桥接和米家账号认证，尚未提供设备列表与真实米家设备桥接。

以下命令均在仓库根目录执行，并使用 `.migate/` 保存数据。继续使用已有实例时，请将命令中的 `.migate` 替换为原数据目录。

## 启动

```sh
cargo run --locked -- --data-dir .migate
```

程序在前台运行，使用 Ctrl-C 停止，同一数据目录一次只运行一个网桥进程。

Matter 默认使用 UDP 5540。可通过 `MIGATE_MATTER_PORT` 设置端口（`0`–`65535`），设为 `0` 时由系统分配空闲端口；启动日志显示实际端口。运行多个网桥时，分别使用独立数据目录和不同端口。

也可以构建后直接运行：

```sh
cargo build --locked --release
./target/release/migate --data-dir .migate
```

### 调试构建

默认开发构建仅为项目代码保留回溯所需的行号信息，第三方依赖不生成调试信息。需要使用 LLDB/GDB 检查变量或进入依赖源码时，使用 `debugging` profile：

```sh
cargo build --locked --profile debugging
```

产物位于 `target/debugging/migate`，项目和第三方依赖均启用完整调试信息。首次使用该 profile 会重新构建并额外占用磁盘空间。也可通过以下命令构建并启动：

```sh
cargo run --locked --profile debugging -- --data-dir .migate
```

## 米家账号认证

认证需要联网，目前仅支持中国大陆服务区（`cn`），每个数据目录保存一个账号。以下认证命令在 shell 中执行。

### 登录

```sh
cargo run --locked -- --data-dir .migate auth login
```

1. 打开终端显示的授权链接，在浏览器中登录米家账号并授权 HA 应用，无须安装或运行 Home Assistant。
2. 浏览器跳转到 `http://homeassistant.local:8123/api/webhook/…` 后，即使页面显示无法访问，也直接复制地址栏中的完整网址。
3. 将包含 `code` 和 `state` 的完整网址粘贴回等待中的终端并回车。使用本次授权得到的网址；若回调已失效，重新执行登录命令，从新的授权链接开始。
4. 等待终端显示 `Xiaomi: authenticated (cn).` 和 `Gateway certificate: valid until …`，即完成登录。

### 检查与退出登录

```sh
cargo run --locked -- --data-dir .migate auth check
```

检查会显示账号认证与证书状态，并在需要时刷新令牌和续期证书。两项均正常才表示检查成功；提示 `sign-in required` 时重新登录，网络故障时待网络恢复后重试。

网桥每次启动时也会检查一次，运行期间没有定时检查。同一数据目录的认证操作应依次执行，避免与网桥启动检查重叠；通过独立命令更新凭据后，重启网桥以更新其显示的认证状态。

退出登录：

```sh
cargo run --locked -- --data-dir .migate auth logout
```

该命令删除本地米家账号凭据，保留网桥身份与 Apple Home 配对，也不会撤销远端 HA 应用授权。

## 添加到 Apple Home

Mac 与 iPhone / iPad 需处于同一局域网，网络允许 Matter 使用的 UDP 端口（默认 5540）与 mDNS 发现，并保持 Mac 唤醒。虚拟灯配对与控制可以在未登录米家账号时使用。

1. 启动网桥；尚未配对时，终端会显示二维码和手动配对码。
2. 在「家庭」App 中添加配件，扫描二维码或输入终端显示的手动配对码。
3. 当前使用测试认证材料。如提示配件未认证且提供继续入口，可选择继续添加。
4. 添加完成后，会出现一盏名为 `MiGate Virtual Light` 的虚拟灯，可以在「家庭」App 中开关。

配对窗口为 15 分钟，超时后用 Ctrl-C 停止，再以同一数据目录启动即可重新打开。

## 日常操作与退出

在运行网桥的终端中，每行输入一个命令：

| 命令 | 操作 |
| --- | --- |
| `on` | 打开虚拟灯 |
| `off` | 关闭虚拟灯 |
| `status` | 查看虚拟灯状态 |
| `help` | 查看命令帮助与认证状态 |

终端与「家庭」App 控制同一盏灯，状态会相互同步。每次启动时灯都为关闭状态。

Ctrl-C 停止网桥。输入 EOF 只停止终端读取，网桥仍会继续运行。重启时使用同一数据目录，即可保留原配对关系。

## 数据目录与重新配对

数据目录保存网桥身份、Apple Home 配对和米家账号凭据，请妥善保留，避免提交或共享。首次使用的目录必须为空或尚不存在。

通过 `--data-dir <PATH>` 选择目录时，该参数须放在 `auth` 之前。未指定时依次使用 `MIGATE_DATA_DIR` 环境变量和 `~/.migate/`；仓库的 Cargo 配置会在未设置该环境变量时使用项目根目录的 `.migate/`。相对路径按命令执行目录解析，启动日志会显示实际使用的路径。

在线从「家庭」App 移除配件后，使用原数据目录重启，可重新配对。如果在网桥离线时移除，网桥可能仍保留配对关系，此时可用新目录重新开始：

```sh
cargo run --locked -- --data-dir .migate-trial-1
```

新目录代表独立实例，需要重新登录米家账号和添加到「家庭」App。请选择与原目录并列的新目录，原目录的数据会保留。
