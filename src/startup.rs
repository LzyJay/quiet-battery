//! Current-user Startup shortcut, using native Shell COM. No registry autoruns.
use std::{ffi::c_void, path::PathBuf, ptr::null_mut};
use windows_sys::Win32::{
    Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS},
    System::Registry::*,
};
use windows_sys::{
    Win32::System::Com::{
        CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
        CoTaskMemFree, CoUninitialize,
    },
    Win32::UI::Shell::SHGetKnownFolderPath,
    core::GUID,
};

const STARTUP: GUID = GUID::from_u128(0xb97d20bb_f46a_4c97_ba10_5e3608430854);
const SHELL_LINK: GUID = GUID::from_u128(0x00021401_0000_0000_c000_000000000046);
const I_SHELL_LINK: GUID = GUID::from_u128(0x000214f9_0000_0000_c000_000000000046);
const I_PERSIST_FILE: GUID = GUID::from_u128(0x0000010b_0000_0000_c000_000000000046);

pub fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}

/// COM apartments belong to their initializing thread; this guard cannot move.
pub(crate) struct Com(std::marker::PhantomData<std::rc::Rc<()>>);
impl Com {
    pub(crate) fn init() -> Result<Self, String> {
        let result = unsafe { CoInitializeEx(null_mut(), COINIT_APARTMENTTHREADED as u32) };
        if result < 0 {
            Err(format!("COM 初始化失败: 0x{result:08X}"))
        } else {
            Ok(Self(std::marker::PhantomData))
        }
    }
}
impl Drop for Com {
    fn drop(&mut self) {
        unsafe {
            CoUninitialize();
        }
    }
}

struct Interface(*mut c_void);
impl Interface {
    unsafe fn method<T: Copy>(&self, index: usize) -> T {
        unsafe {
            let vtable = *(self.0 as *const *const *const c_void);
            std::mem::transmute_copy(&*vtable.add(index))
        }
    }
    fn persist(&self) -> Result<Self, String> {
        let mut result = null_mut();
        let query: unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> i32 =
            unsafe { self.method(0) };
        check(unsafe { query(self.0, &I_PERSIST_FILE, &mut result) })?;
        Ok(Self(result))
    }
    fn set(&self, index: usize, text: &str) -> Result<(), String> {
        let method: unsafe extern "system" fn(*mut c_void, *const u16) -> i32 =
            unsafe { self.method(index) };
        check(unsafe { method(self.0, wide(text).as_ptr()) })
    }
}
impl Drop for Interface {
    fn drop(&mut self) {
        let release: unsafe extern "system" fn(*mut c_void) -> u32 = unsafe { self.method(2) };
        unsafe {
            release(self.0);
        }
    }
}
fn check(result: i32) -> Result<(), String> {
    if result < 0 {
        Err(format!("快捷方式操作失败: 0x{result:08X}"))
    } else {
        Ok(())
    }
}
fn link() -> Result<Interface, String> {
    let mut pointer = null_mut();
    check(unsafe {
        CoCreateInstance(
            &SHELL_LINK,
            null_mut(),
            CLSCTX_INPROC_SERVER,
            &I_SHELL_LINK,
            &mut pointer,
        )
    })?;
    Ok(Interface(pointer))
}
const APPROVAL_KEY: &str =
    "Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\StartupApproved\\StartupFolder";
