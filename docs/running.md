# 运行 MiGate

当前运行范围为 macOS / Apple Silicon，需要 Rust 1.96 和 Cargo；Linux 部署暂缓。仅支持中国大陆服务区，一次运行接入一个米家自有家庭。首次绑定时，Mac 与该家庭的小米主中枢须处于同一局域网。

## 启动

在仓库根目录执行：

```sh
cargo run --locked -- auth login
cargo run --locked
```

首次登录按下方「米家账号」完成授权，之后日常启动只需第二条命令。仓库的 Cargo 配置默认使用项目下的 `.migate/` 保存数据。日常重启继续使用同一目录，保留账号、设备名称和 Apple Home 配对。

程序在前台运行，使用 Ctrl-C 或 SIGTERM 停止；终端输入结束（EOF）不会停止服务。同一数据目录只能由一个进程使用，执行认证命令前也需先停止网桥。

也可以构建后直接运行：

```sh
cargo build --locked --release
./target/release/migate --data-dir .migate
```

直接运行构建产物不会读取仓库的 Cargo 环境配置，请明确指定原数据目录。

## 米家账号

以下命令在 shell 中执行。登录和账号检查需要联网。

### 登录

```sh
cargo run --locked -- auth login
```

1. 打开终端显示的授权链接，登录米家账号并授权 HA 应用，无须安装或运行 Home Assistant。
2. 浏览器跳转到 `http://homeassistant.local:8123/api/webhook/…` 后，即使页面显示无法访问，也复制地址栏中的完整网址。
3. 将完整网址粘贴回等待中的终端并回车。若授权已失效，重新执行登录命令，从新的授权链接开始。
4. 终端显示 `Xiaomi: authenticated (cn).` 和 `Gateway certificate: valid until …` 后，登录完成。

### 检查与退出登录

```sh
cargo run --locked -- auth check
```

检查会显示账号与证书状态，并在需要时自动更新。要恢复云端同步而提示 `sign-in required` 时，重新执行 `auth login`，再使用原数据目录启动；网络故障时待恢复后重试。已经确认并保存直连凭据的设备，可在云端登录或中枢证书过期时按下文离线恢复直连。

退出登录：

```sh
cargo run --locked -- auth logout
```

该命令删除本地米家账号及已保存的设备直连凭据，保留 Apple Home 配对，也不会撤销远端 HA 应用授权。

## 添加到 Apple Home

Mac 与 iPhone / iPad 须处于同一局域网，并保持 Mac 唤醒。网络需允许 mDNS 发现和 Matter UDP 通信，默认端口为 5540。

1. 启动 MiGate，等待终端显示二维码和手动配对码。暂无受支持设备时，也可以先添加网桥。
2. 在「家庭」App 中添加配件，扫描二维码或输入手动配对码。
3. 当前尚未通过 Matter 认证。如提示配件未认证且提供继续入口，可选择继续添加。
4. 完成 App 中的房间与名称配置。

配对窗口为 15 分钟。超时后用 Ctrl-C 停止，再以同一数据目录启动即可重新打开。

设备初始名称来自米家，之后可在 Apple Home 中独立改名。多路插座可能显示同名或默认编号，请逐路操作、核对实际通道后分别改名，不要根据显示顺序判断通道。

## 设备接入

MiGate 自动接入受支持的插座、灯光、米家原生灯组、窗帘、风扇、空调，以及只读环境温度、相对湿度、CO₂、照度、门窗开合、运动检测和整机人体存在状态。同一设备可接入多个控制或同类功能，以及测量和状态组合；共享电量按设备能力提供。插座提供开关，灯光按能力提供亮度、色温和彩色控制，窗帘提供位置控制，风扇提供开关、风速及设备支持的水平摇头，空调提供开关、模式、温度、风速及设备支持的上下摆风。用 `list` 查看实际功能和识别问题，代表型号见 [README](../README.md)，功能边界见[已知限制](known-limitations.md)。

