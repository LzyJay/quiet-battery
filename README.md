<div align=center>

<h1>Quiet Battery · 外设电量</h1>

<img src="assets/source/software/icon-256.png" width="120" alt="Quiet Battery">

<img src="https://img.shields.io/badge/platform-Windows%2010%2022H2-0078D4?style=for-the-badge&logo=windows&logoColor=white" />
<img src="https://img.shields.io/badge/Rust-2024%20edition-000000?style=for-the-badge&logo=rust&logoColor=white" />
<img src="https://img.shields.io/badge/version-0.5.0-2EA043?style=for-the-badge" />
<img src="https://img.shields.io/badge/license-PolyForm%20NC-3DA639?style=for-the-badge" />

<p>Windows 原生外设电量托盘：一个单文件 EXE，低频读取鼠标、键盘、耳机的电量。<br>
Rust + Win32 自绘界面，不装运行时、不常驻网页进程；按设备记住电量、托盘图标与低电提醒。</p>

</div>

## 支持平台

- Windows 10 22H2（当前实测环境）。Windows 11 未做同版本真机回归，结论见[验证记录](docs/VALIDATION.md)
- 单文件免安装，运行不需要 Rust、Python、SDK 或额外驱动
- 程序未做代码签名，Windows 可能提示「未知发布者」

## 界面

以下为 `main` 当前构建的实拍（深色模式）。主窗口没有标题栏，是一块自绘的弹出面板，默认钉在屏幕右下角任务栏上方，内容变多时底边不动、向上生长。

<p align=center><img alt="主窗口：设备行、历史使用入口、托盘与提醒开关" src="docs/images/main.png" width="560"></p>
<p align=center><sub>主窗口 · 每台设备一行，右上角是「历史使用」入口</sub></p>

<p align=center><img alt="设置：自启、提醒阈值、刷新间隔、窗口固定" src="docs/images/settings.png" width="560"></p>
<p align=center><sub>设置 · 左右两列，末排是「窗口固定」</sub></p>

<p align=center><img alt="添加设备：已监控与未适配设备" src="docs/images/add-devices.png" width="560"></p>
<p align=center><sub>添加设备 · 已监控的设备可移除，其他设备可复制信息</sub></p>

<p align=center><img alt="托盘图标：设备造型加电量尺" src="docs/images/tray.png" width="36"></p>
<p align=center><sub>托盘 · 设备造型 + 连续电量尺，悬停看百分比</sub></p>

## 功能

- [x] 无边框自绘面板：跟随系统浅色/深色，头部可拖动，Esc 或再点一次托盘图标收起
- [x] 默认钉在任务栏上方；「窗口固定」默认开启，关掉后可拖动并记住上次位置
- [x] 每台设备独立托盘图标，与主窗口共用同一套设备造型
- [x] 历史使用：每台设备的满电续航最长/最短（各留最近 5 次）与当前这一段
- [x] 电量显示：有百分比用设备值，纯档位由适配器归一化（如 3/4 → 75%），支持部分填充
- [x] 连接状态独立检查：活跃 15 秒、闲置 60 秒，只更新「是否连接」，不改电量数值
- [x] 电量刷新：默认 5 分钟兜底，BLE 通知可抢先；低于阈值提速到 2 分钟，长时间无输入放宽 4 倍
- [x] 断开即从列表和托盘移除，重连自动恢复；休眠或超时保留上次读数
- [x] 低电提醒：严格低于阈值提醒一次，严格超过后恢复资格；充电时抑制，重启不重置周期
- [x] 设置与读数持久化到 `%LOCALAPPDATA%\QuietBattery`，首次升级自动迁移旧 `data`
- [ ] EXE 代码签名（当前发布未签名，只能对照 SHA256 校验）
- [ ] GitHub Actions 自动构建与测试（仓库只有模板，未启用）

## 支持设备

| 协议 / 设备 | 当前范围 | 验证状态 |
|---|---|---|
| 罗技 G304（接收器 `046D:C53F`） | 报告 90 就显示 90%；数值随固件档位跳变 | 真机读取、错误保留与恢复已核 |
| 罗技 HID++ `1000` / `1004`、直连 FF、接收器槽位 1–6 | 动态识别名称、类型、身份与电量能力，没有写死 G304 | `1000` 已实测；`1004` 与其余型号待真机 |
| 达摩鲨 N3（`248A:FF30` / 配对 `248A:FF10`） | 官方只读命令核对配对身份后读电量 | 真机读到 9%；充电判定、物理拔插待补测 |
| Razer Viper V2/V3 Pro、DeathAdder V3 Pro、Basilisk V3 Pro | 明确接收器/有线 ID，电量与独立充电查询 | 已实现，待真机 |
| ATK / VXE / VGN（F1 Ultimate、A9 Ultimate、MAD R 系列、R1 Pro Max 等） | 明确接收器/有线 ID 与 17 字节协议 | 已实现，待真机 |
| 多彩 DELUX（Beken 状态通道） | M900 Pro、M900/M900 Mini、M800 Mini、M600 Pro/8K、M500 Pro、M700 Pro、M300 Pro 等；仅状态帧确认型号 | 已实现，待真机 |
| 迈从 MCHOSE A5 V3 Ultra（接收器 `3837:1014`） | HUB Protocol V2，64 字节 report `4D` | 真机连续三次读到 69%，校验和与充电字段已核 |
| 迈从 V9 Pro 2（接收器 `3837:6011`） | 只读命令 `01` 参数 `0B` | 真机读到 89%，但参数语义待确认（见 Q&A） |
| 经典蓝牙耳机（HFP `AT+BIEV` 缓存） | 读 Windows 在 `BthHFEnum` 保存的最近一次上报 | HONOR Earbuds S、Redmi Buds 4 Pro/3、荣耀亲选、MCHOSE V9 Pro 2 与系统一致 |
| 标准 BLE BAS `180F` / `2A19` | 优先 Notify 订阅，不可用时低频读取；可选 `2BED` 充电状态 | 曾取得 BT5.0 KB 的 100%；零售型号与订阅未确认 |
| 其他设备 | 需要核对共用协议或新增适配器 | 枚举到设备 ≠ 能读电量 |

