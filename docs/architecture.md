# 项目结构与生命周期

MiGate 保持单个 Rust 包，由 `main.rs` 组装 SQLite 存储、Xiaomi 运行时、Matter 网桥和终端。`DeviceService` 提供协议无关的功能、命令和确认状态，Matter 与终端使用同一份设备服务。模块通过明确的类型、方法和事件协作；`device.rs`、`xiaomi/catalog/mod.rs`、`xiaomi/runtime/mod.rs` 显式列出导出项。

## 测试文件组织

单元测试统一使用外部子模块，生产文件只声明 `#[cfg(test)] mod tests;`。例如 `xiaomi/runtime/state.rs` 对应 `xiaomi/runtime/state/tests.rs`，测试仍属于原模块的子模块，可访问其私有成员。平台条件保留在模块声明处；只供测试调用的构造或观察接口继续由 `#[cfg(test)]` 限定。

已有独立职责的测试分组可以继续使用子文件，例如 `admission/tests/review_regressions.rs`。仓库根目录的 `tests/` 验证公开入口；依赖命令、状态或会话内部接口的回归测试放在所属模块下，不为测试扩大公开接口。按职责组织测试，不按固定行数拆分。

较大的测试套件进一步按主题组织：`matter/tests/device_bridge/` 按设备类型分组，照明下再区分场景与连续调整，`interaction.rs` 保存共享的协议请求工具，`subscription_recovery.rs` 验证订阅恢复；`catalog/tests/` 区分范围、物模型校验、目录装配与各类映射；`runtime/sessions/tests/` 区分认证、生命周期、各通信路径和目录协调。各自的 `tests.rs` 或测试入口文件保留共享夹具，子文件保持原有私有成员访问。

## 系统适配边界

当前支持范围为 macOS 和 Linux。`xiaomi/discovery/network.rs` 保存共享的接口筛选、网段判断、网络快照和休眠后重连逻辑；`platform.rs` 将系统结果转换成统一记录。`platform/macos.rs` 读取 Darwin 链路信息、默认路由和 Mach 时钟，`platform/linux.rs` 读取 sysfs 网卡属性、procfs 主路由表，以及不计／计入休眠时间的两种单调时钟。网卡是否为物理接口由系统适配层提供，共享逻辑不依赖网卡名称前缀。

`socket.rs` 通过 `socket2` 按接口索引和本地地址绑定 UDP，两平台使用同一套调用。Matter 的 mDNS 后端继续使用固定版本 `rs-matter` 的 `AstroMdns`，macOS 由 Bonjour 提供，Linux 由 Avahi DNS-SD 兼容库提供。信号退出及数据目录权限沿用两边共用的 Unix 接口。新增平台时只扩展必要的系统边界，不复制设备准入、路由策略或状态机。

