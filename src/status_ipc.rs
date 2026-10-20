//! On-demand status via bounded Win32 messages. No snapshots on disk, pipes or threads.
use crate::startup::wide;
use std::{
    ptr::{null, null_mut},
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::*,
    System::{DataExchange::COPYDATASTRUCT, LibraryLoader::GetModuleHandleW},
    UI::WindowsAndMessaging::*,
};
pub const REQUEST: u32 = WM_APP + 5;
const CLIENT: &str = "QuietBattery.StatusReply.Window";
const MAGIC: usize = 0x51425332;
const MAX_REPLY: usize = 1024 * 1024;
struct Reply {
    server: HWND,
    value: Option<String>,
}
unsafe extern "system" fn receive(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == WM_COPYDATA && lparam != 0 {
        let pointer = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut Reply;
        if !pointer.is_null() {
            let reply = unsafe { &mut *pointer };
            let data = unsafe { &*(lparam as *const COPYDATASTRUCT) };
            if wparam == reply.server as usize
                && data.dwData == MAGIC
                && data.cbData as usize <= MAX_REPLY
                && !data.lpData.is_null()
            {
                let bytes = unsafe {
                    std::slice::from_raw_parts(data.lpData as *const u8, data.cbData as usize)
                };
                if let Ok(value) = std::str::from_utf8(bytes) {
                    reply.value = Some(value.to_owned());
                    return 1;
                }
            }
        }
        return 0;
    }
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}
pub fn valid_client(window: HWND, pid: u32) -> bool {
    let mut actual = 0;
    let mut name = [0u16; 80];
    let length = unsafe { GetClassNameW(window, name.as_mut_ptr(), name.len() as i32) };
    unsafe { GetWindowThreadProcessId(window, &mut actual) };
    length > 0
        && actual == pid
        && actual != 0
        && String::from_utf16_lossy(&name[..length as usize]) == CLIENT
}
pub fn respond(client: HWND, server: HWND, value: &str) {
    if value.len() > MAX_REPLY {
        return;
    }
    let data = COPYDATASTRUCT {
        dwData: MAGIC,
        cbData: value.len() as u32,
        lpData: value.as_ptr() as *mut _,
    };
    unsafe {
        SendMessageTimeoutW(
            client,
            WM_COPYDATA,
            server as usize,
            &data as *const _ as isize,
            SMTO_ABORTIFHUNG | SMTO_BLOCK,
            200,
            null_mut(),
        );
    }
}
pub fn query(server: HWND) -> Result<String, String> {
    query_timeout(server, Duration::from_secs(3))
}
fn query_timeout(server: HWND, timeout: Duration) -> Result<String, String> {
    unsafe {
        let instance = GetModuleHandleW(null());
        let class = wide(CLIENT);
        let registered = RegisterClassW(&WNDCLASSW {
            lpfnWndProc: Some(receive),
            hInstance: instance,
            lpszClassName: class.as_ptr(),
            ..std::mem::zeroed()
        }) != 0;
        if !registered && GetLastError() != ERROR_CLASS_ALREADY_EXISTS {
            return Err("无法创建状态查询通道".into());
        }
        let client = CreateWindowExW(
            0,
            class.as_ptr(),
            null(),
            0,
            0,
            0,
            0,
            0,
            HWND_MESSAGE,
            null_mut(),
            instance,
            null(),
        );
        if client.is_null() {
            if registered {
                UnregisterClassW(class.as_ptr(), instance);
            }
            return Err("无法创建状态查询窗口".into());
        }
        let mut reply = Box::new(Reply {
            server,
            value: None,
        });
        SetWindowLongPtrW(client, GWLP_USERDATA, (&mut *reply as *mut Reply) as isize);
        let result = (|| {
            let mut pid = 0;
            GetWindowThreadProcessId(server, &mut pid);
            if pid == 0
                || PostMessageW(
                    server,
                    REQUEST,
                    std::process::id() as usize,
                    client as isize,
                ) == 0
            {
                return Err("托盘未运行或不允许查询".into());
            }
            let start = Instant::now();
            loop {
                let mut message = std::mem::zeroed();
                while PeekMessageW(&mut message, client, 0, 0, PM_REMOVE) != 0 {
                    DispatchMessageW(&message);
                }
                if let Some(value) = reply.value.take() {
                    let expected = format!("{{\"pid\":{pid},");
                    if value.starts_with(&expected) {
                        return Ok(value);
                    }
                    return Err("状态回复不属于当前托盘进程".into());
                }
                if start.elapsed() >= timeout {
                    return Err("托盘状态查询超时".into());
                }
                MsgWaitForMultipleObjectsEx(0, null(), 20, QS_ALLINPUT, MWMO_INPUTAVAILABLE);
            }
        })();
        SetWindowLongPtrW(client, GWLP_USERDATA, 0);
        DestroyWindow(client);
        if registered {
            UnregisterClassW(class.as_ptr(), instance);
        }
        result
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    unsafe extern "system" fn server(hwnd: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
        if m == REQUEST && valid_client(l as HWND, w as u32) {
            respond(
                l as HWND,
                hwnd,
                &format!("{{\"pid\":{},\"reading\":90}}", std::process::id()),
            );
            return 0;
        }
        unsafe { DefWindowProcW(hwnd, m, w, l) }
    }
    #[test]
    fn hidden_native_roundtrip_repeat_timeout_and_cleanup() {
        unsafe {
            let class = wide("QuietBattery.Status.Test");
            let instance = GetModuleHandleW(null());
            RegisterClassW(&WNDCLASSW {
                lpfnWndProc: Some(server),
                hInstance: instance,
                lpszClassName: class.as_ptr(),
                ..std::mem::zeroed()
            });
            let hwnd = CreateWindowExW(
                0,
                class.as_ptr(),
                null(),
                0,
                0,
                0,
                0,
                0,
                HWND_MESSAGE,
                null_mut(),
                instance,
                null(),
            );
            assert!(!hwnd.is_null());
            // A same-thread posted server request needs pumping in this test.
            let target = hwnd as usize;
            let worker = std::thread::spawn(move || {
                for _ in 0..8 {
                    assert!(query(target as HWND).unwrap().contains("\"reading\":90"));
                }
            });
            while !worker.is_finished() {
                let mut message = std::mem::zeroed();
                while PeekMessageW(&mut message, null_mut(), 0, 0, PM_REMOVE) != 0 {
                    DispatchMessageW(&message);
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            worker.join().unwrap();
            assert!(!valid_client(hwnd, std::process::id()));
            assert!(query_timeout(hwnd, Duration::from_millis(40)).is_err());
            DestroyWindow(hwnd);
            UnregisterClassW(class.as_ptr(), instance);
            assert!(query(null_mut()).is_err());
        }
    }
}
