//! Passive metadata only: no battery adapters, report traffic, or input buffers.
use super::{Cancellation, DeviceError};
use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Devices::{DeviceAndDriverInstallation::*, HumanInterfaceDevice::*, Properties::*},
    Foundation::{
        CloseHandle, DEVPROPKEY, ERROR_NO_MORE_ITEMS, GetLastError, HANDLE, INVALID_HANDLE_VALUE,
    },
    Storage::FileSystem::{CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING},
};

pub(super) const MAX_COLLECTIONS: usize = 512;
const MAX_TEXT: usize = 512;
const BUDGET: Duration = Duration::from_secs(8);

#[derive(Clone, Debug)]
pub(super) struct Field {
    pub value: Option<String>,
    source: &'static str,
    error: Option<String>,
    truncated: bool,
}
impl Field {
    pub fn text(value: Option<&str>, source: &'static str) -> Self {
        let value = value.filter(|v| !v.trim().is_empty());
        Self {
            value: value.map(|v| v.chars().take(MAX_TEXT).collect()),
            source,
            error: value.is_none().then(|| "unavailable".into()),
            truncated: value.is_some_and(|v| v.chars().count() > MAX_TEXT),
        }
    }
    pub fn number(value: impl ToString, source: &'static str) -> Self {
        Self::text(Some(&value.to_string()), source)
    }
    pub fn hidapi_number(value: u16) -> Self {
        if value == 0 {
            Self::failed("hidapi.DeviceInfo", "zero is unconfirmed: Windows native hidapi may substitute zero after metadata failure".into())
        } else {
            Self::number(value, "hidapi.DeviceInfo")
        }
    }
    fn failed(source: &'static str, error: String) -> Self {
        Self {
            value: None,
            source,
            error: Some(error),
            truncated: false,
        }
    }
    fn json(&self) -> String {
        let quote = crate::app::json_string;
        format!(
            "{{\"value\":{},\"source\":{},\"error\":{},\"truncated\":{}}}",
            self.value.as_deref().map_or_else(|| "null".into(), quote),
            quote(self.source),
            self.error.as_deref().map_or_else(|| "null".into(), quote),
            self.truncated
        )
    }
}

#[derive(Clone, Debug, Default)]
pub(super) struct Collection {
    pub path: String,
    pub container: Option<String>,
    pub fields: BTreeMap<&'static str, Field>,
    pub errors: Vec<String>,
}
impl Collection {
    pub fn key(&self) -> String {
        self.container
            .clone()
            .unwrap_or_else(|| super::discovery::receiver_key(&self.path))
    }
    pub fn json(&self, group: &str, endpoint: &str, identifiers: bool) -> String {
        let quote = crate::app::json_string;
        let identifiers = if identifiers {
            format!(
                ",\"path\":{},\"container_id\":{}",
                quote(&self.path),
                self.container
                    .as_deref()
                    .map_or_else(|| "null".into(), quote)
            )
        } else {
            String::new()
        };
        format!(
            "{{\"group\":{},\"endpoint\":{},\"fields\":{{{}}},\"errors\":[{}]{}}}",
            quote(group),
            quote(endpoint),
            self.fields
                .iter()
                .map(|(name, value)| format!("{}:{}", quote(name), value.json()))
                .collect::<Vec<_>>()
                .join(","),
            self.errors
                .iter()
                .map(|e| quote(e))
                .collect::<Vec<_>>()
                .join(","),
            identifiers
        )
    }
}

