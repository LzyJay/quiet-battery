# Quiet Battery · 外设电量

<img src="assets/source/software/icon-256.png" width="80" alt="Quiet Battery">

Windows 原生外设电量托盘工具。Rust + Win32，单 EXE，后台低频读取；按设备保存电量、托盘选择和低电提醒。

**[下载 Windows x64 测试版](https://github.com/TheWebDog/quiet-battery/releases/tag/v0.1.0)** · [设备扩展](docs/ADDING_DEVICES.md) · [商业授权](COMMERCIAL.md)

## 使用

1. 在 Releases 下载 `quiet-battery-windows-x64.exe`，放到准备长期保留的文件夹后运行。无需安装 Rust、Python 或网页运行时。
2. 左键托盘打开主窗口，右键打开菜单。受支持的鼠标默认监控；其他设备通过“添加设备”选择。
3. 在“设置”开启自启、设置低电阈值，以及分别选择设备的托盘和提醒。关闭窗口继续运行；菜单“退出”才结束程序。

EXE 旁的 `data` 文件夹保存本地偏好和上次成功电量。移动 EXE 后重新设置自启。卸载时先关闭自启、退出，再删除 EXE 和数据文件夹。

程序未做代码签名。Windows 如提示未知发布者，可先对照 Release 中的 SHA256 校验下载文件。

## 界面

以下为运行程序的实际托盘截图。

![托盘](docs/images/tray.png)

## 功能

- 型号、真实百分比、十格满量程电池和手动刷新。精细数据支持部分填充；纯档位由适配器归一化，例如3/4→75%。
- 休眠或超时保留上次电量；确认断开才隐藏，重连自动恢复。没有成功值时显示未知。
- 每台设备独立托盘：设备填色图标与连续电量尺，悬停查看百分比。主窗口与托盘共用设备造型。
- 默认低电阈值20%：严格低于时提醒一次，严格超过后恢复资格；充电时抑制，确认充电结束且仍低电时再提醒。重启、重连、关闭再开启提醒不重置已通知周期。使用 Windows 原生通知。
- 设备提供可确认的充电状态时显示闪电，不猜测充电和续航。

## 支持范围

| 设备/协议 | 当前范围 |
|---|---|
| 罗技 G304，接收器046D:C53F | 已完成真机读取、错误保留及恢复检查；报告90就显示90%，数值随固件档位跳变 |
| Logitech HID++ | 已实现0x1000和能力校验后的0x1004；后者仅报文夹具验证，其他型号仍需本机验证 |
| 标准蓝牙 BAS：180F / 2A19 | 已实现已配对、当前连接且已有访问许可设备的标准电池读取及变化订阅；尚无 BAS 外设真机验证 |
| 其他厂商 | 需要验证共用协议或增加独立适配器；枚举到设备不等于能读电量 |

16类图标覆盖不等于16类硬件协议都已适配。未适配设备仅可复制适配信息。程序不联网、不上传设备数据。

资源实测与验证边界见 [验证记录](docs/VALIDATION.md)，数值对应单台 G304 和短时托盘运行，不代表长期稳定性或所有外设的结果。

## 构建与继续开发

需要 Windows、Rust stable（本轮使用1.94.0）、MSVC 工具链及 Windows SDK：

```powershell
./tools/build.ps1
```

脚本执行格式检查、单元测试、Clippy 和 release 构建，成品为 `dist/quiet-battery.exe`。依赖及构建缓存保存在 `.build` 下。提交的图标资产可直接构建；只有修改图标时才需要 Python / Pillow。

- [实现结构与技术选择](docs/ARCHITECTURE.md)
- [新增厂商与型号](docs/ADDING_DEVICES.md)
- [构建、诊断和发布](docs/DEVELOPMENT.md)
- [视觉与交互约定](docs/DESIGN.md)
- [后续任务](docs/NEXT-TASKS.md)
- [代理约定](AGENTS.md) · [参与开发](CONTRIBUTING.md)

## 许可与来源

源码公开，采用 [PolyForm Noncommercial 1.0.0](LICENSE)。许可范围内可非商业使用、修改和分发；未获该许可允许的商业用途须取得[单独授权](COMMERCIAL.md)。这属于非商业源码许可，并非 OSI 定义的开源许可。依赖保留各自的[第三方许可](THIRD_PARTY_NOTICES.txt)，EXE 内也包含许可，可用 `--licenses` 导出。

灵感来自 [2.4G-battery-viewer](https://github.com/yszyszysz233/2.4G-battery-viewer)。当前实现使用 Rust、公开 HID++ / BAS 协议资料及本机验证；该项目列出的 ATK / DARKRIM 设备不因此成为本项目已支持型号。