通用测试在两平台执行；Linux 路由解析和网卡元数据测试也能在 macOS 执行。真实接口采集、时钟和 socket 绑定测试调用当前宿主的适配层。进程测试使用 Rust mDNS 浏览和系统调用发送信号，系统依赖与 CI 的运行前提见[运行指南](running.md#运行环境)。

## 能力定义与协议绑定

```text
xiaomi/catalog/
├── compiler.rs      物模型编译入口
├── spec.rs          MIoT-Spec 结构、数值元数据及引用校验
├── scope.rs         设备类别和明确排除规则
├── mapping/         灯、温控、窗帘、风扇、传感器和扫地机映射
├── binding.rs       Xiaomi 属性、命令、事件绑定及其编码、解码入口
├── codec.rs         协议值转换规则
├── data.rs          家庭目录装配、拆分通道合并和缓存恢复
└── legacy.rs        mcn02 的传统 miIO 映射
```

编译器先读取物模型头部并判断设备范围，再解析所接入设备的服务。范围规则先于服务识别，被排除的设备不能通过附属灯或传感器接入。`mapping/` 只处理已经解析的服务、访问权限、量程和型号差异，不访问网络或存储。浴霸的温控和风机组合与空调映射放在 `climate.rs`，照明复用灯的映射。

编译结果 `FeatureDescriptor` 组合两个对象：

| 对象 | 内容与使用者 |
| --- | --- |
| `device::FeatureDefinition` | 服务实例、功能角色、名称和统一能力。定义位于协议无关的核心模块，可用于发布设备功能。 |
| `XiaomiBinding` | MIoT 属性、命令、事件及值转换绑定。Xiaomi 命令、读取和报告通过它编码、解码。 |

运行时保留这两个对象的组合，用于识别能力或绑定变化并撤销过时操作。设备服务对外的 `Feature` 包含稳定身份、名称和能力；Matter 不依赖 Xiaomi 的属性编号或通信路径。没有建立第二份核心设备模型。

## 运行时与状态归属

```text
xiaomi/runtime/
├── mod.rs                 XiaomiRuntime 对外入口、组件组装与共享句柄
├── lifecycle.rs           同一运行时的启动退出、事件循环与跨组件更新顺序
├── status.rs              状态快照、诊断类型、诊断去重和记录
├── auth.rs                认证观察、维护任务和退避时钟
├── catalog_refresh.rs     完整目录获取、物模型解析任务和刷新时钟
├── discovery.rs           网络监测、mDNS 浏览和重试时钟
├── sessions.rs            会话状态、事件分发与跨路径协调
├── sessions/
│   ├── gateway.rs         中枢选择、准入、通知和路由发布
│   ├── lan.rs             局域网连接策略及准入
│   ├── cloud.rs           云通知选择、回退来源和云路由发布
│   ├── catalog.rs         目录完成结果与发布协调
│   ├── tests.rs           会话测试入口与共享模拟工具
│   └── tests/             按认证、生命周期和通信路径分组的测试
├── gateway_connection.rs  中枢连接尝试、订阅、目录请求和退出
├── lan_connection.rs      LAN 连接尝试、路由租约、推送和退出
├── cloud_connection.rs    云通知连接、订阅名单更新和重连
├── connection.rs          建连并发额度、退避和安全错误码
├── startup.rs             建立并认证单次协议会话
├── admission.rs           家庭绑定、设备准入和功能发布
├── command.rs             有界命令队列和派发语义
├── state.rs               推送来源、补读、状态归一化及持久化调度
└── transports.rs          当前会话授权、有效路由和协议调用
```

`XiaomiRuntime` 直接组装并运行认证、目录、发现与会话组件，`lifecycle.rs` 是这一类型的生命周期方法实现。私有的 `Runner` 只持有 `AuthMaintenance`、`CatalogRefresh`、`NetworkDiscovery` 和 `DeviceSessions` 四个组件。每个组件私有持有自己的运行状态，运行时调用调度方法并等待事件；取消一次等待不会丢失组件中尚未完成的任务。会话策略通过 `SessionContext` 读取认证和网络结果，以及访问状态、路由、存储与诊断接口，不能修改其它组件的任务或计时器。

运行时公开接口只保留 `XiaomiRuntime`、启动错误及状态快照所需的类型。命令与状态执行器、会话授权、传输和建连接口限制在包内；只供测试使用的辅助方法受 `#[cfg(test)]` 限定。认证报告的通用状态格式和日志由 `xiaomi/auth/report.rs` 提供，终端补充登录命令等交互提示，运行时不依赖终端模块。

`status.rs` 中的私有 `RuntimeStatus` 封装状态快照与有界诊断记录，提供准入快照更新、候选状态更新和诊断记录方法。`SessionContext` 不暴露这些数据的 `RefCell` 或诊断队列；诊断去重和容量限制只在状态模块内维护。

`DeviceSessions` 的状态仍集中在 `sessions.rs`；`sessions/` 子模块中的方法共用同一所有者，按中枢、LAN、云通知和目录应用组织策略。`gateway_connection.rs`、`lan_connection.rs`、`cloud_connection.rs` 负责连接任务的资源生命周期，`sessions/gateway.rs`、`lan.rs`、`cloud.rs` 处理连接事实的准入与发布，不建立第二套连接状态。`catalog_refresh.rs` 持有目录获取与物模型解析任务，`sessions/catalog.rs` 将完成结果应用到当前设备会话。

| 所有者 | 维护的状态 | 向外提供的结果或操作 |
| --- | --- | --- |
| `AuthMaintenance` | 已处理的认证观察、维护任务、重查标记、截止时间和退避 | 会话／凭据修订变化、当前凭据快照、认证报告。凭据的持久化权威来源仍是认证存储。 |
| `CatalogRefresh` | 云端客户端、两阶段目录任务和刷新时钟 | 完整归属目录、解析后的能力目录；支持取消、重新调度和退避。 |
| `NetworkDiscovery` | 网络监测任务、当前网络代次、mDNS 浏览任务和候选中枢 | 网络变化与发现事件、只读网络和候选列表。 |
| `DeviceSessions` | 当前候选目录、连接句柄、连接尝试、发布过程及路由选择记录 | 应用认证／网络／目录变化，向准入、路由注册表和状态运行时发布结果。 |
| `AdmissionController` | 绑定家庭、设备本地证明及准入状态 | 持久化并发布有效功能，生成准入快照。 |
| 连接任务及尝试资源 | 协议连接、订阅、重试和取消状态 | 就绪、断连、订阅完成、通知等事件；LAN 尝试资源退出时统一撤销授权、路由租约和订阅 token。 |
| `CurrentSessionRegistry` | 当前可派发路由与会话授权 | 派发前检查、路由撤销和失效事件。 |
| `CommandRuntime` | 命令队列、设备执行顺序、发送状态及完成通知 | 受时限和授权约束的命令结果。 |
| `StateRuntime` | 每设备的有效推送 token、查询代次、补读和持久化任务 | 将有效报告写入 `DeviceService`，丢弃旧来源或旧查询结果。 |
| `DeviceService` | 已发布功能及其确认状态、最后已知值和可用性 | Matter 与终端共享的功能、快照、命令入口和变化通知。 |

运行时状态和诊断界面是这些结果的视图，不作为新的控制资格来源。中枢与云通知由设备会话组件处理，LAN 报告通过尝试资源送入状态运行时；属性编码、查询和命令派发由各自模块处理。

跨组件顺序保持明确：

1. 观察到注销或换账号时，先使旧路由授权失效，再停止连接、取消目录任务并更新准入；成功应用后才记录已处理的认证观察。
2. 接收异步认证、目录或连接结果时，核对认证修订、会话代次或连接尝试。凭据修订应用失败时恢复尚未确认的观察快照。
3. 完整归属目录先撤回已移出的成员，再异步解析物模型，避免慢物模型请求延迟撤销资格。单个远程物模型缺失不抹掉其它设备的目录。
4. 网络切换先撤销旧连接，再推进网络代次、更新准入视图和安装发现快照；普通网络故障保留已确认的家庭成员资格。
5. 控制路径和通知来源分别选择，每台设备只使用一个有效推送来源。连接恢复可以重新查询状态，不重放历史控制命令。

## Matter 规划、组装与报告

```text
matter/
├── topology/plan.rs      纯端点规划：设备类型、簇、可见属性与签名
├── topology/registry.rs  活跃端点快照、配置协调和模型重建请求
├── endpoint.rs           根据计划与已分配身份组装协议处理器
├── reporting.rs          已报告值与可达性去重、属性及配置通知
├── device_bridge.rs      动态元数据和协议调用分发
├── scene_context.rs      端点场景存储及响应上下文
├── bridge.rs             Matter 传输生命周期、配对及模型重建
├── lighting.rs           照明共享状态与公共入口
├── lighting/
│   ├── power.rs          开关及定时关闭命令
│   ├── level.rs          亮度簇、量程转换与命令
│   ├── color.rs          颜色／色温簇、转换与命令
│   ├── scenes.rs         场景捕获、回放与确认
│   └── transition.rs     连续调整、定时推进及剩余时间报告
└── 各设备处理器          灯、风扇、温控、窗帘、传感器和扫地机的标准语义
```

`EndpointPlan` 只由核心功能角色和能力计算，不读取数据库、不创建协议处理器，也不分配 endpoint。`TopologyRegistry` 先比较期望计划与当前端点：形状相同时复用处理器并更新名称、能力和配置签名；形状变化时记录目标签名并请求模型重建。只有初次建立模型或增加端点时，`endpoint.rs` 才消费计划及持久化身份建立处理器、读取标签设置。已有端点的 Identify、场景和连续调整状态随处理器保留。

`ReportedState` 私有保存已报告的值和可达性，用于去重，不替代设备服务的确认状态。`DeviceBridgeModel` 负责分发和等待设备变化，`DeviceBridge` 在模型重建期间保留运行中的 Matter 传输。

照明的所有簇、场景与连续调整仍共用一个 `LightingHandler`，子模块不复制状态或创建独立命令队列。共享命令意图、定时关闭、调整任务和确认状态仍由这一处理器持有；文件拆分保留原有串行控制、停止和报告顺序。

## 依赖与持久化边界

| 模块 | 主要依赖 | 边界 |
| --- | --- | --- |
| `device/` | 核心身份、能力、状态类型 | 不依赖 Xiaomi 或 Matter。 |
| Spec 解析与映射 | JSON 边界、核心能力类型 | 纯计算，不读网络或数据库。 |
| Xiaomi 协议模块 | HTTP、MQTT、局域网协议 | 负责协议交互，不决定家庭准入或分配 Matter 身份。 |
| Xiaomi 运行时 | 协议、目录、存储和设备服务 | 持有调度与会话策略；命令和状态各有执行路径。 |
| Matter | 核心设备服务、Matter 协议和持久化接口 | 不读取 Xiaomi 绑定或自行选择 Xiaomi 路由。 |
| 入口与终端 | 对外门面 | 组装、启动、诊断与退出，共享同一设备服务。 |

`storage/device.rs` 的 `DeviceStore::publish_topology` 是统一发布事务：认证代次检查、家庭绑定、设备记录、功能定义及 endpoint 分配同时提交，事务成功后才发布到内存设备服务。身份分配仍由这一存储边界完成，不拆成独立的 Matter 分配事务，也不在事务内等待网络。已有真实设备数据库格式、公开 ID、endpoint 和配对数据继续复用。

增加型号时，先查看 `catalog/scope.rs` 与对应 `catalog/mapping/` 文件；调整标准 Matter 表达时，查看 `matter/topology/plan.rs` 和对应处理器；改变连接行为时，同时检查连接资源退出和设备会话发布顺序。运行方式和设备范围以[运行指南](running.md)为准，行为契约以[真实设备桥接设计](specs/2026-09-14-xiaomi-device-bridge.md)为准。
