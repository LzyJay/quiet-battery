//! Classic Bluetooth headset battery from the Windows HFP battery property.
//!
//! A classic (BR/EDR) audio device reports its battery with the Hands-Free Profile
//! `AT+BIEV` indicator. Windows parses that inside the audio stack and stores the
//! last value on the `BthHFEnum` device node, under the property set
//! `{104EA319-6EE2-4701-BD47-8DDBF425BBE5}` id 2. Reading that property is passive:
//! it opens no radio connection, changes no pairing and sends no command.
use super::{
    Cancellation, Candidate, Connection, DeviceDescriptor, DeviceError, DeviceKind, PowerState,
    ProtocolAdapter, Reading,
};
use std::ptr::null_mut;
use windows_sys::Win32::Devices::Bluetooth::{
    BLUETOOTH_ADDRESS, BLUETOOTH_ADDRESS_0, BLUETOOTH_DEVICE_INFO, BluetoothGetDeviceInfo,
};
use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    DIGCF_PRESENT, HDEVINFO, SP_DEVINFO_DATA, SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInfo,
    SetupDiGetClassDevsW, SetupDiGetDevicePropertyW,
};
use windows_sys::Win32::Devices::Properties::DEVPROPTYPE;
use windows_sys::Win32::Foundation::DEVPROPKEY;
use windows_sys::core::GUID;

/// Device class "System"; the `BthHFEnum` nodes live there.
const SYSTEM_CLASS: GUID = GUID::from_u128(0x4d36_e97d_e325_11ce_bfc1_0800_2be1_0318);
/// `DEVPKEY_Device_{Service,FriendlyName}` share this property set.
const DEVICE_FMTID: GUID = GUID::from_u128(0xa45c_254e_df1c_4efd_8020_67d1_46a8_50e0);
const PKEY_SERVICE: DEVPROPKEY = DEVPROPKEY {
    fmtid: DEVICE_FMTID,
    pid: 6,
};
const PKEY_FRIENDLY_NAME: DEVPROPKEY = DEVPROPKEY {
    fmtid: DEVICE_FMTID,
    pid: 14,
};
/// `DEVPKEY_Device_InstanceId`
const PKEY_INSTANCE_ID: DEVPROPKEY = DEVPROPKEY {
    fmtid: GUID::from_u128(0x78c3_4fc8_104a_4aca_9ea4_524d_5299_6e57),
    pid: 256,
};
/// Bluetooth battery set published by the HFP audio gateway.
const PKEY_BLUETOOTH_BATTERY: DEVPROPKEY = DEVPROPKEY {
    fmtid: GUID::from_u128(0x104e_a319_6ee2_4701_bd47_8ddb_f425_bbe5),
    pid: 2,
};
const DEVPROP_TYPE_BYTE: DEVPROPTYPE = 0x0000_0003;
const HFP_SERVICE: &str = "BthHFEnum";
const HFP_SUFFIX: &str = " Hands-Free AG";
const MAX_PROPERTY_BYTES: usize = 1024;

struct Entry {
    address: String,
    name: String,
    percent: u8,
}

unsafe fn property(
    set: HDEVINFO,
    info: &SP_DEVINFO_DATA,
    key: &DEVPROPKEY,
    cancel: &Cancellation,
) -> Option<(u32, Vec<u8>)> {
    cancel.check().ok()?;
    let mut kind: DEVPROPTYPE = 0;
    let mut required = 0u32;
    let mut buffer = [0u8; MAX_PROPERTY_BYTES];
    let ok = unsafe {
        SetupDiGetDevicePropertyW(
            set,
            info,
            key,
            &mut kind,
            buffer.as_mut_ptr(),
            buffer.len() as u32,
            &mut required,
            0,
        )
    };
    if ok == 0 || required as usize > buffer.len() {
        return None;
    }
    Some((kind, buffer[..required as usize].to_vec()))
}

fn utf16(bytes: &[u8]) -> String {
    let units = bytes
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .take_while(|unit| *unit != 0)
        .collect::<Vec<_>>();
    String::from_utf16_lossy(&units)
}

fn string_property(
    set: HDEVINFO,
    info: &SP_DEVINFO_DATA,
    key: &DEVPROPKEY,
    cancel: &Cancellation,
) -> Option<String> {
    let (_, bytes) = unsafe { property(set, info, key, cancel) }?;
    Some(utf16(&bytes))
}

