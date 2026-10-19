# 增加一种外设

目标：后续智能体在自己的电脑上补充所需设备，复用设备列表、缓存、自启和托盘，不复制一套应用。先读根目录 AGENTS.md，再按本页操作。

## 先确认需要补哪一层

| 本机情况 | 修改范围 |
| --- | --- |
| 现有适配器已确认协议和电量能力 | 直接实机验证；更新支持记录，不必为每个型号添加白名单 |
| 共用协议，但识别、名称或字段有例外 | 在相应适配模块补必要规则和报文测试 |
| 新协议或新传输方式 | 新建独立模块，在 Registry 登记发现和读取入口；UI 与设备管理不改 |
| 只能枚举到设备，不知道电量协议 | 保留为未支持候选，不生成读数，不把枚举当适配完成 |

## 接线地图

- `src/device/mod.rs`：`DeviceKind`、`DeviceDescriptor`、`Candidate`、`Reading`、`BatterySource`、`DeviceError` 与 `Registry`。
- `src/device/logitech.rs`：现有 HID++ 家族适配器、查询白名单、回复匹配、连接边界和解析测试。
- `src/device/discovery.rs`：公共HID候选与Windows ContainerId分组，排除适配器已认领的物理设备；新品牌不要复制公共枚举。
- `src/device/bas.rs`：共用标准 BLE BAS，180F / 2A19 和可选 2BED；支持已配对、当前连接且已有访问许可的设备，不自动配对或请求同意。标准服务缺失不等于任意蓝牙私有协议已适配。
- `src/manager.rs`：选择、用户移除、托盘开关、成功缓存和状态文件。新品牌不在这里增加协议分支。
- `src/notifications.rs`：独立提醒周期与 Shell 通知，新协议只返回真实 Reading / PowerState，不单独实现重复提醒逻辑。
- `src/app.rs`：后台调度、原生列表、添加窗口、设置和托盘。这里不能加入品牌报文字节。
- `src/icons.rs`：按通用 `DeviceKind` 选择图形；现有类别直接复用，新类别无专用图标时用 Generic。

Registry 是集中入口。新增协议实现 `ProtocolAdapter` 的 `name`、`owns`、`discover`、`read`，在 `mod.rs` 的 `ADAPTERS` 静态表登记一次。该模块使用独立 ID 前缀，发现返回通用 Candidate，读取按选定身份返回 Reading。不能在扫描时直接选“枚举列表第一项”作为用户设备。

适配器内部需要隔离同协议下多个设备的失败时，可覆盖可选 `discover_report`，同时返回成功候选与失败信息；默认实现仍包装 `discover`。Manager 保留未完整确认设备的缓存，成功返回的设备不继承其他设备的错误。取消必须返回 `Cancelled`，不能把半次发现当作已确认断开。

描述中 `physical_id` 标识物理容器，`endpoint` 标识容器内的槽位或逻辑连接，`id` 标识具体外设，`provider` 对应适配器的 `name`。`recoverable` 表示活动后可重试发现的连接。临时失败候选沿用相同容器/端点；整台接收器不可查询时，端点可为 None。Manager 用这些通用字段关联缓存，不解析品牌 ID。新模块沿用现有类型，不改 Manager、窗口或托盘分发。

解析出数值后使用 `Reading::from_percent(name, percent, power)`；纯档位使用 `Reading::from_levels(name, current, total)`，不需要伪造 HID++ 字段。协议专用诊断字段只在确实读取到时填写。旧缓存缺少的元数据以 `metadata_known=false` 表示，不能宣称协议版本为 0.0。

`discover` 和 `read` 都接收 `Cancellation`；在枚举、槽位和等待循环之间调用 `check()`。返回 `Cancelled` 时丢弃本次未完成扫描，不将半份结果合并为断开状态。单个适配器失败通过 `DiscoveryReport.failures` 独立记录，不中断其他适配器。

可用的协议变化事件通过 Registry 的 change handler 送回通用读数；只能确认状态变化而需要新百分比时，使用 refresh handler 请求串行读取。不得在回调内直接操作 GUI。BAS 已实现这条路径，运行时由查询线程创建 MTA，取消和关闭句柄后才释放。标准 2A19 没有充电字段；仅实际读取成功的标准 2BED 状态允许产生 Charging / Discharging。当前没有 BAS 真机证据，解析夹具与 Windows API 路径实现不能写成型号已测。

