# 运行 MiGate

当前支持 macOS / Apple Silicon，使用 Rust 1.96 构建。在仓库根目录运行：

```sh
cargo run --locked
# 或构建后直接执行
cargo build --locked --release
./target/release/migate --data-dir .migate-trial-1
```

一次只运行一个网桥进程；切换网桥数据目录前先 Ctrl-C 停止。独立认证命令不启动 Matter 服务，网桥启动时的凭据检查结束后，可以保持网桥运行并执行认证命令。同一数据目录的认证流程按顺序执行，不要重叠启动检查或登录。Mac 和控制端处于同一局域网，网络允许 Matter UDP 5540 与 mDNS 发现，并保持 Mac 唤醒。程序在前台运行，不安装常驻服务。

## 数据目录与身份

目录优先级：`--data-dir <PATH>`、运行时 `MIGATE_DATA_DIR`、`~/.migate/`。显式参数或环境变量中的相对路径按启动工作目录解析。空目录或不存在的目录会初始化新身份。数据库无法读取、必要数据缺失或协议数据无法解码时，程序报错退出并保留数据；存储错误会标明失败的操作、数据路径和相关协议键。

身份、配对凭据和协议元数据保存在目录内的 SQLite 数据库 `state.db` 中，每次写入或删除都会立即提交。米家令牌、OAuth 客户端标识、中枢客户端标识、私钥和证书保存为独立的单账号记录。目录权限为 `0700`，数据库权限为 `0600`，不额外导出私钥文件。虚拟灯的开关状态只保存在内存中。首次初始化要求目录为空；已有数据库按版本事务迁移，保留身份与 Matter 数据。必要表缺失、内容损坏或数据库版本过新时明确报错，不自动重建。

仓库 `.cargo/config.toml` 为 `cargo run` 和 `cargo run --release` 注入项目根目录下 `.migate/`；已有环境变量优先。直接执行二进制不读取 Cargo 配置，因此应显式传目录，或设置环境变量。启动日志显示最终路径；网桥模式还显示持久化网桥和子设备标识，以及固定的本地标识 `virtual-light-1`。Endpoint 0 为根端点，1 为 Aggregator，2 为虚拟灯，虚拟灯默认名称为 `MiGate Virtual Light`。

数据包含身份和配对凭据，请保留并避免提交或共享。仓库忽略 `.migate/` 与 `.migate-*/`。全新配对可使用新的顶层目录，例如 `cargo run --locked -- --data-dir .migate-trial-2`；新目录代表新的网桥实例，需要重新添加，原目录不会被修改。不要把新实例放在尚未初始化的 `.migate/` 内，否则该父目录会成为非空、不完整的数据目录。

## 米家账号认证

认证命令在 shell 中执行，形式为 `migate [--data-dir <PATH>] auth <login|check|logout>`，显式数据目录参数必须位于 `auth` 之前。每个数据目录只保存一个账号，当前仅支持中国大陆服务区 `cn`。

```sh
cargo run --locked -- auth login
cargo run --locked -- auth check
cargo run --locked -- auth logout

# 直接执行构建产物时，显式选择与网桥相同的数据目录
./target/release/migate --data-dir .migate auth login
./target/release/migate --data-dir .migate auth check
```

认证命令沿用上述数据目录优先级与初始化规则；`cargo run` 仍使用 Cargo 注入的项目目录。它们不启动 Matter 服务、配对窗口或虚拟灯终端。

首次登录步骤：

1. 执行 `auth login`，打开终端显示的授权链接，在浏览器中登录米家账号并授权 HA 应用。
2. 浏览器最终会跳转到 `http://homeassistant.local:8123/api/webhook/…`。MiGate 没有回调 HTTP 服务，页面可能显示无法访问；这时复制地址栏中包含 `code` 和 `state` 的完整最终网址。
3. 将完整网址粘贴回等待中的终端并回车。网址必须属于当前这次授权；只粘贴授权码、使用旧网址或缺少参数都会失败。MiGate 只解析该网址，不访问它。
4. 等待云端访问验证与中枢客户端证书准备完成。只有所有步骤成功并整体保存后，登录才成功。

授权码和本次随机状态只保留在内存中。登录过程中的 EOF、Ctrl-C、输入错误或云端及证书失败均不覆盖已有本地凭据，但不能保证旧授权在云端继续有效。同一账号重新登录复用已有中枢客户端标识与私钥；换账号时生成新身份，不复用旧账号证书。

`auth check` 与网桥启动时的一次异步检查复用相同流程。访问令牌在有效期经过 70% 时提前刷新；受保护接口明确返回 HTTP `401` 时，尚未刷新则刷新一次并重试失败的操作。设备读取和证书申请共用这一次刷新额度。有效刷新结果立即保存，后续步骤失败也不会丢失轮换后的刷新令牌。HTTP `403`、未知业务码、网络或解析错误均按对应操作失败报告，不直接判为令牌失效。

