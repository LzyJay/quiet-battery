//! Shared physical discovery. Enumeration alone never claims battery support.
use super::{Cancellation, Candidate, DeviceDescriptor, DeviceError, DeviceKind};
use hidapi::HidApi;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Mutex, OnceLock};
static DIAGNOSTICS: OnceLock<Mutex<BTreeMap<String, String>>> = OnceLock::new();
pub(super) fn diagnostic_information(id: &str) -> Option<String> {
    DIAGNOSTICS.get()?.lock().ok()?.get(id).cloned()
}
pub(super) fn generic_candidates(
    claimed: &BTreeSet<String>,
    cancel: &Cancellation,
) -> Result<Vec<Candidate>, DeviceError> {
    let api = HidApi::new().map_err(|e| DeviceError::Io(e.to_string()))?;
    let mut found = BTreeMap::new();
    let mut diagnostics: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for info in api.device_list().filter(|d| {
        (d.usage_page() == 1 && matches!(d.usage(), 2 | 4 | 5 | 6))
            || (d.usage_page() == 0x0b && d.usage() == 5)
    }) {
        cancel.check()?;
        let path = info.path().to_string_lossy();
        let key = physical_key(&path);
        if claimed.contains(&key) {
            continue;
        }
        let name = info
            .product_string()
            .filter(|n| !n.trim().is_empty())
            .unwrap_or("HID 外设");
        if name.to_ascii_lowercase().contains("virtual") {
            continue;
        }
        let kind = match (info.usage_page(), info.usage()) {
            (1, 2) => DeviceKind::Mouse,
            (1, 6) => DeviceKind::Keyboard,
            (1, 5) => DeviceKind::Gamepad,
            (0x0b, 5) => DeviceKind::Headset,
            _ => DeviceKind::Generic,
        };
        diagnostics
            .entry(format!("hid:{key}"))
            .or_default()
            .insert(format!(
                "VID: {:04X}；PID: {:04X}；Usage Page: {:04X}；Usage: {:04X}；Interface: {}",
                info.vendor_id(),
                info.product_id(),
                info.usage_page(),
                info.usage(),
                info.interface_number()
            ));
        found.entry(key.clone()).or_insert(Candidate {
            descriptor: DeviceDescriptor {
                id: format!("hid:{key}"),
                physical_id: key,
                endpoint: None,
                recoverable: false,
                name: name.into(),
                kind,
                supported: false,
                provider: "unsupported".into(),
            },
            reading: None,
            error: Some(DeviceError::Unsupported("未确认电量查询协议".into())),
        });
    }
    let diagnostics = diagnostics
        .into_iter()
        .take(512)
        .map(|(id, lines)| {
            (
                id,
                format!(
                    "{}\r\n接口按 Windows 物理容器分组；报文长度和电量协议尚未确认。",
                    lines.into_iter().take(32).collect::<Vec<_>>().join("\r\n")
                ),
            )
        })
        .collect();
    *DIAGNOSTICS
        .get_or_init(|| Mutex::new(BTreeMap::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = diagnostics;
    Ok(found.into_values().collect())
}
/// Windows container IDs group sibling input/vendor interfaces without merging
/// physically separate receivers; retain conservative path identity on failure.
pub(super) fn physical_key(path: &str) -> String {
    container_key(path).unwrap_or_else(|| receiver_key(path))
}
fn container_key(path: &str) -> Option<String> {
    use windows_sys::Win32::Devices::{
        DeviceAndDriverInstallation::*,
        Properties::{DEVPKEY_Device_ContainerId, DEVPROP_TYPE_GUID},
    };
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    unsafe {
        let list = SetupDiCreateDeviceInfoList(std::ptr::null(), std::ptr::null_mut());
        if list == INVALID_HANDLE_VALUE as isize {
            return None;
        }
        let result = (|| {
            let path: Vec<u16> = path.encode_utf16().chain(Some(0)).collect();
            let mut interface: SP_DEVICE_INTERFACE_DATA = std::mem::zeroed();
            interface.cbSize = std::mem::size_of::<SP_DEVICE_INTERFACE_DATA>() as u32;
            if SetupDiOpenDeviceInterfaceW(list, path.as_ptr(), 0, &mut interface) == 0 {
                return None;
            }
            let mut needed = 0;
            SetupDiGetDeviceInterfaceDetailW(
                list,
                &interface,
                std::ptr::null_mut(),
                0,
                &mut needed,
                std::ptr::null_mut(),
            );
            if needed == 0 || needed > 65536 {
                return None;
            }
            let mut storage = vec![0u64; (needed as usize).div_ceil(8)];
            let detail = storage
                .as_mut_ptr()
                .cast::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>();
            (*detail).cbSize = std::mem::size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32;
            let mut info: SP_DEVINFO_DATA = std::mem::zeroed();
            info.cbSize = std::mem::size_of::<SP_DEVINFO_DATA>() as u32;
            if SetupDiGetDeviceInterfaceDetailW(
                list,
                &interface,
                detail,
                needed,
                &mut needed,
                &mut info,
            ) == 0
            {
                return None;
            }
            let mut bytes = [0u8; 16];
            let mut kind = 0;
            let mut size = 0;
            if SetupDiGetDevicePropertyW(
                list,
                &info,
                &DEVPKEY_Device_ContainerId,
                &mut kind,
                bytes.as_mut_ptr(),
                16,
                &mut size,
                0,
            ) == 0
                || kind != DEVPROP_TYPE_GUID
                || size != 16
                || bytes.iter().all(|b| *b == 0)
            {
                return None;
            }
            Some(format!(
                "container:{}",
                bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()
            ))
        })();
        SetupDiDestroyDeviceInfoList(list);
        result
    }
}
pub(super) fn receiver_key(path: &str) -> String {
    let normalized = path.to_ascii_lowercase();
    let mut parts: Vec<String> = normalized.split('#').map(str::to_owned).collect();
    // Windows HID paths encode a collection in two places: &Col01/02 in
    // the hardware ID, and &0000/0001 at the end of its instance ID.
    // Preserve the physical instance root (e.g. 7&1ce076a8&0) so two receivers
    // at different USB locations never share a transport. This topology is
    // deliberately scoped to the verified C53F MI_02 vendor collections.
    if parts.len() >= 4 && parts[1].contains("&col") {
        if let Some(start) = parts[1].find("&col") {
            if parts[1].len() >= start + 6 {
                parts[1].replace_range(start..start + 6, "");
            }
        }
        if let Some((root, collection)) = parts[2].rsplit_once('&') {
            if collection.len() == 4 && collection.bytes().all(|c| c.is_ascii_hexdigit()) {
                parts[2] = root.to_owned();
            }
        }
    }
    parts.join("#")
}