HID“复制信息”只读取后台发现缓存，包括同一物理组内的输入和厂商集合、VID/PID/release、usage、接口、总线、厂商/产品字符串与 Windows 属性。按容器身份分组后才挑选输入候选；鼠标+键盘混合组保守显示 Generic，厂商专用集合不生成默认鼠标。名字和类型的选择不依赖枚举顺序，两个同 VID/PID 容器保持独立。不在 GUI 线程枚举设备；适配器已认领的设备也可在 `--scan` 获取兄弟集合缓存。

### 手上只有 IDs 时怎么继续

1. 先用 `tools/export-diagnostics.ps1` 导出默认匿名 `--diagnose`，查看同一 `group` 内的输入与 vendor usage 集合、字符串来源、Windows 描述/枚举器/服务、VID/PID、release、接口和长度。拒绝访问的集合仍在清单中；`null` 和失败码是未知，不是缺少电量能力。若目标根本没有 HID 接口，按 [开发说明](DEVELOPMENT.md) 用设备管理器或只读 PnP 属性找到它所属的传输层。
2. VID/PID 定位 USB 注册厂商/产品标识，不能独自证明零售品牌；Windows manufacturer 可能描述驱动提供者，hidapi 字符串也可能来自 Windows 回退。USB 只证明报告中的总线，不能证明设备使用2.4GHz无线。结合用户确认的实物型号、接收器和连接方式，再查厂商公开协议、标准或原始实现。不要把猜测改写成已确认字段。
3. 本机需要对照具体集合时，再显式导出 `--include-identifiers`。路径/容器可能含可识别内容，不加入公开夹具。共享容器只说明 Windows 物理分组，不是接收器里某台已配对外设的槽位身份。
4. 对照已知协议的 report-ID、usage、所需查询和回复结构。Windows 集合长度包含 report-ID 槽位，只能帮助排除错误端点；它不提供电量字段、每包布局或原始描述符。没有查询依据就保留未支持状态；不靠遍历未知命令、写 Feature/Output、读取键鼠输入流来试错。
5. 确认可复用协议就验证现有适配器；共用协议例外补在其模块；新协议使用下方最小流程。把每个判断标为实测、资料推断或待确认，不凭“可枚举”宣布电量支持。


从清单提取本轮分组、端点、搜索用 VID:PID 和产品信息：

```powershell
$qbReport = Get-Content ./validation/diagnostics.json -Raw -Encoding UTF8 | ConvertFrom-Json
$qbReport.collections | ForEach-Object {
    $qbFields = $_.fields
    $qbIds = if ($null -ne $qbFields.vid.value -and $null -ne $qbFields.pid.value) {
        '{0:X4}:{1:X4}' -f [int]$qbFields.vid.value, [int]$qbFields.pid.value
    } else { 'unavailable' }
    [pscustomobject]@{
        Group = $_.group; Endpoint = $_.endpoint; VID_PID = $qbIds
        Manufacturer = $qbFields.manufacturer.value; Product = $qbFields.product.value
    }
} | Format-Table -AutoSize
```

JSON 的 VID/PID、usage page/usage 和长度都是十进制字符串。例如 VID `1133` 转为十六进制才是 `046D`，PID `50495` 转为 `C53F`；检索时用 `046D:C53F`，不要把原始十进制当十六进制。选定端点后检查每项的 `source` / `error`，例如 `$qbReport.collections[0].fields.product`；缺字符串时再看 `friendly_name`、`description`、`windows_manufacturer`、`bus_description`。这些字段可能描述驱动或接收器，不能直接当作零售品牌或已配对外设型号。

### 最小适配与夹具流程

新建 `src/device/<protocol>.rs`，实现 `ProtocolAdapter`，使用独立 ID 前缀；在 `ADAPTERS` 登记一次。发现返回 `Candidate` 与可靠的 `physical_id` / `endpoint`，读取只按选定身份返回通用 `Reading`。查询、锁和等待有界，失败与取消遵守缓存边界；无需改 Manager、UI、托盘或通知规则。

先在模块测试里加入已核对来源的最小“查询→回复→字段解释”夹具。至少包含正常、极值、截断、错误、无关回复和请求身份不匹配；涉及档位或充电时分别覆盖其有效与未知情况。夹具剥离本机路径、容器和序列号。运行 `tools/build.ps1 -SkipCopy -Offline`，再按下列步骤取得真实设备证据。只有实机成功的型号/场景才进入支持表，资料与夹具不能替代真机。