fn approved() -> Result<bool, String> {
    let mut data = [0u8; 12];
    let mut size = data.len() as u32;
    let result = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            wide(APPROVAL_KEY).as_ptr(),
            wide("Quiet Battery.lnk").as_ptr(),
            RRF_RT_REG_BINARY,
            null_mut(),
            data.as_mut_ptr().cast(),
            &mut size,
        )
    };
    if result == ERROR_FILE_NOT_FOUND {
        return Ok(true);
    }
    if result != ERROR_SUCCESS || size < 4 {
        return Err(format!("无法读取 Windows 登录启动状态: {result}"));
    }
    match u32::from_le_bytes(data[..4].try_into().unwrap()) {
        2 | 6 => Ok(true),
        3 | 7 => Ok(false),
        value => Err(format!("Windows 登录启动状态未知: {value}")),
    }
}
fn clear_explicit_disable() -> Result<(), String> {
    if approved()? {
        return Ok(());
    }
    let mut key = null_mut();
    let result = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            wide(APPROVAL_KEY).as_ptr(),
            0,
            KEY_SET_VALUE,
            &mut key,
        )
    };
    if result != ERROR_SUCCESS {
        return Err(format!("无法恢复 Windows 登录启动状态: {result}"));
    }
    let result = unsafe { RegDeleteValueW(key, wide("Quiet Battery.lnk").as_ptr()) };
    unsafe {
        RegCloseKey(key);
    }
    if result == ERROR_SUCCESS || result == ERROR_FILE_NOT_FOUND {
        Ok(())
    } else {
        Err(format!("无法恢复 Windows 登录启动状态: {result}"))
    }
}
pub fn shortcut_path() -> Result<PathBuf, String> {
    let mut path: *mut u16 = null_mut();
    check(unsafe { SHGetKnownFolderPath(&STARTUP, 0, null_mut(), &mut path) })?;
    let mut length = 0;
    unsafe {
        while *path.add(length) != 0 {
            length += 1;
        }
    }
    let text = unsafe { String::from_utf16_lossy(std::slice::from_raw_parts(path, length)) };
    unsafe {
        CoTaskMemFree(path.cast());
    }
    Ok(PathBuf::from(text).join("Quiet Battery.lnk"))
}

