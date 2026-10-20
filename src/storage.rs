//! Current-user storage and conservative one-time migration. No EXE-side writes.
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::OnceLock,
};
use windows_sys::Win32::{
    System::Com::CoTaskMemFree,
    UI::Shell::{FOLDERID_LocalAppData, SHGetKnownFolderPath},
};
static DIRECTORY: OnceLock<PathBuf> = OnceLock::new();
const STATE: &str = "devices-v2.state";

pub fn initialize() -> Result<(), String> {
    let target = local_directory()?;
    let executable = std::env::current_exe().map_err(|e| e.to_string())?;
    migrate(&target, &legacy_directories(&executable))
        .map_err(|e| format!("无法迁移本地设置，旧文件已保留：{e}"))?;
    DIRECTORY
        .set(target)
        .map_err(|_| "存储位置已初始化".to_string())
}
fn local_directory() -> Result<PathBuf, String> {
    let mut raw = std::ptr::null_mut();
    let result =
        unsafe { SHGetKnownFolderPath(&FOLDERID_LocalAppData, 0, std::ptr::null_mut(), &mut raw) };
    if result < 0 {
        return Err(format!("无法定位 LocalAppData：HRESULT {result:#x}"));
    }
    if raw.is_null() {
        return Err("LocalAppData 返回空路径".into());
    }
    let path = unsafe {
        let mut length = 0;
        while *raw.add(length) != 0 {
            length += 1;
        }
        let text = String::from_utf16_lossy(std::slice::from_raw_parts(raw, length));
        CoTaskMemFree(raw.cast());
        PathBuf::from(text).join("QuietBattery")
    };
    Ok(path)
}
pub fn directory() -> PathBuf {
    if cfg!(test) {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".build/tmp")
    } else {
        DIRECTORY
            .get()
            .expect("storage initialized before application state")
            .clone()
    }
}
fn legacy_directories(executable: &Path) -> Vec<PathBuf> {
    let parent = executable.parent().unwrap_or_else(|| Path::new("."));
    let old = if parent.file_name().is_some_and(|n| n == "dist") {
        parent.parent().unwrap_or(parent).join(".build")
    } else if parent
        .parent()
        .and_then(Path::file_name)
        .is_some_and(|n| n == "target")
    {
        parent
            .parent()
            .and_then(Path::parent)
            .unwrap_or(parent)
            .to_path_buf()
    } else {
        parent.join("data")
    };
    let mut paths = vec![old];
    let adjacent = parent.join("data");
    if !paths.contains(&adjacent) {
        paths.push(adjacent);
    }
    paths
}
pub(crate) fn migrate(target: &Path, legacy: &[PathBuf]) -> Result<(), String> {
    let destination = target.join(STATE);
    for source_dir in legacy {
        if source_dir == target {
            continue;
        }
        let source = source_dir.join(STATE);
        match fs::read(&source) {
            Ok(bytes) => {
                if !destination.try_exists().map_err(|e| e.to_string())? {
                    fs::create_dir_all(target).map_err(|e| e.to_string())?;
                    // Reuse the app-owned atomic-save temporary so interrupted
                    // migration never accumulates one file per process.
                    let temporary = destination.with_extension("tmp");
                    let mut file = fs::OpenOptions::new()
                        .write(true)
                        .create(true)
                        .truncate(true)
                        .open(&temporary)
                        .map_err(|e| e.to_string())?;
                    let guard = Temporary(temporary.clone());
                    let written = file.write_all(&bytes).and_then(|_| file.sync_all());
                    drop(file);
                    written.map_err(|e| e.to_string())?;
                    #[cfg(test)]
                    crate::fault_tests::pause("migration-before-publish");
                    if unsafe {
                        windows_sys::Win32::Storage::FileSystem::MoveFileExW(
                            crate::startup::wide(&temporary.to_string_lossy()).as_ptr(),
                            crate::startup::wide(&destination.to_string_lossy()).as_ptr(),
                            windows_sys::Win32::Storage::FileSystem::MOVEFILE_WRITE_THROUGH,
                        )
                    } == 0
                    {
                        return Err(std::io::Error::last_os_error().to_string());
                    }
                    #[cfg(test)]
                    crate::fault_tests::pause("migration-after-publish");
                    drop(guard);
                }
                // Existing settings win. Conflicting old data stays available;
                // remove a legacy state only after byte-for-byte confirmation.
                if fs::read(&destination).map_err(|e| e.to_string())? == bytes {
                    let _ = fs::remove_file(&source);
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.to_string()),
        }
        for name in ["runtime-status.json", "runtime-status.json.tmp"] {
            let _ = fs::remove_file(source_dir.join(name));
        }
        // Never recursively remove legacy folders or unrelated user content.
        if source_dir.file_name().is_some_and(|n| n == "data") {
            let _ = fs::remove_dir(source_dir);
        }
    }
    // Pre-existing obsolete diagnostics are expendable, never periodic output.
    for name in ["runtime-status.json", "runtime-status.json.tmp"] {
        let _ = fs::remove_file(target.join(name));
    }
    Ok(())
}
struct Temporary(PathBuf);
impl Drop for Temporary {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture(name: &str) -> PathBuf {
        let root = directory().join(format!("storage-{name}-{}", std::process::id()));
        if root.exists() {
            fs::remove_dir_all(&root).unwrap();
        }
        fs::create_dir_all(&root).unwrap();
        root
    }
    #[test]
    fn migration_preserves_exact_preferences_and_cleans_only_owned_files() {
        let root = fixture("migrate");
        let old = root.join("data");
        let new = root.join("local");
        fs::create_dir(&old).unwrap();
        let state = b"QuietBattery\t6\npreferences\t0\t37\n";
        fs::write(old.join(STATE), state).unwrap();
        fs::write(old.join("runtime-status.json"), b"old").unwrap();
        fs::write(old.join("keep.txt"), b"user").unwrap();
        migrate(&new, std::slice::from_ref(&old)).unwrap();
        assert_eq!(fs::read(new.join(STATE)).unwrap(), state);
        assert!(!old.join(STATE).exists());
        assert!(!old.join("runtime-status.json").exists());
        assert!(old.join("keep.txt").exists());
        migrate(&new, std::slice::from_ref(&old)).unwrap();
        assert_eq!(fs::read_dir(&new).unwrap().count(), 1);
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn existing_target_wins_failure_keeps_source_and_empty_old_folder_is_removed() {
        let root = fixture("conflict");
        let old = root.join("data");
        let new = root.join("local");
        fs::create_dir(&old).unwrap();
        fs::create_dir(&new).unwrap();
        fs::write(old.join(STATE), b"old").unwrap();
        fs::write(new.join(STATE), b"new").unwrap();
        migrate(&new, std::slice::from_ref(&old)).unwrap();
        assert_eq!(fs::read(new.join(STATE)).unwrap(), b"new");
        assert_eq!(fs::read(old.join(STATE)).unwrap(), b"old");
        fs::write(new.join(STATE), b"old").unwrap();
        migrate(&new, std::slice::from_ref(&old)).unwrap();
        assert!(!old.exists());
        fs::create_dir(&old).unwrap();
        fs::write(old.join(STATE), b"keep on error").unwrap();
        let blocked = root.join("blocked");
        fs::write(&blocked, b"file").unwrap();
        assert!(migrate(&blocked, std::slice::from_ref(&old)).is_err());
        assert!(old.join(STATE).exists());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn locked_temporary_keeps_source_and_interrupted_temporary_is_reused() {
        use std::os::windows::fs::OpenOptionsExt;
        let root = fixture("temporary");
        let old = root.join("data");
        let new = root.join("local");
        fs::create_dir(&old).unwrap();
        fs::create_dir(&new).unwrap();
        fs::write(old.join(STATE), b"settings").unwrap();
        let temporary = new.join("devices-v2.tmp");
        fs::write(&temporary, b"interrupted write").unwrap();
        let lock = fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&temporary)
            .unwrap();
        assert!(migrate(&new, std::slice::from_ref(&old)).is_err());
        assert_eq!(fs::read(old.join(STATE)).unwrap(), b"settings");
        assert!(!new.join(STATE).exists());
        drop(lock);
        assert_eq!(fs::read(&temporary).unwrap(), b"interrupted write");
        migrate(&new, std::slice::from_ref(&old)).unwrap();
        assert_eq!(fs::read(new.join(STATE)).unwrap(), b"settings");
        assert!(!temporary.exists());
        assert!(!old.exists());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn empty_start_is_read_only_and_legacy_developer_paths_are_recognized() {
        let root = fixture("empty");
        let target = root.join("local");
        migrate(&target, &[root.join("data")]).unwrap();
        assert!(!target.exists());
        assert_eq!(
            legacy_directories(Path::new("C:/app/quiet.exe"))[0],
            PathBuf::from("C:/app/data")
        );
        assert_eq!(
            legacy_directories(Path::new("C:/repo/dist/quiet.exe"))[0],
            PathBuf::from("C:/repo/.build")
        );
        assert_eq!(
            legacy_directories(Path::new("C:/repo/.build/target/release/quiet.exe"))[0],
            PathBuf::from("C:/repo/.build")
        );
        assert_eq!(
            local_directory().unwrap().file_name().unwrap(),
            "QuietBattery"
        );
        fs::remove_dir_all(root).unwrap();
    }
}
