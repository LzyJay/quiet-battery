#![cfg_attr(not(test), windows_subsystem = "windows")]

mod app;
mod device;
mod icons;
mod launch;
mod licenses;
mod manager;
mod notifications;
mod save_dialog;
mod startup;
mod status_ipc;
mod storage;

use device::Registry;
use std::io::Write;
use windows_sys::Win32::Storage::FileSystem::WriteFile;
use windows_sys::Win32::System::Console::{
    ATTACH_PARENT_PROCESS, AttachConsole, GetStdHandle, STD_OUTPUT_HANDLE,
};

fn output(text: &str) {
    // A GUI-subsystem executable retains redirected handles. Attach only when absent.
    unsafe {
        let mut handle = GetStdHandle(STD_OUTPUT_HANDLE);
        if handle.is_null() || handle as isize == -1 {
            AttachConsole(ATTACH_PARENT_PROCESS);
            handle = GetStdHandle(STD_OUTPUT_HANDLE);
        }
        let bytes = format!("{text}\r\n");
        if !handle.is_null() && handle as isize != -1 {
            let mut written = 0;
            WriteFile(
                handle,
                bytes.as_ptr(),
                bytes.len() as u32,
                &mut written,
                std::ptr::null_mut(),
            );
        } else {
            let _ = std::io::stdout().write_all(bytes.as_bytes());
        }
    }
}

fn main() {
    let argument = std::env::args().nth(1);
    let _device_runtime = if matches!(argument.as_deref(), Some("--scan" | "--probe" | "--once")) {
        match Registry::runtime() {
            Ok(runtime) => Some(runtime),
            Err(error) => {
                output(&format!(
                    "{{\"error\":{}}}",
                    app::json_string(&error.to_string())
                ));
                std::process::exit(1);
            }
        }
    } else {
        None
    };
    let result = match argument.as_deref() {
        Some("--support-save-dialog") => save_dialog::run_helper(std::env::args().nth(2)),
        Some("--licenses") => {
            output(licenses::TEXT);
            Ok(())
        }
        Some("--version") => {
            output(concat!("Quiet Battery ", env!("CARGO_PKG_VERSION")));
            Ok(())
        }
        Some("--diagnose") => match diagnose_identifiers(std::env::args().skip(2)) {
            Ok(identifiers) => Registry::diagnose(&device::Cancellation::default(), identifiers)
                .map(|json| output(&json))
                .map_err(|error| error.to_string()),
            Err(error) => Err(error),
        },
        Some("--scan") => Registry::discover_cancelled(&device::Cancellation::default())
            .map(|report| {
                output(&format!(
                    "{{\"schema\":2,\"devices\":[{}],\"discovery_errors\":[{}]}}",
                    report
                        .candidates
                        .iter()
                        .map(app::descriptor_json)
                        .collect::<Vec<_>>()
                        .join(","),
                    report
                        .failures
                        .iter()
                        .map(|(provider, error)| format!(
                            "{{\"provider\":{},\"error\":{}}}",
                            app::json_string(provider),
                            app::json_string(error)
                        ))
                        .collect::<Vec<_>>()
                        .join(",")
                ))
            })
            .map_err(|error| error.to_string()),
        Some("--probe") => match std::env::args().nth(2) {
            Some(id) => Registry::read(&id)
                .map(|reading| {
                    output(&format!(
                        "{{\"id\":{},\"reading\":{}}}",
                        app::json_string(&id),
                        app::reading_json(&reading)
                    ))
                })
                .map_err(|e| e.to_string()),
            None => Err("--probe 需要 --scan 返回的设备 id".into()),
        },
        Some("--once") => match Registry::discover() {
            Ok(candidates) => match candidates
                .into_iter()
                .find_map(|candidate| candidate.reading)
            {
                Some(reading) => {
                    output(&app::reading_json(&reading));
                    Ok(())
                }
                None => Err("没有取得已确认支持设备的电量".into()),
            },
            Err(error) => Err(error.to_string()),
        },
        Some("--test-notification") => app::control(app::TEST_NOTIFICATION),
        Some("--status") => app::print_status(),
        Some("--refresh") => app::control(app::REFRESH),
        Some("--quit") => app::control(app::QUIT),
        Some("--show") => app::control(app::SHOW_PANEL),
        Some("--show-menu") => app::control(app::SHOW_MENU),
        Some("--hide-panel") => app::control(app::HIDE_PANEL),
        Some("--simulate-device-error") => app::control(app::SIMULATE_ERROR),
        Some("--simulate-recovery") => app::control(app::RECOVER),
        Some("--simulate-explorer-restart") => app::control(app::EXPLORER_RESTART),
        Some("--startup-enable") => startup::set_enabled(true).and_then(|()| print_startup()),
        Some("--startup-disable") => startup::set_enabled(false).and_then(|()| print_startup()),
        Some("--startup-status") => print_startup(),
        Some("--help" | "-h") => {
            output(
                "Quiet Battery\n默认打开主窗口并运行托盘；--background 仅后台托盘（登录自启使用）；--diagnose 被动 HID 元数据（可选 --include-identifiers 导出本机标识）；--scan 发现并查询电量；--probe <id> 查询指定设备；--once 读取一次；--status 当前托盘快照；--refresh 刷新；--quit 退出；--show 打开主窗口；--hide-panel 关闭主窗口；--show-menu 打开真实托盘菜单；--startup-status / --startup-enable / --startup-disable 管理当前用户登录启动。\n隔离通知自测：--test-notification。测试钩子：--simulate-device-error / --simulate-recovery / --simulate-explorer-restart。",
            );
            Ok(())
        }
        Some("--background") => app::run(launch::Mode::Background),
        Some(other) => Err(format!("未知参数: {other}")),
        None => app::run(launch::Mode::Window),
    };
    if let Err(error) = result {
        output(&format!("{{\"error\":{}}}", app::json_string(&error)));
        if launch::Mode::from_argument(argument.as_deref()).is_some() {
            launch::show_error(&error);
        }
        std::process::exit(1);
    }
}

fn print_startup() -> Result<(), String> {
    let state = startup::status()?;
    output(&format!(
        "{{\"exists\":{},\"enabled\":{},\"target\":{},\"shortcut\":{},\"arguments\":{}}}",
        state.exists,
        state.enabled,
        state
            .target
            .as_deref()
            .map(app::json_string)
            .unwrap_or_else(|| "null".into()),
        app::json_string(&state.path.to_string_lossy()),
        state
            .arguments
            .as_deref()
            .map(app::json_string)
            .unwrap_or_else(|| "null".into())
    ));
    Ok(())
}

fn diagnose_identifiers(arguments: impl Iterator<Item = String>) -> Result<bool, String> {
    let arguments: Vec<_> = arguments.collect();
    match arguments.as_slice() {
        [] => Ok(false),
        [flag] if flag == "--include-identifiers" => Ok(true),
        _ => Err("用法: --diagnose [--include-identifiers]".into()),
    }
}

#[cfg(test)]
mod cli_tests {
    use super::*;
    #[test]
    fn diagnostic_identifiers_require_explicit_opt_in() {
        assert!(!diagnose_identifiers(Vec::<String>::new().into_iter()).unwrap());
        assert!(diagnose_identifiers(vec!["--include-identifiers".into()].into_iter()).unwrap());
        assert!(
            diagnose_identifiers(vec!["--include-identifiers=true".into()].into_iter()).is_err()
        );
        assert!(
            diagnose_identifiers(vec!["--include-identifiers".into(), "extra".into()].into_iter())
                .is_err()
        );
    }
}