证书采用 Ed25519 客户端私钥和 CSR，只有 CSR 发送到云端。证书已过期或剩余有效期不超过三天时尝试续期，复用客户端标识与私钥；新证书必须通过身份、公钥和有效期校验后才替换旧证书。续期失败保留旧证书并报告原因。首次授权、令牌刷新、证书申请与续期依赖云端；本轮只在登录、启动和独立检查时更新，不设置周期任务。

云端认证和证书状态分别显示。证书日期采用 UTC RFC 3339，精确到秒：

| 状态 | 终端输出 |
| --- | --- |
| 未登录 | `Xiaomi: not signed in (cn).`，附使用实际可执行文件和数据目录的登录命令。 |
| 正在检查 | `Xiaomi: checking authentication (cn)...` |
| 云端认证成功 | `Xiaomi: authenticated (cn).` |
| 需要重新授权 | `Xiaomi: sign-in required (cn).` |
| 云端暂时无法确认 | `Xiaomi: authentication unavailable (cn).`，附失败操作和简短原因。 |
| 未准备证书 | `Gateway certificate: not prepared.` |
| 证书尚未生效 | `Gateway certificate: not valid before <UTC timestamp>.` |
| 证书有效 | `Gateway certificate: valid until <UTC timestamp>.`，进入续期窗口时附 `renewal due`。 |
| 证书已到期 | `Gateway certificate: expired at <UTC timestamp>.` |

网络不可用或 OAuth 需要重新登录时，本地证书仍按自身真实有效期显示；续期失败也不会清除旧证书。证书准备成功仅为后续中枢连接提供凭据，本轮不发现网关、不建立 MQTT 连接，不验证本地控制能力。云端只读取一页自有家庭与最多 150 个去重设备标识对应的一页设备结果，用于验证认证并取得账号 UID；空设备结果正常，无法取得自有家庭的 UID 时明确报错，不使用共享家庭所有者的身份。不会输出或持久化设备目录，也不创建 Matter endpoint。

检查全部成功才返回零状态；未登录、云端认证失败、证书操作失败或证书仍未生效、已到期均返回非零状态。每个 HTTP 请求超时为 30 秒；Ctrl-C 会取消当前流程，无须等待网络超时，已提交的有效令牌刷新结果保留。

`auth logout` 通过事务删除本地账号令牌、客户端标识、私钥和证书，无记录时同样成功。它不撤销远端 HA 应用授权，不删除数据库、网桥身份或 Apple Home 配对。认证记录内容损坏时，登录与检查报错并保留原记录，可显式执行 `auth logout` 清除后重新登录；数据库本身无法访问、结构损坏或版本不支持仍会报错。

同一数据目录的认证流程不应重叠执行，本轮不处理并发覆盖或跨进程凭据同步。独立命令更新凭据后，运行中的网桥仍展示本次启动检查得到的结果，重启后重新检查。