pub struct Inventory {
    pub(super) collections: Vec<Collection>,
    complete: bool,
    truncated: bool,
    errors: Vec<String>,
}
impl Inventory {
    pub fn json(&self, identifiers: bool) -> String {
        let mut groups = BTreeMap::new();
        let mut rows: Vec<_> = self.collections.iter().collect();
        rows.sort_by(|a, b| a.key().cmp(&b.key()).then(a.path.cmp(&b.path)));
        let collections = rows
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let next = groups.len() + 1;
                let group = *groups.entry(c.key()).or_insert(next);
                c.json(
                    &format!("group-{group}"),
                    &format!("endpoint-{}", i + 1),
                    identifiers,
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        let text_truncated = rows.iter().any(|c| c.fields.values().any(|f| f.truncated));
        format!(
            "{{\"schema\":1,\"mode\":\"passive-hid-inventory\",\"complete\":{},\"truncated\":{},\"include_identifiers\":{},\"limits\":{{\"collections\":{},\"text_characters\":{},\"iteration_budget_ms\":8000}},\"length_semantics\":\"Windows collection maximum byte lengths including report-ID slot; not packet layouts or raw descriptors\",\"errors\":[{}],\"collections\":[{}]}}",
            self.complete,
            self.truncated || text_truncated,
            identifiers,
            MAX_COLLECTIONS,
            MAX_TEXT,
            self.errors
                .iter()
                .map(|e| crate::app::json_string(e))
                .collect::<Vec<_>>()
                .join(","),
            collections
        )
    }
}

struct DeviceSet(isize);
impl Drop for DeviceSet {
    fn drop(&mut self) {
        unsafe {
            SetupDiDestroyDeviceInfoList(self.0);
        }
    }
}
struct QueryHandle(HANDLE);
impl Drop for QueryHandle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}
struct Preparsed(PHIDP_PREPARSED_DATA);
impl Drop for Preparsed {
    fn drop(&mut self) {
        unsafe {
            HidD_FreePreparsedData(self.0);
        }
    }
}

