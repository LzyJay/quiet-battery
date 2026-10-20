# 设备支持与协议依据

本表以v0.3.0为基线，区分**本项目真机通过**和**已实现、待真机确认**。后者已内置发现、查询、解析和测试，可直接下载尝试，不是仅登记型号；但公开资料和模拟事务不能证明特定固件、接收器或Windows驱动一定可用。

## 已有真机证据

| 设备 | 连接与范围 | 未完成验证 |
|---|---|---|
| Logitech G304 | 046D:C53F接收器；HID++ 4.2 / 1000，曾真实取得90% | 其他接收器/型号、真实充电、实体拔插等不能沿用结论 |
| Darmoshark N3 | 248A:FF30接收器配对248A:FF10，历史真实9% | 非零充电字段含义尚未实现；同型号换配对身份限制 |
| 广播名BT5.0 KB的BLE设备 | 标准180F / 2A19，历史真实100%；未确认零售型号 | 变化订阅、可选充电、断开重连 |

以上历史记录不是v0.3.0对全部场景的重复实测；本次结果见[验证记录](VALIDATION.md)。

## 本次内置：已实现、待真机确认

### Logitech HID++ 家族

不按零售型号建立白名单。入口是VID046D、usage FF00:0001/0002；先探测直连地址FF，失败再查询接收器槽位1–6。设备名称、类型、身份和电量功能动态查询。只有1000或1004确实返回有效读数才认定支持；1004按能力区分百分比与档位。已补慢速充电状态4。

首批目标：**G305、PRO Wireless（GPW）、PRO X SUPERLIGHT（GPX）、G502 LIGHTSPEED、MX Master / MX Keys中提供上述功能的型号**。G304仍为已有实测基线；这些目标没有在本项目逐型号真机通过。USB、蓝牙或接收器必须实际暴露可用HID++接口；不能从系列名推定所有连接方式。仅提供电压、HID++1.x或其他电量特性的设备本次不新增估算支持。

### ATK / VXE / VGN：Compx 17字节家族

| 型号 | VID | 2.4G接收器PID | 有线PID |
|---|---|---|---|
| ATK F1 Ultimate | 373B | 1031 | 102E |
| ATK A9 Ultimate | 373B | 11D9 | 11B6 |
| VXE MAD R | 373B | 104D | 103F |
| VXE MAD R Major Plus | 373B | 1040 | 104C |
| VXE R1 Pro Max | 3554 | F58A | F58C |
| VXE R1 SE+ | 3554 | F58E | F58F |
| VGN F1 Pro | 3554 | F503 | F502 |
| VGN F2 Pro Max | 3554 | FB3E | FB3D |

必须同时符合vendor集合FF02:0002、Input/Output长度17。仅识别厂商号、型号文字或系列名不会发送查询。型号其他后缀、替换接收器、1K/4K/8K版本不能自动套用。ATK Zero的64字节协议不在本次范围。

只读事务：report08 / command04，固定请求 `08 04 00 00 00 00 00 00 00 00 00 00 00 00 00 00 49`。回复须同report/command、长度17、全包求和模256为55；百分比在字节6，电源标记在字节7（0放电、1充电、其他未知）。拒绝请求回显、超范围百分比及损坏包。查询前有界排空该vendor输入队列，不读取普通鼠标/键盘输入。

公开来源的35%报文已加入解析测试，其他边界与事务是合成夹具。协议没有逐次请求序号：即使排空旧包、应用内互斥和校验，也无法完全协调厂商软件。

### Razer：90字节电源协议

所有VID为1532；下表仅覆盖这些明确PID。

| 型号 | 2.4G接收器PID | 有线PID |
|---|---|---|
| Viper V2 Pro | 00A6 | 00A5 |
| Viper V3 Pro | 00C1 | 00C0 |
| DeathAdder V3 Pro | 00B7 / 00C3 | 00B6 / 00C2 |
| Basilisk V3 Pro | 00AB | 00AA |

只访问USB接口MI00且Feature长度91（含Windows report-ID槽位）的控制集合。以零访问权限句柄提交Feature，不打开输入流；未满足入口规则保留不可用信息。不同描述符版本若不符合，需补实证，不能扫描任意接口试命令。

