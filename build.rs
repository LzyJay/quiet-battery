//! Embed a real Windows application icon, version and asInvoker/DPI manifest.
//! Uses the existing Windows SDK resource compiler; adds no runtime dependency.
use std::{env, fs, path::PathBuf, process::Command};

fn main() {
    println!("cargo:rerun-if-changed=assets/icons/full.ico");
    println!("cargo:rerun-if-changed=build.rs");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let root = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let output = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let icon = root
        .join("assets/icons/full.ico")
        .to_string_lossy()
        .replace('\\', "/");
    let manifest = output.join("quiet-battery.manifest");
    fs::write(&manifest, r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <assemblyIdentity version="0.1.0.0" processorArchitecture="*" name="QuietBattery" type="win32"/>
  <description>Quiet Battery native battery tray</description>
  <trustInfo xmlns="urn:schemas-microsoft-com:asm.v3"><security><requestedPrivileges>
    <requestedExecutionLevel level="asInvoker" uiAccess="false"/>
  </requestedPrivileges></security></trustInfo>
  <application xmlns="urn:schemas-microsoft-com:asm.v3"><windowsSettings>
    <dpiAwareness xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">PerMonitorV2</dpiAwareness>
    <dpiAware xmlns="http://schemas.microsoft.com/SMI/2005/WindowsSettings">true/pm</dpiAware>
    <longPathAware xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">true</longPathAware>
  </windowsSettings></application>
</assembly>"#).unwrap();
    let manifest_path = manifest.to_string_lossy().replace('\\', "/");
    let resource = output.join("quiet-battery.rc");
    fs::write(
        &resource,
        format!(
            r#"#pragma code_page(65001)
101 ICON "{icon}"
1 24 "{manifest_path}"
1 VERSIONINFO
FILEVERSION 0,1,0,0
PRODUCTVERSION 0,1,0,0
FILEFLAGSMASK 0x3fL
FILEFLAGS 0x0L
FILEOS 0x40004L
FILETYPE 0x1L
FILESUBTYPE 0x0L
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904b0"
    BEGIN
      VALUE "FileDescription", "Quiet Battery - native peripheral battery monitor\0"
      VALUE "FileVersion", "0.1.0\0"
      VALUE "InternalName", "quiet-battery\0"
      VALUE "OriginalFilename", "quiet-battery.exe\0"
      VALUE "ProductName", "Quiet Battery\0"
      VALUE "LegalCopyright", "Copyright (c) 2026 TheWebDog\0"
      VALUE "ProductVersion", "0.1.0\0"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x0409, 1200
  END
END
"#
        ),
    )
    .unwrap();
    let compiler = env::var_os("RC")
        .map(PathBuf::from)
        .or_else(find_sdk_rc)
        .expect(
            "Windows SDK rc.exe was not found. Install the Windows SDK or set RC to its full path.",
        );
    let compiled = output.join("quiet-battery.res");
    let status = Command::new(compiler)
        .arg("/nologo")
        .arg("/fo")
        .arg(&compiled)
        .arg(&resource)
        .status()
        .expect("Could not start Windows SDK resource compiler");
    assert!(status.success(), "Windows resource compilation failed");
    println!("cargo:rustc-link-arg={}", compiled.display());
}

fn find_sdk_rc() -> Option<PathBuf> {
    let kits = env::var_os("WindowsSdkDir")
        .map(PathBuf::from)
        .or_else(|| {
            env::var_os("ProgramFiles(x86)").map(|p| PathBuf::from(p).join("Windows Kits/10"))
        })?;
    let mut versions: Vec<_> = fs::read_dir(kits.join("bin"))
        .ok()?
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().starts_with("10."))
        .map(|e| e.path())
        .collect();
    versions.sort();
    let arch = match env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
        Ok("x86_64") => "x64",
        Ok("aarch64") => "arm64",
        _ => "x86",
    };
    versions
        .into_iter()
        .rev()
        .map(|p| p.join(arch).join("rc.exe"))
        .find(|p| p.is_file())
}