/// Last 12 hex digits of the instance-id tail are the remote Bluetooth address.
fn address_from_instance(instance: &str) -> Option<String> {
    let tail = instance.rsplit('\\').next()?;
    let head = tail.split('_').next()?;
    let candidate = head.rsplit('&').next()?;
    (candidate.len() == 12 && candidate.chars().all(|c| c.is_ascii_hexdigit()))
        .then(|| candidate.to_ascii_uppercase())
}

fn display_name(friendly: &str) -> String {
    friendly
        .strip_suffix(HFP_SUFFIX)
        .unwrap_or(friendly)
        .trim()
        .to_string()
}

/// Twelve hex digits of the instance id as the radio address value.
fn address_value(address: &str) -> Option<u64> {
    if address.len() != 12 || !address.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    u64::from_str_radix(address, 16)
        .ok()
        .filter(|value| *value != 0)
}

/// Link state of a paired device from the local radio cache.
///
/// `BthHFEnum` keeps a node for every paired headset, including powered-off ones,
/// so the devnode cannot tell a live link from a stale battery value. This query
/// reads the cached `fConnected` flag: it issues no inquiry, opens no connection,
/// changes no pairing and sends no profile command. `None` means the local cache
/// could not answer, which must never be treated as a confirmed disconnect.
fn link_state(address: &str) -> Option<bool> {
    let value = address_value(address)?;
    let mut info = BLUETOOTH_DEVICE_INFO {
        dwSize: std::mem::size_of::<BLUETOOTH_DEVICE_INFO>() as u32,
        Address: BLUETOOTH_ADDRESS {
            Anonymous: BLUETOOTH_ADDRESS_0 { ullLong: value },
        },
        ..Default::default()
    };
    let result = unsafe { BluetoothGetDeviceInfo(null_mut(), &mut info) };
    (result == 0).then_some(info.fConnected != 0)
}

fn collect(cancel: &Cancellation) -> Result<Vec<Entry>, DeviceError> {
    cancel.check()?;
    let set =
        unsafe { SetupDiGetClassDevsW(&SYSTEM_CLASS, std::ptr::null(), null_mut(), DIGCF_PRESENT) };
    if set == 0 || set == -1isize as HDEVINFO {
        return Err(DeviceError::Unavailable("无法枚举系统设备节点".into()));
    }
    let guard = SetGuard(set);
    let mut entries = Vec::new();
    let mut index = 0u32;
    loop {
        cancel.check()?;
        let mut info: SP_DEVINFO_DATA = unsafe { std::mem::zeroed() };
        info.cbSize = std::mem::size_of::<SP_DEVINFO_DATA>() as u32;
        if unsafe { SetupDiEnumDeviceInfo(guard.0, index, &mut info) } == 0 {
            break;
        }
        index += 1;
        let Some(service) = string_property(guard.0, &info, &PKEY_SERVICE, cancel) else {
            continue;
        };
        if !service.eq_ignore_ascii_case(HFP_SERVICE) {
            continue;
        }
        let Some(instance) = string_property(guard.0, &info, &PKEY_INSTANCE_ID, cancel) else {
            continue;
        };
        let Some(address) = address_from_instance(&instance) else {
            continue;
        };
        // A paired but powered-off headset keeps its node and last percentage.
        // Only a confirmed live link becomes a present device.
        if link_state(&address) == Some(false) {
            continue;
        }
        let Some((kind, bytes)) =
            (unsafe { property(guard.0, &info, &PKEY_BLUETOOTH_BATTERY, cancel) })
        else {
            continue;
        };
        if kind != DEVPROP_TYPE_BYTE || bytes.len() != 1 {
            continue;
        }
        let percent = bytes[0];
        if percent > 100 {
            continue;
        }
        let name = string_property(guard.0, &info, &PKEY_FRIENDLY_NAME, cancel)
            .map(|friendly| display_name(&friendly))
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| format!("蓝牙耳机 {address}"));
        entries.push(Entry {
            address,
            name,
            percent,
        });
    }
    Ok(entries)
}

struct SetGuard(HDEVINFO);

impl Drop for SetGuard {
    fn drop(&mut self) {
        unsafe {
            SetupDiDestroyDeviceInfoList(self.0);
        }
    }
}