fn os_error(operation: &str) -> String {
    // Deliberately avoid OS/vendor error text: it may embed local identifiers.
    format!("{operation}: Win32 {}", unsafe { GetLastError() })
}
fn guid_bytes(bytes: &[u8]) -> String {
    format!(
        "container:{}",
        bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()
    )
}
fn property(set: isize, info: &SP_DEVINFO_DATA, key: &DEVPROPKEY, source: &'static str) -> Field {
    let mut kind = 0;
    let mut size = 0;
    let mut bytes = [0u8; 2048];
    if unsafe {
        SetupDiGetDevicePropertyW(
            set,
            info,
            key,
            &mut kind,
            bytes.as_mut_ptr(),
            bytes.len() as u32,
            &mut size,
            0,
        )
    } == 0
    {
        return Field::failed(source, os_error("SetupDiGetDevicePropertyW"));
    }
    if kind != DEVPROP_TYPE_STRING || size as usize > bytes.len() || size % 2 != 0 {
        return Field::failed(source, "unexpected property type/length".into());
    }
    let words: Vec<u16> = bytes[..size as usize]
        .chunks_exact(2)
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
        .take_while(|v| *v != 0)
        .collect();
    Field::text(Some(&String::from_utf16_lossy(&words)), source)
}
fn metadata(set: isize, info: &SP_DEVINFO_DATA, collection: &mut Collection) {
    for (name, key, source) in [
        (
            "friendly_name",
            &DEVPKEY_Device_FriendlyName,
            "Windows.DEVPKEY_Device_FriendlyName",
        ),
        (
            "description",
            &DEVPKEY_Device_DeviceDesc,
            "Windows.DEVPKEY_Device_DeviceDesc",
        ),
        (
            "windows_manufacturer",
            &DEVPKEY_Device_Manufacturer,
            "Windows.DEVPKEY_Device_Manufacturer",
        ),
        (
            "bus_description",
            &DEVPKEY_Device_BusReportedDeviceDesc,
            "Windows.DEVPKEY_Device_BusReportedDeviceDesc",
        ),
        (
            "enumerator",
            &DEVPKEY_Device_EnumeratorName,
            "Windows.DEVPKEY_Device_EnumeratorName",
        ),
        (
            "service",
            &DEVPKEY_Device_Service,
            "Windows.DEVPKEY_Device_Service",
        ),
    ] {
        collection
            .fields
            .insert(name, property(set, info, key, source));
    }
    match guid_property(
        set,
        info,
        &DEVPKEY_Device_ContainerId,
        "SetupDiGetDevicePropertyW(ContainerId)",
    ) {
        Ok(bytes) => collection.container = Some(guid_bytes(&bytes)),
        Err(error) => collection
            .errors
            .push(format!("{error}; conservative path fallback")),
    }
    let source = "Windows.DEVPKEY_Device_BusTypeGuid (GUID bytes)";
    collection.fields.insert(
        "bus",
        match guid_property(
            set,
            info,
            &DEVPKEY_Device_BusTypeGuid,
            "SetupDiGetDevicePropertyW(BusTypeGuid)",
        ) {
            Ok(bytes) => Field::text(
                Some(&bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()),
                source,
            ),
            Err(error) => Field::failed(source, error),
        },
    );
}
fn validate_guid(kind: u32, size: u32, bytes: [u8; 16]) -> Result<[u8; 16], String> {
    if kind != DEVPROP_TYPE_GUID {
        return Err(format!("unexpected property type {kind}; expected GUID"));
    }
    if size != 16 {
        return Err(format!("unexpected GUID byte length {size}; expected 16"));
    }
    if bytes.iter().all(|b| *b == 0) {
        return Err("zero GUID unavailable".into());
    }
    Ok(bytes)
}
fn guid_property(
    set: isize,
    info: &SP_DEVINFO_DATA,
    key: &DEVPROPKEY,
    operation: &str,
) -> Result<[u8; 16], String> {
    let mut bytes = [0u8; 16];
    let mut kind = 0;
    let mut size = 0;
    let result = unsafe {
        SetupDiGetDevicePropertyW(
            set,
            info,
            key,
            &mut kind,
            bytes.as_mut_ptr(),
            16,
            &mut size,
            0,
        )
    };
    if result == 0 {
        return Err(os_error(operation));
    }
    validate_guid(kind, size, bytes).map_err(|cause| format!("{operation}: {cause}"))
}
fn detail(
    set: isize,
    interface: &SP_DEVICE_INTERFACE_DATA,
) -> Result<(String, SP_DEVINFO_DATA), String> {
    let mut needed = 0;
    unsafe {
        SetupDiGetDeviceInterfaceDetailW(
            set,
            interface,
            std::ptr::null_mut(),
            0,
            &mut needed,
            std::ptr::null_mut(),
        );
    }
    if !(std::mem::size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32..=65536).contains(&needed)
    {
        return Err("interface detail length unavailable/out of bounds".into());
    }
    let mut storage = vec![0u64; (needed as usize).div_ceil(8)];
    let detail = storage
        .as_mut_ptr()
        .cast::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>();
    let mut info: SP_DEVINFO_DATA = unsafe { std::mem::zeroed() };
    info.cbSize = std::mem::size_of::<SP_DEVINFO_DATA>() as u32;
    unsafe {
        (*detail).cbSize = std::mem::size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32;
        if SetupDiGetDeviceInterfaceDetailW(set, interface, detail, needed, &mut needed, &mut info)
            == 0
        {
            return Err(os_error("SetupDiGetDeviceInterfaceDetailW"));
        }
        let offset = std::mem::offset_of!(SP_DEVICE_INTERFACE_DETAIL_DATA_W, DevicePath);
        let words = std::slice::from_raw_parts(
            (*detail).DevicePath.as_ptr(),
            (needed as usize - offset) / 2,
        );
        let end = words
            .iter()
            .position(|v| *v == 0)
            .ok_or("unterminated interface path")?;
        Ok((String::from_utf16_lossy(&words[..end]), info))
    }
}

/// Cheap SetupDi properties for an already enumerated collection. Never opens HID.
pub(super) fn for_path(path: &str) -> Collection {
    let mut c = Collection {
        path: path.into(),
        ..Default::default()
    };
    let set = unsafe { SetupDiCreateDeviceInfoList(std::ptr::null(), std::ptr::null_mut()) };
    if set == INVALID_HANDLE_VALUE as isize {
        c.errors.push(os_error("SetupDiCreateDeviceInfoList"));
        return c;
    }
    let set = DeviceSet(set);
    let path: Vec<_> = path.encode_utf16().chain(Some(0)).collect();
    let mut interface: SP_DEVICE_INTERFACE_DATA = unsafe { std::mem::zeroed() };
    interface.cbSize = std::mem::size_of::<SP_DEVICE_INTERFACE_DATA>() as u32;
    if unsafe { SetupDiOpenDeviceInterfaceW(set.0, path.as_ptr(), 0, &mut interface) } == 0 {
        c.errors.push(os_error("SetupDiOpenDeviceInterfaceW"));
        return c;
    }
    match detail(set.0, &interface) {
        Ok((_, info)) => metadata(set.0, &info, &mut c),
        Err(e) => c.errors.push(e),
    }
    c
}

