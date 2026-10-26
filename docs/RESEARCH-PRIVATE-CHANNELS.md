# 私有通道：音频接口与驱动层的可达性

本文整理"电量不在标准通道里"时的可选路径、开源实现的做法与已知问题。用于判断某个型号是否值得继续投入，以及投入时该按什么顺序取证。

结论只区分三类依据：**实测**（本机真机验证）、**资料推断**（他人在公开实现中验证过，但未在本机复现）、**待确认**（仅结构上可能）。

## 结论先行

一台 USB 复合外设（尤其 2.4G 接收器）通常同时暴露多条接口。它们由不同类驱动接管，用户态可达性差别很大：

| 接口 | Windows 类驱动 | 用户态能否只读访问 | 代价 |
| --- | --- | --- | --- |
| HID 集合（含厂商 usage） | `hidusb.sys` | 能，`ReadFile` / `hidapi` 直接读 | 无 |
| UAC 音频接口 | `usbaudio.sys` | 不能，接口被类驱动独占 | 需替换驱动，破坏系统音频 |
| 厂商驱动的 KS 节点 | 厂商自定义 | 能，但要用 `IKsControl` | 需厂商驱动在场且已知属性集 |

因此"电量走音频接口"在协议层可能成立，在 Windows 用户态通常**没有**可用的读取入口。这不是权限问题，是接口所有权问题。

对本项目（只查询、不换驱动、不依赖厂商软件）而言，HID 集合是唯一低代价通道；KS 属性是"厂商驱动已装且属性集已知"时的次选；UAC 厂商请求基本可以排除。

## 通道一：USB Audio Class 厂商自定义请求

驱动态通过 `_URB_CONTROL_VENDOR_OR_CLASS_REQUEST` 构造，`bmRequestType` 置 `0x40` / `0xC0` 走 vendor 分支，`bRequest`、`wValue`、`wIndex` 由厂商自定。这是 USB DAC、声卡控制面板最常用的私有控制面。

用户态没有等价 API，只有两条出路：

1. **WinUSB / libusb 独占接口**——需要为该接口安装 `WinUSB` 驱动（`INF` 或 `Zadig` 一类工具）。一旦替换，`usbaudio.sys` 不再接管，该设备的录音/放音功能同时失效。
2. **绕到 KS 属性**——见下节。

公开实现里能查到的取证线索是静态搜索厂商程序的 `UsbDeviceConnection`、`controlTransfer`、`bulkTransfer`、`SET_CUR`、`GET_CUR`、`SET_REPORT` 调用点，记录每一次请求的七元组 `requestType / request / value / index / length / timeout / data`。这说明该路径确实被厂商使用，但同时说明它需要直接持有 USB 设备句柄——在 Windows 上等价于替换驱动。

**判定：不作为本项目的候选。** 换驱动与本项目"不影响设备原有功能"的约束直接冲突。

## 通道二：KS 属性（IKsControl）

这是"音频接口私有通道"在用户态唯一正经的入口。微软对该接口的定位是：把内核流（KS）设备的属性、事件和方法暴露给用户态，**主要面向编写控制面板的硬件厂商**。

调用链（资料推断，本机未复现）：

1. `IConnector::GetDeviceIdConnectedTo` 由音频端点反查物理设备 ID。
2. `IMMDeviceEnumerator::GetDevice` 取到 `IMMDevice`。
3. `IMMDevice::Activate` 传入 `IID_IKsControl`。
4. `IPart::GetLocalId` 取得 KS 节点 ID（低 16 位即 node id）。
5. `IKsControl::KsProperty` 携带属性集 GUID、属性 ID、flags 与缓冲区读写。

底层对应 `IOCTL_KS_PROPERTY`。官方示例 `KSPROPSETID_LowLatencyAudio` 展示了完整的 GET/SET 属性 ID 命名方式。

**关键限制：属性集必须由驱动实现。** 通用 `usbaudio.sys` 不会凭空长出一个"电池"属性。没有厂商驱动，就没有这个节点；有厂商驱动但没有公开或逆向出属性集 GUID/PID，也无从查询。

**判定：仅在"用户已装厂商驱动 + 属性集已知"时可用。** 对当前目标（迈从 V9 Pro 2，2.4G）没有任何属性集证据，保持未支持。

## 通道三：HID 集合

用户态直接可达，不需要替换驱动，也不需要厂商软件在场。这是唯一在本项目约束内成立的通道。

### 标准 Battery Strength

规范位置是 HID Generic Device Controls，Usage Page `0x06`、Usage `0x20`。社区文档明确指出它是"经典蓝牙设备与各类 USB dongle 设备"读取电量的标准手段，并对随包接收器（dongle）场景特别推荐。

两条必须记住的约束：

- **不要把 Battery Strength 放进顶层集合（TLC）**。鼠标、键盘、笔、触摸屏的 TLC 由系统独占打开，放进去会让系统抢走该集合的访问权，反而读不到。
- 顶层集合的访问模式按用途区分：鼠标与键盘为独占，Consumer Controls 为共享，HID UPS Battery 为共享。选错集合会直接拿到"拒绝访问"。