fn descriptor(entry: &Entry) -> DeviceDescriptor {
    DeviceDescriptor {
        id: format!("bthfp:{}", entry.address),
        physical_id: entry.address.clone(),
        endpoint: None,
        recoverable: true,
        name: entry.name.clone(),
        // An HFP audio gateway is a headset or earbud; the name is not used to guess.
        kind: DeviceKind::Headset,
        supported: true,
        provider: "bthfp".into(),
        connection: Connection::BluetoothClassic,
    }
}

pub(super) struct BtHfpAdapter;

impl ProtocolAdapter for BtHfpAdapter {
    fn name(&self) -> &'static str {
        "bthfp"
    }
    fn owns(&self, id: &str) -> bool {
        id.starts_with("bthfp:")
    }
    fn discover(&self, cancel: &Cancellation) -> Result<Vec<Candidate>, DeviceError> {
        let mut candidates = Vec::new();
        for entry in collect(cancel)? {
            let reading =
                Reading::from_percent(entry.name.clone(), entry.percent, PowerState::Unknown(255))?;
            candidates.push(Candidate {
                descriptor: descriptor(&entry),
                reading: Some(reading),
                error: None,
            });
        }
        Ok(candidates)
    }
    fn read(&self, id: &str, cancel: &Cancellation) -> Result<Reading, DeviceError> {
        let wanted = id.strip_prefix("bthfp:").unwrap_or_default();
        let entry = collect(cancel)?
            .into_iter()
            .find(|entry| entry.address == wanted)
            .ok_or(DeviceError::NotFound)?;
        Reading::from_percent(entry.name, entry.percent, PowerState::Unknown(255))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instance_id_yields_the_remote_address() {
        let id = "BTHENUM\\{0000111E-0000-1000-8000-00805F9B34FB}_VID&000102B0_PID&0000\\8&3603D69&0&111122333749_C00000000";
        assert_eq!(address_from_instance(id).as_deref(), Some("111122333749"));
        assert_eq!(
            address_from_instance(
                "BTHENUM\\{0000111c-0000-1000-8000-00805f9b34fb}_LOCALMFG&0002\\8&3603d69&0&9C19C2453A9B_C00000000"
            )
            .as_deref(),
            Some("9C19C2453A9B")
        );
        assert!(address_from_instance("BTHENUM\\{0000111E-0000-1000-8000-00805F9B34FB}").is_none());
        assert!(address_from_instance("").is_none());
    }

    #[test]
    fn friendly_name_drops_the_profile_suffix() {
        assert_eq!(
            display_name("MCHOSE V9 Pro 2 Hands-Free AG"),
            "MCHOSE V9 Pro 2"
        );
        assert_eq!(display_name("Redmi Buds 3 Hands-Free AG"), "Redmi Buds 3");
        assert_eq!(display_name("未命名设备"), "未命名设备");
        assert_eq!(display_name(""), "");
    }

    #[test]
    fn radio_address_is_parsed_without_guessing() {
        assert_eq!(address_value("189067A28B0A"), Some(0x1890_67A2_8B0A));
        assert_eq!(address_value("000000000000"), None);
        assert_eq!(address_value("189067A28B0"), None);
        assert_eq!(address_value("189067A28B0G"), None);
        assert_eq!(address_value(""), None);
    }

    #[test]
    fn descriptor_is_address_identified_and_provider_scoped() {
        let entry = Entry {
            address: "111122333749".into(),
            name: "MCHOSE V9 Pro 2".into(),
            percent: 90,
        };
        let d = descriptor(&entry);
        assert_eq!(d.id, "bthfp:111122333749");
        assert_eq!(d.physical_id, "111122333749");
        assert!(BtHfpAdapter.owns(&d.id));
        assert!(!BtHfpAdapter.owns("mchose:x"));
        let reading =
            Reading::from_percent(entry.name, entry.percent, PowerState::Unknown(255)).unwrap();
        assert_eq!(reading.normalized_percent(), 90);
    }

    #[test]
    fn cancelled_scan_does_not_report_a_partial_list() {
        let cancel = Cancellation::default();
        cancel.cancel();
        assert!(matches!(
            BtHfpAdapter.discover(&cancel),
            Err(DeviceError::Cancelled)
        ));
        assert!(matches!(
            BtHfpAdapter.read("bthfp:111122333749", &cancel),
            Err(DeviceError::Cancelled)
        ));
    }
}
