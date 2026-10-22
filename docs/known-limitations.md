# 已知限制

**主中枢轮换时，不能保证本地接口稳定可用。** 即使米家 App 已正常，本地接口仍可能无响应。云端条件满足时，MiGate 可转向云端；本地连接仍有效时，程序通过设备列表检查恢复本地优先。用 `status` 查看当前路径与恢复进度。[上游相关问题](https://github.com/XiaoMi/ha_xiaomi_home/issues/1791)

**云端仍可能依赖物理网关。** 云端读写或主动通知可能仍经过设备所依赖的网关。网关断电、离线或其互联网链路失效时，设备未必能通过云端工作，切换路径不能保证绕过该依赖。目录显示在线、米家 App 操作成功或云端缓存读取成功，均不能单独证明 MiGate 的实时控制和通知正常。

**多路插座的通道名称可能无法同步到 Apple Home。** 各路可能显示为父配件名称或“插座”“插座 2”等默认名称。需核对实际通道后逐路手动改名，不能依赖显示顺序或默认编号判断通道。

## 温湿度传感器边界

**只读能力与 Matter 发布。** 温湿度传感器按有效 MIoT 标准温度和相对湿度属性接入，分别发布为 Temperature Sensor（`0x0302`）和 Humidity Sensor（`0x0307`）子端点；不提供开关、亮度或颜色控制。原生描述缺少必要测量、读取或通知权限，或范围无法投影为 Matter 有效整数区间时，设备不会按传感器接入。Matter `MeasuredValue` 使用可空值，原生范围按百分之一单位投影；不根据 MIoT 步长声明 Tolerance，也不表示来源具有百分之一的测量精度。

**读数时间与离线判断。** 来源提供的属性更新时间仅用于诊断，不与 MiGate 接收时间互相替代，也不参与排序或来源仲裁。来源未提供时间时保持未知。正常静默不触发超时清空、离线或轮询；单次读取失败、属性错误或非法值保留该项最后有效值，另一项不受影响。所有读取和通知路径失效时设备不可用，但已有读数保留到恢复或进程结束；读数不持久化。以 `blt.` 或 `proxy.` 开头的蓝牙或代理子设备没有可靠的云端在线信号，云端 `isOnline=false` 不独立阻断读取或测量通知资格；普通在线设备仍按离线处理。账号、家庭归属、权限、来源退休和明确禁止始终生效。共享可用性和 Matter `Reachable` 表示 MiGate 的获取通道可用，不保证传感器本体或蓝牙网关仍在工作；终端中的 `Physical online: unknown` 表示来源没有可靠的物理在线信号。

**通知链路需要真机确认。** 目录信息、缓存读取成功或描述声明 `notify` 不能单独证明实际通知链路成立。只有读取路径时，终端会显示读数并标注同步受限，不满足自动更新目标。提升通知可靠性可能需要实际本地中枢或蓝牙网关；MiGate 不扫描主机蓝牙、不直连设备 IP，也不新增网关协议。

**电量是可选整机能力。** 只有描述提供标准电池服务与电量属性、百分比单位、可表示的有效范围和读取／通知权限时，温湿度传感器的物理父端点才增加 Power Source（`0x0011`）和电池 `0x002F`。电量缺失、格式或范围无效、或只有读取权限时不会发布电量，也不阻止有效的温湿度接入；`status DID` 会显示相应诊断。电量不创建第三个测量功能或独立端点，不进行轮询。首次有效电量前，`BatPercentRemaining` 为可空值，非空的 `BatChargeLevel` 与 `BatReplacementNeeded` 返回属性级失败；取得电量后，单次读取失败或静默保留同一份最后有效值，不因此移除 Power Source。`Status` 与 `BatReplaceability` 保持未指定，低电量阈值由 MiGate 按百分比给出，不声称是设备固件告警。Apple Home 是否显示电池百分比、低电量提示及分组样式以实际客户端为准。

## 灯具 Matter 符合性

独立灯与原生灯组按真实能力提供四类标准灯型，并支持相应的目标、软件渐变、Move、Step、Stop 与 Level 的 WithOnOff 变体。当前声明的主要元数据如下；具备亮度时提供 LevelControl，具备颜色能力时提供对应 ColorControl。读写和命令列表按实际实现裁剪，未支持命令返回不支持。

| Cluster | Revision / FeatureMap | 实际属性与命令范围 |
| --- | --- | --- |
| OnOff | 6 / `0x00` | OnOff；Off、On、Toggle |
| LevelControl | 6 / `0x01`（OnOff） | CurrentLevel、RemainingTime、MinLevel、MaxLevel、Options、OnLevel；MoveToLevel、Move、Step、Stop 及四个 WithOnOff 变体 |
| ColorControl（色温灯） | 9 / `0x10`（CT） | RemainingTime、ColorTemperatureMireds、ColorMode、Options、NumberOfPrimaries、EnhancedColorMode、ColorCapabilities、物理色温边界、CoupleColorTempToLevelMinMireds；MoveToColorTemperature、MoveColorTemperature、StepColorTemperature、StopMoveStep |
| ColorControl（彩灯） | 9 / `0x19`（CT + XY + HS） | 上述属性加 CurrentHue、CurrentSaturation、CurrentX、CurrentY；上述命令加 MoveToHue、MoveHue、StepHue、MoveToSaturation、MoveSaturation、StepSaturation、MoveToHueAndSaturation、MoveToColor、MoveColor、StepColor |

灯具类型包括 On/Off Light（`0x0100`）、Dimmable Light（`0x0101`）、Color Temperature Light（`0x010C`）及 Extended Color Light（`0x010D`）；同端点另保留 Descriptor、Bridged Device Basic Information、Groups、Identify。各 cluster 保留标准全局属性。NumberOfPrimaries 返回标准 nullable 空值，EnhancedColorMode 反映 HS／XY／CT 模式，不表示已启用 EnhancedHue 功能。

以下是相应灯型或已选能力下的标准必需项缺口，不能视为普通可选功能。MiGate 当前不声称完整 Matter 灯型符合性或已通过认证；如果缺项阻碍约定的 Home 基础控制与互操作，在 `review` 中针对性处理。

| 未实现或未声明项 | 当前边界 |
| --- | --- |
| Scenes Management，包括 CopyScene | 未实现协议场景存取与复制；Home 中可配置场景或自动化，不证明协议 Scenes 已实现 |
| OnOff Lighting feature 与关联行为 | 未声明 Lighting，缺 GlobalSceneControl、OnTime、OffWaitTime、StartUpOnOff；未实现 OffWithEffect、OnWithRecallGlobalScene、OnWithTimedOff |
| LevelControl Lighting feature 与启动策略 | 仅声明 OnOff，未声明 Lighting，缺 StartUpCurrentLevel；Level 保持锁定依赖的 revision 6，与 Matter 1.6 的 revision 7 存在版本差异，不伪改 revision |
| Level 与色温耦合 | Options 仅接受 ExecuteIfOff（`0x01`），拒绝 CoupleColorTempToLevel（`0x02`）；公开 Color 的 CoupleColorTempToLevelMinMireds 不代表该耦合行为已实现 |
| Color 的启动色温策略 | 缺 CT 能力下必需的 StartUpColorTemperatureMireds，不能将其标作可选省略 |

灯具 Identify 仅保留已实现的 Identify 处理，不下发物理闪烁或其他定位效果；TriggerEffect 未暴露，直接调用也拒绝。物理效果在本轮暂缓。EnhancedHue、ColorLoop、厂商场景、音乐律动和 Apple 自适应照明均未声明；只声明 CT 或 CT + XY + HS。现有插座的基础 cluster 行为保持原范围。

## 灯具转换与实际设备边界

**RGB 转换未做灯具色度校准。** RGB 与 HS／XY 使用确定性转换，亮度与色度分开处理；受 RGB 范围、设备量化和实际 LED 光谱限制，不能保证精确色度或跨灯一致。

色温量程必须可表达为 Matter 的有效整数 mired 区间，无法表达的量程拒绝接入，不用虚构物理边界补齐。未知状态不填默认值：CurrentLevel 使用 nullable 空值，非 nullable 的开关、HS、XY、色温或模式在必要状态未知时返回失败。

**软件过渡受网络与设备写入能力限制。** 每个来源 DID 的写入沿用同一队列，过时中间步骤会合并。Stop 或接管不能撤回已发送的写入，动作受理不等于光学完成。模式与目标的必要属性须完整受理；实际状态只随有效反馈更新。

**描述不能证明固件模式联动与批属性顺序。** 跨服务和自动模式切换按型号适配，不从相似属性推断其他设备行为。独立灯和灯组的实际控制、状态同步及 Apple Home 命令选择需要按各自路径核对，不能以字段解析代替。

**描述自动更新依赖目录中的版本变化。** 发现新的 MIoT URN／版本后会加载并应用受支持能力；同一 URN 下的静默修改不会主动轮询。更新失败时保留上次完整配置。

**状态更新需等待有效反馈。** 灯光命令受理后不提前把目标写成当前状态；通知或读取结果到达前可能显示旧值或未知，相邻相对命令也以当时的反馈为起点。插座保持原有行为。

**虚拟灯组的显示按成员反馈汇总。** 任一成员开启即显示开启，全部确认关闭才显示关闭；亮度取开启成员有效百分比的平均值，全关时取有效亮度设置值的平均值。色温、RGB 与模式取同一稳定代表成员，不表示所有灯都一致，也不保证与米家组面板相同。成员描述或反馈不足时相应状态可能未知，组仍可控制；不以云端组旧值补齐。物理组保留原生状态路径。

**灯组通知不自动打断渐变。** MiGate 明确收到的新命令与 Stop 仍可接管动作；米家或实体操作组及成员只同步状态，后续组渐变可能再次覆盖该调整。独立单灯保留外部通知接管及短期自身回声识别；通知缺少全局原始发令顺序，不能无歧义识别同值操作或任意迟到反馈。

**RGB 补齐仅适用于全员 `devcea.light.ls2307` 的原生灯组。** 开关、亮度和色温直接使用组接口，不补写成员模式或电源；缺失的 RGB 仅写确认亮着的成员，关闭或状态未知的成员跳过，不保存颜色供下次开灯补发。至少一个成员受理即可成功，没有可执行成员或全部失败则失败；协议允许忽略的关灯命令沿用该规则，不声称支持关灯预设 RGB。虚拟组使用上述成员汇总；物理组仅 RGB 与模式取同一个代表成员。这项型号控制修复不推广到其他 RGB 灯。

**配置变化后恢复可能需要数分钟。** 成员关系、来源实例或发布能力变化会自动重建 Matter 实例，保留身份、端点与配对，但可中断未完成交互与灯光动作；恢复后不重放旧动作。新实例等待 Home 重新订阅，状态报告和入口显示可能晚于服务恢复，期间可能显示旧值或暂缺入口。恢复时长由控制器决定；若持续显示旧入口或状态，可尝试重新打开「家庭」后再次核对。

复合设备、多负载开关与无线按键不包含在本轮范围内。云端及 BLE Mesh 所依赖的物理网关限制继续适用，不能因为支持灯具或切换云端便保证绕过网关。
