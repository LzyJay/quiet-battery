//! Identity-keyed selection and last-success persistence, independent of Win32.
#[cfg(test)]
use crate::device::Candidate;
use crate::device::{
    BatterySource, Connection, DeviceDescriptor, DeviceError, DeviceKind, DiscoveryReport,
    PowerState, Reading, level_for,
};
use crate::notifications::AlertRecord;
use std::path::{Path, PathBuf};
/// One completed "full to low" discharge run: from the last reading of 100%
/// seen while the pack was not charging to the first reading at or below the
/// alert threshold. Persisted, because a run can span a process restart.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UsageCycle {
    pub ended: u64,
    pub seconds: u64,
    /// Percentage the pack read when the run closed (the first reading at or
    /// below the alert threshold). `None` for rows written before v12: the
    /// start of a run is always 100%, but the end value was never stored.
    pub ended_percent: Option<u8>,
}
/// How many completed runs are kept per device; older ones fall off the front.
/// Five completed runs per device. The history page shows them all, so this is
/// also what the user sees: enough to compare packs, short of an ever-growing
/// file.
pub const USAGE_CYCLE_LIMIT: usize = 5;
impl UsageCycle {
    /// `ended:seconds` entries joined with commas, or `-` when none exist.
    pub fn encode(cycles: &[UsageCycle]) -> String {
        if cycles.is_empty() {
            return "-".into();
        }
        cycles
            .iter()
            .map(|cycle| {
                format!(
                    "{}:{}:{}",
                    cycle.ended,
                    cycle.seconds,
                    cycle
                        .ended_percent
                        .map_or_else(|| "-".into(), |percent| percent.to_string())
                )
            })
            .collect::<Vec<_>>()
            .join(",")
    }
    pub fn decode(text: &str) -> Option<Vec<UsageCycle>> {
        if text == "-" {
            return Some(Vec::new());
        }
        if text.len() > 512 {
            return None;
        }
        let mut cycles = Vec::new();
        for entry in text.split(',') {
            let mut parts = entry.split(':');
            let ended = parts.next()?.parse::<u64>().ok()?;
            let seconds = parts.next()?.parse::<u64>().ok()?;
            if seconds == 0 {
                return None;
            }
            // v12 appends the closing percentage. An older row ends here and
            // keeps "unknown" rather than borrowing a number from its neighbours.
            let ended_percent = match parts.next() {
                None | Some("-") => None,
                Some(text) => Some(text.parse::<u8>().ok().filter(|percent| *percent <= 100)?),
            };
            if parts.next().is_some() {
                return None;
            }
            cycles.push(UsageCycle {
                ended,
                seconds,
                ended_percent,
            });
        }
        (cycles.len() <= USAGE_CYCLE_LIMIT).then_some(cycles)
    }
}
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
    /// Unix time of the first failed query since the last good one. A receiver
    /// stays on the bus while the mouse behind it is off, so only the query can
    /// tell "off" from "present". Transient, never persisted.
    pub silent_since: Option<u64>,
    /// Start of the current discharge cycle, i.e. when the battery was last
    /// charged. Persisted: an idle period with the device switched off still
    /// counts as "in use" unless the value went up while it was away.
    pub usage_since: Option<u64>,
    /// Percentage the running discharge cycle started from. Persisted from v12;
    /// older files leave it unknown instead of guessing a start value.
    pub usage_from: Option<u8>,
    /// Last value/power pair seen while linked, used to recognise a completed
    /// charge. Transient, rebuilt from the cached reading on load.
    pub last_seen: Option<(u8, PowerState)>,
    /// Completed full-to-threshold runs, oldest first.
    pub usage_cycles: Vec<UsageCycle>,
    /// When the pack last read 100% while not charging: the start of the run
    /// the first threshold reading will close. Persisted so a run that spans a
    /// restart still records one duration instead of two.
    pub full_since: Option<u64>,
}
/// How long a device may stay silent (a timeout, never an invalid frame) before
/// the link is treated as down. A confirmed protocol-level disconnect and a
/// frame that fails its own checks skip this window entirely, so it only ever
/// filters a single dropped query: one grace period covers the pass that failed,
/// the next probe decides.
pub const SILENT_GRACE_SECONDS: u64 = 5;
impl DeviceState {
    /// A failed query is evidence about the link, never about the value.
    fn silence(&mut self, error: &DeviceError, now: u64) {
        self.connection_known = true;
        self.error = Some(error.clone());
        if matches!(error, DeviceError::Protocol(_) | DeviceError::NotFound) {
            // The device answered with something invalid, or the adapter could
            // not find it at all: nothing is behind this endpoint right now.
            // A protocol-level disconnect event (BLE `ConnectionStatusChanged`)
            // arrives here as NotFound, so the link layer's own supervision
            // timeout is the only wait — never an extra app-level window on top.
            self.connected = false;
            self.silent_since = None;
            return;
        }
        let since = *self.silent_since.get_or_insert(now);
        // Inside the grace window the link is still presumed up: one dropped
        // query is not a disconnect, and the merge pass has just cleared the
        // flag for every device it did not see.
        self.connected = now.saturating_sub(since) < SILENT_GRACE_SECONDS;
    }
    fn answered(&mut self) {
        self.connected = true;
        self.connection_known = true;
        self.silent_since = None;
    }
    /// A new discharge cycle starts when a charge is known to have finished:
    /// either the device left the charger while we watched it, or it came back
    /// from a confirmed link-down with a higher value than it had when the link
    /// dropped. `resumed` means exactly that transition; a device that was never
    /// seen before (or just loaded from disk) is not "coming back".
    fn note_charge(&mut self, now: u64, resumed: bool, threshold: u8) {
        let Some(reading) = self.reading.as_ref() else {
            return;
        };
        let percent = reading.normalized_percent();
        let power = reading.power_state;
        let (previous_percent, previous_power) = match self.last_seen {
            Some((value, state)) => (Some(value), Some(state)),
            None => (None, None),
        };
        let left_charger = previous_power.is_some_and(|state| {
            state == PowerState::Charging
                && matches!(power, PowerState::Discharging | PowerState::Full)
        });
        let gained_while_away = resumed && previous_percent.is_some_and(|before| percent > before);
        if left_charger || gained_while_away || self.usage_since.is_none() {
            self.usage_since = Some(now);
            // The percentage this run started from. Without it the history page
            // could only say "98%" and not where the run began.
            self.usage_from = Some(percent);
        }
        // Percent always follows the newest reading; power keeps the last known
        // value so an Unknown sample cannot hide a charge that ended.
        let power = if matches!(power, PowerState::Unknown(_)) {
            previous_power.unwrap_or(power)
        } else {
            power
        };
        // Full-to-low history. A run opens at the first 100% reading seen while
        // the pack is not charging and closes at the first reading at or below
        // the alert threshold, so the recorded duration is exactly "one full
        // charge emptied down to the reminder". Charging abandons an open run:
        // the pack was topped up, so nothing is owed to the threshold.
        if power == PowerState::Charging {
            self.full_since = None;
        } else if percent >= 100 {
            self.full_since.get_or_insert(now);
        } else if percent <= threshold
            && let Some(since) = self.full_since.take()
            && now > since
        {
            self.usage_cycles.push(UsageCycle {
                ended: now,
                seconds: now - since,
                ended_percent: Some(percent),
            });
            if self.usage_cycles.len() > USAGE_CYCLE_LIMIT {
                self.usage_cycles.remove(0);
            }
        }
        self.last_seen = Some((percent, power));
    }
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
    /// Left edge and **bottom** edge of the main window as it was last closed,
    /// in physical pixels. The bottom is remembered because the panel hangs off
    /// the taskbar like a flyout: when the device count changes it has to grow
    /// upwards instead of drifting down over the taskbar. `None` means "never
    /// placed": the shell centres it and drops it on the work-area bottom.
    pub panel_anchor: Option<(i32, i32)>,
    /// Keep the panel nailed to the bottom-right corner. When this is off the
    /// window can be dragged and reopens where it was left; when it is on the
    /// remembered anchor is ignored, because a pinned window has no position of
    /// its own to remember.
    pub window_pinned: bool,
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
            panel_anchor: None,
            window_pinned: true,
            connection_notifications: true,
            pending_alerts: Vec::new(),
            pending_connections: Vec::new(),
            connection_baseline: false,
            persistence_error: None,
            discovery_errors: Vec::new(),
        }
    }
}
/// Five minutes. Devices that publish the standard Battery Level characteristic
/// push their own updates, so this is only the fallback that corrects a UI which
/// has heard nothing for a while. ChromiumOS landed on ten minutes for exactly
/// that reason; five keeps the row honest without waking a sleeping peripheral
/// every two minutes, which is what the previous default did.
pub const DEFAULT_REFRESH_SECONDS: u32 = 300;
/// The historical default. Files written before the settings version carried the
/// new default keep this value, so an upgrade moves it once instead of leaving
/// every existing install on the old cadence forever.
pub const LEGACY_REFRESH_SECONDS: u32 = 120;
/// Bounds accepted from the settings field, in minutes.
pub const REFRESH_MINUTES: std::ops::RangeInclusive<u32> = 1..=60;
/// Link checks while the user is at the machine: a mouse that was just switched
/// off should leave the list before its owner stops looking at it.
pub const PRESENCE_ACTIVE_SECONDS: u32 = 15;
/// Link checks once no input has arrived for [`IDLE_AFTER_SECONDS`]. Nobody is
/// reading the list then, and every probe wakes the peripheral's radio, so the
/// cadence stretches fourfold.
pub const PRESENCE_IDLE_SECONDS: u32 = 60;
/// No keyboard or mouse input for this long means the session is idle. Four
/// active presence cycles: long enough that reading a page does not count as
/// being away, short enough that walking off does not keep the radio awake.
pub const IDLE_AFTER_SECONDS: u64 = 60;
/// Fallback poll below the alert threshold: the next whole-number change is the
/// one the user is waiting for, so this is the one case that justifies speeding
/// up again.
pub const LOW_REFRESH_SECONDS: u32 = 120;
/// Idle sessions stretch the fallback poll by four times, up to this ceiling. A
/// longer interval the user chose is left as it is: the rule only ever slows
/// things down, never shortens one.
pub const IDLE_REFRESH_SECONDS: u32 = 600;
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
                            | "QuietBattery\t8"
                            | "QuietBattery\t9"
                            | "QuietBattery\t10"
                            | "QuietBattery\t11"
                            | "QuietBattery\t12"
                            | "QuietBattery\t13"
                            | "QuietBattery\t14"
                            | "QuietBattery\t15"
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
    /// A monitored, present pack below the alert threshold that is not on the
    /// charger: the only case where a faster fallback poll buys the user
    /// anything. An unknown power state counts as discharging — the threshold
    /// rule that raises the alert uses the same reading.
    fn low_pack(&self) -> bool {
        self.selected().any(|state| {
            state.connected
                && state.reading.as_ref().is_some_and(|reading| {
                    reading.normalized_percent() < self.threshold
                        && !matches!(reading.power_state, PowerState::Charging | PowerState::Full)
                })
        })
    }
    /// Fallback battery poll in seconds. Devices on the standard Battery Level
    /// characteristic have already pushed their value, so this interval only
    /// corrects a row that has not heard from them; it stays loose and reacts to
    /// the two cases that justify a change: a pack inside the alert band, and a
    /// session nobody is watching.
    pub fn refresh_cadence(&self, idle: bool) -> u32 {
        let mut seconds = self.refresh_seconds;
        if self.low_pack() {
            seconds = seconds.min(LOW_REFRESH_SECONDS);
        } else if idle {
            // Same proportional rest for everyone, capped by the idle ceiling and
            // never shorter than the user's own interval: an interval picked as
            // tight as one minute stays minutes, while the default five minutes
            // lands exactly on ten.
            seconds = seconds
                .saturating_mul(4)
                .min(IDLE_REFRESH_SECONDS)
                .max(seconds);
        }
        seconds.clamp(REFRESH_MINUTES.start() * 60, REFRESH_MINUTES.end() * 60)
    }
    /// Link-probe interval in seconds. The battery cadence is independent: a
    /// probe never replaces the displayed value or its timestamp.
    pub fn presence_cadence(&self, idle: bool) -> u32 {
        if idle {
            PRESENCE_IDLE_SECONDS
        } else {
            PRESENCE_ACTIVE_SECONDS
        }
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
        // A device that answered in this pass is not silent, regardless of the
        // placeholder order inside the same report.
        let answered: std::collections::BTreeSet<String> = candidates
            .iter()
            .filter(|c| c.reading.is_some())
            .map(|c| c.descriptor.id.clone())
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
                        && !answered.contains(&state.descriptor.id)
                        && state.descriptor.physical_id == candidate.descriptor.physical_id
                        && state.descriptor.provider == candidate.descriptor.provider
                        && (candidate.descriptor.endpoint.is_none()
                            || state.descriptor.endpoint == candidate.descriptor.endpoint)
                }) {
                    match &candidate.error {
                        Some(error) => state.silence(error, timestamp),
                        None => {
                            // Present but unsupported: no link evidence either way.
                            state.connected = true;
                            state.connection_known = true;
                        }
                    }
                    if candidate.descriptor.connection != Connection::Unknown {
                        // The link type is adapter evidence; a failed query does
                        // not invalidate it.
                        state.descriptor.connection = candidate.descriptor.connection;
                    }
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
                    state.answered();
                    let resumed = before
                        .iter()
                        .find(|(id, _, _)| *id == state.descriptor.id)
                        .is_some_and(|(_, connected, known)| !*connected && *known);
                    state.note_charge(timestamp, resumed, self.threshold);
                } else if let Some(error) = &candidate.error {
                    state.silence(error, timestamp);
                } else {
                    state.error = None;
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
                let last_seen = candidate
                    .reading
                    .as_ref()
                    .map(|reading| (reading.normalized_percent(), reading.power_state));
                // The run starts at this sample, so its starting percentage is
                // this sample's value — recorded here because `note_charge` only
                // fills it when the clock is opened by a later reading.
                let usage_from = if success {
                    last_seen.map(|(percent, _)| percent)
                } else {
                    None
                };
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
                    silent_since: None,
                    // First sighting starts the clock: we have no older charge
                    // evidence, and a blank field would be less useful.
                    usage_since: success.then_some(timestamp),
                    usage_from,
                    last_seen,
                    usage_cycles: Vec::new(),
                    full_since: None,
                });
                // Seed the discharge history from the first sample, exactly as a
                // later update would, so a device first seen at 100% starts its
                // run here instead of waiting for the next pass.
                if let Some(state) = self.devices.last_mut() {
                    state.note_charge(timestamp, false, self.threshold);
                }
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
    /// Label for one row. `peers` is the set of rows shown together, so a
    /// disambiguating suffix only appears where two of them really collide —
    /// never because some unrelated cached device happens to share the name.
    /// `with_link` picks the suffix style: the link type when it tells the rows
    /// apart, otherwise a short identity hash.
    pub fn display_name(peers: &[DeviceState], id: &str, short: bool, with_link: bool) -> String {
        let label = |state: &DeviceState| {
            if short {
                state.descriptor.short_name()
            } else {
                state.descriptor.name.clone()
            }
        };
        let Some(state) = peers.iter().find(|s| s.descriptor.id == id) else {
            return String::new();
        };
        let name = label(state);
        if !peers.iter().any(|other| {
            other.descriptor.id != id && (other.connected || other.selected) && label(other) == name
        }) {
            return name;
        };
        if with_link {
            let link = state.descriptor.connection.short_label();
            let shares_link = peers.iter().any(|other| {
                other.descriptor.id != id
                    && (other.connected || other.selected)
                    && label(other) == name
                    && other.descriptor.connection.short_label() == link
            });
            if !shares_link {
                return format!("{name} · {link}");
            }
        }
        format!(
            "{name} · {:08X}",
            id.bytes().fold(2166136261u32, |h, b| (h ^ u32::from(b))
                .wrapping_mul(16777619))
        )
    }
    /// Battery read result: updates the value, the cache and the link.
    pub fn apply(&mut self, id: &str, result: Result<Reading, DeviceError>, timestamp: u64) {
        self.apply_inner(id, result, timestamp, true);
    }
    /// Link probe result: only the connection state moves. The battery value and
    /// its timestamp stay on the refresh cadence, otherwise the 15 s presence
    /// check would silently replace the user's refresh interval.
    pub fn apply_link(&mut self, id: &str, result: Result<Reading, DeviceError>, timestamp: u64) {
        self.apply_inner(id, result, timestamp, false);
    }
    fn apply_inner(
        &mut self,
        id: &str,
        result: Result<Reading, DeviceError>,
        timestamp: u64,
        keep_value: bool,
    ) {
        let Some(index) = self.devices.iter().position(|s| s.descriptor.id == id) else {
            return;
        };
        let before = (
            self.devices[index].connected,
            self.devices[index].connection_known,
        );
        if let Some(state) = self.devices.iter_mut().find(|s| s.descriptor.id == id) {
            match result {
                Ok(reading) => {
                    if keep_value
                        && state.alert.observe(
                            &reading,
                            self.threshold,
                            self.notifications_enabled && state.notify && state.selected,
                        )
                    {
                        self.pending_alerts.push((
                            state.descriptor.id.clone(),
                            state.descriptor.name.clone(),
                            reading.normalized_percent(),
                        ));
                    }
                    if keep_value {
                        state.reading = Some(reading);
                        state.last_success = Some(timestamp);
                        state.note_charge(timestamp, !before.0 && before.1, self.threshold);
                    }
                    state.error = None;
                    state.answered();
                }
                // Shutdown is not evidence about the device.
                Err(DeviceError::Cancelled) => {}
                Err(error) => state.silence(&error, timestamp),
            }
        }
        let state = &self.devices[index];
        if self.connection_baseline
            && self.connection_notifications
            && state.selected
            && !state.removed
            && before.1
            && state.connection_known
            && before.0 != state.connected
        {
            self.pending_connections
                .push((state.descriptor.id.clone(), state.connected));
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
            "QuietBattery\t15\npreferences\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
            u8::from(self.notifications_enabled),
            self.threshold,
            u8::from(self.auto_refresh),
            self.refresh_seconds,
            u8::from(self.connection_notifications),
            self.panel_anchor
                .map_or_else(|| "-".into(), |(left, _)| left.to_string()),
            self.panel_anchor
                .map_or_else(|| "-".into(), |(_, bottom)| bottom.to_string()),
            u8::from(self.window_pinned)
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
                "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
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
                state.alert.encode(),
                state
                    .usage_since
                    .map_or_else(|| "-".into(), |t| t.to_string()),
                state.descriptor.connection.key(),
                state
                    .full_since
                    .map_or_else(|| "-".into(), |t| t.to_string()),
                UsageCycle::encode(&state.usage_cycles),
                state
                    .usage_from
                    .map_or_else(|| "-".into(), |percent| percent.to_string())
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
            Some("QuietBattery\t8") => 8,
            Some("QuietBattery\t9") => 9,
            Some("QuietBattery\t10") => 10,
            Some("QuietBattery\t11") => 11,
            Some("QuietBattery\t12") => 12,
            Some("QuietBattery\t13") => 13,
            Some("QuietBattery\t14") => 14,
            Some("QuietBattery\t15") => 15,
            _ => return manager,
        };
        let mut device_records = 0;
        for line in lines.take(1024) {
            let fields: Vec<_> = line.split('\t').collect();
            if fields.first() == Some(&"preferences") {
                if version >= 7 {
                    // v13 appends the last window position; older files carry
                    // five values and simply have no remembered spot.
                    let expected = if version >= 15 {
                        9
                    } else if version >= 13 {
                        8
                    } else {
                        6
                    };
                    if fields.len() == expected
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
                        // v11 replaces the old two-minute default once. An
                        // interval the user actually changed (1, 3, 10 minutes…)
                        // is evidence and stays exactly as stored.
                        let stored = fields[4].parse::<u32>().unwrap();
                        manager.refresh_seconds =
                            if version >= 11 || stored != LEGACY_REFRESH_SECONDS {
                                stored
                            } else {
                                DEFAULT_REFRESH_SECONDS
                            };
                        manager.connection_notifications = fields[5] == "1";
                        // v13 stored a top-left; v14 stores left + bottom. The
                        // old pair cannot be converted without the height it was
                        // saved with, so it is dropped rather than misread.
                        manager.panel_anchor = if version >= 14 {
                            match (fields[6], fields[7]) {
                                ("-", "-") => None,
                                (x, y) => match (x.parse::<i32>(), y.parse::<i32>()) {
                                    // Screen coordinates live in an i16 range;
                                    // anything else is a damaged row, not a place
                                    // Windows could ever put a window.
                                    (Ok(x), Ok(y))
                                        if (-32_768..=32_767).contains(&x)
                                            && (-32_768..=32_767).contains(&y) =>
                                    {
                                        Some((x, y))
                                    }
                                    _ => return manager,
                                },
                            }
                        } else {
                            None
                        };
                        // Older files predate the switch: the window was pinned
                        // to the corner then too, so that is what they mean.
                        manager.window_pinned = if version >= 15 {
                            match fields[8] {
                                "0" => false,
                                "1" => true,
                                _ => return manager,
                            }
                        } else {
                            true
                        };
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
                    6 | 7 => 17,
                    8 => 18,
                    9 => 19,
                    10 | 11 => 21,
                    _ => 22,
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
            // v8 stores the discharge-cycle start. Older files start the clock
            // at the last confirmed reading, which is the best evidence they have.
            let usage_since = if version >= 8 {
                match fields[17] {
                    "-" => None,
                    text => match text.parse::<u64>() {
                        Ok(time) if time <= now.saturating_add(300) => Some(time),
                        _ => continue,
                    },
                }
            } else {
                last_success
            };
            // v10 records the full-charge marker and the completed runs.
            // Older files simply start with no history; nothing is invented.
            let (full_since, usage_cycles) = if version >= 10 {
                let full_since = match fields[19] {
                    "-" => None,
                    text => match text.parse::<u64>() {
                        Ok(time) if time <= now.saturating_add(300) => Some(time),
                        _ => continue,
                    },
                };
                let Some(cycles) = UsageCycle::decode(fields[20]) else {
                    continue;
                };
                (full_since, cycles)
            } else {
                (None, Vec::new())
            };
            // v12 records the percentage the current run started from.
            let usage_from = if version >= 12 {
                match fields[21] {
                    "-" => None,
                    text => match text.parse::<u8>() {
                        Ok(percent) if percent <= 100 => Some(percent),
                        _ => continue,
                    },
                }
            } else {
                None
            };
            let last_seen = reading
                .as_ref()
                .map(|reading| (reading.normalized_percent(), reading.power_state));
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
                    connection: if version >= 9 {
                        // The last link type this device was proven to use. It is
                        // evidence, not a preference: an offline row keeps saying
                        // "2.4G" instead of degrading to "unknown".
                        match Connection::from_key(fields[18]) {
                            Some(value) => value,
                            None => continue,
                        }
                    } else {
                        // Older files carry no column. Two adapters cannot report
                        // anything but one link type, so their identity is enough.
                        match fields[10] {
                            "bthfp" => Connection::BluetoothClassic,
                            "bas" => Connection::BluetoothLe,
                            _ => Connection::Unknown,
                        }
                    },
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
                silent_since: None,
                usage_since,
                usage_from,
                last_seen,
                usage_cycles,
                full_since,
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
    fn panel_anchor_survives_a_restart_and_bad_values_are_rejected() {
        let mut manager = Manager::default();
        assert_eq!(manager.panel_anchor, None);
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join(format!(".build/tmp/window-{}.state", std::process::id()));
        manager.panel_anchor = Some((-12, 480));
        manager.save(&path);
        let restored = Manager::load(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(restored.panel_anchor, Some((-12, 480)));
        // A file without the columns has no remembered spot, and neither has a
        // v13 file: that pair held a top-left, not a bottom edge.
        let old = Manager::decode("QuietBattery\t11\npreferences\t1\t20\t1\t300\t1\n");
        assert_eq!(old.panel_anchor, None);
        let v13 = Manager::decode("QuietBattery\t13\npreferences\t1\t20\t1\t300\t1\t120\t340\n");
        assert_eq!(v13.panel_anchor, None);
        // Out-of-range or non-numeric coordinates drop the preference row back
        // to its defaults instead of placing a window Windows cannot show.
        for bad in [
            "QuietBattery\t14\npreferences\t1\t20\t1\t300\t1\tabc\t420\n",
            "QuietBattery\t14\npreferences\t1\t20\t1\t300\t1\t70000\t420\n",
            "QuietBattery\t14\npreferences\t1\t20\t1\t300\t1\t120\n",
        ] {
            let fallen = Manager::decode(bad);
            assert_eq!(fallen.panel_anchor, None);
            assert_eq!(fallen.refresh_seconds, DEFAULT_REFRESH_SECONDS);
        }
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
        assert_eq!(
            Manager::display_name(&manager.devices, "a", true, true),
            "Peripheral"
        );
        manager.merge(
            vec![
                candidate("a", DeviceKind::Mouse, true),
                candidate("b", DeviceKind::Mouse, true),
            ],
            101,
        );
        // Two rows with the same model name in one list need a suffix…
        let peers = manager.devices.clone();
        assert_ne!(
            Manager::display_name(&peers, "a", true, true),
            Manager::display_name(&peers, "b", true, true)
        );
        // …but a device that is not in this list does not create one.
        let just_a: Vec<_> = peers.iter().take(1).cloned().collect();
        assert_eq!(
            Manager::display_name(&just_a, "a", true, true),
            "Peripheral"
        );
        assert_eq!(
            Manager::display_name(&just_a, "a", true, false),
            "Peripheral"
        );
        assert_eq!(manager.devices[0].descriptor.short_name(), "Peripheral");
    }
    #[test]
    fn duplicate_rows_use_the_link_type_not_a_hex_hash() {
        let mut manager = Manager::default();
        let mut bluetooth = candidate("same", DeviceKind::Headset, true);
        bluetooth.descriptor.connection = Connection::BluetoothClassic;
        manager.merge(vec![bluetooth], 100);
        // A cached twin that is no longer present still occupies a settings row.
        let mut twin = manager.devices[0].clone();
        twin.descriptor.id = "same-2".into();
        twin.descriptor.connection = Connection::Receiver;
        twin.descriptor.name = manager.devices[0].descriptor.name.clone();
        twin.connected = false;
        twin.selected = true;
        manager.devices.push(twin);

        let peers = manager.devices.clone();
        assert_eq!(
            Manager::display_name(&peers, "same", true, true),
            "Peripheral · 蓝牙"
        );
        assert_eq!(
            Manager::display_name(&peers, "same-2", true, true),
            "Peripheral · 2.4G"
        );
        // The main list only ever holds visible rows; with no twin in that set
        // there is no suffix at all, whatever the style.
        let alone: Vec<_> = peers.iter().take(1).cloned().collect();
        for with_link in [false, true] {
            assert_eq!(
                Manager::display_name(&alone, "same", true, with_link),
                "Peripheral"
            );
        }
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
    fn unreachable(id: &str, error: DeviceError) -> Candidate {
        let mut item = candidate(id, DeviceKind::Mouse, false);
        item.descriptor.recoverable = true;
        item.descriptor.endpoint = Some(id.into());
        item.descriptor.physical_id = id.into();
        item.reading = None;
        item.error = Some(error);
        item
    }
    fn silent(id: &str) -> Candidate {
        unreachable(id, DeviceError::Unavailable("asleep".into()))
    }
    #[test]
    fn an_invalid_frame_counts_as_disconnected_on_the_first_pass() {
        let mut manager = Manager::default();
        manager.merge(vec![candidate("mouse", DeviceKind::Mouse, true)], 100);
        // A receiver with nothing behind it answers with a frame that fails its
        // own checksum. That is not silence and must not wait for a second pass.
        manager.merge(
            vec![unreachable(
                "mouse",
                DeviceError::Protocol("checksum mismatch".into()),
            )],
            200,
        );
        assert!(!manager.devices[0].connected);
        assert_eq!(manager.visible().count(), 0);
        assert_eq!(
            std::mem::take(&mut manager.pending_connections),
            vec![("mouse".to_owned(), false)]
        );
        assert_eq!(
            manager.devices[0]
                .reading
                .as_ref()
                .unwrap()
                .normalized_percent(),
            75
        );
    }
    #[test]
    fn silent_receiver_hides_after_repeated_passes_and_announces_both_ways() {
        let mut manager = Manager::default();
        manager.merge(vec![candidate("mouse", DeviceKind::Mouse, true)], 100);
        assert!(manager.pending_connections.is_empty());
        // A receiver stays on the bus while the mouse is off, so one silent pass
        // keeps the row and its cached value.
        manager.merge(vec![silent("mouse")], 200);
        assert!(manager.devices[0].connected);
        assert_eq!(manager.visible().count(), 1);
        assert!(manager.pending_connections.is_empty());
        // Repeated silence is the only available evidence of a disconnect.
        manager.merge(vec![silent("mouse")], 300);
        assert!(!manager.devices[0].connected);
        assert_eq!(manager.visible().count(), 0);
        assert_eq!(
            std::mem::take(&mut manager.pending_connections),
            vec![("mouse".to_owned(), false)]
        );
        assert_eq!(
            manager.devices[0]
                .reading
                .as_ref()
                .unwrap()
                .normalized_percent(),
            75
        );
        // One reply restores the row and announces the reconnect.
        manager.merge(vec![candidate("mouse", DeviceKind::Mouse, true)], 400);
        assert_eq!(manager.visible().count(), 1);
        assert_eq!(
            std::mem::take(&mut manager.pending_connections),
            vec![("mouse".to_owned(), true)]
        );
    }
    #[test]
    fn one_late_reply_restarts_the_silence_window() {
        let mut manager = Manager::default();
        manager.merge(vec![candidate("mouse", DeviceKind::Mouse, true)], 100);
        manager.merge(vec![silent("mouse")], 200);
        assert_eq!(manager.devices[0].silent_since, Some(200));
        manager.merge(vec![candidate("mouse", DeviceKind::Mouse, true)], 300);
        assert_eq!(manager.devices[0].silent_since, None);
        // A reply in between means the next silence starts a fresh grace window.
        manager.merge(vec![silent("mouse")], 400);
        assert!(manager.devices[0].connected);
        assert_eq!(manager.devices[0].silent_since, Some(400));
    }
    #[test]
    fn only_a_timeout_waits_for_the_short_grace_window() {
        let mut manager = Manager::default();
        manager.merge(vec![candidate("mouse", DeviceKind::Mouse, true)], 100);
        manager.merge(vec![silent("mouse")], 200);
        assert!(manager.devices[0].connected);
        // The window covers the pass that failed and nothing more: it is not a
        // second 30-second layer stacked on the link's own supervision timeout.
        manager.merge(vec![silent("mouse")], 204);
        assert!(manager.devices[0].connected);
        manager.merge(vec![silent("mouse")], 206);
        assert!(!manager.devices[0].connected);
        assert_eq!(manager.visible().count(), 0);
    }
    #[test]
    fn cadence_slows_when_idle_and_speeds_up_inside_the_alert_band() {
        let mut manager = Manager::default();
        assert_eq!(manager.refresh_seconds, DEFAULT_REFRESH_SECONDS);
        assert_eq!(manager.refresh_cadence(false), DEFAULT_REFRESH_SECONDS);
        assert_eq!(manager.refresh_cadence(true), IDLE_REFRESH_SECONDS);
        // A tight interval stays minutes: the idle rest is proportional, never a
        // flat ten minutes on top of whatever the user picked.
        manager.refresh_seconds = 60;
        assert_eq!(manager.refresh_cadence(true), 240);
        manager.refresh_seconds = LEGACY_REFRESH_SECONDS;
        assert_eq!(manager.refresh_cadence(true), 480);
        manager.refresh_seconds = DEFAULT_REFRESH_SECONDS;
        assert_eq!(manager.presence_cadence(false), PRESENCE_ACTIVE_SECONDS);
        assert_eq!(manager.presence_cadence(true), PRESENCE_IDLE_SECONDS);
        // A pack inside the alert band is worth a tighter fallback even while
        // the session is idle: that reading is the one the user is waiting for.
        manager.merge(
            vec![charged_candidate("mouse", 19, PowerState::Discharging)],
            100,
        );
        assert_eq!(manager.refresh_cadence(false), LOW_REFRESH_SECONDS);
        assert_eq!(manager.refresh_cadence(true), LOW_REFRESH_SECONDS);
        // Charging suppresses the acceleration: the pack is going up and the
        // charger, not the poll, is what changes it.
        manager.merge(
            vec![charged_candidate("mouse", 19, PowerState::Charging)],
            200,
        );
        assert_eq!(manager.refresh_cadence(true), IDLE_REFRESH_SECONDS);
        // A longer interval the user chose survives the idle ceiling…
        manager.refresh_seconds = 3_600;
        assert_eq!(manager.refresh_cadence(true), 3_600);
        // …and a low pack may shorten it, but the accepted range still binds.
        manager.merge(
            vec![charged_candidate("mouse", 10, PowerState::Discharging)],
            300,
        );
        assert_eq!(manager.refresh_cadence(false), LOW_REFRESH_SECONDS);
        manager.threshold = 5;
        manager.merge(
            vec![charged_candidate("mouse", 10, PowerState::Discharging)],
            400,
        );
        assert_eq!(manager.refresh_cadence(false), 3_600);
    }
    #[test]
    fn a_presence_read_also_drives_the_link_and_announces_both_ways() {
        let mut manager = Manager::default();
        manager.merge(vec![candidate("mouse", DeviceKind::Mouse, true)], 100);
        assert!(!manager.devices[0].silent_since.is_some());
        // The 15 s presence poll reports a frame that fails its checksum.
        manager.apply_link(
            "mouse",
            Err(DeviceError::Protocol("checksum mismatch".into())),
            120,
        );
        assert!(!manager.devices[0].connected);
        assert_eq!(
            std::mem::take(&mut manager.pending_connections),
            vec![("mouse".to_owned(), false)]
        );
        // The next poll gets a valid frame back.
        manager.apply_link(
            "mouse",
            Ok(Reading::from_percent("Peripheral".into(), 74, PowerState::Discharging).unwrap()),
            135,
        );
        assert!(manager.devices[0].connected);
        assert_eq!(
            std::mem::take(&mut manager.pending_connections),
            vec![("mouse".to_owned(), true)]
        );
    }
    fn charged_candidate(id: &str, percent: u8, power: PowerState) -> Candidate {
        let mut item = candidate(id, DeviceKind::Mouse, true);
        item.reading = Some(Reading::from_percent("Peripheral".into(), percent, power).unwrap());
        item
    }
    fn gone(id: &str) -> Candidate {
        unreachable(id, DeviceError::NotFound)
    }
    #[test]
    fn a_higher_value_after_a_disconnect_starts_a_new_usage_cycle() {
        let mut manager = Manager::default();
        manager.merge(
            vec![charged_candidate("mouse", 90, PowerState::Discharging)],
            100,
        );
        assert_eq!(manager.devices[0].usage_since, Some(100));
        // Value recorded at disconnect is the cached one: 90%.
        manager.merge(vec![gone("mouse")], 200);
        assert!(!manager.devices[0].connected);
        manager.merge(
            vec![charged_candidate("mouse", 96, PowerState::Discharging)],
            300,
        );
        assert_eq!(manager.devices[0].usage_since, Some(300));
        // Same value, or a lower one, is not a charge.
        manager.merge(vec![gone("mouse")], 400);
        manager.merge(
            vec![charged_candidate("mouse", 96, PowerState::Discharging)],
            500,
        );
        assert_eq!(manager.devices[0].usage_since, Some(300));
        manager.merge(vec![gone("mouse")], 600);
        manager.merge(
            vec![charged_candidate("mouse", 80, PowerState::Discharging)],
            700,
        );
        assert_eq!(manager.devices[0].usage_since, Some(300));
    }
    #[test]
    fn a_restart_is_not_a_reconnect_and_must_not_reset_the_cycle() {
        let mut manager = Manager::default();
        manager.merge(
            vec![charged_candidate("mouse", 80, PowerState::Discharging)],
            100,
        );
        manager.devices[0].usage_since = Some(1_000);
        // A fresh process loads the cache with an unknown link, so the first
        // sample is a first sighting, not a reconnect: a higher value there is
        // not evidence that a charge happened while we were away.
        manager.devices[0].connected = false;
        manager.devices[0].connection_known = false;
        manager.merge(
            vec![charged_candidate("mouse", 95, PowerState::Discharging)],
            2_000,
        );
        assert_eq!(manager.devices[0].usage_since, Some(1_000));
    }
    #[test]
    fn leaving_the_charger_restarts_the_cycle_without_a_disconnect() {
        let mut manager = Manager::default();
        manager.merge(
            vec![charged_candidate("mouse", 40, PowerState::Charging)],
            100,
        );
        assert_eq!(manager.devices[0].usage_since, Some(100));
        manager.merge(
            vec![charged_candidate("mouse", 55, PowerState::Charging)],
            200,
        );
        assert_eq!(manager.devices[0].usage_since, Some(100));
        manager.merge(vec![charged_candidate("mouse", 100, PowerState::Full)], 300);
        assert_eq!(manager.devices[0].usage_since, Some(300));
        // An unknown sample in between must not hide the charger edge.
        manager.merge(
            vec![charged_candidate("mouse", 90, PowerState::Charging)],
            400,
        );
        manager.merge(
            vec![charged_candidate("mouse", 90, PowerState::Unknown(255))],
            500,
        );
        manager.merge(
            vec![charged_candidate("mouse", 90, PowerState::Discharging)],
            600,
        );
        assert_eq!(manager.devices[0].usage_since, Some(600));
    }
    #[test]
    fn usage_cycle_survives_a_restart_and_older_files_start_at_the_last_reading() {
        let mut manager = Manager::default();
        manager.merge(vec![candidate("opaque", DeviceKind::Mouse, true)], 100);
        manager.devices[0].usage_since = Some(4_242);
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join(format!(".build/tmp/usage-{}.state", std::process::id()));
        manager.save(&path);
        let restored = Manager::load(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(restored.devices[0].usage_since, Some(4_242));
        // A v7 record has no field for it: the last confirmed reading is the best
        // starting point, and the clock keeps running from there.
        let id = "fixture-usage";
        let metadata = encode_metadata(
            &Reading::from_percent("Fixture".into(), 90, PowerState::Unknown(255)).unwrap(),
        );
        let old = format!(
            "QuietBattery\t7\npreferences\t1\t20\t1\t120\t1\n{}\t{}\tmouse\t1\t0\t1\t90\t5000\tpercent\t1\tfixture\t{}\t-\t1\t{}\t1\t0,-\n",
            hex(id),
            hex("Fixture"),
            hex("fixture-physical"),
            metadata,
        );
        let migrated = Manager::decode(&old);
        assert_eq!(migrated.devices[0].usage_since, Some(5_000));
    }
    #[test]
    fn full_to_threshold_runs_record_one_cycle_and_survive_a_restart() {
        let mut manager = Manager::default();
        assert_eq!(manager.threshold, 20);
        // 100% while discharging opens the run...
        manager.merge(
            vec![charged_candidate("mouse", 100, PowerState::Discharging)],
            1_000,
        );
        assert_eq!(manager.devices[0].full_since, Some(1_000));
        // ...readings above the threshold leave it open...
        manager.merge(
            vec![charged_candidate("mouse", 60, PowerState::Discharging)],
            2_000,
        );
        assert!(manager.devices[0].usage_cycles.is_empty());
        // ...and the first reading at the alert threshold closes it once.
        manager.merge(
            vec![charged_candidate("mouse", 20, PowerState::Discharging)],
            4_600,
        );
        assert_eq!(manager.devices[0].full_since, None);
        assert_eq!(
            manager.devices[0].usage_cycles,
            vec![UsageCycle {
                ended: 4_600,
                seconds: 3_600,
                // The reading that closed the run: the threshold sample of 20%
                // fed in at 4_600.
                ended_percent: Some(20),
            }]
        );
        manager.merge(
            vec![charged_candidate("mouse", 10, PowerState::Discharging)],
            5_600,
        );
        assert_eq!(manager.devices[0].usage_cycles.len(), 1);

        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join(format!(".build/tmp/cycles-{}.state", std::process::id()));
        manager.save(&path);
        let restored = Manager::load(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(
            restored.devices[0].usage_cycles,
            manager.devices[0].usage_cycles
        );

        // Charging abandons an open run instead of banking it as a cycle.
        manager.merge(
            vec![charged_candidate("mouse", 100, PowerState::Discharging)],
            9_000,
        );
        assert_eq!(manager.devices[0].full_since, Some(9_000));
        manager.merge(
            vec![charged_candidate("mouse", 100, PowerState::Charging)],
            9_100,
        );
        assert_eq!(manager.devices[0].full_since, None);
        assert_eq!(manager.devices[0].usage_cycles.len(), 1);
    }
    #[test]
    fn usage_history_keeps_five_runs_and_their_percentages() {
        let mut manager = Manager::default();
        for run in 0..6u64 {
            let start = 1_000 + run * 1_000;
            manager.merge(
                vec![charged_candidate("mouse", 100, PowerState::Discharging)],
                start,
            );
            manager.merge(
                vec![charged_candidate("mouse", 20, PowerState::Discharging)],
                start + 600,
            );
        }
        // Six runs happened; the oldest fell off instead of growing the file.
        assert_eq!(
            manager.devices[0].usage_cycles.len(),
            USAGE_CYCLE_LIMIT,
            "limit {USAGE_CYCLE_LIMIT}"
        );
        assert_eq!(
            manager.devices[0].usage_cycles.first().unwrap().ended,
            2_600
        );
        assert_eq!(manager.devices[0].usage_cycles.last().unwrap().ended, 6_600);
        assert!(
            manager.devices[0]
                .usage_cycles
                .iter()
                .all(|cycle| cycle.ended_percent == Some(20)),
            "the closing reading is the threshold sample"
        );
        assert_eq!(manager.devices[0].usage_from, Some(100));

        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join(format!(".build/tmp/history-{}.state", std::process::id()));
        manager.save(&path);
        let saved = std::fs::read_to_string(&path).unwrap();
        let restored = Manager::load(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(
            restored.devices[0].usage_cycles,
            manager.devices[0].usage_cycles
        );
        assert_eq!(restored.devices[0].usage_from, Some(100));

        // A v11 file has one column fewer: the runs still load, the start
        // percentage is unknown instead of being guessed.
        let downgraded: String = saved
            .lines()
            .map(|line| {
                if line.starts_with("QuietBattery\t") {
                    "QuietBattery\t11".to_string()
                } else if line.split('\t').count() > 21 {
                    let mut fields: Vec<&str> = line.split('\t').collect();
                    fields.pop();
                    fields.join("\t")
                } else {
                    line.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        let legacy = Manager::decode(&downgraded);
        assert_eq!(legacy.devices.len(), 1);
        assert_eq!(legacy.devices[0].usage_from, None);
        assert_eq!(legacy.devices[0].usage_cycles.len(), USAGE_CYCLE_LIMIT);
    }
    #[test]
    fn usage_cycle_encoding_rejects_damaged_rows() {
        let cycle = UsageCycle {
            ended: 4_600,
            seconds: 3_600,
            ended_percent: Some(9),
        };
        assert_eq!(UsageCycle::encode(&[]), "-");
        assert_eq!(UsageCycle::decode("-"), Some(Vec::new()));
        let text = UsageCycle::encode(&[cycle]);
        assert_eq!(text, "4600:3600:9");
        assert_eq!(UsageCycle::decode(&text), Some(vec![cycle]));
        // Rows written before v12 carry no closing percentage. They stay valid
        // and report "unknown" instead of borrowing a number.
        assert_eq!(
            UsageCycle::decode("4600:3600"),
            Some(vec![UsageCycle {
                ended: 4_600,
                seconds: 3_600,
                ended_percent: None,
            }])
        );
        for damaged in [
            "",
            "4600",
            "4600:",
            "4600:0",
            "a:b",
            "4600:3600:abc",
            "4600:3600:101",
            "4600:3600:9:1",
            "4600:3600,x:1",
        ] {
            assert!(UsageCycle::decode(damaged).is_none(), "{damaged}");
        }
        let too_many = vec![cycle; USAGE_CYCLE_LIMIT + 1];
        assert!(UsageCycle::decode(&UsageCycle::encode(&too_many)).is_none());
    }
    #[test]
    fn the_link_type_survives_a_restart_and_old_rows_infer_it() {
        let mut manager = Manager::default();
        let mut headset = candidate("bt", DeviceKind::Headset, true);
        headset.descriptor.provider = "bthfp".into();
        headset.descriptor.connection = Connection::BluetoothClassic;
        manager.merge(vec![headset], 100);
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join(format!(".build/tmp/link-{}.state", std::process::id()));
        manager.save(&path);
        let restored = Manager::load(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(
            restored.devices[0].descriptor.connection,
            Connection::BluetoothClassic
        );
        // v8 has no column: the two single-transport adapters are still known,
        // everything else stays unknown until it is seen again.
        let metadata = encode_metadata(
            &Reading::from_percent("Fixture".into(), 90, PowerState::Unknown(255)).unwrap(),
        );
        for (provider, expected) in [
            ("bthfp", Connection::BluetoothClassic),
            ("bas", Connection::BluetoothLe),
            ("mchose", Connection::Unknown),
        ] {
            let text = format!(
                "QuietBattery\t8\npreferences\t1\t20\t1\t120\t1\n{}\t{}\tmouse\t1\t0\t1\t90\t5000\tpercent\t1\t{}\t{}\t-\t1\t{}\t1\t0,-\t-\n",
                hex("fixture-link"),
                hex("Fixture"),
                provider,
                hex("fixture-physical"),
                metadata,
            );
            let decoded = Manager::decode(&text);
            assert_eq!(decoded.devices.len(), 1, "{provider}");
            assert_eq!(
                decoded.devices[0].descriptor.connection, expected,
                "{provider}"
            );
        }
    }
    #[test]
    fn a_link_probe_moves_the_link_without_touching_the_value() {
        let mut manager = Manager::default();
        manager.merge(vec![candidate("mouse", DeviceKind::Mouse, true)], 100);
        let shown = manager.devices[0]
            .reading
            .as_ref()
            .unwrap()
            .normalized_percent();
        manager.apply_link(
            "mouse",
            Ok(Reading::from_percent("Peripheral".into(), 12, PowerState::Discharging).unwrap()),
            200,
        );
        assert!(manager.devices[0].connected);
        // The 15 s probe must not turn the 2 minute value cadence into 15 s.
        assert_eq!(manager.devices[0].last_success, Some(100));
        assert_eq!(
            manager.devices[0]
                .reading
                .as_ref()
                .unwrap()
                .normalized_percent(),
            shown
        );
        manager.apply_link("mouse", Err(DeviceError::Protocol("bad".into())), 300);
        assert!(!manager.devices[0].connected);
        assert_eq!(manager.devices[0].last_success, Some(100));
    }
    #[test]
    fn cancelled_reads_are_not_link_evidence() {
        let mut manager = Manager::default();
        manager.merge(vec![candidate("mouse", DeviceKind::Mouse, true)], 100);
        manager.apply("mouse", Err(DeviceError::Cancelled), 200);
        assert!(manager.devices[0].connected);
        assert_eq!(manager.devices[0].silent_since, None);
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
    fn v7_preferences_roundtrip_and_older_files_use_the_current_default() {
        let mut manager = Manager::default();
        manager.merge(vec![candidate("opaque", DeviceKind::Mouse, true)], 100);
        manager.auto_refresh = false;
        manager.refresh_seconds = 300;
        manager.connection_notifications = false;
        manager.panel_anchor = Some((120, 340));
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
        assert_eq!(restored.panel_anchor, Some((120, 340)));
        let migrated = Manager::decode("QuietBattery\t6\npreferences\t0\t31\n");
        assert!(migrated.auto_refresh);
        assert_eq!(migrated.refresh_seconds, DEFAULT_REFRESH_SECONDS);
        assert!(migrated.connection_notifications);
        assert!(!migrated.notifications_enabled);
        assert_eq!(migrated.threshold, 31);
        // v11 moves the historical two-minute default once, so an upgrade does
        // not leave every existing install on the old cadence forever…
        let upgraded = Manager::decode("QuietBattery\t10\npreferences\t1\t20\t1\t120\t1\n");
        assert_eq!(upgraded.refresh_seconds, DEFAULT_REFRESH_SECONDS);
        // …while an interval the user actually picked is evidence and stays.
        let explicit = Manager::decode("QuietBattery\t10\npreferences\t1\t20\t1\t600\t1\n");
        assert_eq!(explicit.refresh_seconds, 600);
        // v11 files carry real values, including a deliberate two minutes.
        let kept = Manager::decode("QuietBattery\t11\npreferences\t1\t20\t1\t120\t1\n");
        assert_eq!(kept.refresh_seconds, LEGACY_REFRESH_SECONDS);
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
