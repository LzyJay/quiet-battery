//! Capability-based HID++ discovery and bounded, query-only battery reads.
//! References: Logitech cpg-docs HID++ 2.0; Linux hid-logitech-hidpp battery mapping;
//! hidapi-rs 2.6.7 windows-native (bounded 1000 ms write).
use super::discovery::physical_key;
#[cfg(test)]
use super::discovery::receiver_key;
use super::{
    Cancellation, Candidate, DeviceDescriptor, DeviceError, DeviceKind, Level, PowerState, Reading,
};
use hidapi::{HidApi, HidDevice};
use std::collections::BTreeMap;
use std::ffi::CString;
use std::sync::atomic::{AtomicU8, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, HANDLE, WAIT_ABANDONED, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::System::Threading::{CreateMutexW, ReleaseMutex, WaitForSingleObject};

const VID: u16 = 0x046d;
const RESPONSE_TIMEOUT: Duration = Duration::from_millis(900);
const QUERY_MUTEX_NAME: &str = "Local\\QuietBattery.Hidpp.ReceiverQuery";
const QUERY_MUTEX_WAIT_MS: u32 = 2000;
static SEQUENCE: AtomicU8 = AtomicU8::new(0);

fn groups(api: &HidApi) -> BTreeMap<String, Vec<(u16, CString)>> {
    let mut groups = BTreeMap::new();
    for info in api
        .device_list()
        .filter(|d| d.vendor_id() == VID && d.usage_page() == 0xff00 && matches!(d.usage(), 1 | 2))
    {
        groups
            .entry(physical_key(&info.path().to_string_lossy()))
            .or_insert_with(Vec::new)
            .push((info.usage(), info.path().to_owned()));
    }
    groups
}
fn open_session(
    api: &HidApi,
    mut paths: Vec<(u16, CString)>,
    index: u8,
    cancel: &Cancellation,
) -> Result<Session, DeviceError> {
    paths.sort_by_key(|(usage, _)| std::cmp::Reverse(*usage));
    let mut channels = Vec::new();
    let mut error = DeviceError::NotFound;
    for (usage, path) in paths {
        cancel.check()?;
        match api.open_path(&path) {
            Ok(device) => channels.push(Channel { usage, device }),
            Err(e) => error = io(e),
        }
    }
    if channels.is_empty() {
        Err(error)
    } else {
        Ok(Session {
            channels,
            index,
            cancel: cancel.clone(),
        })
    }
}
fn id_for(key: &str, index: u8, fingerprint: u64) -> String {
    format!("hidpp:{index}:{key}|{fingerprint:016x}")
}
pub(super) fn read_device(id: &str, cancel: &Cancellation) -> Result<Reading, DeviceError> {
    let mut parts = id.splitn(3, ':');
    if parts.next() != Some("hidpp") {
        return Err(DeviceError::NotFound);
    }
    let index = parts
        .next()
        .and_then(|s| s.parse::<u8>().ok())
        .filter(|i| (1..=6).contains(i))
        .ok_or(DeviceError::NotFound)?;
    let identity = parts.next().ok_or(DeviceError::NotFound)?;
    let (key, expected) = identity.rsplit_once('|').ok_or(DeviceError::NotFound)?;
    let expected = u64::from_str_radix(expected, 16).map_err(|_| DeviceError::NotFound)?;
    let _guard =
        ReceiverQueryGuard::acquire_cancelled(QUERY_MUTEX_NAME, QUERY_MUTEX_WAIT_MS, cancel)?;
    let api = HidApi::new().map_err(io)?;
    let paths = groups(&api).remove(key).ok_or(DeviceError::NotFound)?;
    let session = open_session(&api, paths, index, cancel)?;
    query_device(&session).and_then(|(_, reading, fingerprint)| {
        if fingerprint == expected {
            Ok(reading)
        } else {
            Err(DeviceError::Unsupported(
                "接收器槽位的设备身份已改变，请重新发现".into(),
            ))
        }
    })
}
type SlotReplies<T> = Vec<(u8, Result<T, DeviceError>)>;
fn scan_slots<T>(
    cancel: &Cancellation,
    mut query: impl FnMut(u8) -> Result<T, DeviceError>,
) -> Result<SlotReplies<T>, DeviceError> {
    let mut replies = Vec::new();
    for index in 1..=6 {
        cancel.check()?;
        let result = query(index);
        cancel.check()?;
        if matches!(result, Err(DeviceError::Cancelled)) {
            return Err(DeviceError::Cancelled);
        }
        replies.push((index, result));
    }
    Ok(replies)
}
pub(super) fn discover(cancel: &Cancellation) -> Result<Vec<Candidate>, DeviceError> {
    let _guard =
        ReceiverQueryGuard::acquire_cancelled(QUERY_MUTEX_NAME, QUERY_MUTEX_WAIT_MS, cancel)?;
    let api = HidApi::new().map_err(io)?;
    let groups = groups(&api);
    let mut candidates = Vec::new();
    for (key, paths) in groups {
        cancel.check()?;
        let mut found = false;
        let mut last_error = None;
        let replies = scan_slots(cancel, |index| {
            let session = open_session(&api, paths.clone(), index, cancel)?;
            query_device(&session)
        })?;
        for (index, result) in replies {
            match result {
                Ok((kind, reading, fingerprint)) => {
                    found = true;
                    candidates.push(Candidate {
                        descriptor: DeviceDescriptor {
                            id: id_for(&key, index, fingerprint),
                            physical_id: key.clone(),
                            endpoint: Some(index.to_string()),
                            recoverable: true,
                            name: reading.device_name.clone(),
                            kind,
                            supported: true,
                            provider: "hidpp".into(),
                        },
                        reading: Some(reading),
                        error: None,
                    });
                }
                Err(DeviceError::Cancelled) => return Err(DeviceError::Cancelled),
                Err(error) => last_error = Some(error),
            }
        }
        cancel.check()?;
        if !found {
            candidates.push(Candidate {
                descriptor: DeviceDescriptor {
                    id: id_for(&key, 1, 0),
                    physical_id: key.clone(),
                    endpoint: None,
                    recoverable: true,
                    name: "Logitech HID++".into(),
                    kind: DeviceKind::Generic,
                    supported: false,
                    provider: "hidpp".into(),
                },
                reading: None,
                error: last_error,
            });
        }
    }
    Ok(candidates)
}
fn query_device(session: &impl Query) -> Result<(DeviceKind, Reading, u64), DeviceError> {
    let protocol = session.ping()?;
    if protocol.0 < 2 {
        return Err(DeviceError::Unsupported(format!(
            "HID++ {}.{}",
            protocol.0, protocol.1
        )));
    }
    let name_feature = session.lookup(0x0005)?;
    let name = session.name(name_feature)?;
    let kind = match session.query(name_feature, 2, &[])?.first().copied() {
        Some(0) => DeviceKind::Keyboard,
        Some(1) => DeviceKind::Remote,
        Some(2) => DeviceKind::Numpad,
        Some(3) => DeviceKind::Mouse,
        Some(4) => DeviceKind::Touchpad,
        Some(5) => DeviceKind::Trackball,
        Some(6) => DeviceKind::Presenter,
        Some(8) => DeviceKind::Headset,
        _ => DeviceKind::Generic,
    };
    let mut fingerprint = 0xcbf29ce484222325u64;
    let identity = match session.lookup(0x0003) {
        Ok(feature) => {
            let data = session.query(feature, 0, &[])?;
            if data.len() < 13 || data[1..5].iter().all(|b| *b == 0) {
                return Err(DeviceError::Protocol(
                    "设备身份回复不完整或无有效 unit ID".into(),
                ));
            }
            [&data[1..5], &data[7..13]].concat()
        }
        Err(DeviceError::Unsupported(_)) => name.as_bytes().to_vec(),
        Err(error) => return Err(error),
    };
    for byte in identity {
        fingerprint = (fingerprint ^ u64::from(byte)).wrapping_mul(0x100000001b3);
    }
    let reading = match session.lookup(0x1000) {
        Ok(feature) => {
            let caps = session.query(feature, 1, &[])?;
            validate_capabilities(&caps)?;
            let status = session.query(feature, 0, &[])?;
            let (_, percent, next, power) = parse_battery(&status)?;
            let mut reading = Reading::from_percent(name, percent, power)?;
            reading.protocol = protocol;
            reading.metadata_known = true;
            reading.next_reported_percent = next;
            reading.level_count = caps[0];
            reading.capability_flags = caps[1];
            reading
        }
        Err(DeviceError::Unsupported(_)) => {
            let feature = session.lookup(0x1004)?;
            let caps = session.query(feature, 0, &[])?;
            let status = session.query(feature, 1, &[])?;
            parse_unified(name, protocol, &caps, &status)?
        }
        Err(error) => return Err(error),
    };
    Ok((kind, reading, fingerprint))
}
fn parse_unified(
    name: String,
    protocol: (u8, u8),
    caps: &[u8],
    data: &[u8],
) -> Result<Reading, DeviceError> {
    if caps.len() < 2 {
        return Err(DeviceError::Protocol(
            "Unified Battery 能力回复不完整".into(),
        ));
    }
    let supported = caps[0] & 0x0f;
    let reports_percent = caps[1] & 2 != 0;
    if !reports_percent && supported == 0 {
        return Err(DeviceError::Unsupported(
            "设备未声明可用的百分比或档位".into(),
        ));
    }
    if data.len() < 4 || data[0] > 100 {
        return Err(DeviceError::Protocol(
            "Unified Battery 回复范围或长度异常".into(),
        ));
    }
    let power = match data[2] {
        0 => PowerState::Discharging,
        1 | 2 => PowerState::Charging,
        3 => PowerState::Full,
        _ => return Err(DeviceError::Unavailable("电池状态未确认".into())),
    };
    let mut reading = if reports_percent {
        Reading::from_percent(name, data[0], power)?
    } else {
        if data[1] & !supported != 0 {
            return Err(DeviceError::Protocol(
                "Unified Battery 返回未声明支持的档位".into(),
            ));
        }
        if !data[1].is_power_of_two() {
            return Err(DeviceError::Protocol("Unified Battery 档位位图异常".into()));
        }
        let total = supported.count_ones() as u8;
        let current = (supported & data[1].wrapping_sub(1)).count_ones() as u8 + 1;
        let mut value = Reading::from_levels(name, current, total)?;
        value.power_state = power;
        value
    };
    reading.protocol = protocol;
    reading.metadata_known = true;
    reading.capability_flags = caps[1];
    reading.battery_capability_mask = Some(caps[0]);
    if reports_percent {
        reading.level_count = supported.count_ones() as u8;
    }
    Ok(reading)
}

/// Owned by the acquiring thread until every HID handle in `read_inner` is gone.
struct ReceiverQueryGuard {
    handle: HANDLE,
}

impl ReceiverQueryGuard {
    #[cfg(test)]
    fn acquire(name: &str, wait_ms: u32) -> Result<Self, DeviceError> {
        Self::acquire_cancelled(name, wait_ms, &Cancellation::default())
    }
    fn acquire_cancelled(
        name: &str,
        wait_ms: u32,
        cancel: &Cancellation,
    ) -> Result<Self, DeviceError> {
        let wide_name: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
        let handle = unsafe { CreateMutexW(std::ptr::null(), 0, wide_name.as_ptr()) };
        if handle.is_null() {
            return Err(DeviceError::Io(format!(
                "创建接收器查询锁失败：{}",
                unsafe { GetLastError() }
            )));
        }
        let started = Instant::now();
        let result = loop {
            if let Err(error) = cancel.check() {
                unsafe {
                    CloseHandle(handle);
                }
                return Err(error);
            }
            let result = unsafe { WaitForSingleObject(handle, 50.min(wait_ms)) };
            if result != WAIT_TIMEOUT || started.elapsed().as_millis() >= u128::from(wait_ms) {
                break result;
            }
        };
        match result {
            // WAIT_ABANDONED transfers mutex ownership to this thread too.
            WAIT_OBJECT_0 | WAIT_ABANDONED => Ok(Self { handle }),
            _ => {
                let error = if result == WAIT_TIMEOUT {
                    DeviceError::Unavailable("另一个 Quiet Battery 查询仍在运行，请稍后重试".into())
                } else {
                    DeviceError::Io(format!("等待接收器查询锁失败：{}", unsafe {
                        GetLastError()
                    }))
                };
                unsafe {
                    CloseHandle(handle);
                }
                Err(error)
            }
        }
    }
}

impl Drop for ReceiverQueryGuard {
    fn drop(&mut self) {
        unsafe {
            ReleaseMutex(self.handle);
            CloseHandle(self.handle);
        }
    }
}

fn validate_capabilities(caps: &[u8]) -> Result<(), DeviceError> {
    if caps.len() < 2 {
        return Err(DeviceError::Protocol("电池能力回复不完整".into()));
    }
    if caps[0] == 0 {
        return Err(DeviceError::Unsupported(
            "设备未声明电量档位或百分比能力".into(),
        ));
    }
    Ok(())
}

fn io(e: hidapi::HidError) -> DeviceError {
    DeviceError::Io(e.to_string())
}

struct Channel {
    usage: u16,
    device: HidDevice,
}
struct Session {
    channels: Vec<Channel>,
    index: u8,
    cancel: Cancellation,
}

impl Query for Session {
    // Private, and used exclusively by the allowlisted high-level queries below.
    fn query(&self, feature: u8, function: u8, params: &[u8]) -> Result<Vec<u8>, DeviceError> {
        self.cancel.check()?;
        let sid = SEQUENCE.fetch_add(1, Ordering::Relaxed) % 15 + 1;
        let command = (function << 4) | sid;
        let channel = &self.channels[0];
        let report_id = if channel.usage == 2 { 0x11 } else { 0x10 };
        let length = if report_id == 0x11 { 20 } else { 7 };
        if params.len() > length - 4 {
            return Err(DeviceError::Protocol("查询参数过长".into()));
        }
        let mut report = [0u8; 20];
        report[..4].copy_from_slice(&[report_id, self.index, feature, command]);
        report[4..4 + params.len()].copy_from_slice(params);
        // windows-native may report zero on synchronous success, or the padded
        // output length on async success; only Err is a reliable write failure.
        channel.device.write(&report[..length]).map_err(io)?;
        let deadline = Instant::now() + RESPONSE_TIMEOUT;
        loop {
            self.cancel.check()?;
            for channel in &self.channels {
                let mut input = [0u8; 64];
                let count = channel.device.read_timeout(&mut input, 0).map_err(io)?;
                if count == 0 {
                    continue;
                }
                match match_reply(&input[..count], self.index, feature, command) {
                    Reply::Success(data) => return Ok(data.to_vec()),
                    Reply::Error(code) => {
                        return Err(DeviceError::Unavailable(format!("HID++ 错误 0x{code:02X}")));
                    }
                    Reply::Unrelated => (),
                }
            }
            if Instant::now() >= deadline {
                return Err(DeviceError::Unavailable(
                    "读取超时，鼠标可能休眠或未连接".into(),
                ));
            }
            thread::sleep(Duration::from_millis(8));
        }
    }
}
trait Query {
    fn query(&self, feature: u8, function: u8, params: &[u8]) -> Result<Vec<u8>, DeviceError>;
    fn ping(&self) -> Result<(u8, u8), DeviceError> {
        let clock = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos();
        let marker = (clock as u8) ^ SEQUENCE.load(Ordering::Relaxed);
        let data = self.query(0, 1, &[0, 0, marker])?;
        if data.len() < 3 || data[2] != marker {
            return Err(DeviceError::Protocol("Ping 标记不匹配".into()));
        }
        Ok((data[0], data[1]))
    }

    fn lookup(&self, feature_id: u16) -> Result<u8, DeviceError> {
        // ROOT.GetFeature is read-only; there is deliberately no general setter.
        let data = self.query(0, 0, &feature_id.to_be_bytes())?;
        match data.first().copied() {
            Some(index) if index != 0 => Ok(index),
            None => Err(DeviceError::Protocol("ROOT 特性回复不完整".into())),
            _ => Err(DeviceError::Unsupported(format!(
                "不支持特性 0x{feature_id:04X}"
            ))),
        }
    }

    fn name(&self, feature: u8) -> Result<String, DeviceError> {
        let count = self.query(feature, 0, &[])?;
        let len = usize::from(*count.first().unwrap_or(&0));
        if len == 0 || len > 128 {
            return Err(DeviceError::Protocol("设备名称长度异常".into()));
        }
        let mut name = Vec::with_capacity(len);
        while name.len() < len {
            let fragment = self.query(feature, 1, &[name.len() as u8])?;
            if fragment.is_empty() {
                return Err(DeviceError::Protocol("设备名称回复不完整".into()));
            }
            let remaining = len - name.len();
            name.extend_from_slice(&fragment[..fragment.len().min(remaining)]);
        }
        String::from_utf8(name).map_err(|_| DeviceError::Protocol("设备名称不是 UTF-8".into()))
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Reply<'a> {
    Success(&'a [u8]),
    Error(u8),
    Unrelated,
}

fn match_reply(data: &[u8], device: u8, feature: u8, command: u8) -> Reply<'_> {
    let required = match data.first() {
        Some(0x10) => 7,
        Some(0x11) => 20,
        _ => return Reply::Unrelated,
    };
    if data.len() < required || data[1] != device {
        return Reply::Unrelated;
    }
    // HID++ 2.x error: [report, device, FF, requested feature, command, code].
    // HID++ 1.x-style errors use 8F in the same position.
    if matches!(data[2], 0xff | 0x8f) {
        return if data[3] == feature && data[4] == command {
            Reply::Error(data[5])
        } else {
            Reply::Unrelated
        };
    }
    if data[2] == feature && data[3] == command {
        Reply::Success(&data[4..required])
    } else {
        Reply::Unrelated
    }
}

fn parse_battery(data: &[u8]) -> Result<(Level, u8, u8, PowerState), DeviceError> {
    if data.len() < 3 || data[0] > 100 || data[1] > 100 {
        return Err(DeviceError::Protocol("电池状态数据长度或范围异常".into()));
    }
    let state = match data[2] {
        0 => PowerState::Discharging,
        1 | 2 => PowerState::Charging,
        3 => PowerState::Full,
        n => PowerState::Unknown(n),
    };
    if matches!(state, PowerState::Unknown(_)) || (data[0] == 0 && state == PowerState::Charging) {
        return Err(DeviceError::Unavailable("电量状态未提供有效容量".into()));
    }
    // HID++ 0x1000 four-level convention, including the established Windows/Linux
    // workaround accepting 30 as normal. These are labels, not precise capacity.
    let level = match data[0] {
        0..=10 => Level::Critical,
        11..=29 => Level::Low,
        30..=80 => Level::Medium,
        _ => Level::Full,
    };
    Ok((level, data[0], data[1], state))
}

#[cfg(test)]
mod tests {
    use super::*;

    type Step = (u8, u8, Option<Vec<u8>>, Result<Vec<u8>, DeviceError>);
    struct Transcript {
        steps: std::cell::RefCell<std::collections::VecDeque<Step>>,
    }
    impl Query for Transcript {
        fn query(&self, feature: u8, function: u8, params: &[u8]) -> Result<Vec<u8>, DeviceError> {
            let (expected_feature, expected_function, expected_params, response) = self
                .steps
                .borrow_mut()
                .pop_front()
                .expect("unexpected query");
            assert_eq!((feature, function), (expected_feature, expected_function));
            if let Some(expected) = expected_params {
                assert_eq!(params, expected);
            }
            if feature == 0 && function == 1 {
                return Ok(vec![4, 2, params[2]]);
            }
            response
        }
    }
    fn transcript_prefix(identity: Result<Vec<u8>, DeviceError>) -> Vec<Step> {
        vec![
            (0, 1, None, Ok(vec![])),
            (0, 0, Some(vec![0, 5]), Ok(vec![2])),
            (2, 0, Some(vec![]), Ok(vec![4])),
            (2, 1, Some(vec![0]), Ok(b"Test".to_vec())),
            (2, 2, Some(vec![]), Ok(vec![3])),
            (0, 0, Some(vec![0, 3]), identity),
        ]
    }
    #[test]
    fn query_transcript_routes_1000_and_unified_capabilities_without_setters() {
        for (percent_cap, status, expected) in [
            (true, vec![0, 8, 0, 0], 0),
            (true, vec![87, 4, 0, 0], 87),
            (false, vec![0, 4, 0, 0], 75),
        ] {
            let mut steps = transcript_prefix(Ok(vec![0]));
            steps.extend([
                (0, 0, Some(vec![0x10, 0]), Ok(vec![0])),
                (0, 0, Some(vec![0x10, 4]), Ok(vec![7])),
                (
                    7,
                    0,
                    Some(vec![]),
                    Ok(vec![15, if percent_cap { 2 } else { 0 }]),
                ),
                (7, 1, Some(vec![]), Ok(status)),
            ]);
            let transport = Transcript {
                steps: std::cell::RefCell::new(steps.into()),
            };
            let (kind, reading, _) = query_device(&transport).unwrap();
            assert_eq!(kind, DeviceKind::Mouse);
            assert_eq!(reading.normalized_percent(), expected);
            assert_eq!(reading.has_fine_estimate(), percent_cap);
            assert_eq!(
                matches!(reading.source, super::super::BatterySource::ReportedPercent),
                percent_cap
            );
            assert!(transport.steps.borrow().is_empty());
        }
        let mut steps = transcript_prefix(Ok(vec![0]));
        steps.extend([
            (0, 0, Some(vec![0x10, 0]), Ok(vec![5])),
            (5, 1, Some(vec![]), Ok(vec![4, 0])),
            (5, 0, Some(vec![]), Ok(vec![90, 50, 0])),
        ]);
        let transport = Transcript {
            steps: std::cell::RefCell::new(steps.into()),
        };
        let (_, reading, _) = query_device(&transport).unwrap();
        assert_eq!(reading.reported_percent, 90);
        assert!(!reading.has_fine_estimate());
        assert!(transport.steps.borrow().is_empty());
    }
    #[test]
    fn identity_falls_back_only_when_feature_is_absent() {
        for error in [
            DeviceError::Unavailable("timeout".into()),
            DeviceError::Io("io".into()),
            DeviceError::Protocol("malformed".into()),
        ] {
            let transport = Transcript {
                steps: std::cell::RefCell::new(transcript_prefix(Err(error)).into()),
            };
            assert!(query_device(&transport).is_err());
            assert!(transport.steps.borrow().is_empty());
        }
        for data in [vec![], vec![0; 13]] {
            let mut steps = transcript_prefix(Ok(vec![3]));
            steps.push((3, 0, Some(vec![]), Ok(data)));
            let transport = Transcript {
                steps: std::cell::RefCell::new(steps.into()),
            };
            assert!(matches!(
                query_device(&transport),
                Err(DeviceError::Protocol(_))
            ));
            assert!(transport.steps.borrow().is_empty());
        }
    }
    #[test]
    fn cancelled_scan_discards_partial_work_and_stops_next_slot() {
        let cancel = Cancellation::default();
        let mut calls = 0;
        let result = scan_slots(&cancel, |_| {
            calls += 1;
            cancel.cancel();
            Ok(90)
        });
        assert!(matches!(result, Err(DeviceError::Cancelled)));
        assert_eq!(calls, 1);
    }
    #[test]
    fn unified_reported_and_level_only_fixtures() {
        for percent in [0, 87, 90, 100] {
            let reading =
                parse_unified("Device".into(), (4, 2), &[15, 2], &[percent, 0, 0, 0]).unwrap();
            assert_eq!(reading.reported_percent, percent);
        }
        let reading = parse_unified("Keyboard".into(), (4, 2), &[15, 0], &[0, 4, 0, 0]).unwrap();
        assert_eq!(reading.normalized_percent(), 75);
        assert_eq!(
            reading.source,
            super::super::BatterySource::Levels {
                current: 3,
                total: 4
            }
        );
        for data in [
            &[101, 0, 0, 0][..],
            &[0, 3, 0, 0],
            &[90, 0, 255, 0],
            &[90, 0, 0],
        ] {
            assert!(parse_unified("Device".into(), (4, 2), &[15, 0], data).is_err());
        }
    }
    #[test]
    fn unified_capabilities_distinguish_zero_from_unknown_and_reject_errors() {
        let zero = parse_unified("Fine".into(), (4, 2), &[15, 2], &[0, 8, 0, 0]).unwrap();
        assert_eq!(zero.normalized_percent(), 0);
        assert_eq!(zero.source, super::super::BatterySource::ReportedPercent);
        assert!(parse_unified("Levels".into(), (4, 2), &[3, 0], &[0, 4, 0, 0]).is_err());
        assert!(parse_unified("Fine".into(), (4, 2), &[15, 2], &[90, 4, 4, 0]).is_err());
        assert!(parse_unified("None".into(), (4, 2), &[0, 0], &[0, 0, 0, 0]).is_err());
        assert!(parse_unified("Truncated".into(), (4, 2), &[15], &[90, 4, 0, 0]).is_err());
    }
    #[test]
    fn unified_normalizes_only_declared_levels_and_keeps_capabilities() {
        let value = parse_unified("Two-level".into(), (4, 2), &[12, 0], &[0, 4, 0, 0]).unwrap();
        assert_eq!(value.normalized_percent(), 50);
        assert_eq!(
            value.source,
            super::super::BatterySource::Levels {
                current: 1,
                total: 2
            }
        );
        assert_eq!(value.battery_capability_mask, Some(12));
        assert_eq!(value.capability_flags, 0);
    }
    #[test]
    fn capabilities_are_not_model_or_four_level_gated() {
        for caps in [[4, 0], [3, 0], [10, 0], [100, 2], [4, 2]] {
            assert!(validate_capabilities(&caps).is_ok());
        }
        assert!(validate_capabilities(&[0, 0]).is_err());
        assert!(validate_capabilities(&[4]).is_err());
    }
    fn unique_test_lock(purpose: &str) -> String {
        format!(
            "Local\\QuietBattery.Test.{}.{}.{}",
            std::process::id(),
            purpose,
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        )
    }

    // This entry point is also run in a separate test executable process by the
    // test below. It uses an isolated test mutex and never opens real hardware.
    #[test]
    fn named_lock_child_process() {
        let Ok(name) = std::env::var("QUIET_BATTERY_TEST_MUTEX") else {
            return;
        };
        let should_acquire =
            std::env::var("QUIET_BATTERY_TEST_MUTEX_EXPECT").unwrap() == "acquired";
        let result = ReceiverQueryGuard::acquire(&name, 40);
        assert_eq!(result.is_ok(), should_acquire);
        if !should_acquire {
            assert!(matches!(result, Err(DeviceError::Unavailable(_))));
        }
    }

    #[test]
    fn named_lock_serializes_processes_and_releases_on_drop() {
        let name = unique_test_lock("process");
        let owner = ReceiverQueryGuard::acquire(&name, 100).unwrap();
        let run_child = |expected: &str| {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "device::logitech::tests::named_lock_child_process",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env("QUIET_BATTERY_TEST_MUTEX", &name)
                .env("QUIET_BATTERY_TEST_MUTEX_EXPECT", expected)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "child failed: {}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
        };
        run_child("timeout");
        drop(owner);
        run_child("acquired");
    }

    #[test]
    fn abandoned_lock_is_owned_and_released_normally() {
        let name = unique_test_lock("abandoned");
        let child_name = name.clone();
        let raw_handle = thread::spawn(move || {
            let guard = ReceiverQueryGuard::acquire(&child_name, 100).unwrap();
            let raw = guard.handle as usize;
            // Simulate an owning thread that exits without ReleaseMutex while
            // preserving its handle for deterministic cleanup in the parent.
            std::mem::forget(guard);
            raw
        })
        .join()
        .unwrap();
        let acquired = ReceiverQueryGuard::acquire(&name, 100).unwrap();
        unsafe {
            CloseHandle(raw_handle as HANDLE);
        }
        drop(acquired);
        let after_drop = ReceiverQueryGuard::acquire(&name, 100).unwrap();
        drop(after_drop);
    }

    #[test]
    fn coarse_battery_report_and_boundaries() {
        let (level, current, next, state) = parse_battery(&[0x5a, 0x32, 0]).unwrap();
        assert_eq!(
            (level, current, next, state),
            (Level::Full, 90, 50, PowerState::Discharging)
        );
        for (value, expected) in [
            (0, Level::Critical),
            (10, Level::Critical),
            (11, Level::Low),
            (29, Level::Low),
            (30, Level::Medium),
            (50, Level::Medium),
            (80, Level::Medium),
            (81, Level::Full),
            (100, Level::Full),
        ] {
            assert_eq!(parse_battery(&[value, 0, 0]).unwrap().0, expected);
        }
    }

    #[test]
    fn invalid_and_unknown_battery_are_not_empty() {
        for data in [
            &[90, 50][..],
            &[101, 50, 0],
            &[90, 101, 0],
            &[0, 0, 1],
            &[0, 0, 7],
            &[90, 50, 4],
        ] {
            assert!(parse_battery(data).is_err());
        }
    }

    #[test]
    fn replies_require_exact_request_identity() {
        let mut reply = [0u8; 20];
        reply[..7].copy_from_slice(&[0x11, 1, 5, 0x07, 90, 50, 0]);
        assert_eq!(match_reply(&reply, 1, 5, 0x07), Reply::Success(&reply[4..]));
        for (device, feature, command) in [(2, 5, 7), (1, 6, 7), (1, 5, 8), (1, 5, 0x17), (1, 5, 0)]
        {
            assert_eq!(
                match_reply(&reply, device, feature, command),
                Reply::Unrelated
            );
        }
        assert_eq!(match_reply(&reply[..7], 1, 5, 7), Reply::Unrelated);
    }

    #[test]
    fn short_reports_and_correlated_errors() {
        let reply = [0x10, 1, 5, 7, 90, 50, 0];
        assert_eq!(match_reply(&reply, 1, 5, 7), Reply::Success(&reply[4..]));
        for marker in [0xff, 0x8f] {
            let error = [0x10, 1, marker, 5, 7, 8, 0];
            assert_eq!(match_reply(&error, 1, 5, 7), Reply::Error(8));
            assert_eq!(match_reply(&error, 1, 5, 9), Reply::Unrelated);
        }
        assert_eq!(
            match_reply(&[0, 1, 5, 7, 90, 50, 0], 1, 5, 7),
            Reply::Unrelated
        );
    }

    #[test]
    fn collections_group_only_on_same_receiver() {
        let short = r"\\?\HID#VID_046D&PID_C53F&MI_02&Col01#7&1ce076a8&0&0000#{guid}";
        let long = r"\\?\HID#VID_046D&PID_C53F&MI_02&Col02#7&1ce076a8&0&0001#{guid}";
        assert_eq!(receiver_key(short), receiver_key(long));
        let other = long.replace("1ce076a8", "2cc01234");
        assert_ne!(receiver_key(short), receiver_key(&other));
        assert_eq!(receiver_key("unexpected-path"), "unexpected-path");
    }
}