## 在本机开发

1. 用发布版的 `--scan` 查看相关设备与是否已确认支持；用 `--probe <id>` 验证指定设备。命令行需等待进程并捕获输出，不能仅凭启动成功判断已读到。只有扫描、读取成功才能记录相应证据。
2. 记录必要的 VID/PID、usage page/usage、报文长度、物理接收器及外设关系。一个设备可能暴露多个集合，一个接收器也可能连接多台外设。不要按显示名称或相同 VID/PID 合并。
3. 查厂商协议、标准字段和原始实现资料，说明每个请求为什么是查询。未知厂商命令不能靠自动遍历试发。
4. 明确读数来源。有百分比时保留它；纯档位由适配器解释编号方向后换算，如 3/4→75%。统一百分比在 0..100 内；总档位为零或编号越界应返回错误。不能凭电压套用其他电池曲线。
5. 使用通用描述和读数构造器。名称、类型来自协议或明确规则；不靠英文名称猜类型。保留必要的来源、能力与充电状态，诊断信息不塞进显示名称。
6. 建立身份与换设备规则。至少区分物理接收器和槽位；有硬件 unit identity 时应纳入校验。身份查询超时不能悄悄换成另一个 ID，只有明确不支持该功能时才允许保守回退。换配对设备不能继承上一台的电量。名称回退无法可靠区分同槽位重新配对的同型号设备；跨 USB 端口或更换接收器的身份限制也应明确记录。
7. 为解析增加必要的原始报文夹具：正常值、极值、档位换算、充电、错误、截断、无关回复、超时。测试不能访问真机或修改配对。
8. 真机重复读取，测试休眠保留、唤醒恢复、断开与重连、同类多台、同接收器多设备。不能完成的物理场景写“未测”，不能拿模拟代替。
9. 执行格式、单测、Clippy 和 release 构建；再验证 UI/托盘读取的是同一台设备。记录内存、CPU、句柄与菜单响应，确认没有因新适配器阻塞 UI。
10. 更新 README 支持表和本轮 validation 记录。已有历史验证记录不改数字。

## 适配器最低要求

- 仅查询，不修改 DPI、按键、灯效、固件、配对或驱动。
- 所有等待、查询次数和锁获取有边界。同物理接收器串行，不同设备结果按身份路由。
- 失败保留缓存，不能返回 0 代替异常；读取函数不能自行伪造“最近成功”。
- 回复核对设备、特性、函数及请求标识。允许设备通知经过，但不能把通知或旧回复当成本次结果。
- 接收器可能从另一个协议集合回复，监听对应集合但不打开正常键盘输入流做查询；应用内互斥不协调其他厂商软件。
- 明确句柄所有权；退出、超时、错误路径均释放资源。
- 不新增主线程读设备、无限自动重试、解释器或常驻服务。

## 提交支持证据

支持记录应包含：型号、连接方式、USB/协议标识、协议功能、读数来源、已执行场景、未执行场景、构建版本或哈希。附最小化的“查询 → 回复 → 字段解释”夹具和可重复执行命令，不收集无关设备序列号。

“协议已实现”允许其他智能体复用；“型号已实测”必须来自该型号真机。无需为了看起来兼容很多设备而列一串未经验证的型号。

## G304 已知基线

接收器 `046D:C53F`，vendor usage page `0xFF00`，短/长集合 usage `1/2`，实测设备索引 1、HID++ 4.2。名称功能为 `0x0005`，电量功能为 `0x1000`；功能 index 必须动态查询。能力 `04 00` 表示四档且没有精细估算标志；状态 `5A 32 00` 对应报告值 90、下一档 50、放电中。

这个组合是真机回归基线，不是型号限制。其余型号共享HID++功能不等于已真机测试；界面与Manager不依赖G304名称、四档能力或固定接收器数量。

只有身份功能明确“不支持”时才允许名称回退；超时不能更换身份。名称回退无法可靠区分同型号重新配到同一接收器槽位，新增适配须记录此限制。

协议结构参考：[罗技 HID++ 官方说明](https://github.com/Logitech/cpg-docs/blob/master/hidpp20/README.rst)。