#[derive(Debug)]
pub struct StartupStatus {
    pub exists: bool,
    pub enabled: bool,
    pub target: Option<String>,
    pub arguments: Option<String>,
    pub path: PathBuf,
}
struct LoadedLink {
    shell: Interface,
    persist: Interface,
    target: String,
    arguments: String,
}
fn load_link(path: &std::path::Path) -> Result<LoadedLink, String> {
    let shell = link()?;
    let persist = shell.persist()?;
    let load: unsafe extern "system" fn(*mut c_void, *const u16, u32) -> i32 =
        unsafe { persist.method(5) };
    check(unsafe { load(persist.0, wide(&path.to_string_lossy()).as_ptr(), 0) })?;
    let mut buffer = [0u16; 32768];
    let get: unsafe extern "system" fn(*mut c_void, *mut u16, i32, *mut c_void, u32) -> i32 =
        unsafe { shell.method(3) };
    check(unsafe {
        get(
            shell.0,
            buffer.as_mut_ptr(),
            buffer.len() as i32,
            null_mut(),
            4,
        )
    })?;
    let target = String::from_utf16_lossy(
        &buffer[..buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len())],
    );
    let mut args = [0u16; 1024];
    let get_args: unsafe extern "system" fn(*mut c_void, *mut u16, i32) -> i32 =
        unsafe { shell.method(10) };
    check(unsafe { get_args(shell.0, args.as_mut_ptr(), args.len() as i32) })?;
    let arguments =
        String::from_utf16_lossy(&args[..args.iter().position(|&c| c == 0).unwrap_or(args.len())]);
    Ok(LoadedLink {
        shell,
        persist,
        target,
        arguments,
    })
}
fn save_link(persist: &Interface, path: &std::path::Path) -> Result<(), String> {
    let save: unsafe extern "system" fn(*mut c_void, *const u16, i32) -> i32 =
        unsafe { persist.method(6) };
    check(unsafe { save(persist.0, wide(&path.to_string_lossy()).as_ptr(), 1) })
}
fn same_target(target: &str, executable: &std::path::Path) -> bool {
    PathBuf::from(target)
        .canonicalize()
        .ok()
        .zip(executable.canonicalize().ok())
        .is_some_and(|(target, executable)| target == executable)
}
fn background_arguments(arguments: &str) -> bool {
    arguments.is_empty() || arguments == "--background"
}
pub fn status() -> Result<StartupStatus, String> {
    let _com = Com::init()?;
    let path = shortcut_path()?;
    if !path.exists() {
        return Ok(StartupStatus {
            exists: false,
            enabled: false,
            target: None,
            arguments: None,
            path,
        });
    }
    let info = load_link(&path)?;
    let executable = std::env::current_exe().map_err(|e| e.to_string())?;
    let enabled = same_target(&info.target, &executable)
        && background_arguments(&info.arguments)
        && approved()?;
    Ok(StartupStatus {
        exists: true,
        enabled,
        target: Some(info.target),
        arguments: Some(info.arguments),
        path,
    })
}
fn write_link(
    path: &std::path::Path,
    executable: &std::path::Path,
    arguments: &str,
) -> Result<(), String> {
    let shell = link()?;
    shell.set(20, &executable.to_string_lossy())?;
    shell.set(
        9,
        &executable
            .parent()
            .ok_or("EXE路径没有父目录")?
            .to_string_lossy(),
    )?;
    shell.set(11, arguments)?;
    shell.set(7, "Quiet Battery · 外设电量")?;
    save_link(&shell.persist()?, path)
}
fn upgrade_at(path: &std::path::Path, executable: &std::path::Path) -> Result<bool, String> {
    if !path.exists() {
        return Ok(false);
    }
    let info = load_link(path)?;
    if !same_target(&info.target, executable) || !info.arguments.is_empty() {
        return Ok(false);
    }
    info.shell.set(11, "--background")?;
    save_link(&info.persist, path)?;
    Ok(true)
}
/// Update only our legacy blank-argument shortcut. Never create/enable a startup
/// entry, change its target, or clear Windows StartupApproved on an upgrade.
pub fn upgrade_background() -> Result<(), String> {
    let _com = Com::init()?;
    upgrade_at(
        &shortcut_path()?,
        &std::env::current_exe().map_err(|e| e.to_string())?,
    )
    .map(|_| ())
}
pub fn set_enabled(enabled: bool) -> Result<(), String> {
    let _com = Com::init()?;
    let path = shortcut_path()?;
    if !enabled {
        if path.exists() {
            std::fs::remove_file(&path).map_err(|e| e.to_string())?;
        }
        return Ok(());
    }
    write_link(
        &path,
        &std::env::current_exe().map_err(|e| e.to_string())?,
        "--background",
    )?;
    clear_explicit_disable()
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn legacy_shortcut_upgrades_once_without_retargeting_or_creating_entries() {
        let _com = Com::init().unwrap();
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join(format!(".build/tmp/launch-shortcut-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("Quiet Battery.lnk");
        let executable = std::env::current_exe().unwrap();
        assert!(!upgrade_at(&path, &executable).unwrap());
        assert!(!path.exists());
        write_link(&path, &executable, "").unwrap();
        assert!(background_arguments(&load_link(&path).unwrap().arguments));
        assert!(upgrade_at(&path, &executable).unwrap());
        let info = load_link(&path).unwrap();
        assert_eq!(info.arguments, "--background");
        assert!(same_target(&info.target, &executable));
        drop(info);
        assert!(!upgrade_at(&path, &executable).unwrap());
        write_link(&path, &executable, "--status").unwrap();
        let before = std::fs::read(&path).unwrap();
        assert!(!upgrade_at(&path, &executable).unwrap());
        assert_eq!(before, std::fs::read(&path).unwrap());
        assert!(!background_arguments("--status"));
        let other = root.join("other.exe");
        std::fs::write(&other, b"test fixture").unwrap();
        write_link(&path, &other, "").unwrap();
        let before = std::fs::read(&path).unwrap();
        assert!(!upgrade_at(&path, &executable).unwrap());
        assert_eq!(before, std::fs::read(&path).unwrap());
        std::fs::remove_file(path).unwrap();
        std::fs::remove_file(other).unwrap();
        std::fs::remove_dir(root).unwrap();
    }
}