fn query_caps(c: &mut Collection) {
    let source = "Windows.HidP_GetCaps";
    for name in [
        "usage_page",
        "usage",
        "input_report_bytes",
        "output_report_bytes",
        "feature_report_bytes",
    ] {
        c.fields
            .insert(name, Field::failed(source, "caps unavailable".into()));
    }
    for name in ["vid", "pid", "release"] {
        c.fields.insert(
            name,
            Field::failed(
                "Windows.HidD_GetAttributes",
                "attributes unavailable".into(),
            ),
        );
    }
    // Hardware IDs in a Windows interface path are useful even when metadata
    // access is denied; expose only the parsed numbers with their provenance.
    for (name, marker) in [("vid", "vid_"), ("pid", "pid_")] {
        if let Some(value) = path_hex(&c.path, marker, 4) {
            c.fields.insert(
                name,
                Field::number(value, "Windows HID interface path hardware ID"),
            );
        }
    }
    for name in ["manufacturer", "product"] {
        c.fields.insert(
            name,
            Field::failed("Windows.HidD strings", "HID string unavailable".into()),
        );
    }
    let interface = c
        .path
        .to_ascii_lowercase()
        .split("&mi_")
        .nth(1)
        .and_then(|s| s.get(..2))
        .and_then(|s| u8::from_str_radix(s, 16).ok());
    c.fields.insert(
        "interface",
        match interface {
            Some(n) => Field::number(n, "Windows interface path MI field"),
            None => Field::failed("Windows interface path MI field", "unavailable".into()),
        },
    );
    let path: Vec<_> = c.path.encode_utf16().chain(Some(0)).collect();
    // Zero desired access: metadata only. No ReadFile, GetFeature or writes.
    let handle = unsafe {
        CreateFileW(
            path.as_ptr(),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        c.errors.push(os_error("CreateFileW zero-access metadata"));
        return;
    }
    let handle = QueryHandle(handle);
    let mut attributes = HIDD_ATTRIBUTES {
        Size: std::mem::size_of::<HIDD_ATTRIBUTES>() as u32,
        ..Default::default()
    };
    if unsafe { HidD_GetAttributes(handle.0, &mut attributes) } {
        for (name, value) in [
            ("vid", attributes.VendorID),
            ("pid", attributes.ProductID),
            ("release", attributes.VersionNumber),
        ] {
            c.fields
                .insert(name, Field::number(value, "Windows.HidD_GetAttributes"));
        }
    } else {
        c.errors.push(os_error("HidD_GetAttributes"));
    }
    for (name, query, source) in [
        (
            "manufacturer",
            HidD_GetManufacturerString
                as unsafe extern "system" fn(HANDLE, *mut core::ffi::c_void, u32) -> bool,
            "Windows.HidD_GetManufacturerString",
        ),
        (
            "product",
            HidD_GetProductString
                as unsafe extern "system" fn(HANDLE, *mut core::ffi::c_void, u32) -> bool,
            "Windows.HidD_GetProductString",
        ),
    ] {
        let mut words = [0u16; 256];
        let field = if unsafe { query(handle.0, words.as_mut_ptr().cast(), 512) } {
            let end = words.iter().position(|w| *w == 0).unwrap_or(words.len());
            Field::text(Some(&String::from_utf16_lossy(&words[..end])), source)
        } else {
            Field::failed(source, os_error("HidD string query"))
        };
        c.fields.insert(name, field);
    }
    let mut preparsed = 0;
    if !unsafe { HidD_GetPreparsedData(handle.0, &mut preparsed) } {
        c.errors.push(os_error("HidD_GetPreparsedData"));
        return;
    }
    let preparsed = Preparsed(preparsed);
    let mut caps = HIDP_CAPS::default();
    let status = unsafe { HidP_GetCaps(preparsed.0, &mut caps) };
    if status != HIDP_STATUS_SUCCESS {
        c.errors.push(format!("HidP_GetCaps: NTSTATUS {status:#x}"));
        return;
    }
    store_caps(c, &caps);
}

fn store_caps(c: &mut Collection, caps: &HIDP_CAPS) {
    let source = "Windows.HidP_GetCaps";
    for (name, value) in [
        ("usage_page", caps.UsagePage),
        ("usage", caps.Usage),
        ("input_report_bytes", caps.InputReportByteLength),
        ("output_report_bytes", caps.OutputReportByteLength),
        ("feature_report_bytes", caps.FeatureReportByteLength),
    ] {
        c.fields.insert(name, Field::number(value, source));
    }
}

fn path_hex(path: &str, marker: &str, width: usize) -> Option<u16> {
    let lower = path.to_ascii_lowercase();
    let value = lower.split(marker).nth(1)?.get(..width)?;
    u16::from_str_radix(value, 16).ok()
}

fn collect_inventory(
    cancel: &Cancellation,
    limit: usize,
    mut elapsed: impl FnMut() -> Duration,
    mut next: impl FnMut(u32, bool) -> Result<Option<Collection>, String>,
) -> Result<Inventory, DeviceError> {
    let mut report = Inventory {
        collections: Vec::new(),
        complete: true,
        truncated: false,
        errors: Vec::new(),
    };
    for index in 0u32.. {
        cancel.check()?;
        if elapsed() >= BUDGET {
            report.complete = false;
            report.truncated = true;
            report
                .errors
                .push("iteration budget reached; synchronous OS calls may exceed budget".into());
            break;
        }
        let within_limit = report.collections.len() < limit;
        match next(index, within_limit) {
            Ok(None) => break,
            Ok(Some(collection)) if within_limit => report.collections.push(collection),
            Ok(Some(_)) => {
                report.complete = false;
                report.truncated = true;
                report.errors.push("collection limit reached".into());
                break;
            }
            Err(error) => {
                report.complete = false;
                report.errors.push(error);
                break;
            }
        }
    }
    cancel.check()?;
    Ok(report)
}
pub fn inventory(cancel: &Cancellation) -> Result<Inventory, DeviceError> {
    cancel.check()?;
    let start = Instant::now();
    let mut guid = windows_sys::core::GUID::from_u128(0);
    unsafe {
        HidD_GetHidGuid(&mut guid);
    }
    let set = unsafe {
        SetupDiGetClassDevsW(
            &guid,
            std::ptr::null(),
            std::ptr::null_mut(),
            DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
        )
    };
    if set == INVALID_HANDLE_VALUE as isize {
        let error = os_error("SetupDiGetClassDevsW");
        cancel.check()?;
        return Ok(Inventory {
            collections: Vec::new(),
            complete: false,
            truncated: false,
            errors: vec![error],
        });
    }
    let set = DeviceSet(set);
    collect_inventory(
        cancel,
        MAX_COLLECTIONS,
        || start.elapsed(),
        |index, within_limit| {
            let mut interface: SP_DEVICE_INTERFACE_DATA = unsafe { std::mem::zeroed() };
            interface.cbSize = std::mem::size_of::<SP_DEVICE_INTERFACE_DATA>() as u32;
            if unsafe {
                SetupDiEnumDeviceInterfaces(set.0, std::ptr::null(), &guid, index, &mut interface)
            } == 0
            {
                let code = unsafe { GetLastError() };
                return if code == ERROR_NO_MORE_ITEMS {
                    Ok(None)
                } else {
                    Err(format!("SetupDiEnumDeviceInterfaces: Win32 {code}"))
                };
            }
            // Probe one boundary item to distinguish exactly-full enumeration from
            // truncation, without querying its metadata or retaining an extra row.
            if !within_limit {
                return Ok(Some(Collection::default()));
            }
            let mut c = Collection::default();
            match detail(set.0, &interface) {
                Ok((path, info)) => {
                    c.path = path;
                    metadata(set.0, &info, &mut c);
                    if cancel.check().is_ok() {
                        query_caps(&mut c);
                    }
                }
                Err(error) => {
                    c.path = format!("unresolved-endpoint-{index}");
                    c.errors.push(error);
                }
            }
            Ok(Some(c))
        },
    )
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn privacy_escaping_and_partial_fields() {
        let mut c = Collection {
            path: "secret-path-serial".into(),
            container: Some("secret-container".into()),
            ..Default::default()
        };
        c.fields
            .insert("product", Field::text(Some("name\n\u{1}\"\\"), "fixture"));
        c.fields
            .insert("manufacturer", Field::failed("fixture", "Win32 5".into()));
        let report = Inventory {
            collections: vec![c],
            complete: true,
            truncated: false,
            errors: vec![],
        };
        let safe = report.json(false);
        assert!(!safe.contains("secret"));
        assert!(safe.contains("name\\n\\u0001\\\"\\\\"));
        assert!(safe.contains("\"value\":null"));
        assert!(safe.contains("Win32 5"));
        assert!(report.json(true).contains("secret-path-serial"));
    }
    #[test]
    fn text_truncation_does_not_change_traversal_completeness() {
        let mut c = Collection::default();
        c.fields.insert(
            "product",
            Field::text(Some(&"x".repeat(MAX_TEXT + 1)), "fixture"),
        );
        let report = Inventory {
            collections: vec![c],
            complete: true,
            truncated: false,
            errors: vec![],
        };
        let json = report.json(false);
        assert!(json.contains("\"complete\":true"));
        assert!(json.contains("\"truncated\":true"));
    }
    fn fixture_row(index: u32) -> Collection {
        let mut c = Collection {
            path: format!("fixture-{index}"),
            ..Default::default()
        };
        c.fields.insert(
            "product",
            Field::text(Some(&format!("row-{index}")), "fixture"),
        );
        c
    }
    #[test]
    fn traversal_keeps_rows_below_exactly_at_and_above_limit() {
        for (total, expected, complete, truncated) in [
            (1, 1, true, false),
            (2, 2, true, false),
            (3, 2, false, true),
        ] {
            let mut calls = Vec::new();
            let report = collect_inventory(
                &Cancellation::default(),
                2,
                || Duration::ZERO,
                |index, within| {
                    calls.push((index, within));
                    Ok((index < total).then(|| fixture_row(index)))
                },
            )
            .unwrap();
            assert_eq!(report.collections.len(), expected);
            assert_eq!(report.complete, complete);
            assert_eq!(report.truncated, truncated);
            assert_eq!(report.errors.is_empty(), complete);
            for (index, row) in report.collections.iter().enumerate() {
                assert_eq!(row.path, format!("fixture-{index}"));
            }
            assert_eq!(calls.last().unwrap().0, total.min(2));
            if total >= 2 {
                assert!(!calls.last().unwrap().1);
            }
        }
    }
    #[test]
    fn time_and_enumeration_failure_keep_accumulated_evidence_and_cancel_discards_it() {
        let mut ticks = 0;
        let time = collect_inventory(
            &Cancellation::default(),
            4,
            || {
                ticks += 1;
                if ticks > 2 { BUDGET } else { Duration::ZERO }
            },
            |index, _| Ok(Some(fixture_row(index))),
        )
        .unwrap();
        assert_eq!(time.collections.len(), 2);
        assert!(!time.complete);
        assert!(time.truncated);
        assert!(time.errors[0].contains("iteration budget"));
        let failure = collect_inventory(
            &Cancellation::default(),
            4,
            || Duration::ZERO,
            |index, _| {
                if index == 2 {
                    Err("SetupDiEnumDeviceInterfaces: Win32 5".into())
                } else {
                    Ok(Some(fixture_row(index)))
                }
            },
        )
        .unwrap();
        assert_eq!(failure.collections.len(), 2);
        assert_eq!(failure.collections[1].path, "fixture-1");
        assert!(!failure.complete);
        assert!(!failure.truncated);
        assert_eq!(failure.errors, ["SetupDiEnumDeviceInterfaces: Win32 5"]);
        let cancel = Cancellation::default();
        let result = collect_inventory(
            &cancel,
            4,
            || Duration::ZERO,
            |index, _| {
                cancel.cancel();
                Ok(Some(fixture_row(index)))
            },
        );
        assert!(matches!(result, Err(DeviceError::Cancelled)));
    }
    #[test]
    fn guid_validation_reports_precise_causes_and_actual_os_failure() {
        assert!(
            validate_guid(DEVPROP_TYPE_STRING, 16, [1; 16])
                .unwrap_err()
                .contains("type")
        );
        assert!(
            validate_guid(DEVPROP_TYPE_GUID, 8, [1; 16])
                .unwrap_err()
                .contains("length 8")
        );
        assert!(
            validate_guid(DEVPROP_TYPE_GUID, 16, [0; 16])
                .unwrap_err()
                .contains("zero GUID")
        );
        assert_eq!(
            validate_guid(DEVPROP_TYPE_GUID, 16, [1; 16]).unwrap(),
            [1; 16]
        );
        let info = SP_DEVINFO_DATA {
            cbSize: std::mem::size_of::<SP_DEVINFO_DATA>() as u32,
            ..Default::default()
        };
        let error = guid_property(
            INVALID_HANDLE_VALUE as isize,
            &info,
            &DEVPKEY_Device_ContainerId,
            "SetupDiGetDevicePropertyW(ContainerId)",
        )
        .unwrap_err();
        assert!(error.starts_with("SetupDiGetDevicePropertyW(ContainerId): Win32 "));
    }
    #[test]
    fn successful_native_caps_export_keeps_distinct_lengths_provenance_and_valid_zero() {
        let caps = HIDP_CAPS {
            UsagePage: 0xff00,
            Usage: 2,
            InputReportByteLength: 17,
            OutputReportByteLength: 31,
            FeatureReportByteLength: 65,
            ..Default::default()
        };
        let mut c = Collection::default();
        c.fields.insert(
            "input_report_bytes",
            Field::failed("fixture", "previous failure".into()),
        );
        store_caps(&mut c, &caps);
        for (key, value) in [
            ("usage_page", "65280"),
            ("usage", "2"),
            ("input_report_bytes", "17"),
            ("output_report_bytes", "31"),
            ("feature_report_bytes", "65"),
        ] {
            let field = &c.fields[key];
            assert_eq!(field.value.as_deref(), Some(value));
            assert_eq!(field.source, "Windows.HidP_GetCaps");
            assert!(field.error.is_none());
        }
        store_caps(&mut c, &HIDP_CAPS::default());
        assert_eq!(c.fields["usage"].value.as_deref(), Some("0"));
        assert_eq!(c.fields["output_report_bytes"].value.as_deref(), Some("0"));
        assert!(c.fields["output_report_bytes"].error.is_none());
        let cached = Field::hidapi_number(0);
        assert!(cached.value.is_none());
        assert!(cached.error.as_deref().unwrap().contains("unconfirmed"));
        assert_eq!(Field::hidapi_number(17).value.as_deref(), Some("17"));
    }
    #[test]
    fn query_handle_owns_and_closes_native_handle_and_failed_endpoint_keeps_evidence() {
        use windows_sys::Win32::{
            Foundation::GetHandleInformation, System::Threading::CreateEventW,
        };
        let handle = unsafe { CreateEventW(std::ptr::null(), 0, 0, std::ptr::null()) };
        assert!(!handle.is_null());
        let owned = QueryHandle(handle);
        let mut flags = 0;
        assert_ne!(unsafe { GetHandleInformation(handle, &mut flags) }, 0);
        drop(owned);
        assert_eq!(unsafe { GetHandleInformation(handle, &mut flags) }, 0);
        let mut c = Collection {
            path: "\\\\?\\hid#vid_1234&pid_abcd&mi_02#nonexistent".into(),
            ..Default::default()
        };
        c.fields.insert(
            "description",
            Field::text(Some("retained evidence"), "fixture"),
        );
        query_caps(&mut c);
        assert_eq!(c.fields["vid"].value.as_deref(), Some("4660"));
        assert_eq!(c.fields["pid"].value.as_deref(), Some("43981"));
        assert!(c.fields["input_report_bytes"].value.is_none());
        assert!(!c.errors.is_empty());
        assert!(
            c.json("group-1", "endpoint-1", false)
                .contains("retained evidence")
        );
        assert!(
            !c.json("group-1", "endpoint-1", false)
                .contains("nonexistent")
        );
    }
}