90字节payload：transaction=1F，class=07；80读电量、84读充电，data-size=02，CRC为payload字节2–87异或。Windows缓冲前加report-ID零。检查长度、事务、分包/协议字段、class/command、CRC和成功状态；回显、busy、外来命令继续有界等待，超时/失败不生成电量。电量原值0–255按最近整数换算到0–100%，有效零值不会被猜成休眠。充电独立读取，失败只将充电状态置未知，保留已经成功的电量。

共享HyperPolling接收器00B3、其他鼠标、键盘、蓝牙私有协议没有据此加入；共享接收器还需要可靠确认配对型号。

## 来源与实现边界

这些链接用于核对协议事实、型号标识和报文，未将第三方驱动作为运行依赖，也未导入其源码。Rust适配器按上述协议契约独立编写。第三方项目中的“支持”或真机记录不改写成本项目的实测结论。

- [Logitech官方HID++资料](https://github.com/Logitech/cpg-docs)：基础报文与功能模型。另对照[Solaar HID++实现](https://github.com/pwr-Solaar/Solaar/blob/e7304c4c451cc9bb4f206a914844525e67856a28/lib/logitech_receiver/hidpp20.py)的电量状态定义。
- [mouse_tray Nordic52协议及型号表](https://github.com/Fan4Metal/mouse_tray/blob/5dfb40d0fcbf98b8d14aa2ba47a7ed6768c466d7/mouse_tray/drivers/chipset/nordic52.py)：17字节请求、35%公开回复及8组ID。
- [OpenRazer报文结构](https://github.com/openrazer/openrazer/blob/477f4d6d3ab7e9deb6310d5bd3742560c5cbe80d/driver/razercommon.h)、[电源命令](https://github.com/openrazer/openrazer/blob/477f4d6d3ab7e9deb6310d5bd3742560c5cbe80d/driver/razerchromacommon.c)、[接口和事务规则](https://github.com/openrazer/openrazer/blob/477f4d6d3ab7e9deb6310d5bd3742560c5cbe80d/driver/razermouse_driver.c)、[型号ID](https://github.com/openrazer/openrazer/blob/477f4d6d3ab7e9deb6310d5bd3742560c5cbe80d/driver/razermouse_driver.h)。与部分简化工具不同，不交换data-size和command-class位置。

## 没有硬件时的验收范围

1. 固定来源版本，逐字段核对查询只读性、接口、长度、请求和回复；对未知协议不试发枚举命令。
2. 测试实际查询函数的模拟完整事务，覆盖回显、无关回复、busy、错误、超时/次数上限、充电失败和取消；不是只测试一个孤立的字节解析函数。
3. 测试所有profile的VID/PID及控制集合筛选、0/100与越界、报文截断、损坏校验、物理身份和协议路由隔离。使用原生命名管道核验底层收发及取消所有权。
4. 跑fmt、单测、Clippy和release，使用可用的G304回归，短时检查进程和资源；下载发布成品再核对哈希和版本。
5. 这些检查足以发布**明确标注待真机确认的内置适配**，不等于新型号真机认证。后续实际读数、固件/接收器差异、充电、休眠/唤醒、拔插、厂商驱动并发与多台设备仍需独立记录。

新增Compx/Razer身份由物理容器（缺失时按接口路径回退）、VID/PID与有线/接收器路由组成；未读取独立配对unit ID，同接收器重配同型号可能沿用旧缓存。有线与接收器未证明属于同一台时保持分开，可能出现两条记录；不按同名合并，避免误吞同型号的另一台设备。多槽位接收器的实体休眠与换配对场景仍待测。

## 后续接手

新型号先按[扩展指南](ADDING_DEVICES.md)导出匿名诊断。协议相同就补profile和来源；差异则补规则与事务夹具，全新协议才加模块。先核对集合长度和接口再改筛选条件，不为“能枚举”放宽到向所有HID发送命令。取得真机后记录构建哈希、确切型号/接收器、连接方式、连续读数及失败恢复，才升级相应型号和场景的验证状态。
