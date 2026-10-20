//! Real OS fault tests. All files are below .build/tmp; all visible test windows
//! live on a private desktop, never on the user's input desktop.
use crate::{
    device::{Cancellation, support},
    manager::Manager,
    startup::wide,
};
use std::os::windows::process::CommandExt;
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};
const ROOT: &str = "QUIET_BATTERY_FAULT_ROOT";
const CASE: &str = "QUIET_BATTERY_FAULT_CASE";
const POINT: &str = "QUIET_BATTERY_FAULT_POINT";
struct Fixture(PathBuf);
impl Fixture {
    fn new(name: &str) -> Self {
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join(".build/tmp");
        fs::create_dir_all(&base).unwrap();
        let path = base.join(format!("fault-{name}-{}", std::process::id()));
        assert!(!path.exists(), "stale fault fixture requires inspection");
        fs::create_dir(&path).unwrap();
        Self(path.canonicalize().unwrap())
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn child_root() -> Option<PathBuf> {
    let root = PathBuf::from(std::env::var_os(ROOT)?)
        .canonicalize()
        .unwrap();
    let base = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(".build/tmp")
        .canonicalize()
        .unwrap();
    assert!(root.starts_with(&base) && root != base);
    Some(root)
}
pub(crate) fn pause(point: &str) {
    if std::env::var(POINT).ok().as_deref() != Some(point) {
        return;
    }
    let root = child_root().unwrap();
    fs::write(root.join("ready"), point).unwrap();
    let start = Instant::now();
    while !root.join("resume").exists() {
        assert!(
            start.elapsed() < Duration::from_secs(15),
            "fault parent did not resume/stop child"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}
struct Process(Child);
impl Process {
    fn spawn(root: &Path, case: &str, point: &str, desktop: &str) -> Self {
        Self(
            Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "fault_tests::child", "--nocapture"])
                .env(ROOT, root)
                .env(CASE, case)
                .env(POINT, point)
                .env("QUIET_BATTERY_FAULT_DESKTOP", desktop)
                .creation_flags(0x08000000)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        )
    }
    fn finish(&mut self) -> std::process::ExitStatus {
        let start = Instant::now();
        loop {
            if let Some(status) = self.0.try_wait().unwrap() {
                return status;
            }
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "fault child did not exit"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    fn kill(&mut self) {
        self.0.kill().unwrap();
        self.0.wait().unwrap();
    }
}
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn wait_file(path: &Path) {
    let start = Instant::now();
    while !path.exists() {
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "fault child readiness deadline"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}
// These test-only entry points cannot be invoked in the released GUI binary.
#[test]
fn child() {
    let Some(root) = child_root() else {
        return;
    };
    let case = std::env::var(CASE).unwrap();
    if let Ok(name) = std::env::var("QUIET_BATTERY_FAULT_DESKTOP") {
        if !name.is_empty() {
            use windows_sys::Win32::System::StationsAndDesktops::*;
            let desk = unsafe {
                OpenDesktopW(
                    wide(&name).as_ptr(),
                    0,
                    0,
                    DESKTOP_CREATEWINDOW | DESKTOP_READOBJECTS | DESKTOP_WRITEOBJECTS,
                )
            };
            assert!(!desk.is_null());
            assert_ne!(unsafe { SetThreadDesktop(desk) }, 0);
            // Process lifetime owns the active desktop handle.
        }
    }
    match case.as_str() {
        "migration" => {
            let result = crate::storage::migrate(&root.join("new"), &[root.join("old")]);
            fs::write(
                root.join("outcome"),
                if result.is_ok() { "ok" } else { "error" },
            )
            .unwrap();
        }
        "export" => {
            support::save(
                &root.join("chosen.json"),
                "new report",
                &Cancellation::default(),
            )
            .unwrap();
        }
        "owner" => unsafe {
            use windows_sys::Win32::UI::WindowsAndMessaging::*;
            let hwnd = CreateWindowExW(
                0,
                wide("STATIC").as_ptr(),
                wide("fault-owner").as_ptr(),
                0,
                0,
                0,
                100,
                100,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null(),
            );
            assert!(!hwnd.is_null());
            fs::write(root.join("owner"), (hwnd as usize).to_string()).unwrap();
            let mut msg: MSG = std::mem::zeroed();
            while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        },
        "picker" => {
            let owner = fs::read_to_string(root.join("owner")).unwrap();
            crate::save_dialog::run_helper(Some(owner)).unwrap();
        }
        _ => panic!("unknown test case"),
    }
}
#[test]
fn migration_survives_real_process_termination_before_and_after_publish() {
    for phase in ["before", "after"] {
        let f = Fixture::new(&format!("interrupted-{phase}"));
        fs::create_dir(f.0.join("old")).unwrap();
        let state = b"QuietBattery\t6\npreferences\t0\t37\n";
        fs::write(f.0.join("old/devices-v2.state"), state).unwrap();
        let point = format!("migration-{phase}-publish");
        let mut process = Process::spawn(&f.0, "migration", &point, "");
        wait_file(&f.0.join("ready"));
        process.kill();
        assert_eq!(fs::read(f.0.join("old/devices-v2.state")).unwrap(), state);
        let target = f.0.join("new/devices-v2.state");
        if phase == "before" {
            assert!(!target.exists());
        } else {
            assert_eq!(fs::read(&target).unwrap(), state);
        }
        crate::storage::migrate(&f.0.join("new"), &[f.0.join("old")]).unwrap();
        assert_eq!(fs::read(&target).unwrap(), state);
        assert!(!f.0.join("old/devices-v2.state").exists());
        assert!(!f.0.join("new/devices-v2.tmp").exists());
        assert_eq!(Manager::load(&target).unwrap().threshold, 37);
    }
}
#[test]
fn migration_does_not_overwrite_a_destination_created_during_publish() {
    let f = Fixture::new("migration-race");
    fs::create_dir(f.0.join("old")).unwrap();
    fs::write(f.0.join("old/devices-v2.state"), b"old settings").unwrap();
    let mut process = Process::spawn(&f.0, "migration", "migration-before-publish", "");
    wait_file(&f.0.join("ready"));
    fs::write(f.0.join("new/devices-v2.state"), b"new settings").unwrap();
    fs::write(f.0.join("resume"), b"resume").unwrap();
    assert!(process.finish().success());
    assert_eq!(fs::read_to_string(f.0.join("outcome")).unwrap(), "error");
    assert_eq!(
        fs::read(f.0.join("new/devices-v2.state")).unwrap(),
        b"new settings"
    );
    assert_eq!(
        fs::read(f.0.join("old/devices-v2.state")).unwrap(),
        b"old settings"
    );
    assert!(!f.0.join("new/devices-v2.tmp").exists());
}
struct Deny {
    path: PathBuf,
    sid: String,
}
impl Deny {
    fn new(path: &Path, rights: &str) -> Self {
        let raw = Command::new("whoami.exe")
            .args(["/user", "/fo", "csv", "/nh"])
            .output()
            .unwrap();
        assert!(raw.status.success());
        let text = String::from_utf8(raw.stdout).unwrap();
        let sid = text.split('"').nth(3).unwrap().to_owned();
        assert!(sid.starts_with("S-1-"));
        let rule = format!("*{sid}:({rights})");
        let result = Command::new("icacls.exe")
            .arg(path)
            .args(["/deny", &rule])
            .output()
            .unwrap();
        assert!(result.status.success(), "icacls deny failed");
        Self {
            path: path.into(),
            sid,
        }
    }
}
impl Drop for Deny {
    fn drop(&mut self) {
        let status = Command::new("icacls.exe")
            .arg(&self.path)
            .args(["/remove:d", &format!("*{}", self.sid)])
            .output();
        if !status.is_ok_and(|r| r.status.success()) {
            eprintln!("Could not restore test-only ACL");
        }
    }
}
#[test]
fn real_acl_denials_preserve_settings_and_leave_no_export_temporary() {
    let f = Fixture::new("access-denied");
    let state = f.0.join("settings.state");
    let data = b"QuietBattery\t6\npreferences\t0\t37\n";
    fs::write(&state, data).unwrap();
    {
        let _deny = Deny::new(&state, "R");
        assert_eq!(fs::read(&state).unwrap_err().raw_os_error(), Some(5));
        assert!(Manager::load(&state).is_err());
    }
    assert_eq!(fs::read(&state).unwrap(), data);
    let old = f.0.join("old");
    let new = f.0.join("new");
    fs::create_dir(&old).unwrap();
    fs::create_dir(&new).unwrap();
    fs::write(old.join("devices-v2.state"), data).unwrap();
    {
        let _deny = Deny::new(&new, "W");
        assert_eq!(
            fs::write(new.join("denied"), b"probe")
                .unwrap_err()
                .raw_os_error(),
            Some(5)
        );
        assert!(crate::storage::migrate(&new, std::slice::from_ref(&old)).is_err());
        assert_eq!(fs::read(old.join("devices-v2.state")).unwrap(), data);
        assert!(support::save(&new.join("chosen.json"), "{}", &Cancellation::default()).is_err());
    }
    assert_eq!(fs::read_dir(&new).unwrap().count(), 0);
    crate::storage::migrate(&new, &[old]).unwrap();
    assert_eq!(
        Manager::load(&new.join("devices-v2.state"))
            .unwrap()
            .threshold,
        37
    );
}
#[test]
fn locked_export_destination_preserves_old_report_and_recovers_after_release() {
    use std::os::windows::fs::OpenOptionsExt;
    let f = Fixture::new("export-locked");
    let target = f.0.join("chosen.json");
    fs::write(&target, b"old report").unwrap();
    let lock = fs::OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(&target)
        .unwrap();
    assert!(support::save(&target, "new report", &Cancellation::default()).is_err());
    drop(lock);
    assert_eq!(fs::read(&target).unwrap(), b"old report");
    assert_eq!(fs::read_dir(&f.0).unwrap().count(), 1);
    support::save(&target, "new report", &Cancellation::default()).unwrap();
    assert_eq!(fs::read(&target).unwrap(), b"new report");
    assert_eq!(fs::read_dir(&f.0).unwrap().count(), 1);
}
#[test]
fn invalid_utf8_unknown_header_and_missing_state_have_distinct_results() {
    let f = Fixture::new("invalid-state");
    let target = f.0.join("settings.state");
    assert_eq!(Manager::load(&target).unwrap().threshold, 20);
    for bytes in [&b"\xff\xfe\x00"[..], b"QuietBattery\t999\n", b""] {
        fs::write(&target, bytes).unwrap();
        assert!(Manager::load(&target).is_err());
        assert_eq!(fs::read(&target).unwrap(), bytes);
    }
}
#[test]
fn settings_save_lock_keeps_original_cleans_temporary_and_retries() {
    use std::os::windows::fs::OpenOptionsExt;
    let f = Fixture::new("settings-save");
    let target = f.0.join("settings.state");
    let original = b"QuietBattery\t6\npreferences\t0\t37\n";
    fs::write(&target, original).unwrap();
    let mut manager = Manager::load(&target).unwrap();
    manager.threshold = 55;
    let lock = fs::OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(&target)
        .unwrap();
    for _ in 0..3 {
        manager.save(&target);
        assert!(manager.persistence_error.is_some());
        assert!(!target.with_extension("tmp").exists());
    }
    drop(lock);
    assert_eq!(fs::read(&target).unwrap(), original);
    manager.save(&target);
    assert!(manager.persistence_error.is_none());
    assert_eq!(Manager::load(&target).unwrap().threshold, 55);
    assert_eq!(fs::read_dir(&f.0).unwrap().count(), 1);
}
#[test]
fn export_process_termination_keeps_previous_report_and_exposes_orphan_boundary() {
    let f = Fixture::new("export-interrupted");
    let target = f.0.join("chosen.json");
    fs::write(&target, b"old report").unwrap();
    let mut child = Process::spawn(&f.0, "export", "export-before-publish", "");
    wait_file(&f.0.join("ready"));
    child.kill();
    assert_eq!(fs::read(&target).unwrap(), b"old report");
    // A forced OS termination bypasses Rust destructors. Preserve this boundary
    // explicitly; never scan/delete other files in a user-selected directory.
    let temporary: Vec<_> = fs::read_dir(&f.0)
        .unwrap()
        .filter_map(|e| {
            let p = e.unwrap().path();
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with(".quiet-battery-")
                .then_some(p)
        })
        .collect();
    assert_eq!(temporary.len(), 1);
    assert_eq!(fs::read(&temporary[0]).unwrap(), b"new report");
    fs::remove_file(&temporary[0]).unwrap();
    support::save(&target, "new report", &Cancellation::default()).unwrap();
    assert_eq!(fs::read(&target).unwrap(), b"new report");
}

struct Desktop(
    windows_sys::Win32::System::StationsAndDesktops::HDESK,
    String,
);
impl Desktop {
    fn new() -> Self {
        use windows_sys::Win32::System::StationsAndDesktops::*;
        let name = format!("QuietBattery.Fault.Test.{}", std::process::id());
        let handle = unsafe {
            CreateDesktopW(
                wide(&name).as_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                0,
                DESKTOP_CREATEWINDOW
                    | DESKTOP_ENUMERATE
                    | DESKTOP_READOBJECTS
                    | DESKTOP_WRITEOBJECTS,
                std::ptr::null(),
            )
        };
        assert!(!handle.is_null());
        Self(handle, name)
    }
    fn dialog(&self, pid: u32) -> Option<usize> {
        use windows_sys::Win32::{Foundation::HWND, UI::WindowsAndMessaging::*};
        unsafe extern "system" fn find(hwnd: HWND, data: isize) -> i32 {
            let context = unsafe { &mut *(data as *mut (u32, usize)) };
            let mut pid = 0;
            unsafe {
                GetWindowThreadProcessId(hwnd, &mut pid);
            }
            let mut class = [0u16; 32];
            let n = unsafe { GetClassNameW(hwnd, class.as_mut_ptr(), 32) };
            if pid == context.0 && String::from_utf16_lossy(&class[..n.max(0) as usize]) == "#32770"
            {
                context.1 = hwnd as usize;
                return 0;
            }
            1
        }
        let mut context = (pid, 0usize);
        unsafe {
            windows_sys::Win32::System::StationsAndDesktops::EnumDesktopWindows(
                self.0,
                Some(find),
                &mut context as *mut _ as isize,
            );
        }
        (context.1 != 0).then_some(context.1)
    }
    fn wait_dialog(&self, pid: u32) -> usize {
        let start = Instant::now();
        loop {
            if let Some(hwnd) = self.dialog(pid) {
                return hwnd;
            }
            assert!(
                start.elapsed() < Duration::from_secs(8),
                "native save dialog not found"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
impl Drop for Desktop {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::System::StationsAndDesktops::CloseDesktop(self.0);
        }
    }
}
#[test]
fn native_picker_cancel_and_owner_crash_leave_no_helper_process() {
    use windows_sys::Win32::{Foundation::HWND, UI::WindowsAndMessaging::*};
    let f = Fixture::new("picker");
    let desktop = Desktop::new();
    let mut owner = Process::spawn(&f.0, "owner", "", &desktop.1);
    wait_file(&f.0.join("owner"));
    let mut picker = Process::spawn(&f.0, "picker", "", &desktop.1);
    let hwnd = desktop.wait_dialog(picker.0.id());
    assert_ne!(unsafe { PostMessageW(hwnd as HWND, WM_CLOSE, 0, 0) }, 0);
    assert!(picker.finish().success());
    let mut picker = Process::spawn(&f.0, "picker", "", &desktop.1);
    desktop.wait_dialog(picker.0.id());
    owner.kill();
    assert!(!picker.finish().success());
    assert!(desktop.dialog(picker.0.id()).is_none());
    assert!(!f.0.join("QuietBattery-device.json").exists());
}
