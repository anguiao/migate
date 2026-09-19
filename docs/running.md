# 运行 MiGate

MiGate 在本机前台运行，将当前局域网中经过归属确认的米家设备桥接到 Matter。当前开发和验证环境是 macOS / Apple Silicon，使用 Rust 1.96 和 Cargo 构建。

从旧虚拟灯版本切换时，请选择新的并列数据目录，重新登录米家并在 Apple Home 中配对。旧目录不会迁移到真实设备结构，也不要删除旧目录来创建新实例。

数据目录保存网桥身份、Apple Home 配对、米家凭据、家庭绑定、设备身份和最后确认状态。首次使用的目录必须不存在或为空，同一目录一次只运行一个网桥进程；`auth` 子命令是独立进程，应按下文先完成登录，再以前台网桥命令启动运行。

## 登录米家

目前只支持中国大陆服务区，每个数据目录保存一个账号。全局参数必须放在 `auth` 前面。

```sh
cargo run --locked -- --data-dir .migate-real auth login
```

1. 打开终端给出的授权链接并登录米家账号。
2. 浏览器跳转到 `http://homeassistant.local:8123/api/webhook/…` 后，复制地址栏中的完整网址；页面无法打开不影响复制。
3. 将包含本次 `code` 和 `state` 的网址粘贴回终端。
4. 等待认证和中枢证书状态完成。

```sh
cargo run --locked -- --data-dir .migate-real auth check
cargo run --locked -- --data-dir .migate-real auth logout
```

认证子命令不会启动 Matter、设备发现或真实设备运行时。`logout` 删除本地米家凭据，但保留家庭绑定、已发布 endpoint、Matter 身份和 Apple Home 配对；设备会显示不可用。重新登录同一账号并完成目录核对后，仍属于绑定家庭的设备会恢复原身份。登录另一账号后，不符合新账号和家庭范围的旧 endpoint 不再执行命令，并从有效拓扑移除；分配记录保留。

## 启动与配对

```sh
cargo run --locked -- --data-dir .migate-real
```

进程同时运行米家运行时、Matter 和终端。未登录、尚未发现中枢或暂时没有已发布设备时，Matter 仍提供根节点和 Aggregator。终端 EOF 只停止命令输入；Ctrl-C 会停止 Matter、发现、连接、命令、状态刷新和认证维护后退出。

Matter 默认绑定 UDP 5540。端口优先读取 `MIGATE_MATTER_PORT`；设为 `0` 时由系统分配，日志中的实际端口也是 mDNS 公告端口。数据目录优先使用命令行 `--data-dir`，其次为 `MIGATE_DATA_DIR`，最后为 `~/.migate/`。本仓库的 Cargo 配置在未显式指定目录时会覆盖为项目根目录的 `.migate/`。

尚未配对时，终端显示二维码和手动配对码，commissioning 窗口开放 900 秒。在「家庭」App 中添加配件并扫描二维码或输入配对码。当前仍使用开发测试认证材料；Apple Home 可能显示未认证配件提示。窗口过期后，以同一数据目录重启即可再次开窗，同时保留网桥身份和真实设备 endpoint；已经完成的 Apple Home 配对也会保留。

也可以构建后直接运行：

```sh
cargo build --locked --release
./target/release/migate --data-dir .migate-real
```

## 家庭绑定与通信路径

MiGate 读取账号自有家庭的完整目录，并用当前局域网中经过认证的米家中枢自动匹配和持久化目标家庭；没有家庭选择参数。首次接入的新设备必须取得本地证明：中枢子设备和灯组由认证中枢确认，IP 设备需要在本地接口发现并以 token 核对 DID。仅在云端目录出现的设备不会因此获得控制资格。

设备首次准入后，普通断网、休眠或连接重建不会撤销成员资格。控制路径按健康的中枢、设备局域网直连、小米云顺序选择。启动和重新登录后会完成目录校对；目录明确显示设备移出绑定家庭时停止新命令并移出有效拓扑。多台认证中枢报告冲突家庭时会暂停真实设备控制，`devices` 会明确显示冲突。

灯组和单灯拥有各自稳定 ID 和 endpoint；满足准入条件时会同时显示，不推断或隐藏灯组成员。

## 终端命令

先用 `devices` 查看稳定的公开功能 ID。命令不会隐式选择第一台设备。

```text
devices
status <id>
on <id>
off <id>
set <id> <property> <value>
action <id> <action>
refresh
help
```

`status <id>` 会列出该功能实际支持的命令、合法范围、步长和枚举：