**新增型号已经内置读取，可以直接运行尝试**，但报文测试不能代替真机验证。精确 VID/PID、控制集合与来源见[设备支持表](docs/DEVICE-SUPPORT.md)，逐项结果见[验证记录](docs/VALIDATION.md)。同品牌、相近名称或不同倍率接收器不自动视为兼容。

## 获取与构建

本仓库**只发源码**，不提供二进制下载，也没有 Releases 附件。要跑起来需要自行构建；`dist/` 下的 EXE 与 zip 是本地打包产物，已在 `.gitignore` 中排除。

需要 Windows、Rust stable（`rust-version = 1.85`，开发使用 1.99）、MSVC 工具链与 Windows SDK：

```powershell
git clone https://github.com/LzyJay/quiet-battery.git
cd quiet-battery
./tools/build.ps1
```

| 命令 | 作用 |
|---|---|
| `./tools/build.ps1` | 格式检查 + 单元测试 + Clippy（`-D warnings`）+ release 构建，产物 `dist/quiet-battery.exe` |
| `./tools/build.ps1 -CheckOnly` | 只跑检查，不产出 EXE |
| `./tools/build.ps1 -SkipCopy` | 完整构建，但把 EXE 留在 `.build/target/release/` |
| `./tools/build.ps1 -Offline` | 离线构建（依赖已缓存时可用） |

依赖与构建缓存都在 `.build/` 下，脚本会把 `CARGO_HOME`、`TARGET_DIR`、`TEMP` 指到那里，不污染全局环境。图标资产已入库，直接构建即可；只有修改图标时还需要 Python / Pillow（`tools/generate_assets.py`）。

## 使用方法

- **入口**：左键托盘图标打开主窗口，右键打开菜单；关闭窗口只是收起，菜单「退出」才结束进程
- **添加设备**：受支持的鼠标默认监控，其他设备在「添加设备」里挑。休眠或静止时可能没有新帧，按页面提示连续移动约 3 秒再手动刷新
- **上报适配信息**：在添加页对目标设备点「复制信息」或「导出文件」，收到的是该物理设备的 HID 接口、报告长度、匹配原因与上次读数；默认隐藏路径、容器 ID 和蓝牙地址，不查序列号、不自动配对

命令行（便于脚本与排查）：

| 参数 | 作用 |
|---|---|
| `--show` / `--hide-panel` | 打开 / 收起主窗口 |
| `--show-menu` | 打开真实托盘菜单 |
| `--status` | 以 JSON 返回当前快照（含 `pid`、各设备读数与连接状态） |
| `--refresh` | 请求一次刷新 |
| `--background` | 仅后台托盘，登录自启用这个模式 |
| `--startup-status` / `--startup-enable` / `--startup-disable` | 查看与管理当前用户登录启动 |
| `--scan` / `--probe <id>` / `--once` | 发现并查询 / 查询指定设备 / 读取一次 |
| `--diagnose` | 被动 HID 元数据，加 `--include-identifiers` 才导出本机标识 |
| `--licenses` / `--version` / `--quit` | 导出许可文本 / 版本 / 退出 |

## 数据与配置

设置与上次成功电量统一保存在用户目录，EXE 旁不创建任何文件或文件夹：

```
%LOCALAPPDATA%\QuietBattery\
└── devices-v2.state     设置、每台设备的缓存读数与提醒周期
```

- 首次升级会从旧 EXE 旁的 `data` 迁移：先写完整临时文件、逐字节核对后才清理旧设置，只删除空的旧 `data` 目录
- 新位置已有设置时以新位置为准，冲突的旧设置保留不覆盖；若迁移前移动过 EXE，把旧 `data` 放回新 EXE 旁运行一次即可
- 文件头无法识别、被占用或拒绝读取时停止启动并保留原文件，不会用默认设置覆盖
- 移动 EXE 不影响设置；开启自启后移动 EXE 需要重新设置自启。卸载时先关闭自启并退出，再删 EXE 与上述目录

