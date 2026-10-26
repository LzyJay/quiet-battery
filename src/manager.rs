//! Identity-keyed selection and last-success persistence, independent of Win32.
#[cfg(test)]
use crate::device::Candidate;
use crate::device::{
    BatterySource, Connection, DeviceDescriptor, DeviceError, DeviceKind, DiscoveryReport,
    PowerState, Reading, level_for,
};
use crate::notifications::AlertRecord;
use std::path::{Path, PathBuf};
#[derive(Clone, Debug)]
pub struct DeviceState {
    pub descriptor: DeviceDescriptor,
    pub selected: bool,
    pub removed: bool,
    pub show_tray: bool,
    pub notify: bool,
    pub alert: AlertRecord,
    pub reading: Option<Reading>,
    pub last_success: Option<u64>,
    pub error: Option<DeviceError>,
    pub connected: bool,
    pub connection_known: bool,
}
#[derive(Clone)]
pub struct Manager {
    pub devices: Vec<DeviceState>,
    pub notifications_enabled: bool,
    pub threshold: u8,
    /// Periodic background refresh of battery values.
    pub auto_refresh: bool,
    /// Seconds between automatic refreshes when `auto_refresh` is set.
    pub refresh_seconds: u32,
    /// Shell toast for confirmed link up/down transitions.
    pub connection_notifications: bool,
    pub pending_alerts: Vec<(String, String, u8)>,
    /// Confirmed link transitions: device id and the new state.
    pub pending_connections: Vec<(String, bool)>,
    /// False until the first discovery has established a baseline, so starting
    /// the app never announces every already-connected device.
    pub connection_baseline: bool,
    pub persistence_error: Option<String>,
    pub discovery_errors: Vec<(String, String)>,
}
impl Default for Manager {
    fn default() -> Self {
        Self {
            devices: Vec::new(),
            notifications_enabled: true,
            threshold: 20,
            auto_refresh: true,
            refresh_seconds: DEFAULT_REFRESH_SECONDS,
            connection_notifications: true,
            pending_alerts: Vec::new(),
            pending_connections: Vec::new(),
            connection_baseline: false,
            persistence_error: None,
            discovery_errors: Vec::new(),
        }
    }
}
/// Two minutes: the historical cadence before the setting existed.
pub const DEFAULT_REFRESH_SECONDS: u32 = 120;
/// Bounds accepted from the settings field, in minutes.
pub const REFRESH_MINUTES: std::ops::RangeInclusive<u32> = 1..=60;
impl Manager {
    pub fn load(path: &Path) -> Result<Self, String> {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                if !matches!(
                    text.lines().next(),
                    Some(
                        "QuietBattery\t4"
                            | "QuietBattery\t5"
                            | "QuietBattery\t6"
                            | "QuietBattery\t7"
                    )
                ) {
                    return Err("设置文件格式无法识别，原文件未修改".into());
                }
                Ok(Self::decode(&text))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(format!("无法读取设置文件：{error}")),
        }
    }
    pub fn selected(&self) -> impl Iterator<Item = &DeviceState> {
        self.devices.iter().filter(|s| s.selected)
    }
    pub fn visible(&self) -> impl Iterator<Item = &DeviceState> {
        self.selected()
            .filter(|s| s.connected || !s.connection_known)
    }
    #[cfg(test)]
    pub fn merge(&mut self, candidates: Vec<Candidate>, timestamp: u64) {
        self.merge_discovery(
            DiscoveryReport {
                candidates,
                failures: Vec::new(),
            },
            timestamp,
        )
    }
    pub fn merge_discovery(&mut self, report: DiscoveryReport, timestamp: u64) {
        self.discovery_errors = report.failures;
        let candidates = report.candidates;
        let before: Vec<(String, bool, bool)> = self
            .devices
            .iter()
            .map(|s| (s.descriptor.id.clone(), s.connected, s.connection_known))
            .collect();
        let seen: std::collections::BTreeSet<_> =
            candidates.iter().map(|c| c.descriptor.id.clone()).collect();
        for state in &mut self.devices {
            if !self
                .discovery_errors
                .iter()
                .any(|(provider, _)| provider == &state.descriptor.provider)
            {
                state.connected = false;
                state.connection_known = true;
            }
        }
        for candidate in candidates {
            let id = &candidate.descriptor.id;
            if !candidate.descriptor.supported && candidate.descriptor.recoverable {
                let mut known = false;
                for state in self.devices.iter_mut().filter(|state| {
                    state.descriptor.supported
                        && state.descriptor.recoverable
                        && state.descriptor.physical_id == candidate.descriptor.physical_id
                        && state.descriptor.provider == candidate.descriptor.provider
                        && (candidate.descriptor.endpoint.is_none()
                            || state.descriptor.endpoint == candidate.descriptor.endpoint)
                }) {
                    state.connected = true;
                    state.connection_known = true;
                    state.error = candidate.error.clone();
                    known = true;
                }
                if known {
                    continue;
                }
            }

            if let Some(state) = self.devices.iter_mut().find(|s| s.descriptor.id == *id) {
                state.connected = true;
                state.connection_known = true;
                // A transient first discovery may create an unsupported placeholder.
                // Its first confirmed mouse discovery gets the same default selection
                // as a new supported mouse. Removal is the explicit monitoring opt-out;
                // existing supported devices and their preferences keep their choices.
                if !state.descriptor.supported
                    && candidate.descriptor.supported
                    && candidate.descriptor.kind == DeviceKind::Mouse
                    && state.reading.is_none()
                    && state.last_success.is_none()
                    && !state.removed
                {
                    state.selected = true;
                }
                if candidate.descriptor.supported {
                    state.descriptor = candidate.descriptor;
                }
                if let Some(reading) = candidate.reading {
                    if state.alert.observe(
                        &reading,
                        self.threshold,
                        self.notifications_enabled && state.notify && state.selected,
                    ) {
                        self.pending_alerts.push((
                            state.descriptor.id.clone(),
                            state.descriptor.name.clone(),
                            reading.normalized_percent(),
                        ));
                    }
                    state.reading = Some(reading);
                    state.last_success = Some(timestamp);
                    state.error = None;
                } else {
                    state.error = candidate.error;
                }
            } else {
                let automatic = candidate.descriptor.supported
                    && candidate.descriptor.kind == DeviceKind::Mouse;
                let success = candidate.reading.is_some();
                let mut alert = AlertRecord::default();
                if let Some(reading) = &candidate.reading {
                    if alert.observe(
                        reading,
                        self.threshold,
                        self.notifications_enabled && automatic,
                    ) {
                        self.pending_alerts.push((
                            candidate.descriptor.id.clone(),
                            candidate.descriptor.name.clone(),
                            reading.normalized_percent(),
                        ));
                    }
                }
                self.devices.push(DeviceState {
                    descriptor: candidate.descriptor,
                    selected: automatic,
                    removed: false,
                    show_tray: true,
                    notify: true,
                    alert,
                    reading: candidate.reading,
                    last_success: success.then_some(timestamp),
                    error: candidate.error,
                    connected: true,
                    connection_known: true,
                });
            }
        }
        for state in &mut self.devices {
            if let Some((_, error)) = self
                .discovery_errors
                .iter()
                .find(|(provider, _)| provider == &state.descriptor.provider)
            {
                if !seen.contains(&state.descriptor.id) {
                    state.error = Some(DeviceError::Io(error.clone()));
                }
            } else if !state.connected {
                state.error = Some(DeviceError::NotFound);
            }
        }
        self.devices.retain(|state| {
            state.connected
                || state.selected
                || state.removed
                || state.reading.is_some()
                || !state.show_tray
        });
        self.record_connections(&before);
    }
    /// Link transitions are only reported once the previous state was confirmed.
    /// Discovery updates several flags per pass, so the whole merge is compared
    /// against one snapshot instead of intermediate values.
    fn record_connections(&mut self, before: &[(String, bool, bool)]) {
        let first_pass = !self.connection_baseline;
        self.connection_baseline = true;
        if first_pass || !self.connection_notifications {
            return;
        }
        for state in self.devices.iter().filter(|s| s.selected && !s.removed) {
            let Some((_, was, known)) = before.iter().find(|(id, _, _)| *id == state.descriptor.id)
            else {
                // First sighting of a device is not a reconnect.
                continue;
            };
            if *known && state.connection_known && *was != state.connected {
                self.pending_connections
                    .push((state.descriptor.id.clone(), state.connected));
            }
        }
    }
    pub fn display_name(&self, id: &str, short: bool) -> String {
        let Some(state) = self.devices.iter().find(|s| s.descriptor.id == id) else {
            return String::new();
        };
        let name = if short {
            state.descriptor.short_name()
        } else {
            state.descriptor.name.clone()
        };
        if self.devices.iter().any(|other| {
            other.descriptor.id != id
                && (other.connected || other.selected)
                && (if short {
                    other.descriptor.short_name()
                } else {
                    other.descriptor.name.clone()
                }) == name
        }) {
            format!(
                "{name} · {:08X}",
                id.bytes().fold(2166136261u32, |h, b| (h ^ u32::from(b))
                    .wrapping_mul(16777619))
            )
        } else {
            name
        }
    }
    pub fn apply(&mut self, id: &str, result: Result<Reading, DeviceError>, timestamp: u64) {
        if let Some(state) = self.devices.iter_mut().find(|s| s.descriptor.id == id) {
            match result {
                Ok(reading) => {
                    if state.alert.observe(
                        &reading,
                        self.threshold,
                        self.notifications_enabled && state.notify && state.selected,
                    ) {
                        self.pending_alerts.push((
                            state.descriptor.id.clone(),
                            state.descriptor.name.clone(),
                            reading.normalized_percent(),
                        ));
                    }
                    state.reading = Some(reading);
                    state.last_success = Some(timestamp);
                    state.error = None;
                    state.connected = true;
                    state.connection_known = true;
                }
                Err(error) => {
                    if matches!(error, DeviceError::NotFound) {
                        state.connected = false;
                        state.connection_known = true;
                    }
                    state.error = Some(error);
                }
            }
        }
    }
    pub fn select(&mut self, id: &str) -> bool {
        if let Some(state) = self
            .devices
            .iter_mut()
            .find(|s| s.descriptor.id == id && s.descriptor.supported)
        {
            state.selected = true;
            state.removed = false;
            true
        } else {
            false
        }
    }
    pub fn remove(&mut self, id: &str) {
        if let Some(state) = self.devices.iter_mut().find(|s| s.descriptor.id == id) {
            state.selected = false;
            state.removed = true;
        }
    }
    pub fn toggle_tray(&mut self, id: &str) {
        if let Some(state) = self.devices.iter_mut().find(|s| s.descriptor.id == id) {
            state.show_tray = !state.show_tray;
        }
    }
    pub fn save(&mut self, path: &Path) {
        self.persistence_error = self.write(path).err();
    }
    fn write(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let mut text = format!(
            "QuietBattery\t7\npreferences\t{}\t{}\t{}\t{}\t{}\n",
            u8::from(self.notifications_enabled),
            self.threshold,
            u8::from(self.auto_refresh),
            self.refresh_seconds,
            u8::from(self.connection_notifications)
        );
        for state in &self.devices {
            // Only user choices and successful data are durable; enumeration-only
            // unsupported rows are rediscovered and do not grow the state file.
            if !state.selected && !state.removed && state.reading.is_none() {
                continue;
            }
            let reading = state.reading.as_ref();
            let source = match reading.map(|r| r.source) {
                Some(BatterySource::Levels { current, total }) => format!("{current}/{total}"),
                _ => "percent".into(),
            };
            text.push_str(&format!(
                "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
                hex(&state.descriptor.id),
                hex(&state.descriptor.name),
                state.descriptor.kind.key(),
                u8::from(state.selected),
                u8::from(state.removed),
                u8::from(state.show_tray),
                reading.map_or_else(|| "-".into(), |r| r.normalized_percent().to_string()),
                state
                    .last_success
                    .map_or_else(|| "-".into(), |t| t.to_string()),
                source,
                u8::from(state.descriptor.supported),
                state.descriptor.provider,
                hex(&state.descriptor.physical_id),
                state
                    .descriptor
                    .endpoint
                    .as_ref()
                    .map_or_else(|| "-".into(), |s| hex(s)),
                u8::from(state.descriptor.recoverable),
                reading.map_or_else(|| "-".into(), encode_metadata),
                u8::from(state.notify),
                state.alert.encode()
            ));
        }
        let temporary = path.with_extension("tmp");
        std::fs::write(&temporary, text).map_err(|e| e.to_string())?;
        crate::app::replace_file(&temporary, path)
    }
    fn decode(text: &str) -> Self {
        let mut manager = Self::default();
        let mut lines = text.lines();
        let version = match lines.next() {
            Some("QuietBattery\t4") => 4,
            Some("QuietBattery\t5") => 5,
            Some("QuietBattery\t6") => 6,
            Some("QuietBattery\t7") => 7,
            _ => return manager,
        };
        let mut device_records = 0;
        for line in lines.take(1024) {
            let fields: Vec<_> = line.split('\t').collect();
            if fields.first() == Some(&"preferences") {
                if version == 7 {
                    if fields.len() == 6
                        && matches!(fields[1], "0" | "1")
                        && fields[2]
                            .parse::<u8>()
                            .is_ok_and(|n| (1..=100).contains(&n))
                        && matches!(fields[3], "0" | "1")
                        && fields[4].parse::<u32>().is_ok_and(|n| {
                            (REFRESH_MINUTES.start() * 60..=REFRESH_MINUTES.end() * 60).contains(&n)
                        })
                        && matches!(fields[5], "0" | "1")
                    {
                        manager.notifications_enabled = fields[1] == "1";
                        manager.threshold = fields[2].parse().unwrap();
                        manager.auto_refresh = fields[3] == "1";
                        manager.refresh_seconds = fields[4].parse().unwrap();
                        manager.connection_notifications = fields[5] == "1";
                    }
                } else if version == 6
                    && fields.len() == 3
                    && matches!(fields[1], "0" | "1")
                    && fields[2]
                        .parse::<u8>()
                        .is_ok_and(|n| (1..=100).contains(&n))
                {
                    manager.notifications_enabled = fields[1] == "1";
                    manager.threshold = fields[2].parse().unwrap();
                }
                continue;
            }
            if device_records >= 512 {
                break;
            }
            device_records += 1;
            if fields.len()
                != match version {
                    4 => 12,
                    5 => 15,
                    _ => 17,
                }
            {
                continue;
            }
            let Some(id) = unhex(fields[0]) else { continue };
            let Some(name) = unhex(fields[1]) else {
                continue;
            };
            let Some(kind) = DeviceKind::from_key(fields[2]) else {
                continue;
            };
            if id.is_empty()
                || id.len() > 4096
                || name.is_empty()
                || name.len() > 1024
                || manager.devices.iter().any(|s| s.descriptor.id == id)
            {
                continue;
            }
            let flags: Option<Vec<bool>> = [fields[3], fields[4], fields[5], fields[9]]
                .into_iter()
                .map(|f| match f {
                    "0" => Some(false),
                    "1" => Some(true),
                    _ => None,
                })
                .collect();
            let Some(flags) = flags else { continue };
            let mut reading = if fields[6] == "-" {
                None
            } else {
                let Ok(percent) = fields[6].parse::<u8>() else {
                    continue;
                };
                if percent > 100 {
                    continue;
                }
                let source = if fields[8] == "percent" {
                    BatterySource::ReportedPercent
                } else {
                    let Some((a, b)) = fields[8].split_once('/') else {
                        continue;
                    };
                    let (Ok(current), Ok(total)) = (a.parse::<u8>(), b.parse::<u8>()) else {
                        continue;
                    };
                    if total == 0
                        || current > total
                        || u16::from(current) * 100 / u16::from(total) != u16::from(percent)
                    {
                        continue;
                    }
                    BatterySource::Levels { current, total }
                };
                Some(Reading {
                    device_name: name.clone(),
                    protocol: (0, 0),
                    metadata_known: false,
                    level: level_for(percent),
                    reported_percent: percent,
                    next_reported_percent: 0,
                    level_count: 0,
                    capability_flags: 0,
                    battery_capability_mask: None,
                    power_state: PowerState::Unknown(255),
                    source,
                })
            };
            if version >= 5 {
                if let Some(reading) = reading.as_mut() {
                    if !decode_metadata(fields[14], reading) {
                        continue;
                    }
                } else if fields[14] != "-" {
                    continue;
                }
            }
            let last_success = if fields[7] == "-" {
                None
            } else {
                let Ok(time) = fields[7].parse::<u64>() else {
                    continue;
                };
                Some(time)
            };
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            if last_success.is_some_and(|time| time > now.saturating_add(300)) {
                continue;
            }
            if reading.is_some() != last_success.is_some() {
                continue;
            }
            manager.devices.push(DeviceState {
                descriptor: DeviceDescriptor {
                    physical_id: match unhex(fields[11]) {
                        Some(value) if !value.is_empty() => value,
                        _ => continue,
                    },
                    id,
                    name,
                    kind,
                    supported: flags[3],
                    provider: fields[10].to_owned(),
                    // The link type is adapter evidence, not a persisted choice.
                    // The next discovery repopulates it for present devices.
                    connection: Connection::Unknown,
                    endpoint: if version == 4 || fields[12] == "-" {
                        None
                    } else {
                        match unhex(fields[12]) {
                            Some(value) => Some(value),
                            _ => continue,
                        }
                    },
                    recoverable: if version == 4 {
                        flags[3]
                    } else {
                        match fields[13] {
                            "0" => false,
                            "1" => true,
                            _ => continue,
                        }
                    },
                },
                selected: flags[0] && flags[3] && !flags[1],
                removed: flags[1],
                show_tray: flags[2],
                notify: if version >= 6 {
                    match fields[15] {
                        "0" => false,
                        "1" => true,
                        _ => continue,
                    }
                } else {
                    true
                },
                alert: if version >= 6 {
                    match AlertRecord::decode(fields[16]) {
                        Some(value) => value,
                        None => continue,
                    }
                } else {
                    AlertRecord::default()
                },
                reading,
                last_success,
                error: None,
                connected: false,
                connection_known: false,
            });
        }
        manager
    }
}
fn encode_metadata(reading: &Reading) -> String {
    let level = match reading.level {
        crate::device::Level::Full => 0,
        crate::device::Level::Medium => 1,
        crate::device::Level::Low => 2,
        crate::device::Level::Critical => 3,
    };
    let power = match reading.power_state {
        PowerState::Discharging => 0,
        PowerState::Charging => 1,
        PowerState::Full => 2,
        PowerState::Unknown(n) => 256 + u16::from(n),
    };
    format!(
        "{},{},{},{},{},{},{},{},{},{}",
        hex(&reading.device_name),
        reading.protocol.0,
        reading.protocol.1,
        level,
        reading.next_reported_percent,
        reading.level_count,
        reading.capability_flags,
        reading
            .battery_capability_mask
            .map_or_else(|| "-".into(), |m| m.to_string()),
        power,
        u8::from(reading.metadata_known)
    )
}
fn decode_metadata(text: &str, reading: &mut Reading) -> bool {
    let fields: Vec<_> = text.split(',').collect();
    if fields.len() != 10 {
        return false;
    }
    let Some(name) = unhex(fields[0]) else {
        return false;
    };
    if name.is_empty() || name.len() > 1024 {
        return false;
    }
    let parsed: Option<Vec<u8>> = fields[1..7].iter().map(|f| f.parse().ok()).collect();
    let Some(values) = parsed else { return false };
    if values[3] > 100 {
        return false;
    }
    let level = match values[2] {
        0 => crate::device::Level::Full,
        1 => crate::device::Level::Medium,
        2 => crate::device::Level::Low,
        3 => crate::device::Level::Critical,
        _ => return false,
    };
    let mask = if fields[7] == "-" {
        None
    } else {
        let Ok(mask) = fields[7].parse::<u8>() else {
            return false;
        };
        Some(mask)
    };
    let power = match fields[8].parse::<u16>() {
        Ok(0) => PowerState::Discharging,
        Ok(1) => PowerState::Charging,
        Ok(2) => PowerState::Full,
        Ok(n) if (256..=511).contains(&n) => PowerState::Unknown((n - 256) as u8),
        _ => return false,
    };
    let known = match fields[9] {
        "0" => false,
        "1" => true,
        _ => return false,
    };
    reading.device_name = name;
    reading.protocol = (values[0], values[1]);
    reading.level = level;
    reading.next_reported_percent = values[3];
    reading.level_count = values[4];
    reading.capability_flags = values[5];
    reading.battery_capability_mask = mask;
    reading.power_state = power;
    reading.metadata_known = known;
    true
}
fn hex(text: &str) -> String {
    text.as_bytes().iter().map(|b| format!("{b:02x}")).collect()
}
fn unhex(text: &str) -> Option<String> {
    if text.len() % 2 != 0 || text.len() > 8192 {
        return None;
    }
    let bytes: Option<Vec<u8>> = text
        .as_bytes()
        .chunks_exact(2)
        .map(|b| {
            std::str::from_utf8(b)
                .ok()
                .and_then(|s| u8::from_str_radix(s, 16).ok())
        })
        .collect();
    String::from_utf8(bytes?).ok()
}
pub fn state_path() -> PathBuf {
    crate::app::data_directory().join(if cfg!(test) {
        format!("devices-test-{:?}.state", std::thread::current().id())
    } else {
        "devices-v2.state".into()
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn locked_settings_must_not_silently_become_defaults() {
        use std::os::windows::fs::OpenOptionsExt;
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!(
            ".build/tmp/read-locked-{}.state",
            std::process::id()
        ));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"QuietBattery\t6\npreferences\t0\t37\n").unwrap();
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&path)
            .unwrap();
        let loaded = Manager::load(&path);
        drop(lock);
        std::fs::remove_file(path).unwrap();
        assert!(loaded.err().is_some_and(|e| e.contains("无法读取设置文件")));
    }
    #[test]
    fn v6_preferences_do_not_consume_the_512_device_budget() {
        let mut text = "QuietBattery\t6\npreferences\t0\t21\n".to_owned();
        for index in 0..513 {
            let id = hex(&format!("device-{index}"));
            text.push_str(&format!(
                "{id}\t{}\tmouse\t1\t0\t1\t-\t-\tpercent\t1\tfixture\t{id}\t-\t1\t-\t1\t0,-\n",
                hex("Fixture")
            ));
        }
        let manager = Manager::decode(&text);
        assert_eq!(manager.devices.len(), 512);
        assert_eq!(manager.devices[511].descriptor.id, "device-511");
        assert_eq!(manager.threshold, 21);
        assert!(!manager.notifications_enabled);
    }
    fn candidate(id: &str, kind: DeviceKind, supported: bool) -> Candidate {
        Candidate {
            descriptor: DeviceDescriptor {
                id: id.into(),
                physical_id: id.into(),
                endpoint: Some(id.into()),
                recoverable: supported,
                name: "Peripheral".into(),
                kind,
                supported,
                provider: "fixture".into(),
                connection: Connection::Receiver,
            },
            reading: supported.then_some(Reading::from_levels("Peripheral".into(), 3, 4).unwrap()),
            error: None,
        }
    }
    #[test]
    fn selection_identity_cache_and_restart() {
        let mut m = Manager::default();
        m.merge(
            vec![
                candidate("a", DeviceKind::Mouse, true),
                candidate("b", DeviceKind::Keyboard, true),
                candidate("c", DeviceKind::Mouse, false),
            ],
            100,
        );
        assert_eq!(m.selected().count(), 1);
        assert!(m.select("b"));
        assert!(!m.select("c"));
        m.apply("a", Err(DeviceError::NotFound), 101);
        assert_eq!(
            m.devices[0].reading.as_ref().unwrap().normalized_percent(),
            75
        );
        m.toggle_tray("a");
        m.remove("a");
        m.merge(vec![candidate("a", DeviceKind::Mouse, true)], 102);
        assert!(!m.devices[0].selected);
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join(format!(".build/tmp/manager-{}.state", std::process::id()));
        m.save(&path);
        assert!(m.persistence_error.is_none());
        let restored = Manager::load(&path).unwrap();
        assert_eq!(restored.selected().count(), 1);
        assert!(restored.devices[0].removed);
        assert!(!restored.devices[0].show_tray);
        assert_eq!(
            restored.devices[1]
                .reading
                .as_ref()
                .unwrap()
                .normalized_percent(),
            75
        );
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn first_successful_mouse_after_placeholder_is_selected_but_keyboard_is_manual() {
        for kind in [DeviceKind::Mouse, DeviceKind::Keyboard] {
            let mut manager = Manager::default();
            let mut unavailable = candidate("first", kind, false);
            unavailable.descriptor.recoverable = true;
            unavailable.error = Some(DeviceError::Unavailable("asleep on first discovery".into()));
            manager.merge(vec![unavailable], 100);
            assert!(!manager.devices[0].selected);
            let mut success = candidate("first", kind, true);
            success.reading = Some(
                Reading::from_percent("Peripheral".into(), 9, PowerState::Discharging).unwrap(),
            );
            manager.merge(vec![success], 200);
            let state = &manager.devices[0];
            assert_eq!(state.selected, kind == DeviceKind::Mouse);
            assert_eq!(
                manager.visible().count(),
                usize::from(kind == DeviceKind::Mouse)
            );
            assert!(state.connected && state.show_tray && state.descriptor.supported);
            assert_eq!(state.last_success, Some(200));
            assert_eq!(state.reading.as_ref().unwrap().normalized_percent(), 9);
        }
    }
    #[test]
    fn removed_mouse_preserves_opt_out_and_preferences_across_failure_and_recovery() {
        let mut manager = Manager::default();
        manager.merge(vec![candidate("removed", DeviceKind::Mouse, true)], 100);
        manager.remove("removed");
        manager.devices[0].show_tray = false;
        manager.devices[0].notify = false;
        let mut failed = candidate("removed", DeviceKind::Mouse, false);
        failed.descriptor.recoverable = true;
        failed.error = Some(DeviceError::Unavailable("asleep".into()));
        manager.merge(vec![failed], 200);
        assert_eq!(manager.devices[0].last_success, Some(100));
        manager.merge(vec![candidate("removed", DeviceKind::Mouse, true)], 300);
        let state = &manager.devices[0];
        assert!(state.removed && state.connected && state.descriptor.supported);
        assert!(!state.selected && !state.show_tray && !state.notify);
        assert_eq!(state.last_success, Some(300));
        assert_eq!(manager.visible().count(), 0);
    }
    #[test]
    fn full_metadata_roundtrip_and_v4_cache_migration() {
        let mut manager = Manager::default();
        let mut item = candidate("opaque-real-4ef9b33fcf5047ae", DeviceKind::Mouse, true);
        let mut reading =
            Reading::from_percent("Full model".into(), 87, PowerState::Charging).unwrap();
        reading.metadata_known = true;
        reading.protocol = (4, 2);
        reading.next_reported_percent = 50;
        reading.level_count = 4;
        reading.capability_flags = 2;
        reading.battery_capability_mask = Some(15);
        item.reading = Some(reading.clone());
        manager.merge(vec![item], 100);
        manager.toggle_tray("opaque-real-4ef9b33fcf5047ae");
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!(
            ".build/tmp/full-metadata-{}.state",
            std::process::id()
        ));
        manager.save(&path);
        let restored = Manager::load(&path).unwrap();
        let value = restored.devices[0].reading.as_ref().unwrap();
        assert_eq!(value.device_name, "Full model");
        assert_eq!(value.protocol, (4, 2));
        assert_eq!(value.power_state, PowerState::Charging);
        assert_eq!(value.capability_flags, 2);
        assert_eq!(value.next_reported_percent, 50);
        assert_eq!(value.battery_capability_mask, Some(15));
        assert!(value.has_fine_estimate());
        assert!(!restored.devices[0].show_tray);
        assert_eq!(
            restored.devices[0].descriptor.endpoint.as_deref(),
            Some("opaque-real-4ef9b33fcf5047ae")
        );
        std::fs::remove_file(path).unwrap();
        let id = "hidpp:1:container:real|4ef9b33fcf5047ae";
        let old = format!(
            "QuietBattery\t4\n{}\t{}\tmouse\t1\t0\t0\t90\t100\tpercent\t1\thidpp\t{}\n",
            hex(id),
            hex("G304"),
            hex("container:real")
        );
        let migrated = Manager::decode(&old);
        assert_eq!(migrated.devices[0].descriptor.id, id);
        assert!(migrated.devices[0].selected);
        assert!(!migrated.devices[0].show_tray);
        let value = migrated.devices[0].reading.as_ref().unwrap();
        assert_eq!(value.reported_percent, 90);
        assert!(!value.metadata_known);
        assert!(crate::app::reading_json(value).contains("\"protocol\":null"));
    }
    #[test]
    fn recovery_and_stale_pruning_are_provider_independent() {
        let mut manager = Manager::default();
        let mut known = candidate("opaqueidentity", DeviceKind::Mouse, true);
        known.descriptor.physical_id = "connection".into();
        known.descriptor.provider = "other-family".into();
        manager.merge(
            vec![known, candidate("old-no-cache", DeviceKind::Generic, false)],
            100,
        );
        let mut unavailable = candidate("placeholder", DeviceKind::Generic, false);
        unavailable.descriptor.physical_id = "connection".into();
        unavailable.descriptor.provider = "other-family".into();
        unavailable.descriptor.endpoint = None;
        unavailable.descriptor.recoverable = true;
        unavailable.error = Some(DeviceError::Unavailable("asleep".into()));
        manager.merge(vec![unavailable], 101);
        assert_eq!(manager.devices.len(), 1);
        assert!(manager.devices[0].selected);
        assert!(manager.devices[0].connected);
        assert_eq!(
            manager.devices[0]
                .reading
                .as_ref()
                .unwrap()
                .reported_percent,
            75
        );
        manager.merge_discovery(
            DiscoveryReport {
                candidates: Vec::new(),
                failures: vec![("other-family".into(), "adapter failed".into())],
            },
            102,
        );
        assert!(manager.devices[0].connected);
        assert!(matches!(manager.devices[0].error, Some(DeviceError::Io(_))));
        manager.remove("opaqueidentity");
        manager.merge(Vec::new(), 103);
        assert!(manager.devices[0].removed);
    }
    #[test]
    fn duplicate_names_get_stable_suffix_only_when_needed() {
        let mut manager = Manager::default();
        manager.merge(vec![candidate("a", DeviceKind::Mouse, true)], 100);
        assert_eq!(manager.display_name("a", true), "Peripheral");
        manager.merge(
            vec![
                candidate("a", DeviceKind::Mouse, true),
                candidate("b", DeviceKind::Mouse, true),
            ],
            101,
        );
        assert_ne!(
            manager.display_name("a", true),
            manager.display_name("b", true)
        );
        assert_eq!(manager.devices[0].descriptor.short_name(), "Peripheral");
    }
    #[test]
    fn malformed_cache_is_ignored() {
        for value in [
            "",
            "QuietBattery\t1\n",
            "QuietBattery\t4\nzz\tzz\tmouse\t1\t0\t1\t900\t1\tpercent\t1",
        ] {
            assert!(Manager::decode(value).devices.is_empty());
        }
    }
    #[test]
    fn results_route_only_by_identity() {
        let mut m = Manager::default();
        m.merge(
            vec![
                candidate("receiver-a:1", DeviceKind::Mouse, true),
                candidate("receiver-b:1", DeviceKind::Mouse, true),
            ],
            100,
        );
        m.apply(
            "receiver-b:1",
            Ok(Reading::from_levels("B".into(), 1, 4).unwrap()),
            101,
        );
        assert_eq!(
            m.devices[0].reading.as_ref().unwrap().normalized_percent(),
            75
        );
        assert_eq!(
            m.devices[1].reading.as_ref().unwrap().normalized_percent(),
            25
        );
    }
    #[test]
    fn link_transitions_need_a_confirmed_previous_state() {
        let mut manager = Manager::default();
        // The first discovery is the baseline: an already-present mouse is silent.
        manager.merge(vec![candidate("mouse", DeviceKind::Mouse, true)], 100);
        assert!(manager.pending_connections.is_empty());
        assert!(manager.connection_baseline);
        // Power-cycling the mouse confirms the link went down, then up again.
        manager.merge(Vec::new(), 200);
        assert_eq!(
            std::mem::take(&mut manager.pending_connections),
            vec![("mouse".to_owned(), false)]
        );
        manager.merge(vec![candidate("mouse", DeviceKind::Mouse, true)], 300);
        assert_eq!(
            std::mem::take(&mut manager.pending_connections),
            vec![("mouse".to_owned(), true)]
        );
        // A device appearing for the first time is not a reconnect.
        let mut fresh = Manager::default();
        fresh.merge(vec![candidate("mouse", DeviceKind::Mouse, true)], 100);
        fresh.merge(
            vec![
                candidate("mouse", DeviceKind::Mouse, true),
                candidate("late", DeviceKind::Mouse, true),
            ],
            200,
        );
        assert!(fresh.pending_connections.is_empty());
    }
    #[test]
    fn connection_toasts_follow_their_own_switch_and_opt_out() {
        let mut manager = Manager::default();
        manager.merge(vec![candidate("mouse", DeviceKind::Mouse, true)], 100);
        manager.connection_notifications = false;
        manager.merge(Vec::new(), 200);
        assert!(manager.pending_connections.is_empty());
        manager.connection_notifications = true;
        // An unmonitored device never announces anything.
        manager.remove("mouse");
        manager.merge(Vec::new(), 300);
        manager.merge(vec![candidate("mouse", DeviceKind::Mouse, true)], 400);
        assert!(manager.pending_connections.is_empty());
    }
    #[test]
    fn v7_preferences_roundtrip_and_older_files_keep_the_old_cadence() {
        let mut manager = Manager::default();
        manager.merge(vec![candidate("opaque", DeviceKind::Mouse, true)], 100);
        manager.auto_refresh = false;
        manager.refresh_seconds = 300;
        manager.connection_notifications = false;
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!(
            ".build/tmp/preferences-{}.state",
            std::process::id()
        ));
        manager.save(&path);
        let restored = Manager::load(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert!(!restored.auto_refresh);
        assert_eq!(restored.refresh_seconds, 300);
        assert!(!restored.connection_notifications);
        let migrated = Manager::decode("QuietBattery\t6\npreferences\t0\t31\n");
        assert!(migrated.auto_refresh);
        assert_eq!(migrated.refresh_seconds, DEFAULT_REFRESH_SECONDS);
        assert!(migrated.connection_notifications);
        assert!(!migrated.notifications_enabled);
        assert_eq!(migrated.threshold, 31);
        // An out-of-range interval falls back to the whole-line default.
        for bad in [
            "QuietBattery\t7\npreferences\t1\t20\t1\t5\t1\n",
            "QuietBattery\t7\npreferences\t1\t20\t1\t3601\t1\n",
            "QuietBattery\t7\npreferences\t1\t20\t2\t300\t1\n",
            "QuietBattery\t7\npreferences\t1\t20\t1\t300\n",
        ] {
            let fallen = Manager::decode(bad);
            assert_eq!(fallen.refresh_seconds, DEFAULT_REFRESH_SECONDS);
            assert!(fallen.auto_refresh);
            assert!(fallen.connection_notifications);
        }
    }
    #[test]
    fn persisted_alert_cycle_and_unconfirmed_connection_survive_restart() {
        let mut manager = Manager::default();
        let mut item = candidate("low", DeviceKind::Mouse, true);
        item.reading =
            Some(Reading::from_percent("Low".into(), 19, PowerState::Discharging).unwrap());
        manager.merge(vec![item], 100);
        assert_eq!(manager.pending_alerts.len(), 1);
        manager.pending_alerts.clear();
        manager.threshold = 20;
        manager.notifications_enabled = false;
        manager.devices[0].notify = false;
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join(format!(".build/tmp/alerts-{}.state", std::process::id()));
        manager.save(&path);
        let mut restored = Manager::load(&path).unwrap();
        assert_eq!(restored.visible().count(), 1);
        assert!(!restored.notifications_enabled);
        assert!(!restored.devices[0].notify);
        assert!(restored.devices[0].alert.notified);
        restored.notifications_enabled = true;
        restored.devices[0].notify = true;
        restored.apply(
            "low",
            Ok(Reading::from_percent("Low".into(), 18, PowerState::Discharging).unwrap()),
            101,
        );
        assert!(restored.pending_alerts.is_empty());
        restored.apply("low", Err(DeviceError::NotFound), 102);
        assert_eq!(restored.visible().count(), 0);
        assert!(restored.devices[0].selected);
        assert_eq!(restored.devices[0].last_success, Some(101));
        restored.apply(
            "low",
            Ok(Reading::from_percent("Low".into(), 18, PowerState::Discharging).unwrap()),
            103,
        );
        assert_eq!(restored.visible().count(), 1);
        assert!(restored.pending_alerts.is_empty());
        std::fs::remove_file(path).unwrap();
    }
}
