# 构建、诊断与发布

## 构建

Windows x64、Rust stable、MSVC工具链与Windows SDK。当前验证工具链为Rust1.94.0。Cargo.lock锁定依赖，缓存留在 `.build/`。

```powershell
./tools/build.ps1                # fmt、test、Clippy、release，复制到dist
./tools/build.ps1 -SkipCopy      # 只暂存，不替换运行实例
./tools/build.ps1 -Offline       # 缓存完整时可离线
```

运行中的EXE先正常退出再替换；不要结束鼠标驱动或改变自启偏好。发布由维护者选择通过验证的成品。

`tools/github-actions-build.yml.example` 提供执行相同检查的工作流模板。当前GitHub登录不含workflow权限，因此未启用；有该权限的维护者可将其复制到 `.github/workflows/build.yml` 后提交。

图标已提交，普通构建不需要Python。修改图标后用Python/Pillow执行 `tools/generate_assets.py`，同时核对原生显示。升级依赖或Rust后执行 `python tools/update_notices.py`；需要对应rust-docs，或同一编译器的已提交声明。

## 诊断

GUI子系统EXE应等待退出并捕获UTF-8输出；设备ID用参数列表传递，不拼接未经转义的Shell命令。

| 参数 | 用途 |
|---|---|
| `--scan` / `--probe <id>` / `--once` | 扫描、读取指定设备、单次兼容读取 |
| `--status` / `--refresh` / `--quit` | 当前快照、后台刷新、正常退出 |
| `--startup-status` | 读取真实自启状态 |
| `--test-notification` | 隔离的原生通知自测，不改电量和提醒周期 |
| `--version` / `--licenses` | 版本与二进制内的完整许可 |

`--simulate-device-error`、`--simulate-recovery`、`--simulate-explorer-restart`是验证钩子，不是物理休眠、拔插或系统重启证据。公开截图不附原始状态文件与身份。

## 适度核验

设备修改后读一次真机；常驻逻辑修改后确认单实例、正常退出、错误保留/恢复与菜单响应。界面改动看实际窗口和托盘。

```powershell
./tools/sample_running.ps1 -DurationSeconds 30 -SampleSeconds 3
```

采样记录EXE哈希、Working Set、Private Bytes、CPU及GDI/USER/句柄。默认30秒，异常再延长，不裁剪工作集。生命周期检查先确认没有用户运行实例，再对测试启动的实例做启动/正常退出；自启偏好保持不变。菜单与可见界面操作遵守当前用户的桌面授权。

## 发布

1. 核对代码、支持范围、许可、截图；执行完整构建及必要真机检查。
2. 从 `dist/quiet-battery.exe` 复制成品为 `quiet-battery-windows-x64.exe`，生成SHA256与简明说明。
3. 用GitHub CLI创建Release并上传成品；发布后下载一次，核对哈希与 `--version`。
4. 不上传缓存、本地data、原始validation、旧设计和机器身份。许可与第三方声明已编入EXE，也可附文本供查阅。