- `brightness-percent`：百分比。
- `color-temperature-kelvin`：Kelvin。
- `color-rgb`：`R,G,B`，每个分量为 0–255。
- `target-temperature-celsius`：摄氏度。
- `position-percent`：0 表示全关，100 表示全开。
- `hvac-mode`、`fan-speed`、`swing-mode`、`oscillation`、`clean-mode`：只接受 `status` 列出的值。

动作包括设备确实支持时的 `curtain-stop`、`vacuum-start`、`vacuum-stop` 和 `vacuum-dock`。参数类型、范围、步长和枚举会在进入网络队列前验证；未知 ID、缺少参数或多余参数不会派发请求。`refresh` 进入与后台相同的合并发现、目录和状态刷新流程。

命令返回 `Accepted` 表示控制接口已经接受请求，仍需等待设备报告确认状态；`Ambiguous` 表示请求可能因超时或断线而没有明确结果，控制运行时不会自动补发。先查看 `status` 再决定是否重试。状态值标为 `Current` 时来自当前确认报告；`LastKnown` 是重启或断连前的缓存值，不代表当前值；`Unknown` 表示没有可确认的当前值。来源和更新时间随状态显示。具体路径、失败阶段和安全错误码写入 stderr 日志。

## 实现范围与验证边界

下表是当前实现目标，不是逐型号实机通过声明。

| 类型 | 目标型号 | 核心能力 |
| --- | --- | --- |
| 灯与灯组 | `yeelink.light.light3`、`yeelink.light.ml9`、`yeelink.light.spot2`、`xiaomi.light.ceil04`、`xiaomi.light.bar2`、`devcea.light.ls2307`、`lemesh.light.wy0c15`、`mijia.light.group3` | 开关、亮度、实际支持的色温／颜色 |
| 开关、插座、面板负载 | `xiaomi.switch.w3`、`zimi.switch.dhkg01`、`zimi.switch.dhkg02`、`zimi.switch.dhkg05`、`xiaomi.controller.86v1`、`cuco.plug.cp7pd`、`cuco.plug.v3`、`qmi.plug.psv3`、`zimi.plug.zncz01` | 各实际负载通道开关与状态 |
| 空调伴侣 | `lumi.acpartner.mcn02`、`lumi.acpartner.mcn04` | 开关、模式、目标温度、风速和实际支持的摆风 |
| 窗帘 | `xiaomi.curtain.acn010` | 开、关、停止、目标和当前位置 |
| 风扇 | `dmaker.fan.p5c` | 开关、离散风速、摇头 |
| 温湿度 | `miaomiaoce.sensor_ht.t2/t6/t8/t9`、`xiaomi.sensor_ht.mini` | 温度、湿度、电量 |
| 运动／存在／照度 | `xiaomi.motion.pir1`、`izq.sensor_occupy.trio`、`linp.sensor_occupy.hb01`、`xiaomi.sensor_occupy.03/p1` | 运动或整体存在、真实数值照度、电量 |
| 门窗 | `isa.magnet.dw2hl`、`linp.magnet.m1` | 开合状态、电量 |
| 扫地机器人 | `xiaomi.vacuum.c104` | 启停、回充、清扫类型、状态、故障、电量 |
| 浴霸 | `yeelink.bhf_light.v13` | 照明、送风、换气、取暖及目标温度，按功能拆分 |

不接入推窗器、无线按钮、旋钮、`yeelink.light.nl1` 夜灯，以及设计文档中列出的后续设备类别。配置项、能耗历史、地图、规则引擎和管理网页也不在本轮范围。

Matter 照明的启动配置属性没有可信的米家断电记忆映射，因此读取返回 unavailable、写入返回 unsupported；不会伪造设置或在重启时回放控制。自动化测试覆盖协议、存储和模拟边界；真实硬件响应、Apple Home 展示和订阅行为仍需在代表设备上验证。

## 排障

- `devices` 没有设备：确认已登录中国区账号，并让本机与自有中枢处于同一局域网。候选项会区分未发现、等待本地验证、不支持、无法识别和绑定家庭之外。
- 功能显示 unavailable：查看 `status <id>` 的关键状态是否仍为 `Unknown`，并查看 stderr 中的实际路径和失败阶段。暂时断网不会撤销已准入身份。
- 家庭冲突：停止真实控制，确认当前网络中认证中枢属于同一个账号自有家庭；本轮不提供手工选择家庭。
- 需要独立实例：使用新的并列目录和不同端口，例如 `.migate-real-2`；该实例需要重新登录和配对。

## 构建 profile

默认 `dev` profile 为项目代码保留行号信息。需要使用 LLDB/GDB 检查依赖变量时使用 `debugging`：

```sh
cargo build --locked --profile debugging
cargo run --locked --profile debugging -- --data-dir .migate-real
```

产物位于 `target/debugging/migate`，项目和依赖均启用完整调试信息。
