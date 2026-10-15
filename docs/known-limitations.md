# 已知限制

**主中枢轮换时，不能保证本地接口稳定可用。** 即使米家 App 已正常，本地接口仍可能无响应。云端条件满足时，MiGate 可转向云端；本地连接仍有效时，程序通过设备列表检查恢复本地优先。用 `status` 查看当前路径与恢复进度。[上游相关问题](https://github.com/XiaoMi/ha_xiaomi_home/issues/1791)

**云端仍可能依赖物理网关。** 云端读写或主动通知可能仍经过设备所依赖的网关。网关断电、离线或其互联网链路失效时，设备未必能通过云端工作，切换路径不能保证绕过该依赖。目录显示在线、米家 App 操作成功或云端缓存读取成功，均不能单独证明 MiGate 的实时控制和通知正常。

**多路插座的通道名称可能无法同步到 Apple Home。** 各路可能显示为父配件名称或“插座”“插座 2”等默认名称。需核对实际通道后逐路手动改名，不能依赖显示顺序或默认编号判断通道。

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

**描述不能证明固件模式联动与批属性顺序。** 七个样本描述用于静态能力与自动化覆盖；`ls2307` 的白光／彩光双向切换、批写先后要求和彩光亮度耦合仍需在 `review` 真机核对，不能以字段解析代替。Apple Home 的实际界面和命令选择也在该阶段核对。

**没有跨客户端的全局原始发令顺序。** MiGate 排除可关联的写入反馈后按有效观察接管；普通补读、云端缓存及非当前模式缓存不视为新指令。现有通知缺少完整原始时间信息，来源切换和迟到事件仍有协议边界。

**灯组修复规则限定于已定义的能力适配。** 原生功能优先使用组接口；目前具体补齐 `devcea.light.ls2307` 成员的 RGB 与颜色模式，其他能力缺口不会自动套用此规则。修复写入至少一个成员完整受理即可成功，不能据此判断成员状态一致；RGB 与模式来自同一个稳定选择的代表成员，开关、亮度和色温来自原生组。

**配置变化会短暂中断整桥。** 成员关系、来源实例或发布能力变化会自动重建 Matter 实例，保留身份、端点与配对，但可中断未完成交互与灯光动作；恢复后不重放旧动作。Home 控件刷新还取决于客户端。

复合设备、多负载开关与无线按键不包含在本轮范围内。云端及 BLE Mesh 所依赖的物理网关限制继续适用，不能因为支持灯具或切换云端便保证绕过网关。程序只支持当前数据库格式，旧库须另行修复。