本机对 V9 Pro 2 接收器（`3837:6011`）的实测结果是：集合为 `FF07:0222`、`000C:0001`、`000B:0005`、`000C:0001`，**不存在** `0006:0020`。标准通道不存在，这一条已被排除，不是"没试"。

### 厂商集合

`FF07:0222` 已实测无电量字段；耳机侧 BES 通道的 `0x0b` 字节经三次采样确认为自增计数器（`0x58` → `0xD9` → `0xDA`），不是电量。这条也已排除。

## 开源实现里的设计模式

以下四个项目分别覆盖了"多策略降级""协议描述表""能力声明""事务可靠性"，值得分别借鉴。

### 多策略降级链（`2.4G-battery-viewer`，纯 ctypes）

`read_battery()` 按固定顺序尝试，第一个成功即返回：

1. 标准 HID 电量字段
2. 设备私有协议（杰理 TLV）
3. 厂商协议表
4. 罗技 HID++
5. 监听 Input Report（兜底）
6. 手动标定（探测 + JSON 持久化）

两个设计细节值得直接照抄：

- **门控先行**：每个策略先按 VID / usage page / 报文长度做特征匹配，不匹配立即返回 `None`。`None` 表示"未处理"，与"失败"分开记账，避免把不相关设备计入失败率。
- **策略隔离**：单个策略抛异常被捕获并转成失败结果，不影响后续策略继续尝试。

### 协议描述表（同上项目）

把逆向结论做成数据而不是代码分支，`ProtocolSpec` 字段集如下：

| 字段 | 作用 |
| --- | --- |
| `report_id` / `payload` / `length` | 请求构造 |
| `level_offset` / `scale` / `level_max` | 电量解析与合法性 |
| `charging_offset` / `charging_nonzero` | 充电状态及其极性 |
| `voltage_offset` | 小端 16 位电压，仅接受 2500–5000 mV |
| `expect` | `((字节下标, 期望值), ...)` 回复校验 |
| `retries` | 默认 2，因为第一次回复常因设备休眠失败 |
| `mode` | `feature` / `output`，留空自动判断 |

内置协议表已体现的三种现实：

- **VAXEE** 以 5% 为步长上报（`scale=5`、`level_max=20`），原始值 0–20 放大到 0–100。
- **Endgame Gear** 需要连读两次，状态码 `0x01` 有效 / `0x03` 忙 / `0x07` 不支持 / `0x08` 休眠。
- **ATK/VXE** 靠 `expect` 区分"有效回复"与"路过的无关报文"。

用户标定结果落到 `protocols_user.json`，与内置协议走完全相同的执行路径（`all_protocols()` 拼接），不需要改代码。

### 能力声明与注册表（`HeadsetControl`）

- `HIDDevice` 基类要求实现 `getDeviceName` / `getVendorId` / `getProductIds` / `getCapabilities` / `getBattery`。
- `DeviceRegistry::initialize()` 里逐个 `registerDevice(std::make_unique<...>())`。
- 能力用 X-macro 位掩码（`CAPABILITIES_XLIST`）声明，UI 据此决定显示哪些开关。
- 错误用 `Result` 表达，区分 `timeout` / `hidError` / `protocolError` / `notSupported`。
- 开发者模式 `--dev -- --list` 用于枚举 HID，文档直言"新增设备需要抓包"。

与本项目的 `ProtocolAdapter` + `ADAPTERS` 静态表等价，差别只在能力声明：我们没有显式的 capability 位掩码，能力散落在 `Reading` 的字段里。

### 事务可靠性（Razer BlackShark V2 HyperSpeed WebHID）

- `sendCommandAndWait(seq, cmd)` 做序号 + 命令双重关联，800 ms 超时。
- `AsyncMutex` 串行化，避免后台 5 秒轮询与用户命令交错。
- 回复校验同时检查**精确长度**与 XOR 校验和，任一不符即丢弃。
- **接口隔离**：严格按 Usage Page `0xFF14`（主控制）与 `0xFF00`（备选）、Report ID `0x02` 过滤，显式避免绑定到标准音频或电话端点。

最后一条对我们直接相关：V9 Pro 2 接收器带 `000B:0005` 集合（Telephony / Headset），若不显式排除，很容易误绑。

### 单 Report ID 多功能复用（HyperX Cloud III Wireless）

`PID 0x05B7`，统一 Report ID `0x66`，用功能字节区分语义：`0x89` 电量（值在 offset 4）、`0x0D` 连接、`0x0C` 电量变化、`0x8A` 插线、`0x82` 连接状态请求。

这提醒一件事：**report ID 相同不代表布局相同**。必须按功能字节分发，不能按整包偏移猜。

## 常见问题

按踩坑代价从高到低：