在米家加入、移出设备或调整灯组后，等待自动同步，必要时等待下一次五分钟同步；同步需要云端可用。断电、断网、改名或换房间不会删除设备。重启或重新加入时保留 MiGate 已保存的名称，米家后续改名不会覆盖；移出后重新加入，Home 中的房间、场景或自动化可能需要重新设置。

使用受支持的数据目录启动新版 MiGate 后，程序会按当前规则重新识别已有设备和灯组；用 `list` 或 `status DID` 查看结果。运行时型号或实例 URN 变化后，等待云端同步并检查更新结果；固件号变化本身不会触发重新获取描述，详见[已知限制](known-limitations.md#home-显示与恢复)。

### 直连与路径

首次仍按米家账号与家庭接入流程取得目录中的设备候选。当前 IP 直连只支持 `dmaker.fan.p5c` 和 `lumi.acpartner.mcn02`；设备须属于当前家庭，且目录提供有效地址和 token。程序验证设备身份及 token 并完成只读确认后，才使用和保存直连凭据。局域网发现只提供地址线索，不单独授予访问资格。BLE 和代理子设备继续通过现有中枢或云端路径接入。

保留同一数据目录后，已确认资格及凭据的设备可在外网不可用、云端 OAuth 令牌或中枢证书过期时重启并重新认证直连，不必等待云端登录刷新或中枢上线。已知的访问撤销、设备移除、退出账号及家庭范围变化仍会生效；正常目录同步可提供更新的地址或 token，新候选通过验证后才替换已保存记录，无效候选不会覆盖有效记录。地址变化或连接断开时等待该设备自动发现和恢复，其他设备各自恢复；健康闲置时不会持续扫描。

控制与读取正常按中枢（`hub`）→ 直连（`direct`）→ 云端（`cloud`）选择可用路径。中枢业务降级时优先使用直连和云端；两者不可用时用户控制仍可按原有规则尝试合格的中枢，自动读取等待原有恢复调度。中枢恢复后重新获得优先级。通知独立选择可用中枢或云端；不要把直连读取成功当作通知已就绪。

用 `status DID` 查看直连协议或映射、地址、身份是否确认、可用候选、选中的读取和控制路径、通知来源、恢复进度及最近命令的实际路径。正常同步或连接变化后等待自动恢复；若提示凭据无效，检查账号和目录同步，等待连接信息更新。命令超时的结果可能不确定，先核对设备实际状态，程序不会自动重放已经发出或可能发出的控制。

对有可读反馈的控制功能，整次命令的步骤处理结束且至少一步明确受理时，若有直连受理或当前没有可用通知，会安排反馈刷新；即使后续步骤失败也适用。读取复用既有队列，失败后有限重试；无读取路径时等待恢复。用 `status DID` 查看功能读取诊断，控制回执不等待刷新，也不重放控制。灯光、灯组写后和渐变帧回读继续保留。旧 miIO 映射作为当前直连读取路径时，每 30 秒读取一次状态；其他路径不增加周期轮询。刷新只取得快照，外部遥控器或米家的后续变化仍可能不同步。

### 功能操作

空调在同一端点声明温控器和风扇类型，Home 中的入口与图标仍需真机核对。保留原数据目录可继续使用已有的设备身份、名称、端点和配对。

可接入的灯组在 Home 中显示为一盏灯，已唯一匹配的成员灯光入口隐藏，仍可在米家单独控制，也可在终端用 `on DID FUNCTION`、`off DID FUNCTION` 控制；成员的其他控制、测量和状态继续发布，有其他可见功能时，如有共享电量也随父端点保留。成员有多个灯光而无法确定归属时，不会任意隐藏，独立可用的原生组功能仍可接入。用 `status` 查看组的原生能力、成员派生的 RGB、状态来源及成员关系问题。组解散后，受支持且不属于其他已发布组的成员恢复独立入口；Home 的具体显示与分组以客户端为准。

窗帘可在 Home 中设置位置；是否提供独立停止入口取决于客户端。若 Home 没有停止入口，可使用标准 Matter 客户端发送 `StopMotion`。实际位置、设备目标和运动状态以设备报告为准；`status DID` 显示各项的来源时间、接收时间、读取与通知诊断，以及最近命令和回执。首次读取前为未知，读取或连接中断时保留最近有效值；启动和恢复时读取，运行时依靠设备通知及上述控制后刷新。补读可能只读到中途位置和运行状态，后续没有有效读取或通知时会保留该快照，不推断动作完成或自动追读。重启或恢复后不重放之前的目标或停止命令。

空调可选择制冷、制热、自动、送风和除湿，以及自动／低／中／高风速；温度按整度设置，上下摆风只在设备支持时提供。独立开关保留设备记忆的模式、温度和风速；切换模式或设置非零及自动风速时会在必要时先开机。单独调温不会开机或切换模式，风速设为零会关机，上下摆风只调整摆风。可设值取决于当前控制范围；例如 mcn04 的代表配码在自动模式下不允许调温，实际配码请以 `status DID` 为准。终端对空调只提供开关控制；模式、温度、风速和摆风由支持对应功能的 Matter 客户端操作。

Home 选择“自动”用于切换设备的原生自动模式。若当前配码不允许自动模式调温，拖动 Home 显示的温度范围会被拒绝，界面可能回到原值；需要调温时先选择允许调温的模式，再设置整数温度。

用 `status DID` 查看空调的 `Power`、`Mode`、`Target temperature`、`Speed`、可选 `Vertical swing`、各模式控制范围、读取与通知来源及最近命令。mcn04 的 `Control range source` 显示范围来源和诊断；显示 `Control range is awaiting source confirmation` 时，等待来源恢复并重新确认，依赖范围的模式、温度和风速设置暂不可用，独立开关与支持的上下摆风仍按各自命令处理。mcn04 状态以伴侣整包反馈为准；mcn02 直连使用其旧 miIO 映射，其他符合契约的标准空调以设备属性反馈为准，mcn04 的状态及范围补丁不用于 mcn02。命令受理不代表状态已更新。首次读取前状态未知，来源中断保留最近有效值，恢复后重新读取。自动风下若显示 `Current fan level: unknown (automatic speed)`，表示设备未提供实际挡位；恢复或重启不重放之前的空调命令。

空调开机并选择自动风时，Home 的风速数字使用 `33%`（或 1 挡）作为显示占位，使控制界面保持可操作；实际模式仍为 Auto，终端显示当前风速未知是预期行为。不要用这个占位数字判断实际出风强弱或配置依赖真实风速的自动化。切换普通风后显示设备报告的设定，关机后显示零。

为指定空调手动绑定一个已接入的环境温度功能。先用 `list` 查找空调和传感器的 DID 与功能标识，再在运行 MiGate 的终端输入：

```text
bind-sensor 234567890 air-conditioner:3 345678901 temperature:3:7
status 234567890
```

以上标识仅为示例，请替换为 `list` 中的实际值。一个空调功能最多绑定一个温度功能，同一个温度功能可供多个空调使用；再次执行 `bind-sensor` 并指定另一来源即可更换。解除绑定时执行 `unbind-sensor 234567890 air-conditioner:3`。两端须属于当前家庭且具备访问资格，传感器暂时离线也可绑定；环境温度仅供显示，不参与空调控制，独立传感器继续展示。绑定随数据目录保存，重启后等待温度来源重新取得读数；切换米家账号或家庭会清空绑定。

`status 空调DID` 中的 `Target temperature` 是空调设定温度，`Ambient source` 和 `Ambient temperature` 是绑定的来源及环境温度，并显示来源时间和接收时间。未绑定、尚待有效读数、来源不可用或来源不在目录时，环境温度显示 `unknown` 及原因。若来源失效，`Last reading (diagnostic only)` 是仅供排障的旧读数；查看 `Ambient source synchronization` 以及传感器的 `status DID`，核对读取和通知诊断。若显示 `Matter mapping unsupported: native ambient temperature cannot be represented`，来源读数无法用 Matter 温度表示，原始读数仍保留，Matter 返回空值。恢复访问后等待新的有效读数；正常静默及单次同步失败不会自动丢弃已有有效读数。来源移除或撤权时绑定仍保留，也可直接按空调标识解除。

Home 在没有当前有效环境温度时，仍可能显示 `0℃` 或保留旧温度。用 `status 空调DID` 的 `Ambient temperature` 判断读数是否有效；出现 `unknown` 时按原因排查绑定及来源，不将 Home 的数值当作当前有效温度。

风扇的百分比风速映射到设备实际可达的挡位；满足识别条件的普通数值范围（含步长）或有序离散挡位可接入，不表示设备支持连续调速。`0%` 表示关机，非零调速会先确保开机。水平摇头只调整摇头，不改变电源或风速。Home 的具体风速和摇头入口以及何时发送写入，以客户端实际提供的控件为准。

已支持的 `dmaker` 风扇自然风在米家中设置；自然风运行时显示的是设备记忆的直吹风速设定，不是自然风瞬时风速。Home 或其他 Matter 客户端发送非零风速会切回直吹，即使写入的百分比与当前显示值相同也会接管。独立开机保留设备记忆的模式和风速；关机或设置 `0%` 保留原生模式和直吹设定，设备确认关机后显示关闭及零风速。

用 `list` 和 `status DID` 查看风扇状态与诊断；`status DID` 可查看开关、风速、设备支持的摇头、最近命令结果及读取和通知来源，适用时还显示原生模式和直吹设定。各项首次取得有效值前为未知，单次读取失败或来源断连时保留最近有效状态，不将未知或断连推断为关机。恢复时重新获取状态，恢复或重启均不重放之前的开关、调速或摇头命令。

各项测量、状态与电量分别更新，首次取得有效值前显示未知，正常静默时保留最近观测，重启后重新获取。用 `status DID` 查看读数或状态、来源时间、接收时间、当前读取路径、通知来源和诊断。门窗、运动与存在功能还显示参与判定的原生属性值或事件标识、解释后的状态、最近观测来源，以及各属性和事件绑定的订阅状态。读取成功不代表通知同步已就绪；运动同步受限时，分别检查事件订阅和无移动时长属性订阅。单项读取或订阅失败不影响其他功能，电量失败也不影响测量与状态。更新依靠首次及恢复读取和上游通知，不做定时轮询；Home 是否显示电量及如何分组，以客户端为准。

设备或能力调整后，Home 入口与状态恢复可能需要数分钟，无须重新配对。新增功能使用新端点，已有功能的端点保持不变。用 `status` 查看配置是否待应用及最近失败；若持续显示旧入口或旧状态，可尝试重新打开「家庭」App。详情见[已知限制](known-limitations.md#home-显示与恢复)。

## 日常操作

在运行 MiGate 的终端中，每行输入一个命令：

| 命令 | 操作 |
| --- | --- |
| `list` | 查看设备、DID、功能标识、能力及识别和发布信息 |
| `status [DID]` | 查看账号、中枢、设备及各功能读数、状态、时间、当前路径和诊断 |
| `on DID FUNCTION` | 开启指定功能 |
| `off DID FUNCTION` | 关闭指定功能 |
| `bind-sensor TARGET_DID TARGET_FUNCTION SENSOR_DID SENSOR_FUNCTION` | 为空调绑定或更换环境温度来源 |
| `unbind-sensor TARGET_DID TARGET_FUNCTION` | 解除空调的环境温度绑定 |
| `help` | 查看命令帮助 |

先用 `list` 查找 DID 和功能标识，单功能设备也必须指定功能。控制功能以服务为锚点，例如 `light:2`、`fan:3`、`air-conditioner:3`、`outlet:9`；测量和状态功能以属性为锚点，例如 `temperature:3:7`。以下 DID 和功能标识均为示例，请替换为实际值：

```text
list
status 123456789
on 123456789 light:2
off 123456789 light:2
on 987654321 outlet:9
off 987654321 fan:3
status 234567890
on 234567890 air-conditioner:3
off 234567890 air-conditioner:3
bind-sensor 234567890 air-conditioner:3 345678901 temperature:3:7
status 234567890
unbind-sensor 234567890 air-conditioner:3
```

`list` 和 `status DID` 显示设备品类、功能能力、绑定与生效补丁。`core` 是用途说明，可有多个或没有，不影响功能接入和控制；必须明确指定控制目标。`Recognition issue` 提示候选功能、可选能力或补丁的识别问题，需与已接入功能的连接、读取和通知故障区分。灯组的 `Native capabilities`、`Repaired` 和 `State source` 分别说明原生能力、成员派生的增强和状态来源，`Member relation` 提示成员定位问题。

终端输出使用英文，设备名称原样显示。`unknown` 表示尚无确定状态，可能是尚未取得有效输入，也可能是设备明确上报未知。`on`／`off` 可控制受支持的插座、灯光、风扇和空调开关；窗帘、测量与状态功能不支持 `on`／`off`，同机其他受支持的控制功能仍可操作。`upstream time` 是来源提供的时间，缺失时为 `unknown`；`received at` 是 MiGate 收到数据的时间。门窗、运动与存在功能显示的最近观测来源与时间仍对应保留状态，来源切换后可能与当前读取路径或通知来源不同。

`command accepted` 表示请求已受理，不保证物理动作或灯组所有成员均已完成。命令超时后也可能已经执行，请先核对设备状态，MiGate 不会自动重放。

## 常见问题

| 现象 | 处理方式 |
| --- | --- |
| 没有发现设备 | 用 `list` 查看原因，确认账号、米家家庭及云端连接；首次绑定还需 Mac 与中枢处于同一局域网。 |
| 设备不可用或主中枢切换后未恢复 | 检查供电、中枢和网络，用 `status DID` 查看路径与请求结果，等待自动恢复；持续异常时参见[已知限制](known-limitations.md)。 |
| 某项测量长期不更新 | 用 `status DID` 检查该功能的通知来源及同步受限原因，再核对米家读数和实际网关。只有读取路径不能保证自动推送；正常静默不会触发轮询。 |
| 门窗、运动或存在显示 `unknown` | 用 `status DID` 区分首次读取失败、设备明确上报未知和订阅问题，核对米家状态并等待有效读取或通知；不要将未知当作关闭或无人。 |
| 状态保持旧值或显示同步受限 | 用 `status DID` 检查该功能的读取路径、通知来源，以及各属性和事件订阅；来源中断时保留最近观测，正常静默不触发轮询。 |
| 运动长时间保持检测到移动 | 用 `status DID` 检查最近输入是运动事件还是原生无移动时长，并检查时长属性订阅，再核对米家状态。恢复由设备原生报告决定，MiGate 不自行倒计时。 |
| 没有电量或电量不更新 | 用 `status DID` 查看电量能力及错误，核对米家读数。并非所有设备都提供可自动更新的电量；失败时保留已有读数，不影响其他测量与状态。 |
| 提示 Matter 映射不支持 | 用 `status DID` 查看对应功能的原生读数和映射诊断；MiGate 保留原生值，无法表示为协议数值的项目不能正常发布读数。 |
| 云端暂时不可用 | 等待网络或服务恢复。中枢或已确认的设备直连可用时，已有设备仍可使用；新设备接入与移除同步可能延迟。 |
| 直连不可用或地址变化 | 用 `status DID` 检查协议、身份、地址、候选和恢复进度，确认设备供电及局域网连接；等待自动重新发现。凭据无效时检查账号和目录同步，等待地址或 token 更新。其他路径可按各自资格继续使用。 |
| 窗帘位置或运动状态不更新 | 用 `status DID` 分别检查当前位置、目标、运动状态的读取和通知诊断，核对米家状态；读取成功不代表通知已就绪。来源恢复后等待重新读取与通知。 |
| 窗帘停止或目标命令报错 | 查看 `status DID` 中的请求结果并核对实际窗帘；回执超时后命令仍可能执行，MiGate 不会重放。 |
| 风扇状态未更新或调速报错 | 用 `status DID` 检查开关、风速、适用的模式与直吹设定及读取和通知诊断，再核对米家与实际设备。请求受理不代表状态已经回传；超时后命令仍可能执行，MiGate 不会重放。 |
| 空调环境温度显示 `unknown` | 用 `status 空调DID` 确认绑定对象与未知原因，再查看来源设备的 `status DID` 和同步诊断。等待来源恢复并取得新的有效读数；仅供诊断的旧读数不代表当前温度。 |
| 空调状态未更新或设置报错 | 用 `status DID` 检查当前模式、各模式控制范围、读取及通知诊断和最近命令，再核对米家与实际设备。范围未知或等待来源确认时检查连接并等待恢复；已列出的范围可能不允许当前模式下调温或所选风速。受理或超时都不能代替反馈，超时后先核对状态，MiGate 不会重放。 |
| 功能未接入或设备不可用 | 用 `list` 或 `status DID` 查看识别问题和实际功能；没有可发布功能时会显示 `not published` 和原因。已接入但不可用时检查连接、路径、读取与通知诊断。 |
| 灯组未出现或成员仍独立显示 | 用 `status DID` 检查组和成员的接入原因及 `Member relation`，等待云端同步；多个灯光无法定位时不会任意隐藏。 |
| 灯组状态延迟或未知 | 查看组及成员的 `status DID`，确认成员可用且通知正常；组内各成员的状态可能不同。 |
| 灯组关闭的成员没有跟随派生 RGB 颜色 | 成员派生的 RGB 只影响发送时确认开启的成员；先开灯，再重新选择颜色。 |
| 米家调整成员后又被组渐变覆盖 | 先在 Home 停止或替换组动作，再调整成员。 |
| 描述获取或配置保存失败 | 用 `list`、`status` 和错误日志查看原因，检查网络、数据目录权限与可用空间；程序保留上次配置并继续重试。 |
| 启动后提示缓存识别失败 | 用 `list` 或 `status DID` 查看原因并保留原数据目录。该设备继续使用旧配置，其他设备仍可更新；等待目录提供新描述，或更新 MiGate 后重启。相同输入不会定时重试。 |
| 启动失败 | 检查错误日志、数据目录权限、可用空间、端口与网络；修复后用原目录启动，无须清空数据。schema-10 开发数据按下节停机备份后升级，其他旧格式按下节处理。 |
| 日志提示忽略了无法加载的 Matter 数据 | 保留数据目录，等待 Home 恢复订阅；若重新出现配对窗口，按下节使用原目录重新配对。恢复范围见[已知限制](known-limitations.md#home-显示与恢复)。 |
| 提示需要重新登录 | 停止网桥，执行 `auth login`，然后用原目录重启。 |
| 提示数据目录正在使用 | 停止使用该目录的另一个 MiGate 进程，再重试；不要删除锁文件。 |
| Apple Home 无法发现网桥 | 确认配对窗口仍有效、Mac 未休眠，且网络允许 mDNS 和 Matter UDP 通信。 |

需要详细日志时：

```sh
RUST_LOG=info,migate=debug cargo run --locked
```

日志写入标准错误，终端命令结果和配对信息写入标准输出。

若默认端口被占用，可通过 `MIGATE_MATTER_PORT` 设置其他端口；设为 `0` 时由系统分配，启动日志会显示实际端口。运行多个网桥时，分别使用独立数据目录和不同端口。

## 数据目录与重新配对

数据目录包含账号凭据、证书、设备名称与配对信息，请妥善保留，避免提交或共享。首次使用的目录须为空或尚不存在。

目录选择顺序为：

1. `--data-dir <PATH>`，该参数须放在 `auth` 之前。
2. `MIGATE_DATA_DIR` 环境变量；仓库的 Cargo 配置默认将其设为项目下的 `.migate/`。
3. `~/.migate/`。

相对路径按命令执行目录解析，启动日志会显示实际路径。异常退出后可直接使用原目录重启，无须删除 `instance.lock`。重启会恢复设备配置并重新识别已有设备，再同步目录和实际状态，不继续执行上次未完成的灯光动作，也不重放窗帘、风扇或空调命令。

### 更早的 schema-10 环境温度配置调整

从空调环境温度增强之前的开发版本升级时，若原 `state.db` 的配置尚无 `temperature_bindings`，先停止所有使用该目录的 MiGate 进程，再执行以下操作，之后继续下方 schema-11 升级。若已包含该字段，跳过本节，直接执行 schema-11 升级；下方脚本也会检查该字段。将 `.migate` 改为实际数据目录；需要 Python 3。操作备份数据库、补充空绑定，并为现有空调更新配置版本，保留身份、名称、端点和配对。备份同样包含凭据和配对信息，请妥善保管。

```sh
(
  umask 077
  MIGATE_ADJUST_DIR=".migate" python3 - <<'PY'
import json
import os
from pathlib import Path
import sqlite3

directory = Path(os.environ["MIGATE_ADJUST_DIR"]).resolve()
database = directory / "state.db"
backup_path = directory / "state.db.before-air-conditioner-enhancements"
if backup_path.exists():
    raise SystemExit("Backup already exists; check the data directory before continuing")

db = sqlite3.connect(database.as_uri() + "?mode=rw", uri=True)
try:
    if db.execute("PRAGMA user_version").fetchone()[0] != 10:
        raise SystemExit("This operation is only for schema-10 data")
    row = db.execute("SELECT json FROM bridge_configuration WHERE id = 1").fetchone()
    if row is None:
        raise SystemExit("Committed configuration is missing")
    configuration = json.loads(row[0])
    if "temperature_bindings" in configuration:
        raise SystemExit("temperature_bindings already exists; do not repeat this operation")
    affected = [
        device for device in configuration["devices"]
        if device["native"] is not None
        and any(function["kind"] == "air_conditioner"
                for function in device["native"]["functions"])
    ]
    if affected and (configuration["root_version"] >= 0xffffffff
                     or any(device["configuration_version"] >= 0xffffffff for device in affected)):
        raise SystemExit("Configuration version is exhausted")
    with sqlite3.connect(backup_path) as backup:
        db.backup(backup)
    configuration["temperature_bindings"] = []
    if affected:
        configuration["root_version"] += 1
        for device in affected:
            device["configuration_version"] += 1
    with db:
        db.execute(
            "UPDATE bridge_configuration SET json = ? WHERE id = 1",
            (json.dumps(configuration, ensure_ascii=False, separators=(",", ":")),),
        )
    print(f"Updated configuration for {len(affected)} air-conditioner device(s); backup: {backup_path}")
finally:
    db.close()
PY
)
```

此操作只执行一次；若提示字段或备份已存在，先核对是否已经调整，不要清空已有绑定或再次增加版本。仍为 schema-10 的数据库完成此调整后，继续下方升级；全部完成后使用原数据目录启动，再按需绑定温度来源。若需撤销，停止 MiGate 后用备份恢复 `state.db`。

### schema-10 升级到 schema-11

现有 schema-10 开发数据不会自动迁移。先停止所有使用该目录的 MiGate 进程；如需上面的环境温度配置调整，须先完成，再执行以下操作。当前已包含 `temperature_bindings` 的 schema-10 数据无需再次修改 JSON 或配置版本。将 `.migate` 改为实际数据目录，保持指向原目录；需要 Python 3 且其 SQLite 版本至少为 3.37。

脚本只打开已有的 `state.db`，在同一目录创建 `state.db.before-lan-schema-11` 备份，再将独立的设备直连凭据表和 schema 版本一起提交。已有认证、配置 JSON、设备身份、端点、名称、温度绑定、Matter 数据与配对、访问撤销记录均保留。备份包含敏感数据，请与原数据目录一样妥善保管，不要共享或提交。

```sh
(
  umask 077
  MIGATE_UPGRADE_DIR=".migate" python3 - <<'PY'
from contextlib import closing
import json
import os
from pathlib import Path
import sqlite3

if sqlite3.sqlite_version_info < (3, 37, 0):
    raise SystemExit("SQLite 3.37 or newer is required")

directory = Path(os.environ["MIGATE_UPGRADE_DIR"]).resolve()
database = directory / "state.db"
backup_path = directory / "state.db.before-lan-schema-11"
db = sqlite3.connect(database.as_uri() + "?mode=rw", uri=True, isolation_level=None)
try:
    if db.execute("PRAGMA user_version").fetchone()[0] != 10:
        raise SystemExit("This operation is only for schema-10 data")
    row = db.execute("SELECT json FROM bridge_configuration WHERE id = 1").fetchone()
    if row is None:
        raise SystemExit("Committed configuration is missing")
    if "temperature_bindings" not in json.loads(row[0]):
        raise SystemExit("Run the earlier schema-10 temperature configuration adjustment first")

    backup_fd = os.open(backup_path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    os.close(backup_fd)
    with closing(sqlite3.connect(backup_path.as_uri() + "?mode=rw", uri=True)) as backup:
        db.backup(backup)

    try:
        db.execute("BEGIN IMMEDIATE")
        db.execute("""
            CREATE TABLE xiaomi_lan (
                did TEXT PRIMARY KEY,
                uid TEXT NOT NULL,
                home_id TEXT NOT NULL,
                model TEXT NOT NULL,
                address TEXT NOT NULL,
                token BLOB NOT NULL CHECK (length(token) = 16)
            ) STRICT
        """)
        db.execute("PRAGMA user_version = 11")
        db.commit()
    except BaseException:
        db.rollback()
        raise
    print(f"Upgraded schema 10 to 11; backup: {backup_path}")
finally:
    db.close()
PY
)
```

成功后使用原数据目录启动。脚本拒绝其他 schema 版本、重复执行及覆盖已有备份；如果中途失败，原库不会完成升级，事务内失败会回滚，已创建的备份会保留，先检查备份和错误原因再处理。需要恢复备份时，停止 MiGate，将当前 `state.db` 及同名 `-wal`、`-shm` 文件（如存在）移至安全位置，再把备份复制为 `state.db` 并确认权限。恢复到 schema 10 后须重新完成升级，才能使用新版。

当前列出的原目录升级步骤适用于 schema-10；其他不受支持的旧格式会被拒绝。若需要全新实例，可停止 MiGate 并选择一个为空或尚不存在的新目录，依次登录和启动。例如：

```sh
cargo run --locked -- --data-dir .migate-new auth login
cargo run --locked -- --data-dir .migate-new
```

在「家庭」App 中按新显示的配对码添加网桥。新目录建立新的配对身份，需要重新登录米家；原网桥的 Home 名称、房间、场景和自动化不会自动带入新网桥。原数据目录和原配对会保留，请自行整理 Home 中的旧网桥。

在线从「家庭」App 移除配件后，可使用原目录重新配对。若离线移除后无法重新配对，或需要全新实例，同样可选择与原目录并列的新目录：

```sh
cargo run --locked -- --data-dir .migate-trial-1 auth login
cargo run --locked -- --data-dir .migate-trial-1
```

新实例需要重新登录米家账号并添加到 Apple Home，原目录的数据会保留。
