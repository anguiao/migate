# 已知限制

**主中枢轮换时，不能保证本地接口稳定可用。** 即使米家 App 已正常，本地接口仍可能无响应。云端条件满足时，MiGate 可转向云端；本地连接仍有效时，程序通过设备列表检查恢复本地优先。用 `status` 查看当前路径与恢复进度。[上游相关问题](https://github.com/XiaoMi/ha_xiaomi_home/issues/1791)

**云端仍可能依赖物理网关。** 云端读写或主动通知可能仍经过设备所依赖的网关。网关断电、离线或其互联网链路失效时，设备未必能通过云端工作，切换路径不能保证绕过该依赖。目录显示在线、米家 App 操作成功或云端缓存读取成功，均不能单独证明 MiGate 的实时控制和通知正常。

**多路插座的通道名称可能无法同步到 Apple Home。** 各路可能显示为父配件名称或“插座”“插座 2”等默认名称。需核对实际通道后逐路手动改名，不能依赖显示顺序或默认编号判断通道。

## 灯具 Matter 符合性

单灯按实际具备的开关、亮度、色温和 RGB 能力发布，四项能力自由组合；有亮度才发布 LevelControl，有色温或 RGB 才发布 ColorControl，并支持目标、软件渐变、Move、Step、Stop 与 Level 的 WithOnOff 变体。当前声明的主要元数据如下；读写和命令列表按实际实现裁剪，未支持命令返回不支持，不依赖默认空 hook 报成功。

| Cluster | Revision / FeatureMap | 实际属性与命令范围 |
| --- | --- | --- |
| OnOff | 6 / `0x00` | OnOff；Off、On、Toggle |
| LevelControl（具备亮度） | 6 / `0x01`（OnOff） | CurrentLevel、RemainingTime、MinLevel、MaxLevel、Options、OnLevel；MoveToLevel、Move、Step、Stop 及四个 WithOnOff 变体 |
| ColorControl（仅有色温） | 9 / `0x10`（CT） | RemainingTime、ColorTemperatureMireds、ColorMode、Options、NumberOfPrimaries、EnhancedColorMode、ColorCapabilities、物理色温边界、CoupleColorTempToLevelMinMireds；MoveToColorTemperature、MoveColorTemperature、StepColorTemperature、StopMoveStep |
| ColorControl（仅有 RGB） | 9 / `0x09`（HS + XY） | RemainingTime、ColorMode、Options、NumberOfPrimaries、EnhancedColorMode、ColorCapabilities、CurrentHue、CurrentSaturation、CurrentX、CurrentY；MoveToHue、MoveHue、StepHue、MoveToSaturation、MoveSaturation、StepSaturation、MoveToHueAndSaturation、MoveToColor、MoveColor、StepColor、StopMoveStep |
| ColorControl（色温 + RGB） | 9 / `0x19`（CT + HS + XY） | 上述两组属性与命令合并 |

具备开关的灯按实际能力选择设备类型：亮度、色温与 RGB 均具备时为 Extended Color Light（`0x010D`，revision 4）；具备亮度和色温、无 RGB 时为 Color Temperature Light（`0x010C`，revision 4）；仅具备亮度或亮度与 RGB 时为 Dimmable Light（`0x0101`，revision 3）；无亮度时为 On/Off Light（`0x0100`，revision 3）。纯开关灯仍是 On/Off Light，不套用 On/Off Plug-in Unit。缺少开关或色温量程无法映射为标准灯的灯保留实际能力与身份，见[灯具转换与实际设备边界](#灯具转换与实际设备边界)。未分组或退出组的单灯在同一端点另保留 Descriptor、Bridged Device Basic Information、Groups、Identify；组合内的成员子端点不重复声明桥接身份，见[灯组组合边界](#灯组组合边界)。各 cluster 保留标准全局属性。NumberOfPrimaries 返回标准 nullable 空值，EnhancedColorMode 反映 HS／XY／CT 模式，不表示已启用 EnhancedHue 功能。

以下是相应灯型或已选能力下的标准必需项缺口，不能视为普通可选功能。MiGate 当前不声称完整 Matter 灯型符合性或已通过认证；如果缺项阻碍约定的 Home 基础控制与互操作，在 `review` 中针对性处理。

| 未实现或未声明项 | 当前边界 |
| --- | --- |
| Scenes Management，包括 CopyScene | 未实现协议场景存取与复制；Home 中可配置场景或自动化，不证明协议 Scenes 已实现 |
| OnOff Lighting feature 与关联行为 | 未声明 Lighting，缺 GlobalSceneControl、OnTime、OffWaitTime、StartUpOnOff；未实现 OffWithEffect、OnWithRecallGlobalScene、OnWithTimedOff |
| LevelControl Lighting feature 与启动策略 | 具备亮度时仅声明 OnOff，未声明 Lighting，缺 StartUpCurrentLevel；Level 保持锁定依赖的 revision 6，与 Matter 1.6 的 revision 7 存在版本差异，不伪改 revision |
| Level 与色温耦合 | 同时具备亮度与色温时，Options 仅接受 ExecuteIfOff（`0x01`），拒绝 CoupleColorTempToLevel（`0x02`）；公开 Color 的 CoupleColorTempToLevelMinMireds 不代表该耦合行为已实现 |
| Color 的启动色温策略 | 具备色温能力时缺必需的 StartUpColorTemperatureMireds，不能将其标作可选省略 |

灯具 Identify 仅保留已实现的 Identify 处理，不下发物理闪烁或其他定位效果；TriggerEffect 未暴露，直接调用也拒绝。物理效果在本轮暂缓。EnhancedHue、ColorLoop、厂商场景、音乐律动和 Apple 自适应照明均未声明；ColorControl 只按实际能力声明 CT、HS 与 XY。现有插座的基础 cluster 行为保持原范围。

缺少代表设备的组合以自动化验证为完成条件，真机效果待有代表设备后确认；控制器可以忽略设备类型之外的标准 cluster，Apple Home 可能不提供相应控件。代表灯与灯组在 Apple Home 中的实际呈现仍在 `review` 阶段真机验收。

## 灯具转换与实际设备边界

**RGB 转换未做灯具色度校准。** RGB 与 HS／XY 使用确定性转换，亮度与色度分开处理；受 RGB 范围、设备量化和实际 LED 光谱限制，不能保证精确色度或跨灯一致。

色温量程按实际描述解析并保存；物理上有效但无法表达为 Matter 整数 mired 区间的量程不会被虚构边界改写或补齐。缺少开关能力时也不能补造 OnOff。两类灯都保留真实能力、缓存、名称与身份预留，终端会显示未发布原因，但不能发布为标准 Matter 灯，Apple Home 中不会出现该配件。未知状态不填默认值：CurrentLevel 使用 nullable 空值，非 nullable 的开关、HS、XY、色温或模式在必要状态未知时返回失败。

单灯接纳后的对外能力及物理量程按静态描述处理，不支持能力热更新。不兼容的新描述不会替换已有控制功能，设备保持不可用并报告原因；灯组成员关系和发布角色仍可在运行中更新。对外结构变化通过自动重建 Matter 服务生效，期间整个网桥短暂不可用，未完成的请求和渐变会中断且不重放；恢复后沿用原身份、名称与配对。

**软件过渡受网络与设备写入能力限制。** 每盏灯最多一个在途写请求，过时中间步骤会合并。Stop 或接管不能撤回已发送的写入，动作受理不等于光学完成。模式与目标按实际批属性结果更新，部分成功不能算整次操作成功。

**描述不能证明固件模式联动与批属性顺序。** 七个代表样本描述用于静态能力与自动化覆盖，型号列表不构成兼容性保证；`ls2307` 的白光／彩光双向切换、批写先后要求和彩光亮度耦合仍需在 `review` 真机核对，不能以字段解析代替。Apple Home 的实际界面和命令选择也在该阶段核对。

**没有跨客户端的全局原始发令顺序。** MiGate 排除可关联的写入反馈后按有效观察接管；普通补读、云端缓存及非当前模式缓存不视为新指令。现有通知缺少完整原始时间信息，来源切换和迟到事件仍有协议边界。

## 灯组组合边界

灯组仅接入当前家庭内扁平、互不重叠，且全部成员均可作为单灯发布的米家物理组和虚拟组；成员能力可以不同，米家组自身缺少的属性不影响成员。成员保留各自实际的开关、亮度、色温与 RGB 能力；MiGate 只发布组合关系，不提供可控制的整组开关、平均亮度、色温或颜色状态。

以下情况不组合，可发布的成员继续作为独立单灯接入，终端会显示未组合原因：

- 嵌套组，以及多个组声明同一成员的重叠关系。
- 存在无法作为单灯发布的成员，例如缺少开关或色温量程无法映射为标准灯；成员为其他组或非灯设备也属于此类。

成员关系未确认、读取失败或信息未就绪时不发布部分成员表：保留上一次已确认的关系，不自行裁剪旧成员表；此时组可能继续按旧关系存在，终端显示待确认原因。完整当前家庭目录确认的成员删除优先处理：受影响组没有完整有效的新关系时撤销父端点，其余可发布的成员恢复为独立单灯。

组合父端点只承载 Bridged Node、Descriptor 与 Bridged Device Basic Information；成员作为子端点保留原灯型与灯光 cluster，不重复声明桥接身份，因此不承诺 Apple Home 为每个子灯显示独立离线徽标。未分组或退出组的单灯沿用原有单端点结构，桥接身份与灯型在同一端点。逐成员可用性与错误继续在终端可见。

控制器可能忽略成员子端点设备类型之外的标准 cluster，因此 Apple Home 是否提供成员颜色等控件不能由协议发布保证。Apple Home 是否自动形成每组一个入口、成员单控与整组／成员颜色控件，属于 `review` 阶段真机验收项，协议与自动化测试不能替代；验收不通过时返回设计讨论，不以手动编组、额外总灯端点或拆成独立灯作为替代方案。

除上述灯组组合外，复合设备、多负载开关与无线按键暂不包含在单灯接入范围内。云端及 BLE Mesh 所依赖的物理网关限制继续适用，不能因为支持灯具或切换云端便保证绕过网关。
