//! Explicit launch modes and native, visible startup failures. No error log files.
use crate::{app, startup::wide};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    MB_ICONERROR, MB_OK, MB_SETFOREGROUND, MessageBoxW,
};
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Window,
    Background,
}
impl Mode {
    pub fn from_argument(argument: Option<&str>) -> Option<Self> {
        match argument {
            None => Some(Self::Window),
            Some("--background") => Some(Self::Background),
            _ => None,
        }
    }
    pub fn show_window(self) -> bool {
        self == Self::Window
    }
    pub fn existing_command(self) -> usize {
        if self.show_window() {
            app::SHOW_PANEL
        } else {
            app::REFRESH
        }
    }
}
fn failure_text(error: &str) -> String {
    let advice =
        if error.contains("迁移") || error.contains("LocalAppData") || error.contains("设置文件")
        {
            "请检查设置文件和数据目录的访问权限，或关闭占用文件的程序后重试。原设置不会被覆盖。"
        } else if error.contains("已有实例") {
            "请稍候重试；若仍无响应，请从托盘退出旧实例后重新启动。"
        } else {
            "请重新启动软件。若仍失败，请保留这条错误信息以便排查。"
        };
    format!("{error}\n\n{advice}")
}
pub fn show_error(error: &str) {
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            wide(&failure_text(error)).as_ptr(),
            wide("Quiet Battery 启动失败").as_ptr(),
            MB_OK | MB_ICONERROR | MB_SETFOREGROUND,
        );
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_app_launches_show_errors_and_manual_launch_reuses_window() {
        assert_eq!(Mode::from_argument(None), Some(Mode::Window));
        assert_eq!(
            Mode::from_argument(Some("--background")),
            Some(Mode::Background)
        );
        for arg in ["--status", "--diagnose", "--once", "--unknown"] {
            assert!(Mode::from_argument(Some(arg)).is_none());
        }
        assert_eq!(Mode::Window.existing_command(), app::SHOW_PANEL);
        assert_eq!(Mode::Background.existing_command(), app::REFRESH);
        let text = failure_text("无法迁移本地设置：拒绝访问");
        assert!(text.contains("拒绝访问"));
        assert!(text.contains("原设置不会被覆盖"));
        assert!(text.contains("访问权限"));
    }

    #[test]
    fn native_failure_dialog_child() {
        use windows_sys::Win32::System::StationsAndDesktops::*;
        let Ok(name) = std::env::var("QUIET_BATTERY_TEST_DIALOG_DESKTOP") else {
            return;
        };
        unsafe {
            let desktop = OpenDesktopW(
                wide(&name).as_ptr(),
                0,
                0,
                DESKTOP_CREATEWINDOW | DESKTOP_READOBJECTS | DESKTOP_WRITEOBJECTS,
            );
            assert!(!desktop.is_null());
            assert_ne!(SetThreadDesktop(desktop), 0);
            use std::os::windows::fs::OpenOptionsExt;
            let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(format!(
                ".build/tmp/native-startup-{}.state",
                std::process::id()
            ));
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let original = b"QuietBattery\t6\npreferences\t0\t37\n";
            std::fs::write(&path, original).unwrap();
            let lock = std::fs::OpenOptions::new()
                .read(true)
                .share_mode(0)
                .open(&path)
                .unwrap();
            let error = crate::manager::Manager::load(&path)
                .err()
                .expect("real sharing violation");
            assert!(error.contains("无法读取设置文件"));
            show_error(&error);
            drop(lock);
            assert_eq!(std::fs::read(&path).unwrap(), original);
            std::fs::remove_file(path).unwrap();
            CloseDesktop(desktop);
        }
    }
    #[test]
    fn native_failure_dialog_on_private_desktop_keeps_user_focus() {
        use std::{
            os::windows::process::CommandExt,
            process::{Command, Stdio},
            ptr::{null, null_mut},
            time::{Duration, Instant},
        };
        use windows_sys::Win32::{
            Foundation::*, System::StationsAndDesktops::*, UI::WindowsAndMessaging::*,
        };
        unsafe extern "system" fn find(hwnd: HWND, data: isize) -> i32 {
            let mut name = [0u16; 80];
            let n = unsafe { GetWindowTextW(hwnd, name.as_mut_ptr(), 80) };
            if String::from_utf16_lossy(&name[..n.max(0) as usize]) == "Quiet Battery 启动失败"
            {
                unsafe { *(data as *mut HWND) = hwnd };
                return 0;
            }
            1
        }
        unsafe {
            let foreground = GetForegroundWindow();
            let name = format!("QuietBattery.Failure.Test.{}", std::process::id());
            let desk = CreateDesktopW(
                wide(&name).as_ptr(),
                null(),
                null(),
                0,
                DESKTOP_CREATEWINDOW
                    | DESKTOP_ENUMERATE
                    | DESKTOP_READOBJECTS
                    | DESKTOP_WRITEOBJECTS,
                null(),
            );
            assert!(!desk.is_null());
            let mut child = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "launch::tests::native_failure_dialog_child",
                    "--nocapture",
                ])
                .env("QUIET_BATTERY_TEST_DIALOG_DESKTOP", &name)
                .creation_flags(0x08000000)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            let desktop = desk as usize;
            let control = std::thread::spawn(move || {
                assert_ne!(SetThreadDesktop(desktop as HDESK), 0);
                let start = Instant::now();
                let mut dialog: HWND = null_mut();
                let mut ok = false;
                while start.elapsed() < Duration::from_secs(4) {
                    EnumDesktopWindows(
                        desktop as HDESK,
                        Some(find),
                        &mut dialog as *mut _ as isize,
                    );
                    if !dialog.is_null() {
                        ok = true;
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                if ok {
                    PostMessageW(dialog, WM_CLOSE, 0, 0);
                }
                (!dialog.is_null(), ok)
            });
            let (found, ok) = control.join().unwrap();
            let deadline = Instant::now();
            let mut result = None;
            while deadline.elapsed() < Duration::from_secs(2) {
                result = child.try_wait().unwrap();
                if result.is_some() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            if result.is_none() {
                let _ = child.kill();
            }
            let _ = child.wait();
            CloseDesktop(desk);
            assert!(found, "Native failure dialog not found");
            assert!(ok);
            assert!(
                result.is_some_and(|s| s.success()),
                "Dialog child did not exit cleanly"
            );
            assert_eq!(foreground, GetForegroundWindow());
        }
    }
}