1. **同一句柄并发"阻塞读 + 写"会直接杀进程**，连异常栈都不留。正确做法是独立的 `FILE_FLAG_OVERLAPPED` 读句柄 + 事件超时 + `CancelIoEx`。这是实测结论，不是理论风险。
2. **系统独占的顶层集合只能用 `dwDesiredAccess=0` 打开**。能读属性和 Feature Report，但发不出指令。鼠标、键盘、笔、触摸屏都属于这一类。
3. **`dwShareMode=0` 不再保证独占**。Win10 1607 起，`CreateFile` 只给 `GENERIC_READ|GENERIC_WRITE` 加 `dwShareMode=0` 已无法取得某些设备的独占访问，原因是系统进程会连接所有 HID 设备。表现为"明明没别的程序占用，就是打不开"。
4. **Report ID 不匹配时 Windows 返回错误 87**。这个特性可反向利用：用全零报文逐个试 `0x00–0x20`，被接受的即为合法 report ID，是标定的第一步。
5. **充电时电量常报 0**。罗技 `0x1000` 特征即如此，实现里按状态补偿（`charging` → 80、`full` → 100）。不做补偿会把满电显示成 0%。
6. **步长不等于精度**。5% 步长的设备（VAXEE）读数只能是 0/5/10…，不能当连续值展示。EZ2.4G 也明确写了"显示精度可能只有 5% 步进"。
7. **第一次回复经常失败**。设备休眠中不响应，所以 `retries` 默认 2；收不到回复时应提示"动一下设备"而不是判失败。
8. **dongle 不转发**。耳机没开机时接收器完全不回任何数据，与"协议错误"表现相同，必须用超时区分。
9. **缓存值冒充实时值**。Windows HFP 电池属性是上一次上报的缓存，断开后仍保留，只能作为"最后已知"，不能当作当前读数。
10. **加密或带私有握手的协议，抓包也拿不到可用规则**。EZ2.4G 在限制里写得很清楚。
11. **电量在外设侧，接收器只是桥**。接收器自己通常没有电池，它转发的值可能来自另一条链路（例如耳机侧蓝牙），这也解释了"数据走 2.4G、电量走蓝牙"的现象。

## 取证顺序

公开资料给出的顺序是一致的，值得固化成规矩：

1. **只读优先**。先用标准通道和被动枚举确认存在什么，不发任何写命令。
2. **一次只验证一个变量**。改一个字节就重测一轮，不同时改长度、report ID 和载荷。
3. **七元组记录**。每次请求记 `requestType / request / value / index / length / timeout / data`，缺一项就无法复现。
4. **抓包优先于猜**。Wireshark + USBPcap 是多个项目共同采用的手段（Arctis Nova 7X、EZ2.4G、HeadsetControl 文档均提及）。抓的是厂商软件查询电量时的真实流量，不是我们盲发的流量。
5. **不做的事**：不遍历未知命令、不向未知 Feature/Output 试写、不读键鼠输入流。这三条同时也是本仓库 `ADDING_DEVICES.md` 的既有约束。

## 对本项目当前目标的意义

迈从 V9 Pro 2 的 2.4G 模式，公开通道已全部走完：

| 通道 | 状态 |
| --- | --- |
| 标准 `0006:0020` Battery Strength | 实测不存在 |
| 厂商 HID `FF07:0222` | 实测无电量字段 |
| BES HID `0x0b` | 实测为自增计数器 |
| Windows HFP 属性 | 可用，但为最后一次上报的缓存值 |
| UAC 厂商请求 | 结构性排除（需换驱动） |
| KS 属性 | 无属性集证据，无法查询 |
| 驱动层（厂商软件内存 / IPC） | 依赖版本，与"不依赖厂商软件"目标冲突 |

剩余唯一有信息增量的动作是**抓厂商软件查询电量时的 HID 流量**。本机 USBPcap 已安装但过滤驱动未绑定到根集线器实例，需重启后才生效。在拿到该流量之前，V9 Pro 2 的 2.4G 模式保持未支持，不生成读数。

## 参考资料

- [2.4G-battery-viewer](https://github.com/yszyszysz233/2.4G-battery-viewer) — 纯 ctypes 实现，多策略降级与协议描述表的完整样本，含踩坑清单
- [razer-blackshark-v2-hyperspeed-webhid](https://github.com/justik13/razer-blackshark-v2-hyperspeed-webhid) — 序号关联、接口隔离与校验的完整协议文档
- [HeadsetControl](https://github.com/Sapd/HeadsetControl) — 设备注册表、能力位掩码与协议模板的组织方式
- [Arctis Nova 7X HID tools](https://onlybits.org/cheahkhing/arctis-headset-hid) — USBPcap 抓包流程与轮询架构
- [HyperX Cloud III Wireless](https://github.com/Medprj/HyperXWM) — 单 report ID 多功能复用的最小样本
- [EZ2.4G](https://kaiyuanbang.cn/zh-cn/repo/ap0log1ze-ez2-4g.html) — VID:PID 协议表与未知设备标定流程
