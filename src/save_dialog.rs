//! Native save picker in a short-lived instance of this same EXE. Shell dialog
//! caches die with that process instead of inflating the resident tray process.
use crate::{device::DeviceError, startup::wide};
use std::path::PathBuf;
use windows_sys::Win32::{
    Foundation::HWND,
    System::LibraryLoader::{GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW},
    UI::Controls::Dialogs::*,
};
#[cfg(not(test))]
const RESPONSE_LIMIT: u64 = 128 * 1024;
fn decode(text: &str) -> Result<Option<PathBuf>, DeviceError> {
    let text = text.trim_end_matches(['\r', '\n']);
    if text == "cancelled" {
        return Ok(None);
    }
    let path = text
        .strip_prefix("path:")
        .filter(|p| !p.is_empty() && !p.contains(['\r', '\n', '\0']))
        .ok_or_else(|| DeviceError::Protocol("保存窗口未返回有效路径".into()))?;
    let path = PathBuf::from(path);
    if !path.is_absolute() {
        return Err(DeviceError::Protocol("保存路径不是绝对路径".into()));
    }
    Ok(Some(path))
}
#[cfg(not(test))]
pub fn choose(
    owner: usize,
    cancel: &crate::device::Cancellation,
) -> Result<Option<PathBuf>, DeviceError> {
    use std::os::windows::process::CommandExt;
    use std::{
        io::Read,
        process::{Command, Stdio},
        time::Duration,
    };
    cancel.check()?;
    let mut child =
        Command::new(std::env::current_exe().map_err(|e| DeviceError::Io(e.to_string()))?)
            .arg("--support-save-dialog")
            .arg(owner.to_string())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .creation_flags(0x08000000)
            .spawn()
            .map_err(|e| DeviceError::Io(e.to_string()))?;
    let output = child.stdout.take().unwrap();
    // Drain while the picker is alive: long UTF-8 paths can exceed a pipe buffer.
    // This small reader exists only for this export and is joined on every path.
    let reader = match std::thread::Builder::new()
        .name("save-picker-output".into())
        .stack_size(128 * 1024)
        .spawn(move || {
            let mut bytes = Vec::new();
            output
                .take(RESPONSE_LIMIT + 1)
                .read_to_end(&mut bytes)
                .map(|_| bytes)
        }) {
        Ok(reader) => reader,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(DeviceError::Io(error.to_string()));
        }
    };
    let outcome = loop {
        if let Err(error) = cancel.check() {
            break Err(error);
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                break if status.success() {
                    Ok(())
                } else {
                    Err(DeviceError::Unavailable("保存窗口未正常完成".into()))
                };
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(error) => break Err(DeviceError::Io(error.to_string())),
        }
    };
    if outcome.is_err() {
        let _ = child.kill();
        let _ = child.wait();
    }
    let output = reader.join();
    outcome?;
    let bytes = output
        .map_err(|_| DeviceError::Io("保存窗口输出线程异常".into()))?
        .map_err(|e| DeviceError::Io(e.to_string()))?;
    if bytes.len() as u64 > RESPONSE_LIMIT {
        return Err(DeviceError::Protocol("保存窗口输出超限".into()));
    }
    let text = String::from_utf8(bytes)
        .map_err(|_| DeviceError::Protocol("保存窗口输出不是UTF-8".into()))?;
    decode(&text)
}
// A crashed tray process cannot cancel its outstanding picker. Keep an exact
// process handle (rather than polling a reusable PID) and end this temporary
// process if its owner exits. No watcher exists in the resident tray process.
fn watch_owner(owner: HWND) -> Result<(), String> {
    use windows_sys::Win32::{
        Foundation::CloseHandle,
        System::Threading::{
            GetCurrentProcessId, INFINITE, OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
        },
        UI::WindowsAndMessaging::GetWindowThreadProcessId,
    };
    let mut pid = 0;
    unsafe {
        GetWindowThreadProcessId(owner, &mut pid);
    }
    if pid == 0 || pid == unsafe { GetCurrentProcessId() } {
        return Err("保存窗口所属进程无效".into());
    }
    let process = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
    if process.is_null() {
        return Err(format!(
            "无法监视保存窗口所属进程：{}",
            std::io::Error::last_os_error()
        ));
    }
    let handle = process as usize;
    if let Err(error) = std::thread::Builder::new()
        .name("save-picker-owner".into())
        .stack_size(128 * 1024)
        .spawn(move || {
            unsafe {
                WaitForSingleObject(handle as _, INFINITE);
            }
            unsafe {
                CloseHandle(handle as _);
            }
            // Losing the owner or the wait handle leaves no valid recipient.
            std::process::exit(1);
        })
    {
        unsafe {
            CloseHandle(process);
        }
        return Err(format!("无法监视保存窗口所属进程：{error}"));
    }
    Ok(())
}
/// Only the explicit helper CLI executes this. No monitor, settings, BAS
/// runtime or single-instance lock is initialized in the helper.
pub fn run_helper(argument: Option<String>) -> Result<(), String> {
    let _com = crate::startup::Com::init()?;
    let owner = argument
        .and_then(|s| s.parse::<usize>().ok())
        .ok_or("保存窗口参数无效")? as HWND;
    if owner.is_null()
        || unsafe { windows_sys::Win32::UI::WindowsAndMessaging::IsWindow(owner) } == 0
    {
        return Err("保存窗口所属主窗口已关闭".into());
    }
    watch_owner(owner)?;
    // Resolve dynamically so the tray instance does not even load comdlg32.
    // Leave the module loaded until this short-lived helper exits.
    let module = unsafe {
        LoadLibraryExW(
            wide("comdlg32.dll").as_ptr(),
            std::ptr::null_mut(),
            LOAD_LIBRARY_SEARCH_SYSTEM32,
        )
    };
    if module.is_null() {
        return Err("无法加载Windows保存窗口".into());
    }
    let get = unsafe { GetProcAddress(module, c"GetSaveFileNameW".as_ptr().cast()) }
        .ok_or("Windows保存接口不可用")?;
    let error = unsafe { GetProcAddress(module, c"CommDlgExtendedError".as_ptr().cast()) }
        .ok_or("Windows保存接口不可用")?;
    let get: unsafe extern "system" fn(*mut OPENFILENAMEW) -> i32 =
        unsafe { std::mem::transmute(get) };
    let error: unsafe extern "system" fn() -> u32 = unsafe { std::mem::transmute(error) };
    let mut file = vec![0u16; 32768];
    let initial = wide("QuietBattery-device.json");
    file[..initial.len()].copy_from_slice(&initial);
    let filter = wide("JSON 文件 (*.json)\0*.json\0");
    let title = wide("导出设备适配信息");
    let extension = wide("json");
    let mut dialog = OPENFILENAMEW {
        lStructSize: std::mem::size_of::<OPENFILENAMEW>() as u32,
        hwndOwner: owner,
        lpstrFilter: filter.as_ptr(),
        nFilterIndex: 1,
        lpstrFile: file.as_mut_ptr(),
        nMaxFile: file.len() as u32,
        lpstrTitle: title.as_ptr(),
        lpstrDefExt: extension.as_ptr(),
        Flags: OFN_EXPLORER
            | OFN_PATHMUSTEXIST
            | OFN_OVERWRITEPROMPT
            | OFN_NOCHANGEDIR
            | OFN_NOTESTFILECREATE,
        ..unsafe { std::mem::zeroed() }
    };
    if unsafe { get(&mut dialog) } != 0 {
        let length = file
            .iter()
            .position(|v| *v == 0)
            .ok_or("保存路径未正确终止")?;
        crate::output(&format!(
            "path:{}",
            String::from_utf16_lossy(&file[..length])
        ));
        Ok(())
    } else {
        let code = unsafe { error() };
        if code == 0 {
            crate::output("cancelled");
            Ok(())
        } else {
            Err(format!("无法选择导出位置（Windows错误{code}）"))
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn helper_protocol_preserves_unicode_and_cancellation_without_relative_paths() {
        assert_eq!(decode("cancelled\r\n").unwrap(), None);
        assert_eq!(
            decode("path:C:\\导出目录\\设备信息.json\r\n").unwrap(),
            Some(PathBuf::from("C:\\导出目录\\设备信息.json"))
        );
        for bad in [
            "",
            "path:",
            "path:relative.json",
            "path:C:\\file\nextra",
            "{\"error\":\"failure\"}",
        ] {
            assert!(decode(bad).is_err());
        }
    }
}