## Q&A

<details>
<summary>使用者 Q&A</summary>

#### Q：Windows 提示「未知发布者」？
A：程序未做代码签名，这是预期行为。拿到打包 zip 时可对照同目录 `SHA256SUMS` 校验；从源码构建则没有可比对的官方哈希。

#### Q：为什么某台设备显示「未适配」？
A：枚举到设备不等于能读电量。要么是共用协议的型号还没核对，要么需要新增适配器；此时点「复制信息」发给维护者，比猜测型号更有用。

#### Q：V9 Pro 2 显示 89%，厂商软件显示 90%？
A：接收器参数 `0B` 返回稳定值但语义未确认，无法排除它表示音量。厂商软件的 90% 与 HFP 缓存值相同，两者都不是电量实测，因此不按厂商数值改写本机读数。

#### Q：同一副耳机在列表里出现两次？
A：如果它同时走 HFP 缓存和 2.4G 接收器，两条记录物理身份不同，按规则分列显示、不按名称去重。合并需要跨传输的可靠身份依据。

#### Q：关闭窗口后还在后台跑？
A：这是托盘工具的既定行为：关闭只收起窗口。要结束进程用托盘菜单「退出」或 `--quit`。

</details>

<details>
<summary>开发与贡献 Q&A</summary>

#### Q：为什么 `cargo test` 有时和脚本结果不一致？
A：脚本把 `CARGO_HOME`、`TARGET_DIR`、`TEMP` 都指到 `.build/`，并默认串行执行测试（原生 GDI 资源断言会互相干扰）。本地复现请用 `./tools/build.ps1 -CheckOnly`。

#### Q：加一个型号要改哪里？
A：优先复用已有协议适配器；确需新增时在 `src/device/` 下实现 `ProtocolAdapter` 并登记到 `ADAPTERS`，同时补报文测试与[设备支持表](docs/DEVICE-SUPPORT.md)。流程见[新增厂商与型号](docs/ADDING_DEVICES.md)。

#### Q：可以只跑 release 构建吗？
A：`./tools/build.ps1 -CheckOnly` 只做检查；`-SkipCopy` 会构建但不复制到 `dist/`。直接 `cargo build --release` 也能编译，但不会带上静态 CRT 与路径重映射等发布参数。

</details>

## 开发

| 目录 / 文件 | 职责 |
|---|---|
| `src/app.rs` | 主线程消息循环、四页界面自绘、后台任务桥、Shell 菜单 |
| `src/shapes.rs` | 面板几何与绘制基元 |
| `src/manager.rs` | 按物理身份管理监控/托盘/提醒选择、读数缓存与状态迁移 |
| `src/storage.rs` | `%LOCALAPPDATA%` 状态读写、原子发布与旧目录迁移 |
| `src/device/` | 协议适配器：发现、查询、取消、读数；`ADAPTERS` 静态登记协议家族 |
| `src/device/support.rs` | 适配报告（复制信息 / 导出文件）的分层采集与隐私裁剪 |
| `src/icons.rs` | 按类型与电量生成 HICON，主窗口与托盘同源 |
| `src/notifications.rs` | 低电提醒周期与 Windows 通知投递 |
| `src/status_ipc.rs` | `--status` 的临时消息窗口与 `WM_COPYDATA` 回复 |
| `src/launch.rs` / `src/startup.rs` | 启动模式、可见启动错误、登录自启快捷方式 |
| `src/save_dialog.rs` | 同 EXE 的短时保存窗口子进程 |
| `assets/` | 设备图标（16 类）、掩码、源图与调色板 |
| `docs/` | 架构、设备支持表、验证记录、设计约定、后续任务 |
| `tools/` | `build.ps1`、资产再生成、真实窗口截图脚本 |

- 目录职责与硬约束详见[实现结构](docs/ARCHITECTURE.md)，视觉与交互约定见[视觉约定](docs/DESIGN.md)
- 版本号只改 `Cargo.toml`；界面、结构与调试细节见[构建、诊断和发布](docs/DEVELOPMENT.md)，代理协作约定见[AGENTS.md](AGENTS.md)
- 提交信息用中文/英文约定式提交，一个提交只做一件事；贡献流程见[参与开发](CONTRIBUTING.md)

> 当前 `main` 比最近的标签 `v0.5.0` 多 12 个提交：无边框面板、贴底定位、历史使用页等改动已可在 main 使用，但**尚未发版**。

## 致谢

- Windows 自带的时钟/日历面板 —— 无标题栏弹出面板的交互参考

## 许可

源码公开，采用 [PolyForm Noncommercial 1.0.0](LICENSE)，Copyright (c) 2026 LzyJay。许可范围内可非商业使用、修改与分发；未获该许可允许的商业用途须取得[单独授权](COMMERCIAL.md)。

这属于非商业源码许可，**不是 OSI 定义的开源许可**。第三方依赖保留各自许可，见[第三方声明](THIRD_PARTY_NOTICES.txt)；EXE 内也包含许可，可用 `--licenses` 导出。