协议与公开 CA 材料参考小米官方 Home Assistant 集成的[OAuth 和云接口](https://github.com/XiaoMi/ha_xiaomi_home/blob/main/custom_components/xiaomi_home/miot/miot_cloud.py)、[客户端证书实现](https://github.com/XiaoMi/ha_xiaomi_home/blob/main/custom_components/xiaomi_home/miot/miot_storage.py)及[应用配置与公开 CA](https://github.com/XiaoMi/ha_xiaomi_home/blob/main/custom_components/xiaomi_home/miot/const.py)。MiGate 直接实现所需协议，不依赖运行 HA 或 Miloco。使用的 OAuth 应用标识 `2882303761520251711` 属于官方 HA 集成，授权关系归属于该应用；MiGate 作为个人爱好项目使用该配置，不代表小米为 MiGate 提供独立授权或官方支持。注明来源不会扩大[上游许可证](https://github.com/XiaoMi/ha_xiaomi_home/blob/main/LICENSE.md)的授权范围。

## 终端与退出

按行输入小写 `on`、`off`、`status`；忽略首尾空白和空行。未知命令或多余参数显示帮助，随后仍可继续输入。灯状态来自同一个虚拟设备，终端与 Matter 读写共享它。程序提示与日志使用英文。日志写标准错误，配对指引和终端结果写标准输出。日志级别固定为 info，不读取 `RUST_LOG`。

输入 EOF 只结束终端读取，日志提示桥接服务继续运行。Ctrl-C 停止服务；正常停止返回成功，输入输出、存储或服务故障报错并返回非零状态。每次启动灯都为 `off`，不会恢复之前的开关状态。标准 Lighting、Identify、Groups、Scenes 能力沿用协议栈；`StartUpOnOff` 只接受 Off，以保持固定的关闭启动行为。

启动时异步检查一次米家凭据，终端命令帮助展示最近的云端检查结果，并按当前时间计算证书状态。认证等待、失败或正常完成均不阻塞配对、灯控制和退出，也不会结束网桥运行；存储故障仍使应用报错退出。`on`、`off`、`status` 始终操作虚拟灯，认证命令须回到 shell 执行。

## 添加到 Apple Home

1. 在仓库根目录运行 `cargo run --locked -- --data-dir .migate-home`。首次使用该目录会创建身份，未配对时标准输出显示二维码和手动配对码；后续重启继续使用同一条命令。
2. 在同一局域网的 iPhone/iPad「家庭」App 添加配件，扫描二维码或输入程序显示的手动配对码。
3. 若提示未认证且提供继续入口，可确认添加；若拒绝添加，保留提示信息用于排查。
4. 添加后应只出现一盏桥接虚拟灯，初始关闭。

配对窗口为 15 分钟；超时后重启可重新打开，身份仍复用。使用固定 revision 的上游公开测试认证材料与参数：VID `0xFFF1`、PID `0x8001`、passcode `20202021`、discriminator `3840`。passcode 是生成配对码的参数，不是应直接输入家庭 App 的手动配对码；以程序输出为准。这些公开参数用于个人局域网验证，不代表正式认证产品。

`rs-matter` 固定为上游 revision `ca16b0ca3c3776272d5fa7aa3693501882ec41a3`，启用 `persistent-subscriptions`。重启后保留原订阅 ID，重新建立会话并恢复状态报告。

## 验证控制与重启

家庭 App 保持前台。状态报告恢复后，每次操作观察最多 30 秒；重启恢复使用下述单独的观察流程。

| 检查项 | 操作与预期结果 |
| --- | --- |
| 首次配对 | 新目录添加后只有一盏灯，初始关闭。 |
| 家庭 App 控制 | 分别开启、关闭后，终端 `status` 与最终操作一致。 |
| 设备侧同步 | 终端分别执行 `on`、`off`，家庭 App 显示对应状态且配对不变。 |
| 重启恢复 | 开启后 Ctrl-C，以同一目录重启；恢复已保存订阅的报告，无需重配、没有重复配件，已有灯先同步为关闭，再验证双向控制。 |
| 输入错误恢复 | 开灯后输入未知命令，状态保持；随后终端和家庭 App 都仍能控制。 |

已保存的有效订阅可由 MiGate 主动恢复报告，无需控制端重新订阅。恢复仍需要控制端可达并建立会话，网络发现和重试可能增加等待时间。家庭 App 已经能够控制灯，也不能说明反向状态报告已经恢复。

没有已保存的有效订阅，或控制端已替换对应订阅时，需要等待控制端重新订阅。新订阅建立后会保存当前 ID，供重启时复用。等待时间与最大报告间隔和重试调度有关，例如最大报告间隔为 600 秒时，观察 30 秒不足以判断失败。

重启恢复按以下步骤验证：

1. 确认家庭 App 与终端双向同步后，把灯开启，用 Ctrl-C 停止 MiGate，以同一数据目录重新启动并开始计时。
2. 保持 MiGate 连续运行、家庭 App 前台和网络连通，观察报告是否恢复。若未恢复，保留同一次运行继续观察，最多 20 分钟。
3. 确认家庭 App 中的灯同步为关闭。`Resumed persisted subscription` 表示记录已载入；`Subscription ... primed` 表示新订阅建立，恢复已有订阅时不必出现后者。两条日志均不能代替实际同步验证。
4. 再分别从家庭 App 和终端开关灯；每次操作使用 30 秒观察窗口。随后再重复一次重启。

若 20 分钟后仍未恢复，保留本次启动到超时的日志，区分没有可恢复的订阅、会话或订阅建立失败，以及变化未同步。20 分钟是排查的观察窗口，不是预期恢复耗时或协议承诺。

重启后短暂出现 `No valid session found, replying with SessionNotFound`，表示控制端仍在使用已失效的会话。保存订阅不会保存原会话，需结合会话恢复和实际状态报告判断结果。`Error processing subscription ... will retry` 表示某个订阅暂时无法报告，需按 fabric、node 区分订阅者，并结合网络发现和控制端在线情况排查。

订阅记录缺少必需字段或无法解码时，MiGate 明确报错并保留原始数据，不自动清空配对数据。

在线解除配对沿用 Matter Fabric 删除及持久化行为。最后一个 Fabric 删除后保留身份和当前灯状态，不自动打开新窗口；用同目录重启后重新配对。若在 MiGate 离线时从家庭 App 删除配件，本地可能仍保留配对关系，需选择全新数据目录重新开始。没有自动重置或额外管理命令；实例清理通过家庭 App 和数据目录显式维护。
