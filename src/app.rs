//! Native multi-device shell. Device access is confined to the serial worker.
use crate::{
    device::{
        Cancellation, Candidate, DeviceError, DeviceKind, DiscoveryReport, Reading, Registry,
    },
    icons::{BatteryVisual, SymbolIcon, TrayIcon},
    manager::{self, DeviceState, Manager},
    startup::{self, wide},
};
use std::{
    cell::{Cell, RefCell},
    collections::VecDeque,
    path::PathBuf,
    ptr::{null, null_mut},
    rc::Rc,
    sync::mpsc::{self, Receiver, Sender},
    thread::JoinHandle,
    time::{Instant, SystemTime, UNIX_EPOCH},
};
use windows_sys::Win32::{
    Foundation::*,
    Graphics::Gdi::*,
    System::{
        LibraryLoader::GetModuleHandleW,
        SystemServices::{SS_CENTERIMAGE, SS_LEFT, SS_NOPREFIX},
        Threading::{CreateMutexW, ReleaseMutex},
    },
    UI::{Controls::*, Shell::*, WindowsAndMessaging::*},
};
use windows_sys::Win32::{
    Storage::FileSystem::{MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW},
    UI::{
        HiDpi::*,
        Input::KeyboardAndMouse::{EnableWindow, IsWindowEnabled},
        Input::{RAWINPUTDEVICE, RIDEV_INPUTSINK, RegisterRawInputDevices},
    },
};
const CLASS: &str = "QuietBattery.NativeTray.Window";
const PANEL_CLASS: &str = "QuietBattery.NativeTray.Information";
const CONTROL: u32 = WM_APP + 1;
const RESULT: u32 = WM_APP + 2;
const TRAY: u32 = WM_APP + 3;
const EVENTS_READY: u32 = WM_APP + 4;
pub const REFRESH: usize = 1;
pub const QUIT: usize = 2;
pub const SIMULATE_ERROR: usize = 3;
pub const RECOVER: usize = 4;
pub const EXPLORER_RESTART: usize = 5;
pub const SHOW_PANEL: usize = 6;
pub const HIDE_PANEL: usize = 7;
pub const SHOW_MENU: usize = 8;
pub const TEST_NOTIFICATION: usize = 9;
const DETAILS: u32 = 101;
const MENU_REFRESH: u32 = 102;
const STARTUP: u32 = 103;
const EXIT: u32 = 104;
const ADD: u32 = 105;
const SETTINGS: u32 = 106;
const ROW_BASE: u32 = 1000;
const PANEL_WIDTH: i32 = 560;
const BACK: u32 = 109;
const ALERTS: u32 = 110;
const THRESHOLD: u32 = 111;
const PICK_REFRESH: u32 = 112;
const SETTINGS_BASE: u32 = 4000;
const ADD_BASE: u32 = 8000;
#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Page {
    #[default]
    Main,
    Settings,
    Add,
}
impl Page {
    fn index(self) -> usize {
        match self {
            Self::Main => 0,
            Self::Settings => 1,
            Self::Add => 2,
        }
    }
    fn key(self) -> &'static str {
        match self {
            Self::Main => "main",
            Self::Settings => "settings",
            Self::Add => "add",
        }
    }
}
enum WorkerCommand {
    Discover(bool),
    Read(Vec<String>, bool),
    Stop,
}
enum WorkerResult {
    Discovery(Result<DiscoveryReport, DeviceError>),
    Reading(String, Result<Reading, DeviceError>),
    Complete,
    RefreshRequest(String),
}
#[derive(Clone, Default)]
struct PanelView {
    scroll: i32,
    page: Page,
    threshold_error: bool,
    rows: Vec<DeviceState>,
    symbols: Vec<Rc<SymbolIcon>>,
}
struct UiBridge {
    events: RefCell<VecDeque<UiEvent>>,
    view: RefCell<Rc<PanelView>>,
    taskbar_created: u32,
    wake_window: Cell<HWND>,
    menu_active: Cell<bool>,
    menu_timing: Cell<MenuTiming>,
    activity_queued: Cell<bool>,
    activity_last: Cell<Instant>,
    activity_needed: Cell<bool>,
    activity_seen: Cell<Instant>,
    notification_events: Cell<[u64; 3]>,
    status: RefCell<StatusView>,
}
#[derive(Clone, Copy, Default)]
struct MenuTiming {
    requested: Option<Instant>,
    count: u64,
    enter_loop_us: Option<u64>,
    visible_idle_us: Option<u64>,
}
impl UiBridge {
    fn queue(&self, event: UiEvent) {
        self.events.borrow_mut().push_back(event);
        let hwnd = self.wake_window.get();
        if !hwnd.is_null() {
            unsafe {
                PostMessageW(hwnd, EVENTS_READY, 0, 0);
            }
        }
    }
    fn activity(&self, now: Instant) {
        let previous = self.activity_seen.replace(now);
        if (self.activity_needed.get() || now.duration_since(previous).as_secs() >= 20)
            && !self.activity_queued.get()
            && now.duration_since(self.activity_last.get()).as_secs() >= 10
        {
            self.activity_queued.set(true);
            self.activity_last.set(now);
            self.queue(UiEvent::Activity);
        }
    }
    fn next(&self) -> Option<UiEvent> {
        self.events.borrow_mut().pop_front()
    }
}
enum UiEvent {
    Control(usize),
    Result,
    Heartbeat,
    Refresh,
    Activity,
    Discover,
    Menu(Instant, u32),
    ShowPanel,
    ClosePanel,
    MainDestroyed,
    PanelDestroyed(HWND),
    ActivatePanel,
    DpiChanged(HWND, RECT),
    SettingsChanged,
    TaskbarCreated,
    RowRefresh(String),
    DeviceCommand(Page, u32, String),
    Settings,
    Scroll(i32),
    PageCommand(u32),
    Threshold(bool),
    ThresholdEdited,
}
struct TrayEntry {
    id: String,
    shell_id: u32,
    icon: TrayIcon,
    key: (DeviceKind, Option<u8>, i32),
    registered: bool,
}
struct RowControls {
    id: String,
    model: HWND,
    percent: HWND,
    refresh: HWND,
    symbol: Rc<SymbolIcon>,
    tooltip: HWND,
    _tooltip_text: Vec<u16>,
    extra: Vec<HWND>,
    presentation: (String, String, DeviceKind, bool),
}
#[derive(Clone, Debug, PartialEq, Eq)]
enum FocusAnchor {
    Control(u32),
    Device {
        id: String,
        action: usize,
        index: usize,
    },
}
#[cfg(test)]
type NotificationSink = Box<dyn FnMut(&str, u8) -> bool>;
#[cfg(test)]
#[derive(Default)]
struct TestEffects {
    state_path: Option<PathBuf>,
    notification_sink: Option<NotificationSink>,
}
struct App {
    #[cfg(test)]
    effects: TestEffects,
    hwnd: HWND,
    panel: HWND,
    picker: HWND,
    manager: Manager,
    trays: Vec<TrayEntry>,
    fallback: Option<TrayEntry>,
    rows: Vec<RowControls>,
    sender: Sender<WorkerCommand>,
    receiver: Receiver<WorkerResult>,
    worker: Option<JoinHandle<()>>,
    stop: Cancellation,
    busy: bool,
    pending: bool,
    pending_discovery: bool,
    simulated: bool,
    refresh_count: u64,
    success_count: u64,
    bridge: Rc<UiBridge>,
    panel_fonts: [HFONT; 3],
    panel_icons: [HICON; 2],
    last_activity_refresh: Instant,
    picker_ids: Vec<String>,
    page: Page,
    page_scroll: [i32; 3],
    page_controls: Vec<HWND>,
    threshold_error: bool,
    notification_submissions: u64,
    notification_last_accepted: bool,
    scroll: i32,
}
pub fn json_string(text: &str) -> String {
    let mut out = String::from("\"");
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c < ' ' => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}
pub fn reading_json(reading: &Reading) -> String {
    format!(
        "{{\"device_name\":{},\"protocol\":{},\"reported_percent\":{},\"percent\":{},\"source\":{},\"metadata_known\":{},\"level_count\":{},\"has_fine_estimate\":{},\"coarse\":{},\"power_state\":{},\"level\":{},\"capability_flags\":{},\"battery_capability_mask\":{}}}",
        json_string(&reading.device_name),
        if reading.metadata_known {
            json_string(&format!("{}.{}", reading.protocol.0, reading.protocol.1))
        } else {
            "null".into()
        },
        reading.reported_percent,
        reading.normalized_percent(),
        json_string(&format!("{:?}", reading.source)),
        reading.metadata_known,
        if reading.metadata_known {
            reading.level_count.to_string()
        } else {
            "null".into()
        },
        reading.has_fine_estimate(),
        !reading.has_fine_estimate(),
        if reading.metadata_known {
            json_string(&format!("{:?}", reading.power_state))
        } else {
            "null".into()
        },
        json_string(&format!("{:?}", reading.level)),
        if reading.metadata_known {
            reading.capability_flags.to_string()
        } else {
            "null".into()
        },
        reading
            .battery_capability_mask
            .map_or_else(|| "null".into(), |mask| mask.to_string())
    )
}
pub fn data_directory() -> PathBuf {
    crate::storage::directory()
}

fn find_window() -> HWND {
    unsafe { FindWindowW(wide(CLASS).as_ptr(), null()) }
}
pub fn control(command: usize) -> Result<(), String> {
    let window = find_window();
    if window.is_null() {
        return Err("托盘未运行".into());
    }
    if unsafe { PostMessageW(window, CONTROL, command, 0) } == 0 {
        return Err("无法通知托盘".into());
    }
    crate::output("{\"queued\":true}");
    Ok(())
}
pub fn print_status() -> Result<(), String> {
    let window = find_window();
    if window.is_null() {
        return Err("托盘未运行".into());
    }
    crate::output(&crate::status_ipc::query(window)?);
    Ok(())
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
pub fn replace_file(temporary: &std::path::Path, path: &std::path::Path) -> Result<(), String> {
    if unsafe {
        MoveFileExW(
            wide(&temporary.to_string_lossy()).as_ptr(),
            wide(&path.to_string_lossy()).as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    } == 0
    {
        let error = std::io::Error::last_os_error().to_string();
        let _ = std::fs::remove_file(temporary);
        return Err(error);
    }
    Ok(())
}
fn tray_icon_size() -> i32 {
    unsafe {
        let tray = FindWindowW(wide("Shell_TrayWnd").as_ptr(), null());
        let dpi = if tray.is_null() {
            GetDpiForSystem()
        } else {
            GetDpiForWindow(tray)
        };
        GetSystemMetricsForDpi(SM_CXSMICON, dpi.max(96)).max(16)
    }
}

fn stable_tray_id(id: &str) -> u32 {
    id.bytes()
        .fold(2166136261u32, |h, b| {
            (h ^ u32::from(b)).wrapping_mul(16777619)
        })
        .max(2)
}
fn simulated_error() -> DeviceError {
    DeviceError::Unavailable("模拟测试：设备不可用".into())
}
fn opt_number(value: Option<u64>) -> String {
    value.map_or_else(|| "null".into(), |v| v.to_string())
}
pub fn descriptor_json(candidate: &Candidate) -> String {
    format!(
        "{{\"id\":{},\"name\":{},\"kind\":{},\"supported\":{},\"provider\":{},\"reading\":{},\"error\":{},\"diagnostics\":{}}}",
        json_string(&candidate.descriptor.id),
        json_string(&candidate.descriptor.name),
        json_string(candidate.descriptor.kind.key()),
        candidate.descriptor.supported,
        json_string(&candidate.descriptor.provider),
        candidate
            .reading
            .as_ref()
            .map_or_else(|| "null".into(), reading_json),
        candidate
            .error
            .as_ref()
            .map_or_else(|| "null".into(), |e| json_string(&e.to_string())),
        Registry::diagnostic_information(&candidate.descriptor).unwrap_or_else(|| "null".into())
    )
}
#[derive(Default)]
struct StatusView {
    manager: Manager,
    busy: bool,
    simulated: bool,
    refresh_count: u64,
    success_count: u64,
    tray_registered: bool,
    panel: HWND,
    fallback_tray_id: Option<u32>,
    tray_ids: Vec<(String, u32)>,
    page: Page,
    notification_submissions: u64,
    notification_last_accepted: bool,
}
impl StatusView {
    fn json(&self, menu: MenuTiming, counts: [u64; 3]) -> String {
        let first = self.manager.selected().next();
        let reading = first.and_then(|s| s.reading.as_ref());
        let last = first.and_then(|s| s.last_success);
        let error = first.and_then(|s| s.error.as_ref());
        let stale = first.is_none_or(|s| {
            s.error.is_some() || s.last_success.is_none_or(|t| now().saturating_sub(t) > 240)
        });
        let devices=self.manager.devices.iter().map(|s|format!("{{\"id\":{},\"name\":{},\"kind\":{},\"supported\":{},\"selected\":{},\"removed\":{},\"show_tray\":{},\"tray_id\":{},\"connected\":{},\"last_success_unix\":{},\"reading\":{},\"error\":{}}}",json_string(&s.descriptor.id),json_string(&s.descriptor.name),json_string(s.descriptor.kind.key()),s.descriptor.supported,s.selected,s.removed,s.show_tray,self.tray_ids.iter().find(|(id,_)|*id==s.descriptor.id).map_or_else(||"null".into(),|(_,id)|id.to_string()),s.connected,opt_number(s.last_success),s.reading.as_ref().map_or_else(||"null".into(),reading_json),s.error.as_ref().map_or_else(||"null".into(),|e|json_string(&e.to_string())))).collect::<Vec<_>>().join(",");
        let startup = startup::status();
        let enabled = startup.as_ref().is_ok_and(|s| s.enabled);
        let value = format!(
            "{{\"pid\":{},\"schema\":2,\"updated_unix\":{},\"last_success_unix\":{},\"stale\":{},\"busy\":{},\"simulated\":{},\"refresh_count\":{},\"success_count\":{},\"tray_registered\":{},\"panel_open\":{},\"is_visible\":{},\"startup_enabled\":{},\"reading\":{},\"error\":{},\"persistence_error\":{},\"fallback_tray_id\":{},\"devices\":[{}],\"discovery_errors\":[{}],\"menu\":{{\"count\":{},\"enter_loop_us\":{},\"visible_idle_us\":{}}}}}",
            std::process::id(),
            now(),
            opt_number(last),
            stale,
            self.busy,
            self.simulated,
            self.refresh_count,
            self.success_count,
            self.tray_registered,
            !self.panel.is_null(),
            !self.panel.is_null() && unsafe { IsWindowVisible(self.panel) } != 0,
            enabled,
            reading.map_or_else(|| "null".into(), reading_json),
            error.map_or_else(|| "null".into(), |e| json_string(&e.to_string())),
            self.manager
                .persistence_error
                .as_ref()
                .map_or_else(|| "null".into(), |e| json_string(e)),
            self.fallback_tray_id
                .map_or_else(|| "null".into(), |id| id.to_string()),
            devices,
            self.manager
                .discovery_errors
                .iter()
                .map(|(provider, error)| format!(
                    "{{\"provider\":{},\"error\":{}}}",
                    json_string(provider),
                    json_string(error)
                ))
                .collect::<Vec<_>>()
                .join(","),
            menu.count,
            opt_number(menu.enter_loop_us),
            opt_number(menu.visible_idle_us)
        );
        let value = value.strip_suffix('}').map_or_else(
            || value.clone(),
            |v| {
                format!(
                    "{v},\"page\":{},\"notifications_enabled\":{},\"threshold\":{}}}",
                    json_string(self.page.key()),
                    self.manager.notifications_enabled,
                    self.manager.threshold
                )
            },
        );
        value.strip_suffix('}').map_or_else(||value.clone(),|v|format!("{v},\"notification\":{{\"submissions\":{},\"last_accepted\":{},\"shown\":{},\"hidden\":{},\"timed_out\":{}}}}}",self.notification_submissions,self.notification_last_accepted,counts[0],counts[1],counts[2]))
    }
}
impl App {
    fn persist(&mut self) {
        #[cfg(test)]
        let path = self
            .effects
            .state_path
            .clone()
            .unwrap_or_else(manager::state_path);
        #[cfg(not(test))]
        let path = manager::state_path();
        self.manager.save(&path);
    }
    fn publish_view(&self) {
        *self.bridge.status.borrow_mut() = StatusView {
            manager: self.manager.clone(),
            busy: self.busy,
            simulated: self.simulated,
            refresh_count: self.refresh_count,
            success_count: self.success_count,
            tray_registered: self.trays.iter().all(|t| t.registered)
                && self.fallback.as_ref().is_none_or(|t| t.registered),
            panel: self.panel,
            fallback_tray_id: self.fallback.as_ref().map(|t| t.shell_id),
            tray_ids: self
                .trays
                .iter()
                .map(|t| (t.id.clone(), t.shell_id))
                .collect(),
            page: self.page,
            notification_submissions: self.notification_submissions,
            notification_last_accepted: self.notification_last_accepted,
        };

        self.bridge.activity_needed.set(
            (self.manager.selected().next().is_none()
                && self
                    .manager
                    .devices
                    .iter()
                    .any(|s| s.descriptor.recoverable && !s.descriptor.supported && s.connected))
                || self.manager.selected().any(|s| {
                    s.error.is_some()
                        || s.last_success
                            .is_none_or(|t| now().saturating_sub(t) >= 120)
                }),
        );
        *self.bridge.view.borrow_mut() = Rc::new(PanelView {
            scroll: self.scroll,
            page: self.page,
            threshold_error: self.threshold_error,
            rows: self.page_states(),
            symbols: self.rows.iter().map(|r| r.symbol.clone()).collect(),
        });
        if !self.panel.is_null() {
            unsafe {
                EnableWindow(
                    GetDlgItem(self.panel, PICK_REFRESH as i32),
                    i32::from(!self.busy),
                );
                for row in &self.rows {
                    if let Some(state) = self
                        .manager
                        .devices
                        .iter()
                        .find(|s| s.descriptor.id == row.id)
                    {
                        let number = state.reading.as_ref().map_or_else(
                            || "—%".into(),
                            |r| format!("{}%", r.normalized_percent()),
                        );
                        if self.page == Page::Main || self.page == Page::Add {
                            SetWindowTextW(
                                row.percent,
                                wide(if self.page == Page::Add && !state.descriptor.supported {
                                    "未适配"
                                } else {
                                    &number
                                })
                                .as_ptr(),
                            );
                        }
                        if self.page == Page::Main {
                            EnableWindow(row.refresh, i32::from(!self.busy));
                        }
                        if self.page == Page::Add {
                            SetWindowTextW(
                                row.refresh,
                                wide(if state.selected {
                                    "已添加"
                                } else if state.descriptor.supported {
                                    "添加"
                                } else {
                                    "复制信息"
                                })
                                .as_ptr(),
                            );
                            EnableWindow(row.refresh, i32::from(!state.selected));
                        }
                    }
                }
                InvalidateRect(self.panel, null(), 1);
            }
        }
    }
    fn refresh(&mut self) {
        self.refresh_ids(
            self.manager
                .selected()
                .map(|s| s.descriptor.id.clone())
                .collect(),
        );
    }
    fn refresh_ids(&mut self, ids: Vec<String>) {
        if self.busy {
            self.pending = true;
            return;
        }
        if ids.is_empty() {
            self.discover();
            return;
        }
        self.busy = true;
        self.refresh_count += 1;
        if self
            .sender
            .send(WorkerCommand::Read(ids, self.simulated))
            .is_err()
        {
            self.busy = false;
        }
        self.publish_view();
    }
    fn discover(&mut self) {
        if self.busy {
            self.pending_discovery = true;
            return;
        }
        self.busy = true;
        if self
            .sender
            .send(WorkerCommand::Discover(self.simulated))
            .is_err()
        {
            self.busy = false;
        }
        self.publish_view();
    }
    fn receive(&mut self) {
        if self.stop.check().is_err() {
            return;
        }
        while let Ok(result) = self.receiver.try_recv() {
            // A real query can have started before the diagnostic mode changed.
            // Its late result cannot update cached values/success timestamps.
            let result = if self.simulated {
                match result {
                    WorkerResult::Discovery(Ok(mut report)) => {
                        for candidate in &mut report.candidates {
                            candidate.reading = None;
                            candidate.error = Some(simulated_error());
                        }
                        WorkerResult::Discovery(Ok(report))
                    }
                    WorkerResult::Discovery(Err(_)) => {
                        WorkerResult::Discovery(Err(simulated_error()))
                    }
                    WorkerResult::Reading(id, _) => {
                        WorkerResult::Reading(id, Err(simulated_error()))
                    }
                    WorkerResult::Complete => WorkerResult::Complete,
                    WorkerResult::RefreshRequest(id) => WorkerResult::RefreshRequest(id),
                }
            } else {
                result
            };
            let fresh_result = matches!(
                &result,
                WorkerResult::Reading(_, Ok(_)) | WorkerResult::Discovery(Ok(_))
            );
            match result {
                WorkerResult::Discovery(Ok(report)) => {
                    self.success_count += report
                        .candidates
                        .iter()
                        .filter(|c| c.reading.is_some())
                        .count() as u64;
                    self.manager.merge_discovery(report, now());
                    if self.page == Page::Add {
                        for state in self
                            .manager
                            .devices
                            .iter()
                            .filter(|s| s.connected && !s.selected)
                        {
                            if !self.picker_ids.contains(&state.descriptor.id) {
                                self.picker_ids.push(state.descriptor.id.clone());
                            }
                        }
                        self.picker_ids.retain(|id| {
                            self.manager
                                .devices
                                .iter()
                                .any(|s| s.descriptor.id == *id && s.connected)
                        });
                    }
                    self.persist();
                    self.rebuild_rows();
                }
                WorkerResult::Discovery(Err(error)) => {
                    for state in &mut self.manager.devices {
                        state.error = Some(error.clone());
                    }
                }
                WorkerResult::Reading(id, result) => {
                    if result.is_ok() {
                        self.success_count += 1;
                    }
                    self.manager.apply(&id, result, now());
                    self.persist();
                    self.rebuild_rows();
                }
                WorkerResult::Complete => {
                    self.busy = false;
                    if self.pending_discovery {
                        self.pending_discovery = false;
                        self.discover();
                    } else if self.pending {
                        self.pending = false;
                        self.refresh();
                    }
                }
                WorkerResult::RefreshRequest(id) => self.refresh_ids(vec![id]),
            }
            self.update_icons(false);
            if fresh_result {
                self.deliver_alerts();
            } else {
                self.prune_alerts();
            }
            self.publish_view();
        }
    }
    fn prune_alerts(&mut self) {
        let manager = &self.manager;
        let mut eligible = std::collections::BTreeMap::new();
        for (id, _, _) in &manager.pending_alerts {
            if let Some(state) = manager.devices.iter().find(|s| {
                s.descriptor.id == *id
                    && s.selected
                    && s.notify
                    && (s.connected || !s.connection_known)
            }) {
                if let Some(reading) = state.reading.as_ref().filter(|r| {
                    manager.notifications_enabled
                        && r.normalized_percent() <= manager.threshold
                        && r.power_state != crate::device::PowerState::Charging
                        && state.alert.last_power != Some(crate::device::PowerState::Charging)
                }) {
                    eligible.insert(
                        id.clone(),
                        (state.descriptor.name.clone(), reading.normalized_percent()),
                    );
                }
            }
        }
        self.manager.pending_alerts = eligible
            .into_iter()
            .map(|(id, (name, percent))| (id, name, percent))
            .collect();
    }
    fn deliver_alerts(&mut self) {
        self.prune_alerts();
        if self.manager.persistence_error.is_some() {
            return;
        }
        let alerts = self.manager.pending_alerts.clone();
        for (id, name, percent) in alerts {
            if percent >= self.manager.threshold {
                continue;
            }
            let shell_id = self
                .trays
                .iter()
                .find(|t| t.id == id)
                .or(self.fallback.as_ref())
                .or(self.trays.first())
                .map(|t| t.shell_id);
            #[cfg(test)]
            let shell_id = shell_id.or_else(|| self.effects.notification_sink.as_ref().map(|_| 1));
            if let Some(shell_id) = shell_id {
                #[cfg(test)]
                let accepted = {
                    let _ = shell_id;
                    self.effects
                        .notification_sink
                        .as_mut()
                        .is_some_and(|sink| sink(&name, percent))
                };
                #[cfg(not(test))]
                let accepted =
                    crate::notifications::show(self.hwnd, shell_id, &name, percent, false);
                self.notification_last_accepted = accepted;
                self.notification_submissions += 1;
                if accepted {
                    self.manager
                        .pending_alerts
                        .retain(|(pending, _, _)| pending != &id);
                }
            }
        }
    }
    fn tray_data(&self, shell_id: u32) -> NOTIFYICONDATAW {
        let mut data: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
        data.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        data.hWnd = self.hwnd;
        data.uID = shell_id;
        data
    }
    fn remove_tray(&self, shell_id: u32) {
        unsafe {
            Shell_NotifyIconW(NIM_DELETE, &self.tray_data(shell_id));
        }
    }
    fn update_icons(&mut self, force: bool) {
        if cfg!(test) {
            return;
        }
        let desired: Vec<_> = self
            .manager
            .selected()
            .filter(|s| s.show_tray && (s.connected || !s.connection_known))
            .map(|s| {
                (
                    s.descriptor.id.clone(),
                    s.descriptor.short_name(),
                    s.descriptor.kind,
                    s.reading.as_ref().map(|r| r.normalized_percent()),
                )
            })
            .collect();
        let mut index = 0;
        while index < self.trays.len() {
            if !desired.iter().any(|d| d.0 == self.trays[index].id) {
                let old = self.trays.remove(index);
                self.remove_tray(old.shell_id);
            } else {
                index += 1;
            }
        }
        let size = tray_icon_size();
        for (id, name, kind, percent) in desired {
            let key = (kind, percent, size);
            let existing = self.trays.iter().position(|t| t.id == id);
            let index = if let Some(index) = existing {
                index
            } else {
                let Ok(icon) = TrayIcon::new(
                    size,
                    kind,
                    percent.map_or(
                        if self
                            .manager
                            .devices
                            .iter()
                            .any(|s| s.descriptor.id == id && !s.connected)
                        {
                            BatteryVisual::Offline
                        } else {
                            BatteryVisual::Unknown
                        },
                        BatteryVisual::Percent,
                    ),
                ) else {
                    continue;
                };
                let mut shell_id = stable_tray_id(&id);
                while self
                    .trays
                    .iter()
                    .any(|t| t.shell_id == shell_id && t.id != id)
                {
                    shell_id = shell_id.wrapping_add(1).max(2);
                }
                self.trays.push(TrayEntry {
                    id: id.clone(),
                    shell_id,
                    icon,
                    key,
                    registered: false,
                });
                self.trays.len() - 1
            };
            let replacement = if force || self.trays[index].key != key {
                TrayIcon::new(
                    size,
                    kind,
                    percent.map_or(
                        if self
                            .manager
                            .devices
                            .iter()
                            .any(|s| s.descriptor.id == id && !s.connected)
                        {
                            BatteryVisual::Offline
                        } else {
                            BatteryVisual::Unknown
                        },
                        BatteryVisual::Percent,
                    ),
                )
                .ok()
            } else {
                None
            };
            let mut data = self.tray_data(self.trays[index].shell_id);
            data.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
            data.uCallbackMessage = TRAY;
            data.hIcon = replacement
                .as_ref()
                .map_or_else(|| self.trays[index].icon.get(), |i| i.get());
            let tooltip = format!(
                "{name} · {}",
                percent.map_or_else(|| "—%".into(), |p| format!("{p}%"))
            );
            for (dst, src) in data.szTip.iter_mut().take(127).zip(tooltip.encode_utf16()) {
                *dst = src;
            }
            let operation = if self.trays[index].registered {
                NIM_MODIFY
            } else {
                NIM_ADD
            };
            let registered = unsafe { Shell_NotifyIconW(operation, &data) } != 0;
            self.trays[index].registered = registered;
            if registered {
                if let Some(icon) = replacement {
                    self.trays[index].icon = icon;
                    self.trays[index].key = key;
                }
            }
        }
        // A fallback remains available only when no per-device icon is visible.
        if self.trays.is_empty() {
            if force {
                if let Some(old) = self.fallback.take() {
                    self.remove_tray(old.shell_id);
                }
            }
            if self.fallback.is_none() {
                if let Ok(icon) = TrayIcon::software(size) {
                    self.fallback = Some(TrayEntry {
                        id: String::new(),
                        shell_id: 1,
                        icon,
                        key: (DeviceKind::Generic, None, size),
                        registered: false,
                    });
                }
            }
            if let Some(icon) = self.fallback.as_ref() {
                let mut data = self.tray_data(1);
                data.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
                data.uCallbackMessage = TRAY;
                data.hIcon = icon.icon.get();
                for (dst, src) in data.szTip.iter_mut().zip("外设电量".encode_utf16()) {
                    *dst = src;
                }
                let registered = unsafe {
                    Shell_NotifyIconW(if icon.registered { NIM_MODIFY } else { NIM_ADD }, &data)
                } != 0;
                self.fallback.as_mut().unwrap().registered = registered;
            }
        } else if let Some(old) = self.fallback.take() {
            self.remove_tray(old.shell_id);
        }
    }
    fn remove_icons(&mut self) {
        for entry in &self.trays {
            self.remove_tray(entry.shell_id);
        }
        if let Some(entry) = self.fallback.as_ref() {
            self.remove_tray(entry.shell_id);
        }
        self.trays.clear();
        self.fallback = None;
    }
    fn show_menu(&mut self, requested: Instant, _shell_id: u32, settings: bool) {
        unsafe {
            let menu = CreatePopupMenu();
            if menu.is_null() {
                return;
            }
            AppendMenuW(
                menu,
                MF_STRING,
                DETAILS as usize,
                wide("打开主窗口").as_ptr(),
            );
            AppendMenuW(
                menu,
                MF_STRING,
                MENU_REFRESH as usize,
                wide("手动刷新").as_ptr(),
            );
            AppendMenuW(menu, MF_STRING, ADD as usize, wide("添加设备").as_ptr());
            let enabled = startup::status().is_ok_and(|s| s.enabled);
            AppendMenuW(
                menu,
                MF_STRING | if enabled { MF_CHECKED } else { 0 },
                STARTUP as usize,
                wide("登录时启动").as_ptr(),
            );
            AppendMenuW(menu, MF_STRING, SETTINGS as usize, wide("设置").as_ptr());
            AppendMenuW(menu, MF_SEPARATOR, 0, null());
            AppendMenuW(menu, MF_STRING, EXIT as usize, wide("退出").as_ptr());
            let mut point: POINT = std::mem::zeroed();
            if settings && !self.panel.is_null() {
                let mut bounds: RECT = std::mem::zeroed();
                GetWindowRect(GetDlgItem(self.panel, SETTINGS as i32), &mut bounds);
                point.x = bounds.left;
                point.y = bounds.bottom;
            } else {
                GetCursorPos(&mut point);
            }
            SetForegroundWindow(self.hwnd);
            self.bridge.menu_timing.set(MenuTiming {
                requested: Some(requested),
                count: self.bridge.menu_timing.get().count + 1,
                ..MenuTiming::default()
            });
            self.bridge.menu_active.set(true);
            let command = TrackPopupMenu(
                menu,
                TPM_RETURNCMD | TPM_RIGHTBUTTON,
                point.x,
                point.y,
                0,
                self.hwnd,
                null(),
            ) as u32;
            self.bridge.menu_active.set(false);
            DestroyMenu(menu);
            PostMessageW(self.hwnd, WM_NULL, 0, 0);
            match command {
                DETAILS => self.show_panel(),
                MENU_REFRESH => self.refresh(),
                ADD => self.show_picker(),
                STARTUP => {
                    let enabled = startup::status().is_ok_and(|s| s.enabled);
                    self.set_startup(!enabled);
                }
                EXIT => self.quit(),
                SETTINGS => {
                    self.show_panel();
                    self.switch_page(Page::Settings);
                }
                _ => {}
            }
            self.publish_view();
        }
    }
    fn set_startup(&mut self, desired: bool) {
        if let Err(error) = startup::set_enabled(desired) {
            unsafe {
                MessageBoxW(
                    self.panel,
                    wide(&error).as_ptr(),
                    wide("登录启动设置失败").as_ptr(),
                    MB_OK | MB_ICONERROR,
                );
            }
        }
        self.publish_view();
    }
    fn page_states(&self) -> Vec<DeviceState> {
        match self.page {
            Page::Main => self.manager.visible().cloned().collect(),
            Page::Settings => self.manager.selected().cloned().collect(),
            Page::Add => self
                .picker_ids
                .iter()
                .filter_map(|id| self.manager.devices.iter().find(|s| s.descriptor.id == *id))
                .cloned()
                .collect(),
        }
    }
    fn row_top(&self) -> i32 {
        match self.page {
            Page::Main => 20,
            Page::Settings => 264,
            Page::Add => 76,
        }
    }
    fn viewport_top(&self) -> i32 {
        if self.page == Page::Main { 20 } else { 64 }
    }
    fn content_height(&self) -> i32 {
        if self.rows.is_empty() {
            if self.page == Page::Settings { 136 } else { 0 }
        } else {
            self.row_top() - self.viewport_top() + self.rows.len() as i32 * 64
        }
    }
    fn work_area(&self) -> RECT {
        unsafe {
            let monitor = MonitorFromWindow(self.panel, MONITOR_DEFAULTTONEAREST);
            let mut info: MONITORINFO = std::mem::zeroed();
            info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
            if GetMonitorInfoW(monitor, &mut info) != 0 {
                return info.rcWork;
            }
            let mut work = std::mem::zeroed();
            SystemParametersInfoW(SPI_GETWORKAREA, 0, (&mut work as *mut RECT).cast(), 0);
            work
        }
    }
    fn max_height(&self) -> i32 {
        let work = self.work_area();
        let dpi = if self.panel.is_null() {
            96
        } else {
            unsafe { GetDpiForWindow(self.panel).max(96) as i32 }
        };
        ((work.bottom - work.top) * 96 / dpi - 64).max(180)
    }
    fn footer_height(&self) -> i32 {
        if self.page == Page::Main { 48 } else { 24 }
    }
    fn panel_height(&self) -> i32 {
        let natural = if self.rows.is_empty() {
            match self.page {
                Page::Main => 132,
                Page::Settings => 226,
                Page::Add => 180,
            }
        } else {
            self.row_top() + self.rows.len() as i32 * 64 + self.footer_height()
        };
        natural.min(self.max_height())
    }
    fn place(&self, hwnd: HWND, x: i32, y: i32, w: i32, h: i32, font: usize) {
        if hwnd.is_null() {
            return;
        }
        unsafe {
            let dpi = GetDpiForWindow(self.panel).max(96) as i32;
            let mut top = y * dpi / 96;
            let mut height = h * dpi / 96;
            if GetDlgCtrlID(hwnd) == THRESHOLD as i32 {
                // Single-line EDIT has no vertical alignment style. Center its
                // font-sized client area inside the independently drawn frame.
                let dc = GetDC(hwnd);
                if !dc.is_null() {
                    let old = SelectObject(dc, self.panel_fonts[font]);
                    let mut metrics: TEXTMETRICW = std::mem::zeroed();
                    if GetTextMetricsW(dc, &mut metrics) != 0 {
                        let text_height = metrics.tmHeight.clamp(1, height);
                        top += (height - text_height) / 2;
                        height = text_height;
                    }
                    SelectObject(dc, old);
                    ReleaseDC(hwnd, dc);
                }
            }
            MoveWindow(hwnd, x * dpi / 96, top, w * dpi / 96, height, 1);
            SendMessageW(hwnd, WM_SETFONT, self.panel_fonts[font] as usize, 1);
        }
    }
    fn layout_panel(&mut self) {
        if self.panel.is_null() {
            return;
        }
        unsafe {
            let old_fonts = self.panel_fonts;
            let dpi = GetDpiForWindow(self.panel).max(96) as i32;
            for (index, (height, weight)) in
                [(15, 400), (17, 700), (26, 700)].into_iter().enumerate()
            {
                let font = CreateFontW(
                    -height * dpi / 96,
                    0,
                    0,
                    0,
                    weight,
                    0,
                    0,
                    0,
                    DEFAULT_CHARSET as u32,
                    0,
                    0,
                    CLEARTYPE_QUALITY as u32,
                    0,
                    wide("Microsoft YaHei UI").as_ptr(),
                );
                if !font.is_null() {
                    self.panel_fonts[index] = font;
                }
            }
            let viewport =
                (self.panel_height() - self.viewport_top() - self.footer_height()).max(0);
            self.scroll = self
                .scroll
                .clamp(0, (self.content_height() - viewport).max(0));
            let info = SCROLLINFO {
                cbSize: std::mem::size_of::<SCROLLINFO>() as u32,
                fMask: SIF_RANGE | SIF_PAGE | SIF_POS,
                nMin: 0,
                nMax: (self.content_height() - 1).max(0),
                nPage: viewport as u32,
                nPos: self.scroll,
                nTrackPos: 0,
            };
            SetScrollInfo(self.panel, SB_VERT, &info, 1);
            let get = |id| GetDlgItem(self.panel, id as i32);
            if self.page == Page::Main {
                self.place(
                    get(ADD),
                    if self.rows.is_empty() { 190 } else { 18 },
                    if self.rows.is_empty() {
                        24
                    } else {
                        self.panel_height() - 38
                    },
                    if self.rows.is_empty() { 180 } else { 130 },
                    32,
                    0,
                );
                self.place(get(SETTINGS), 486, self.panel_height() - 38, 56, 32, 0);
            } else {
                self.place(get(BACK), 18, 12, 32, 36, 0);
                if self.page == Page::Add {
                    self.place(get(PICK_REFRESH), 450, 14, 94, 32, 0);
                }
                if self.page == Page::Settings {
                    for (id, x, y, w, h) in [
                        (STARTUP, 24, 68, 512, 40),
                        (ALERTS, 24, 116, 512, 40),
                        (THRESHOLD, 464, 168, 56, 32),
                        (113, 24, 168, 220, 32),
                        (114, 524, 168, 12, 32),
                    ] {
                        let y = y - self.scroll;
                        self.place(get(id), x, y, w, h, 0);
                        ShowWindow(
                            get(id),
                            if y >= 64 && y + h <= self.panel_height() - self.footer_height() {
                                SW_SHOW
                            } else {
                                SW_HIDE
                            },
                        );
                    }
                }
            }
            for (index, row) in self.rows.iter().enumerate() {
                let y = self.row_top() + index as i32 * 64 - self.scroll;
                let visible = y >= self.viewport_top()
                    && y + 56 <= self.panel_height() - self.footer_height();
                for control in [row.model, row.percent, row.refresh]
                    .into_iter()
                    .chain(row.extra.iter().copied())
                {
                    if !control.is_null() {
                        ShowWindow(control, if visible { SW_SHOW } else { SW_HIDE });
                    }
                }
                match self.page {
                    Page::Main => {
                        self.place(row.model, 64, y + 6, 140, 40, 1);
                        self.place(row.percent, 366, y + 2, 88, 44, 2);
                        self.place(row.refresh, 460, y + 7, 82, 36, 0);
                    }
                    Page::Settings => {
                        self.place(row.model, 64, y + 6, 238, 40, 1);
                        self.place(row.percent, 334, y + 8, 40, 36, 0);
                        self.place(row.refresh, 410, y + 8, 40, 36, 0);
                        if let Some(remove) = row.extra.first() {
                            self.place(*remove, 494, y + 8, 50, 36, 0);
                        }
                    }
                    Page::Add => {
                        self.place(row.model, 64, y + 6, 288, 40, 1);
                        self.place(row.percent, 364, y + 6, 80, 40, 0);
                        self.place(row.refresh, 466, y + 6, 70, 36, 0);
                    }
                }
            }
            let mut bounds = RECT {
                left: 0,
                top: 0,
                right: PANEL_WIDTH * dpi / 96,
                bottom: self.panel_height() * dpi / 96,
            };
            AdjustWindowRectExForDpi(
                &mut bounds,
                GetWindowLongPtrW(self.panel, GWL_STYLE) as u32,
                0,
                WS_EX_CONTROLPARENT,
                dpi as u32,
            );
            let work = self.work_area();
            let mut position: RECT = std::mem::zeroed();
            GetWindowRect(self.panel, &mut position);
            let width = bounds.right - bounds.left;
            let height = bounds.bottom - bounds.top;
            SetWindowPos(
                self.panel,
                null_mut(),
                position
                    .left
                    .clamp(work.left, (work.right - width).max(work.left)),
                position
                    .top
                    .clamp(work.top, (work.bottom - height).max(work.top)),
                width,
                height,
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
            if self.page == Page::Main {
                for id in [ADD, SETTINGS] {
                    SetWindowPos(
                        get(id),
                        HWND_BOTTOM,
                        0,
                        0,
                        0,
                        0,
                        SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                    );
                }
            }
            InvalidateRect(self.panel, null(), 1);
            for (old, new) in old_fonts.into_iter().zip(self.panel_fonts) {
                if !old.is_null() && old != new {
                    DeleteObject(old);
                }
            }
        }
    }
    fn clear_rows(&mut self) {
        unsafe {
            for row in self.rows.drain(..) {
                for hwnd in [row.model, row.percent, row.refresh, row.tooltip]
                    .into_iter()
                    .chain(row.extra)
                {
                    if !hwnd.is_null() {
                        DestroyWindow(hwnd);
                    }
                }
            }
        }
    }
    fn rebuild_rows(&mut self) {
        self.rebuild_rows_with(false);
    }
    fn rebuild_rows_with(&mut self, force: bool) {
        if self.panel.is_null() {
            return;
        }
        let states = self.page_states();
        let unchanged = states.len() == self.rows.len()
            && states.iter().zip(&self.rows).all(|(s, row)| {
                row.id == s.descriptor.id
                    && row.presentation
                        == (
                            self.manager.display_name(&s.descriptor.id, true),
                            s.descriptor.name.clone(),
                            s.descriptor.kind,
                            s.descriptor.supported,
                        )
            });
        if unchanged && !force {
            self.layout_panel();
            self.publish_view();
            return;
        }
        let focused = unsafe { windows_sys::Win32::UI::Input::KeyboardAndMouse::GetFocus() };
        let anchor = self.anchor_for(focused);
        self.clear_rows();
        unsafe {
            let dpi = GetDpiForWindow(self.panel).max(96) as i32;
            for (index, state) in states.iter().enumerate() {
                let name = self.manager.display_name(&state.descriptor.id, true);
                let model = create_child(
                    self.panel,
                    "STATIC",
                    &name,
                    SS_LEFT | SS_NOPREFIX | SS_CENTERIMAGE | 0x4000,
                    ROW_BASE + index as u32 * 3,
                );
                let mut extra = Vec::new();
                let (percent, refresh) = match self.page {
                    Page::Main => (
                        create_child(
                            self.panel,
                            "STATIC",
                            "—%",
                            SS_LEFT | SS_NOPREFIX | SS_CENTERIMAGE,
                            ROW_BASE + index as u32 * 3 + 1,
                        ),
                        create_child(
                            self.panel,
                            "BUTTON",
                            "手动刷新",
                            WS_TABSTOP | BS_PUSHBUTTON as u32,
                            ROW_BASE + index as u32 * 3 + 2,
                        ),
                    ),
                    Page::Settings => {
                        let base = SETTINGS_BASE + index as u32 * 3;
                        let tray = create_child(
                            self.panel,
                            "BUTTON",
                            &format!("{name}，显示托盘"),
                            WS_TABSTOP | BS_AUTOCHECKBOX as u32,
                            base,
                        );
                        let notify = create_child(
                            self.panel,
                            "BUTTON",
                            &format!("{name}，低电提醒"),
                            WS_TABSTOP | BS_AUTOCHECKBOX as u32,
                            base + 1,
                        );
                        let remove = create_child(
                            self.panel,
                            "BUTTON",
                            &format!("{name}，移除监控"),
                            WS_TABSTOP | BS_PUSHBUTTON as u32,
                            base + 2,
                        );
                        style_control(tray, true);
                        style_control(notify, true);
                        style_control(remove, false);
                        SendMessageW(tray, BM_SETCHECK, usize::from(state.show_tray), 0);
                        SendMessageW(notify, BM_SETCHECK, usize::from(state.notify), 0);
                        EnableWindow(notify, i32::from(self.manager.notifications_enabled));
                        extra.push(remove);
                        (tray, notify)
                    }
                    Page::Add => (
                        create_child(
                            self.panel,
                            "STATIC",
                            "—%",
                            SS_LEFT | SS_NOPREFIX | SS_CENTERIMAGE,
                            ROW_BASE + index as u32 * 3 + 1,
                        ),
                        create_child(
                            self.panel,
                            "BUTTON",
                            if state.descriptor.supported {
                                "添加"
                            } else {
                                "复制信息"
                            },
                            WS_TABSTOP | BS_PUSHBUTTON as u32,
                            ADD_BASE + index as u32,
                        ),
                    ),
                };
                if self.page != Page::Settings {
                    style_control(refresh, false);
                }
                let mut text = wide(&state.descriptor.name);
                let tooltip = CreateWindowExW(
                    WS_EX_TOPMOST,
                    wide("tooltips_class32").as_ptr(),
                    null(),
                    WS_POPUP | TTS_ALWAYSTIP,
                    0,
                    0,
                    0,
                    0,
                    self.panel,
                    null_mut(),
                    null_mut(),
                    null(),
                );
                if !tooltip.is_null() {
                    let tool = TTTOOLINFOW {
                        cbSize: std::mem::size_of::<TTTOOLINFOW>() as u32,
                        uFlags: TTF_IDISHWND | TTF_SUBCLASS,
                        hwnd: self.panel,
                        uId: model as usize,
                        lpszText: text.as_mut_ptr(),
                        ..std::mem::zeroed()
                    };
                    SendMessageW(
                        tooltip,
                        TTM_ADDTOOLW,
                        0,
                        &tool as *const TTTOOLINFOW as isize,
                    );
                }
                if let Ok(symbol) = SymbolIcon::new(28 * dpi / 96, state.descriptor.kind) {
                    self.rows.push(RowControls {
                        id: state.descriptor.id.clone(),
                        model,
                        percent,
                        refresh,
                        symbol: Rc::new(symbol),
                        tooltip,
                        _tooltip_text: text,
                        extra,
                        presentation: (
                            name.clone(),
                            state.descriptor.name.clone(),
                            state.descriptor.kind,
                            state.descriptor.supported,
                        ),
                    });
                }
            }
        }
        self.layout_panel();
        self.publish_view();
        if let Some(anchor) = anchor {
            let current = unsafe { windows_sys::Win32::UI::Input::KeyboardAndMouse::GetFocus() };
            if current.is_null() || unsafe { IsChild(self.panel, current) } != 0 {
                if let Some((target, index)) = self.resolve_anchor(&anchor) {
                    self.focus_target(target, index);
                }
            }
        }
    }
    fn anchor_for(&self, hwnd: HWND) -> Option<FocusAnchor> {
        if hwnd.is_null() || self.panel.is_null() || unsafe { IsChild(self.panel, hwnd) } == 0 {
            return None;
        }
        for (index, row) in self.rows.iter().enumerate() {
            for (action, control) in [row.percent, row.refresh]
                .into_iter()
                .chain(row.extra.iter().copied())
                .enumerate()
            {
                if control == hwnd {
                    return Some(FocusAnchor::Device {
                        id: row.id.clone(),
                        action,
                        index,
                    });
                }
            }
        }
        Some(FocusAnchor::Control(unsafe { GetDlgCtrlID(hwnd) } as u32))
    }
    fn resolve_anchor(&self, anchor: &FocusAnchor) -> Option<(HWND, Option<usize>)> {
        let enabled = |h: HWND| !h.is_null() && unsafe { IsWindowEnabled(h) } != 0;
        match anchor {
            FocusAnchor::Control(id) => {
                let h = unsafe { GetDlgItem(self.panel, *id as i32) };
                if enabled(h) {
                    return Some((h, None));
                }
            }
            FocusAnchor::Device { id, action, index } => {
                let index = self
                    .rows
                    .iter()
                    .position(|r| r.id == *id)
                    .unwrap_or_else(|| (*index).min(self.rows.len().saturating_sub(1)));
                if let Some(row) = self.rows.get(index) {
                    let controls = [row.percent, row.refresh]
                        .into_iter()
                        .chain(row.extra.iter().copied())
                        .collect::<Vec<_>>();
                    if let Some(h) = controls.get(*action).copied().filter(|h| {
                        enabled(*h)
                            && unsafe { GetWindowLongPtrW(*h, GWL_STYLE) } & WS_TABSTOP as isize
                                != 0
                    }) {
                        return Some((h, Some(index)));
                    }
                    if let Some(h) = controls.into_iter().find(|h| {
                        enabled(*h)
                            && unsafe { GetWindowLongPtrW(*h, GWL_STYLE) } & WS_TABSTOP as isize
                                != 0
                    }) {
                        return Some((h, Some(index)));
                    }
                }
            }
        }
        self.focus_targets().into_iter().next()
    }
    fn create_page_controls(&mut self) {
        unsafe {
            for hwnd in self.page_controls.drain(..) {
                DestroyWindow(hwnd);
            }
            let mut add = |class: &str, label: &str, style: u32, id: u32| {
                let h = create_child(self.panel, class, label, style, id);
                self.page_controls.push(h);
                h
            };
            if self.page == Page::Main {
                for (id, label) in [(ADD, "＋ 添加设备"), (SETTINGS, "设置")] {
                    let h = add("BUTTON", label, WS_TABSTOP | BS_PUSHBUTTON as u32, id);
                    style_control(h, false);
                }
            } else {
                let back = add("BUTTON", "返回", WS_TABSTOP | BS_PUSHBUTTON as u32, BACK);
                style_control(back, false);
                if self.page == Page::Add {
                    let h = add(
                        "BUTTON",
                        "手动刷新",
                        WS_TABSTOP | BS_PUSHBUTTON as u32,
                        PICK_REFRESH,
                    );
                    style_control(h, false);
                }
                if self.page == Page::Settings {
                    for (id, label, checked) in [
                        (
                            STARTUP,
                            "开机自启",
                            startup::status().is_ok_and(|s| s.enabled),
                        ),
                        (ALERTS, "低电提醒", self.manager.notifications_enabled),
                    ] {
                        let h = add("BUTTON", label, WS_TABSTOP | BS_AUTOCHECKBOX as u32, id);
                        style_control(h, true);
                        SendMessageW(h, BM_SETCHECK, usize::from(checked), 0);
                    }
                    add("STATIC", "提醒阈值", SS_LEFT | SS_CENTERIMAGE, 113);
                    let edit = add(
                        "EDIT",
                        &self.manager.threshold.to_string(),
                        WS_TABSTOP | ES_CENTER as u32 | ES_AUTOHSCROLL as u32,
                        THRESHOLD,
                    );
                    SendMessageW(edit, EM_SETLIMITTEXT, 8, 0);
                    EnableWindow(edit, i32::from(self.manager.notifications_enabled));
                    add("STATIC", "%", SS_LEFT | SS_CENTERIMAGE, 114);
                }
            }
        }
    }
    fn switch_page(&mut self, page: Page) {
        self.page_scroll[self.page.index()] = self.scroll;
        self.threshold_error = false;
        self.clear_rows();
        self.page = page;
        self.scroll = self.page_scroll[page.index()];
        if page == Page::Add {
            self.picker_ids = self
                .manager
                .devices
                .iter()
                .filter(|s| s.connected && !s.selected)
                .map(|s| s.descriptor.id.clone())
                .collect();
        }
        self.create_page_controls();
        self.rebuild_rows();
        self.layout_panel();
        self.publish_view();
        if !cfg!(test) {
            unsafe {
                let first = GetNextDlgTabItem(self.panel, null_mut(), 0);
                if !first.is_null() {
                    windows_sys::Win32::UI::Input::KeyboardAndMouse::SetFocus(first);
                }
            }
        }
        self.publish_view();
    }
    fn show_panel(&mut self) {
        unsafe {
            if self.panel.is_null() {
                self.page = Page::Main;
                self.panel = CreateWindowExW(
                    WS_EX_CONTROLPARENT,
                    wide(PANEL_CLASS).as_ptr(),
                    wide("外设电量").as_ptr(),
                    WS_OVERLAPPED
                        | WS_CAPTION
                        | WS_SYSMENU
                        | WS_MINIMIZEBOX
                        | WS_CLIPCHILDREN
                        | WS_VSCROLL,
                    CW_USEDEFAULT,
                    CW_USEDEFAULT,
                    560,
                    180,
                    null_mut(),
                    null_mut(),
                    GetModuleHandleW(null()),
                    null(),
                );
                if self.panel.is_null() {
                    crate::launch::show_error(&format!(
                        "无法创建主窗口（Windows错误{}）",
                        GetLastError()
                    ));
                    return;
                }
                attach_bridge(self.panel, &self.bridge);
                set_panel_icons(self.panel, &mut self.panel_icons);
                self.create_page_controls();
                self.rebuild_rows();
                self.layout_panel();
            }
            self.publish_view();
            ShowWindow(self.panel, if cfg!(test) { SW_HIDE } else { SW_RESTORE });
            if !cfg!(test) {
                SetForegroundWindow(self.panel);
            }
        }
        self.publish_view();
    }
    fn release_panel_resources(&mut self) {
        unsafe {
            for font in &mut self.panel_fonts {
                let old = std::mem::replace(font, null_mut());
                if !old.is_null() {
                    DeleteObject(old);
                }
            }
            for icon in &mut self.panel_icons {
                let old = std::mem::replace(icon, null_mut());
                if !old.is_null() {
                    DestroyIcon(old);
                }
            }
        }
        self.rows.clear();
        self.page_controls.clear();
    }
    fn close_panel(&mut self) {
        self.close_picker();
        let panel = std::mem::replace(&mut self.panel, null_mut());
        if !panel.is_null() {
            unsafe {
                DestroyWindow(panel);
            }
        }
        self.release_panel_resources();
        self.publish_view();
    }
    fn show_picker(&mut self) {
        if self.panel.is_null() {
            self.show_panel();
        }
        self.switch_page(Page::Add);
        self.discover();
    }
    fn close_picker(&mut self) {
        self.picker_ids.clear();
    }
    fn page_command(&mut self, id: u32) {
        let previous = self.manager.clone();
        match id {
            BACK => self.switch_page(Page::Main),
            PICK_REFRESH => {
                self.picker_ids = self
                    .manager
                    .devices
                    .iter()
                    .filter(|s| s.connected && !s.selected)
                    .map(|s| s.descriptor.id.clone())
                    .collect();
                self.rebuild_rows();
                self.discover();
            }
            STARTUP => {
                let enabled = startup::status().is_ok_and(|s| s.enabled);
                self.set_startup(!enabled);
                unsafe {
                    SendMessageW(
                        GetDlgItem(self.panel, STARTUP as i32),
                        BM_SETCHECK,
                        usize::from(startup::status().is_ok_and(|s| s.enabled)),
                        0,
                    );
                }
            }
            ALERTS => {
                self.manager.notifications_enabled = !self.manager.notifications_enabled;
                self.persist();
                unsafe {
                    EnableWindow(
                        GetDlgItem(self.panel, THRESHOLD as i32),
                        i32::from(self.manager.notifications_enabled),
                    );
                    for row in &self.rows {
                        EnableWindow(row.refresh, i32::from(self.manager.notifications_enabled));
                    }
                }
            }
            id if (SETTINGS_BASE..ADD_BASE).contains(&id) => {
                let index = ((id - SETTINGS_BASE) / 3) as usize;
                let action = (id - SETTINGS_BASE) % 3;
                if let Some(row) = self.rows.get(index) {
                    let device_id = row.id.clone();
                    if action == 0 {
                        self.manager.toggle_tray(&device_id);
                    } else if action == 1 {
                        if let Some(s) = self
                            .manager
                            .devices
                            .iter_mut()
                            .find(|s| s.descriptor.id == device_id)
                        {
                            s.notify = !s.notify;
                        }
                    } else {
                        self.manager.remove(&device_id);
                    }
                    self.persist();
                    self.rebuild_rows();
                    self.update_icons(false);
                }
            }
            id if id >= ADD_BASE => {
                if let Some(row) = self.rows.get((id - ADD_BASE) as usize) {
                    let device_id = row.id.clone();
                    let supported = self
                        .manager
                        .devices
                        .iter()
                        .any(|s| s.descriptor.id == device_id && s.descriptor.supported);
                    if supported {
                        self.manager.select(&device_id);
                        self.persist();
                        self.update_icons(false);
                        self.publish_view();
                        self.refresh_ids(vec![device_id.clone()]);
                        unsafe {
                            let next = self
                                .rows
                                .iter()
                                .skip((id - ADD_BASE) as usize + 1)
                                .find(|r| IsWindowEnabled(r.refresh) != 0)
                                .map_or(GetDlgItem(self.panel, PICK_REFRESH as i32), |r| r.refresh);
                            if !cfg!(test) {
                                windows_sys::Win32::UI::Input::KeyboardAndMouse::SetFocus(next);
                            }
                        }
                    } else if let Some(state) = self
                        .manager
                        .devices
                        .iter()
                        .find(|s| s.descriptor.id == device_id)
                    {
                        let mut info = format!(
                            "型号: {}\r\n类型: {}\r\n发现方式: {}\r\n设备身份: {}",
                            state.descriptor.name,
                            state.descriptor.kind.key(),
                            state.descriptor.provider,
                            state.descriptor.id
                        );
                        if let Some(details) = Registry::diagnostic_information(&state.descriptor) {
                            info.push_str("\r\n");
                            info.push_str(&details);
                        }
                        if let Err(e) = copy_text(self.panel, &info) {
                            self.operation_error(&e);
                        }
                    }
                }
            }
            _ => {}
        }
        if let Some(error) = self.manager.persistence_error.clone() {
            self.manager = previous;
            self.manager.persistence_error = Some(error.clone());
            self.clear_rows();
            self.create_page_controls();
            self.rebuild_rows();
            self.update_icons(false);
            self.operation_error(&format!("无法保存设置：{error}"));
        }
        self.sync_preferences();
        self.publish_view();
    }
    fn sync_preferences(&self) {
        if self.panel.is_null() || self.page != Page::Settings {
            return;
        }
        unsafe {
            SendMessageW(
                GetDlgItem(self.panel, ALERTS as i32),
                BM_SETCHECK,
                usize::from(self.manager.notifications_enabled),
                0,
            );
            EnableWindow(
                GetDlgItem(self.panel, THRESHOLD as i32),
                i32::from(self.manager.notifications_enabled),
            );
            for row in &self.rows {
                if let Some(state) = self
                    .manager
                    .devices
                    .iter()
                    .find(|s| s.descriptor.id == row.id)
                {
                    SendMessageW(row.percent, BM_SETCHECK, usize::from(state.show_tray), 0);
                    SendMessageW(row.refresh, BM_SETCHECK, usize::from(state.notify), 0);
                    EnableWindow(row.refresh, i32::from(self.manager.notifications_enabled));
                }
            }
        }
    }
    fn focus_targets(&self) -> Vec<(HWND, Option<usize>)> {
        let mut targets = Vec::new();
        if self.page != Page::Main {
            for h in &self.page_controls {
                if unsafe { GetWindowLongPtrW(*h, GWL_STYLE) } & WS_TABSTOP as isize != 0 {
                    targets.push((*h, None));
                }
            }
        }
        for (index, row) in self.rows.iter().enumerate() {
            if self.page == Page::Settings {
                targets.push((row.percent, Some(index)));
            }
            targets.push((row.refresh, Some(index)));
            for h in &row.extra {
                targets.push((*h, Some(index)));
            }
        }
        if self.page == Page::Main {
            for h in &self.page_controls {
                targets.push((*h, None));
            }
        }
        targets.retain(|(h, _)| unsafe { IsWindowEnabled(*h) } != 0);
        targets
    }
    fn focus_target(&mut self, hwnd: HWND, index: Option<usize>) {
        let item = index
            .map(|index| (self.row_top() - self.viewport_top() + index as i32 * 64, 56))
            .or_else(|| {
                if self.page == Page::Settings {
                    match unsafe { GetDlgCtrlID(hwnd) } as u32 {
                        STARTUP => Some((4, 40)),
                        ALERTS => Some((52, 40)),
                        THRESHOLD => Some((104, 32)),
                        _ => None,
                    }
                } else {
                    None
                }
            });
        if let Some((y, h)) = item {
            let available =
                (self.panel_height() - self.viewport_top() - self.footer_height()).max(40);
            if y < self.scroll {
                self.scroll = y;
            } else if y + h > self.scroll + available {
                self.scroll = y + h - available;
            }
            self.layout_panel();
            self.publish_view();
        }
        if !cfg!(test) {
            unsafe {
                windows_sys::Win32::UI::Input::KeyboardAndMouse::SetFocus(hwnd);
            }
        }
    }
    fn tab_focus(&mut self, backwards: bool) {
        let targets = self.focus_targets();
        if targets.is_empty() {
            return;
        }
        let focused = unsafe { windows_sys::Win32::UI::Input::KeyboardAndMouse::GetFocus() };
        let current = targets.iter().position(|(h, _)| *h == focused);
        let index = if backwards {
            current.map_or(targets.len() - 1, |i| {
                (i + targets.len() - 1) % targets.len()
            })
        } else {
            current.map_or(0, |i| (i + 1) % targets.len())
        };
        self.focus_target(targets[index].0, targets[index].1);
    }
    fn operation_error(&self, error: &str) {
        if !cfg!(test) {
            unsafe {
                MessageBoxW(
                    self.panel,
                    wide(error).as_ptr(),
                    wide("操作未完成").as_ptr(),
                    MB_OK | MB_ICONERROR,
                );
            }
        }
    }
    fn submit_threshold(&mut self, enter: bool) {
        if self.page != Page::Settings {
            return;
        }
        unsafe {
            let edit = GetDlgItem(self.panel, THRESHOLD as i32);
            let mut text = [0u16; 32];
            let len = GetWindowTextW(edit, text.as_mut_ptr(), 32);
            let text = String::from_utf16_lossy(&text[..len as usize]);
            let parsed = if !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()) {
                text.parse::<u8>().ok().filter(|n| (1..=100).contains(n))
            } else {
                None
            };
            if let Some(value) = parsed {
                let previous = self.manager.threshold;
                self.manager.threshold = value;
                self.threshold_error = false;
                SendMessageW(edit, EM_HIDEBALLOONTIP, 0, 0);
                self.persist();
                if let Some(error) = &self.manager.persistence_error {
                    self.manager.threshold = previous;
                    SetWindowTextW(edit, wide(&previous.to_string()).as_ptr());
                    self.operation_error(&format!("无法保存提醒阈值：{error}"));
                }
            } else {
                self.threshold_error = true;
                if enter {
                    SendMessageW(edit, EM_SETSEL, 0, -1);
                } else {
                    SetWindowTextW(edit, wide(&self.manager.threshold.to_string()).as_ptr());
                }
                if !cfg!(test) {
                    let title = wide("提醒阈值");
                    let message = wide("请输入1–100的整数");
                    let tip = EDITBALLOONTIP {
                        cbStruct: std::mem::size_of::<EDITBALLOONTIP>() as u32,
                        pszTitle: title.as_ptr(),
                        pszText: message.as_ptr(),
                        ttiIcon: TTI_ERROR,
                    };
                    SendMessageW(
                        edit,
                        EM_SHOWBALLOONTIP,
                        0,
                        &tip as *const EDITBALLOONTIP as isize,
                    );
                    windows_sys::Win32::UI::Accessibility::NotifyWinEvent(
                        EVENT_SYSTEM_ALERT,
                        edit,
                        OBJID_CLIENT,
                        0,
                    );
                }
            }
            InvalidateRect(self.panel, null(), 1);
        }
        self.publish_view();
    }
    fn process_events(&mut self) {
        while let Some(event) = self.bridge.next() {
            match event {
                UiEvent::Control(command) => match command {
                    REFRESH => self.refresh(),
                    QUIT => self.quit(),
                    SIMULATE_ERROR => {
                        self.simulated = true;
                        self.refresh();
                    }
                    RECOVER => {
                        self.simulated = false;
                        self.refresh();
                    }
                    EXPLORER_RESTART => {
                        self.remove_icons();
                        self.update_icons(true);
                    }
                    SHOW_PANEL => self.show_panel(),
                    HIDE_PANEL => self.close_panel(),
                    SHOW_MENU => self.show_menu(Instant::now(), 0, false),
                    TEST_NOTIFICATION => {
                        if let Some(entry) = self.fallback.as_ref().or(self.trays.first()) {
                            self.notification_last_accepted =
                                crate::notifications::show(self.hwnd, entry.shell_id, "", 0, true);
                            self.notification_submissions += 1;
                            self.publish_view();
                        }
                    }
                    value if value == ADD as usize => self.show_picker(),
                    _ => {}
                },
                UiEvent::Result => self.receive(),
                UiEvent::Heartbeat => {
                    self.receive();
                    self.update_icons(false);
                    if !self.manager.pending_alerts.is_empty() {
                        self.persist();
                        self.deliver_alerts();
                    }
                    self.publish_view();
                }
                UiEvent::Refresh => self.refresh(),
                UiEvent::Discover => self.discover(),
                UiEvent::Activity => {
                    self.bridge.activity_queued.set(false);
                    if self.last_activity_refresh.elapsed().as_secs() >= 10 {
                        self.last_activity_refresh = Instant::now();
                        self.refresh();
                    }
                }
                UiEvent::Menu(requested, id) => self.show_menu(requested, id, false),
                UiEvent::Settings => self.switch_page(Page::Settings),
                UiEvent::PageCommand(id) => self.page_command(id),
                UiEvent::Threshold(enter) => self.submit_threshold(enter),
                UiEvent::ThresholdEdited => {
                    self.threshold_error = false;
                    self.publish_view();
                }
                UiEvent::ShowPanel => self.show_panel(),
                UiEvent::ClosePanel => self.close_panel(),
                UiEvent::RowRefresh(id) => {
                    if self.manager.selected().any(|s| s.descriptor.id == id) {
                        self.refresh_ids(vec![id]);
                    }
                }
                UiEvent::DeviceCommand(page, action, id) => {
                    if self.page == page {
                        if let Some(index) = self.rows.iter().position(|r| r.id == id) {
                            self.page_command(if page == Page::Settings {
                                SETTINGS_BASE + index as u32 * 3 + action
                            } else {
                                ADD_BASE + index as u32
                            });
                        }
                    }
                }
                UiEvent::Scroll(delta) => {
                    self.scroll = (self.scroll + delta).max(0);
                    self.layout_panel();
                    self.publish_view();
                }
                UiEvent::MainDestroyed => {
                    self.stop.cancel();
                    self.close_picker();
                    self.close_panel();
                    self.remove_icons();
                    let _ = self.sender.send(WorkerCommand::Stop);
                    unsafe {
                        PostQuitMessage(0);
                    }
                }
                UiEvent::PanelDestroyed(hwnd) => {
                    if self.panel == hwnd {
                        self.panel = null_mut();
                        self.release_panel_resources();
                    }
                }
                UiEvent::ActivatePanel => self.publish_view(),
                UiEvent::DpiChanged(hwnd, bounds) => {
                    if self.picker == hwnd {
                        unsafe {
                            SetWindowPos(
                                hwnd,
                                null_mut(),
                                bounds.left,
                                bounds.top,
                                bounds.right - bounds.left,
                                bounds.bottom - bounds.top,
                                SWP_NOZORDER | SWP_NOACTIVATE,
                            );
                        }
                    }
                    if self.panel == hwnd {
                        unsafe {
                            SetWindowPos(
                                hwnd,
                                null_mut(),
                                bounds.left,
                                bounds.top,
                                bounds.right - bounds.left,
                                bounds.bottom - bounds.top,
                                SWP_NOZORDER | SWP_NOACTIVATE,
                            );
                            set_panel_icons(hwnd, &mut self.panel_icons);
                        }
                        self.retheme_rows();
                    }
                }
                UiEvent::SettingsChanged => {
                    self.update_icons(true);
                    self.retheme_rows();
                    self.publish_view();
                }
                UiEvent::TaskbarCreated => {
                    for entry in &mut self.trays {
                        entry.registered = false;
                    }
                    if let Some(entry) = self.fallback.as_mut() {
                        entry.registered = false;
                    }
                    self.update_icons(true);
                    self.publish_view();
                }
            }
        }
    }
    fn retheme_rows(&mut self) {
        if !self.panel.is_null() {
            self.rebuild_rows_with(true);
            self.layout_panel();
        }
    }
    fn quit(&mut self) {
        self.stop.cancel();
        unsafe {
            KillTimer(self.hwnd, 1);
            KillTimer(self.hwnd, 2);
        }
        self.close_picker();
        self.close_panel();
        self.remove_icons();
        let _ = self.sender.send(WorkerCommand::Stop);
        unsafe {
            DestroyWindow(self.hwnd);
        }
    }
}
unsafe fn create_child(parent: HWND, class: &str, text: &str, style: u32, id: u32) -> HWND {
    unsafe {
        CreateWindowExW(
            0,
            wide(class).as_ptr(),
            wide(text).as_ptr(),
            WS_CHILD | WS_VISIBLE | style,
            0,
            0,
            1,
            1,
            parent,
            id as usize as HMENU,
            null_mut(),
            null(),
        )
    }
}
fn copy_text(hwnd: HWND, text: &str) -> Result<(), String> {
    use windows_sys::Win32::System::{DataExchange::*, Memory::*};
    unsafe {
        let text = wide(text);
        let memory = GlobalAlloc(GMEM_MOVEABLE, text.len() * 2);
        if memory.is_null() {
            return Err("无法分配复制缓冲区".into());
        }
        let pointer = GlobalLock(memory);
        if pointer.is_null() {
            GlobalFree(memory);
            return Err("无法访问复制缓冲区".into());
        }
        std::ptr::copy_nonoverlapping(text.as_ptr(), pointer.cast::<u16>(), text.len());
        GlobalUnlock(memory);
        if OpenClipboard(hwnd) == 0 {
            GlobalFree(memory);
            return Err("剪贴板正被其他程序使用".into());
        }
        let success = EmptyClipboard() != 0 && !SetClipboardData(13, memory).is_null();
        CloseClipboard();
        if !success {
            GlobalFree(memory);
            return Err("复制信息失败".into());
        }
    }
    Ok(())
}
// Caption artwork is independent of the taskbar's light/dark palette.
// Load private handles to avoid LR_SHARED reusing a different-DPI resource.
// The caller owns each installed icon until replacement or window destruction.
unsafe fn set_panel_icons(hwnd: HWND, owned: &mut [HICON; 2]) {
    unsafe {
        let instance = GetModuleHandleW(null());
        let dpi = GetDpiForWindow(hwnd).max(96);
        for (index, (kind, metric)) in [(ICON_SMALL, SM_CXSMICON), (ICON_BIG, SM_CXICON)]
            .into_iter()
            .enumerate()
        {
            let size = GetSystemMetricsForDpi(metric, dpi);
            let icon = LoadImageW(instance, 101usize as *const u16, IMAGE_ICON, size, size, 0);
            if !icon.is_null() {
                SendMessageW(hwnd, WM_SETICON, kind as usize, icon as isize);
                let old = std::mem::replace(&mut owned[index], icon);
                if !old.is_null() {
                    DestroyIcon(old);
                }
            }
        }
    }
}

unsafe fn attach_bridge(hwnd: HWND, bridge: &Rc<UiBridge>) {
    unsafe {
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, Rc::into_raw(bridge.clone()) as isize);
    }
}
unsafe fn callback_bridge(hwnd: HWND) -> Option<Rc<UiBridge>> {
    let pointer = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const UiBridge;
    if pointer.is_null() {
        None
    } else {
        unsafe {
            Rc::increment_strong_count(pointer);
            Some(Rc::from_raw(pointer))
        }
    }
}
unsafe fn release_window_bridge(hwnd: HWND) {
    let pointer = unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0) } as *const UiBridge;
    if !pointer.is_null() {
        unsafe {
            drop(Rc::from_raw(pointer));
        }
    }
}
struct PanelColors {
    paper: u32,
    text: u32,
    secondary: u32,
    fill: u32,
    empty: u32,
    outline: u32,
    high_contrast: bool,
}
fn panel_colors() -> PanelColors {
    // HIGHCONTRASTW is this stable native structure; no extra Win32 feature is needed.
    #[repr(C)]
    struct HighContrast {
        size: u32,
        flags: u32,
        scheme: *mut u16,
    }
    let mut contrast = HighContrast {
        size: std::mem::size_of::<HighContrast>() as u32,
        flags: 0,
        scheme: null_mut(),
    };
    let enabled = unsafe {
        SystemParametersInfoW(
            SPI_GETHIGHCONTRAST,
            contrast.size,
            (&mut contrast as *mut HighContrast).cast(),
            0,
        )
    } != 0
        && contrast.flags & 1 != 0;
    if enabled {
        unsafe {
            PanelColors {
                paper: GetSysColor(COLOR_WINDOW),
                text: GetSysColor(COLOR_WINDOWTEXT),
                secondary: GetSysColor(COLOR_WINDOWTEXT),
                fill: GetSysColor(COLOR_HIGHLIGHT),
                empty: GetSysColor(COLOR_GRAYTEXT),
                outline: GetSysColor(COLOR_WINDOWTEXT),
                high_contrast: true,
            }
        }
    } else {
        PanelColors {
            paper: 0x00f5f8fa,
            text: 0x00292d30,
            secondary: 0x00636b71,
            fill: 0x003e59a9,
            empty: 0x00cbd7df,
            outline: 0x00646e75,
            high_contrast: false,
        }
    }
}

// Keep native BUTTON behavior, keyboard routing and accessibility semantics;
// only its pixels change. Each child owns its original procedure until destroy.
struct StyledControl {
    original: Cell<WNDPROC>,
    checkbox: bool,
    hovered: Cell<bool>,
    redraw_enabled: Cell<bool>,
    native_redraw_suppressed: Cell<bool>,
    #[cfg(test)]
    invalidations: Cell<u32>,
}
unsafe fn style_control(hwnd: HWND, checkbox: bool) {
    unsafe {
        SetWindowTheme(hwnd, wide("").as_ptr(), wide("").as_ptr());
        let state = Rc::new(StyledControl {
            original: Cell::new(None),
            checkbox,
            hovered: Cell::new(false),
            redraw_enabled: Cell::new(true),
            native_redraw_suppressed: Cell::new(false),
            #[cfg(test)]
            invalidations: Cell::new(0),
        });
        let pointer = Rc::into_raw(state);
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, pointer as isize);
        let original = SetWindowLongPtrW(
            hwnd,
            GWLP_WNDPROC,
            styled_control_proc as *const () as isize,
        );
        if original == 0 {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            drop(Rc::from_raw(pointer));
            return;
        }
        (*pointer)
            .original
            .set(std::mem::transmute::<isize, WNDPROC>(original));
    }
}
unsafe extern "system" fn styled_control_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        let pointer = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const StyledControl;
        if pointer.is_null() {
            return DefWindowProcW(hwnd, message, wparam, lparam);
        }
        // Native BUTTON dispatch can synchronously destroy this HWND. Retain
        // its state through callbacks, also preventing allocation-address reuse
        // from making a replacement control appear to be this same instance.
        Rc::increment_strong_count(pointer);
        let state = Rc::from_raw(pointer);
        let original = state.original.get();
        let checkbox = state.checkbox;
        if message == WM_NCDESTROY {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            drop(Rc::from_raw(pointer)); // Release the window's own reference.
            let result = CallWindowProcW(original, hwnd, message, wparam, lparam);
            return result;
        }
        if message == WM_PAINT {
            if state.redraw_enabled.get() && !state.native_redraw_suppressed.get() {
                paint_control(hwnd, checkbox);
            }
            return 0;
        }
        // The completed custom frame paints every client pixel. A native erase
        // between frames exposes the stock BUTTON background during hover.
        if message == WM_ERASEBKGND {
            return 1;
        }
        if message == WM_PRINTCLIENT {
            if wparam != 0 && state.redraw_enabled.get() && !state.native_redraw_suppressed.get() {
                buffer_control(hwnd, checkbox, wparam as HDC);
            }
            return 0;
        }
        if message == WM_SETREDRAW {
            state.redraw_enabled.set(wparam != 0);
            // A nested caller's redraw choice takes effect after our bounded
            // suppression, without enabling native painting in its middle.
            if state.native_redraw_suppressed.get() {
                return 0;
            }
            let result = CallWindowProcW(original, hwnd, message, wparam, lparam);
            if wparam != 0 && GetWindowLongPtrW(hwnd, GWLP_USERDATA) == pointer as isize {
                InvalidateRect(hwnd, null(), 0);
            }
            return result;
        }
        let hover_changed = (message == WM_MOUSEMOVE && !state.hovered.get())
            || (message == WM_MOUSELEAVE && state.hovered.get());
        if message == WM_MOUSEMOVE && hover_changed {
            state.hovered.set(true);
            let mut tracking = windows_sys::Win32::UI::Input::KeyboardAndMouse::TRACKMOUSEEVENT {
                cbSize: std::mem::size_of::<
                    windows_sys::Win32::UI::Input::KeyboardAndMouse::TRACKMOUSEEVENT,
                >() as u32,
                dwFlags: windows_sys::Win32::UI::Input::KeyboardAndMouse::TME_LEAVE,
                hwndTrack: hwnd,
                dwHoverTime: 0,
            };
            windows_sys::Win32::UI::Input::KeyboardAndMouse::TrackMouseEvent(&mut tracking);
        }
        if message == WM_MOUSELEAVE && hover_changed {
            state.hovered.set(false);
        }
        // BUTTON can draw directly while handling these messages, bypassing
        // WM_PAINT. Preserve native state/text/accessibility processing while
        // leaving all resulting pixels to our buffered painter. Do not wrap
        // mouse/key/BM_CLICK dispatch: removing WS_VISIBLE during native focus
        // or capture handling would change input semantics.
        let changes_frame = matches!(
            message,
            WM_SETTEXT
                | WM_UPDATEUISTATE
                | BM_SETSTATE
                | BM_SETCHECK
                | WM_ENABLE
                | WM_SETFOCUS
                | WM_KILLFOCUS
        );
        let suppress = changes_frame
            && state.redraw_enabled.get()
            && !state.native_redraw_suppressed.get()
            && GetWindowLongPtrW(hwnd, GWL_STYLE) as u32 & WS_VISIBLE != 0;
        if suppress {
            state.native_redraw_suppressed.set(true);
            CallWindowProcW(original, hwnd, WM_SETREDRAW, 0, 0);
            if GetWindowLongPtrW(hwnd, GWLP_USERDATA) != pointer as isize {
                return 0;
            }
        }
        let before = CallWindowProcW(original, hwnd, BM_GETSTATE, 0, 0);
        if GetWindowLongPtrW(hwnd, GWLP_USERDATA) != pointer as isize {
            return 0;
        }
        let result = CallWindowProcW(original, hwnd, message, wparam, lparam);
        if GetWindowLongPtrW(hwnd, GWLP_USERDATA) != pointer as isize {
            return result;
        }
        if suppress {
            state.native_redraw_suppressed.set(false);
            if state.redraw_enabled.get() {
                CallWindowProcW(original, hwnd, WM_SETREDRAW, 1, 0);
                if GetWindowLongPtrW(hwnd, GWLP_USERDATA) != pointer as isize {
                    return result;
                }
            }
        }
        let after = CallWindowProcW(original, hwnd, BM_GETSTATE, 0, 0);
        if GetWindowLongPtrW(hwnd, GWLP_USERDATA) != pointer as isize {
            return result;
        }
        if state.redraw_enabled.get() && (hover_changed || before != after || changes_frame) {
            #[cfg(test)]
            {
                state.invalidations.set(state.invalidations.get() + 1);
            }
            InvalidateRect(hwnd, null(), 0);
        }
        result
    }
}
unsafe fn use_panel_coordinates(dc: HDC, hwnd: HWND) {
    unsafe {
        let dpi = GetDpiForWindow(hwnd).max(96) as i32;
        SetMapMode(dc, MM_ANISOTROPIC);
        SetWindowExtEx(dc, 96, 96, null_mut());
        SetViewportExtEx(dc, dpi, dpi, null_mut());
        SetBkMode(dc, TRANSPARENT as i32);
    }
}
unsafe fn focus_cues_visible(hwnd: HWND) -> bool {
    unsafe {
        let root = GetAncestor(hwnd, GA_ROOT);
        let root = if root.is_null() { hwnd } else { root };
        SendMessageW(root, WM_QUERYUISTATE, 0, 0) & UISF_HIDEFOCUS as isize == 0
    }
}
unsafe fn set_focus_cues(hwnd: HWND, visible: bool) {
    unsafe {
        let root = GetAncestor(hwnd, GA_ROOT);
        let root = if root.is_null() { hwnd } else { root };
        if focus_cues_visible(root) == visible {
            return;
        }
        let action = if visible { UIS_CLEAR } else { UIS_SET };
        SendMessageW(
            root,
            WM_CHANGEUISTATE,
            (action as usize) | ((UISF_HIDEFOCUS as usize) << 16),
            0,
        );
        RedrawWindow(root, null(), null_mut(), RDW_INVALIDATE | RDW_ALLCHILDREN);
    }
}
unsafe fn input_focus_cues(panel: HWND, message: &MSG) {
    unsafe {
        if panel.is_null() || (message.hwnd != panel && IsChild(panel, message.hwnd) == 0) {
            return;
        }
        match message.message {
            WM_LBUTTONDOWN | WM_LBUTTONDBLCLK | WM_RBUTTONDOWN => set_focus_cues(panel, false),
            WM_KEYDOWN if matches!(message.wParam, 9 | 13 | 32) => set_focus_cues(panel, true),
            _ => {}
        }
    }
}
unsafe fn panel_text(dc: HDC, text: &str, bounds: RECT, height: i32, weight: i32, format: u32) {
    unsafe {
        let font = CreateFontW(
            -height,
            0,
            0,
            0,
            weight,
            0,
            0,
            0,
            DEFAULT_CHARSET as u32,
            0,
            0,
            CLEARTYPE_QUALITY as u32,
            0,
            wide("Microsoft YaHei UI").as_ptr(),
        );
        let previous = SelectObject(dc, font);
        let mut bounds = bounds;
        DrawTextW(
            dc,
            wide(text).as_ptr(),
            -1,
            &mut bounds,
            format | DT_SINGLELINE,
        );
        SelectObject(dc, previous);
        DeleteObject(font);
    }
}
unsafe fn paint_control(hwnd: HWND, checkbox: bool) {
    unsafe {
        let mut paint: PAINTSTRUCT = std::mem::zeroed();
        let dc = BeginPaint(hwnd, &mut paint);
        buffer_control(hwnd, checkbox, dc);
        EndPaint(hwnd, &paint);
    }
}
unsafe fn buffer_control(hwnd: HWND, checkbox: bool, target: HDC) {
    unsafe {
        let mut bounds: RECT = std::mem::zeroed();
        GetClientRect(hwnd, &mut bounds);
        if bounds.right <= 0 || bounds.bottom <= 0 {
            return;
        }
        let memory = CreateCompatibleDC(target);
        if memory.is_null() {
            draw_control(hwnd, checkbox, target);
            return;
        }
        let bitmap = CreateCompatibleBitmap(target, bounds.right, bounds.bottom);
        if bitmap.is_null() {
            DeleteDC(memory);
            draw_control(hwnd, checkbox, target);
            return;
        }
        let previous = SelectObject(memory, bitmap);
        draw_control(hwnd, checkbox, memory);
        let saved = SaveDC(target);
        SetMapMode(target, MM_TEXT);
        BitBlt(
            target,
            0,
            0,
            bounds.right,
            bounds.bottom,
            memory,
            0,
            0,
            SRCCOPY,
        );
        RestoreDC(target, saved);
        SelectObject(memory, previous);
        DeleteObject(bitmap);
        DeleteDC(memory);
    }
}
unsafe fn draw_control(hwnd: HWND, checkbox: bool, dc: HDC) {
    unsafe {
        draw_control_colors(hwnd, checkbox, dc, &panel_colors());
    }
}
unsafe fn draw_control_colors(hwnd: HWND, checkbox: bool, dc: HDC, colors: &PanelColors) {
    unsafe {
        let saved = SaveDC(dc);
        SetMapMode(dc, MM_TEXT);
        let mut bounds: RECT = std::mem::zeroed();
        GetClientRect(hwnd, &mut bounds);
        let paper = CreateSolidBrush(colors.paper);
        FillRect(dc, &bounds, paper);
        use_panel_coordinates(dc, hwnd);
        let dpi = GetDpiForWindow(hwnd).max(96) as i32;
        bounds.right = bounds.right * 96 / dpi;
        bounds.bottom = bounds.bottom * 96 / dpi;
        let state = SendMessageW(hwnd, BM_GETSTATE, 0, 0);
        let pushed = state & BST_PUSHED as isize != 0;
        let focused = state & BST_FOCUS as isize != 0;
        let enabled = IsWindowEnabled(hwnd) != 0;
        SetTextColor(
            dc,
            if enabled {
                colors.text
            } else {
                colors.secondary
            },
        );
        let fill = CreateSolidBrush(if pushed { colors.empty } else { colors.paper });
        let border = CreatePen(PS_SOLID, 1, colors.outline);
        let old_pen = SelectObject(dc, border);
        let old_brush = SelectObject(dc, fill);
        SelectObject(dc, GetStockObject(DC_PEN));
        SelectObject(dc, GetStockObject(DC_BRUSH));
        let label = bounds;
        let control_id = GetDlgCtrlID(hwnd) as u32;
        let mut text = [0u16; 512];
        let len = GetWindowTextW(hwnd, text.as_mut_ptr(), 512);
        let text = String::from_utf16_lossy(&text[..len as usize]);
        let control_state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const StyledControl;
        let hovered = enabled && !control_state.is_null() && (*control_state).hovered.get();
        if checkbox {
            let checked = SendMessageW(hwnd, BM_GETCHECK, 0, 0) == BST_CHECKED as isize;
            let x = if bounds.right > 100 {
                bounds.right - 38
            } else {
                (bounds.right - 38) / 2
            };
            let y = (bounds.bottom - 22) / 2;
            SetDCPenColor(
                dc,
                if checked && enabled {
                    colors.fill
                } else {
                    colors.outline
                },
            );
            SetDCBrushColor(
                dc,
                if checked && enabled {
                    colors.fill
                } else {
                    colors.outline
                },
            );
            RoundRect(dc, x, y, x + 38, y + 22, 22, 22);
            let knob_color = if checked && enabled && colors.high_contrast {
                GetSysColor(COLOR_HIGHLIGHTTEXT)
            } else {
                colors.paper
            };
            SetDCPenColor(dc, knob_color);
            SetDCBrushColor(dc, knob_color);
            let knob = if checked { x + 20 } else { x + 3 };
            Ellipse(dc, knob, y + 3, knob + 16, y + 19);
            if bounds.right > 100 {
                SetTextColor(
                    dc,
                    if enabled {
                        colors.text
                    } else {
                        colors.secondary
                    },
                );
                panel_text(
                    dc,
                    &text,
                    RECT {
                        left: 0,
                        top: 0,
                        right: x - 12,
                        bottom: bounds.bottom,
                    },
                    15,
                    400,
                    DT_LEFT | DT_VCENTER,
                );
            }
        } else {
            let primary = control_id >= ADD_BASE && text != "复制信息";
            if primary || hovered || pushed {
                SetDCPenColor(dc, if primary { colors.fill } else { colors.empty });
                SetDCBrushColor(
                    dc,
                    if primary && enabled {
                        colors.fill
                    } else {
                        colors.empty
                    },
                );
                RoundRect(dc, 1, 1, bounds.right - 1, bounds.bottom - 1, 7, 7);
            }
            SetTextColor(
                dc,
                if primary && enabled {
                    if colors.high_contrast {
                        GetSysColor(COLOR_HIGHLIGHTTEXT)
                    } else {
                        colors.paper
                    }
                } else if control_id == ADD {
                    colors.fill
                } else {
                    colors.secondary
                },
            );
            let visible = if control_id == BACK {
                "←"
            } else if (SETTINGS_BASE..ADD_BASE).contains(&control_id)
                && (control_id - SETTINGS_BASE) % 3 == 2
            {
                "移除"
            } else {
                &text
            };
            panel_text(
                dc,
                visible,
                label,
                if control_id == BACK { 22 } else { 15 },
                400,
                DT_VCENTER | DT_CENTER,
            );
        }
        if focused && focus_cues_visible(hwnd) {
            let focus = CreatePen(PS_SOLID, 2, colors.fill);
            let prior = SelectObject(dc, focus);
            let prior_brush = SelectObject(dc, GetStockObject(NULL_BRUSH));
            RoundRect(dc, 2, 2, bounds.right - 2, bounds.bottom - 2, 6, 6);
            SelectObject(dc, prior_brush);
            SelectObject(dc, prior);
            DeleteObject(focus);
        }
        SelectObject(dc, old_pen);
        SelectObject(dc, old_brush);
        DeleteObject(border);
        DeleteObject(fill);
        DeleteObject(paper);
        RestoreDC(dc, saved);
    }
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let Some(bridge) = (unsafe { callback_bridge(hwnd) }) else {
        return unsafe { DefWindowProcW(hwnd, message, wparam, lparam) };
    };
    if message == crate::status_ipc::REQUEST {
        let client = lparam as HWND;
        if crate::status_ipc::valid_client(client, wparam as u32) {
            let value = bridge
                .status
                .borrow()
                .json(bridge.menu_timing.get(), bridge.notification_events.get());
            crate::status_ipc::respond(client, hwnd, &value);
        }
        return 0;
    }
    if message == EVENTS_READY {
        return 0;
    }
    if bridge.menu_active.get() {
        let mut timing = bridge.menu_timing.get();
        if let Some(requested) = timing.requested {
            let elapsed = || requested.elapsed().as_micros().min(u64::MAX as u128) as u64;
            if message == WM_ENTERMENULOOP && timing.enter_loop_us.is_none() {
                timing.enter_loop_us = Some(elapsed());
            }
            if message == WM_ENTERIDLE
                && wparam == MSGF_MENU as usize
                && timing.visible_idle_us.is_none()
                && !(lparam as HWND).is_null()
                && unsafe { IsWindowVisible(lparam as HWND) } != 0
            {
                timing.visible_idle_us = Some(elapsed());
            }
        }
        bridge.menu_timing.set(timing);
        if message == WM_CLOSE || (message == CONTROL && wparam == QUIT) {
            unsafe {
                EndMenu();
            }
        }
    }
    if message == bridge.taskbar_created {
        bridge.queue(UiEvent::TaskbarCreated);
        return 0;
    }
    let event = match message {
        CONTROL if wparam == SHOW_MENU => UiEvent::Menu(Instant::now(), 0),
        CONTROL => UiEvent::Control(wparam),
        RESULT => UiEvent::Result,
        WM_TIMER if wparam == 2 => UiEvent::Heartbeat,
        WM_TIMER => UiEvent::Discover,
        WM_DEVICECHANGE => UiEvent::Discover,
        WM_INPUT => {
            bridge.activity(Instant::now());
            return unsafe { DefWindowProcW(hwnd, message, wparam, lparam) };
        }
        WM_POWERBROADCAST if wparam == 7 || wparam == 18 => UiEvent::Refresh,
        TRAY if [NIN_BALLOONSHOW, NIN_BALLOONHIDE, NIN_BALLOONTIMEOUT]
            .contains(&(lparam as u32)) =>
        {
            let mut counts = bridge.notification_events.get();
            let index = if lparam as u32 == NIN_BALLOONSHOW {
                0
            } else if lparam as u32 == NIN_BALLOONHIDE {
                1
            } else {
                2
            };
            counts[index] += 1;
            bridge.notification_events.set(counts);
            UiEvent::Heartbeat
        }
        TRAY if lparam as u32 == WM_RBUTTONUP || lparam as u32 == WM_CONTEXTMENU => {
            UiEvent::Menu(Instant::now(), wparam as u32)
        }
        TRAY if lparam as u32 == WM_LBUTTONUP => UiEvent::ShowPanel,
        WM_CLOSE => UiEvent::Control(QUIT),
        WM_DESTROY => UiEvent::MainDestroyed,
        WM_SETTINGCHANGE | WM_THEMECHANGED | WM_DISPLAYCHANGE => UiEvent::SettingsChanged,
        WM_NCDESTROY => {
            unsafe {
                release_window_bridge(hwnd);
            }
            return unsafe { DefWindowProcW(hwnd, message, wparam, lparam) };
        }
        _ => return unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    };
    bridge.queue(event);
    0
}
unsafe extern "system" fn panel_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let Some(bridge) = (unsafe { callback_bridge(hwnd) }) else {
        return unsafe { DefWindowProcW(hwnd, message, wparam, lparam) };
    };
    let event = match message {
        WM_LBUTTONDOWN => {
            let (page, scroll) = {
                let view = bridge.view.borrow();
                (view.page, view.scroll)
            };
            let dpi = unsafe { GetDpiForWindow(hwnd).max(96) as i32 };
            let x = (lparam as i16) as i32;
            let y = ((lparam >> 16) as i16) as i32;
            if page == Page::Settings
                && x >= 464 * dpi / 96
                && x < 520 * dpi / 96
                && y >= (168 - scroll) * dpi / 96
                && y < (200 - scroll) * dpi / 96
            {
                // Keep the original full-height click target around the
                // shorter text field; never focus a disabled threshold.
                unsafe {
                    let edit = GetDlgItem(hwnd, THRESHOLD as i32);
                    if IsWindowEnabled(edit) != 0 {
                        windows_sys::Win32::UI::Input::KeyboardAndMouse::SetFocus(edit);
                    }
                }
                return 0;
            }
            return unsafe { DefWindowProcW(hwnd, message, wparam, lparam) };
        }
        WM_COMMAND if (wparam >> 16) as u32 == BN_CLICKED => match (wparam & 0xffff) as u32 {
            ADD => UiEvent::Control(ADD as usize),
            SETTINGS => UiEvent::Settings,
            id if id >= SETTINGS_BASE => {
                let view = bridge.view.borrow();
                let (index, action) = if id >= ADD_BASE {
                    ((id - ADD_BASE) as usize, 0)
                } else {
                    (
                        ((id - SETTINGS_BASE) / 3) as usize,
                        (id - SETTINGS_BASE) % 3,
                    )
                };
                let Some(row) = view.rows.get(index) else {
                    return 0;
                };
                UiEvent::DeviceCommand(view.page, action, row.descriptor.id.clone())
            }
            BACK | STARTUP | ALERTS | PICK_REFRESH => {
                UiEvent::PageCommand((wparam & 0xffff) as u32)
            }
            id if id >= ROW_BASE && (id - ROW_BASE) % 3 == 2 => {
                let view = bridge.view.borrow();
                let Some(row) = view.rows.get(((id - ROW_BASE) / 3) as usize) else {
                    return 0;
                };
                UiEvent::RowRefresh(row.descriptor.id.clone())
            }
            _ => return unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
        },
        WM_COMMAND
            if (wparam & 0xffff) as u32 == THRESHOLD && (wparam >> 16) as u32 == EN_SETFOCUS =>
        {
            unsafe {
                InvalidateRect(hwnd, null(), 1);
            }
            return 0;
        }
        WM_COMMAND
            if (wparam & 0xffff) as u32 == THRESHOLD && (wparam >> 16) as u32 == EN_KILLFOCUS =>
        {
            UiEvent::Threshold(false)
        }
        WM_COMMAND
            if (wparam & 0xffff) as u32 == THRESHOLD && (wparam >> 16) as u32 == EN_CHANGE =>
        {
            unsafe {
                SendMessageW(lparam as HWND, EM_HIDEBALLOONTIP, 0, 0);
                InvalidateRect(hwnd, null(), 1);
            }
            UiEvent::ThresholdEdited
        }
        WM_VSCROLL => UiEvent::Scroll(match (wparam & 0xffff) as i32 {
            SB_LINEUP => -64,
            SB_LINEDOWN => 64,
            SB_PAGEUP => -192,
            SB_PAGEDOWN => 192,
            SB_THUMBTRACK | SB_THUMBPOSITION => {
                ((wparam >> 16) as i32 / 64) * 64 - bridge.view.borrow().scroll
            }
            _ => 0,
        }),
        WM_MOUSEWHEEL => UiEvent::Scroll(if ((wparam >> 16) as i16) > 0 { -64 } else { 64 }),
        WM_CLOSE => UiEvent::ClosePanel,
        WM_DESTROY => UiEvent::PanelDestroyed(hwnd),
        WM_ACTIVATE if (wparam & 0xffff) != WA_INACTIVE as usize => UiEvent::ActivatePanel,
        WM_PAINT => {
            let view = bridge.view.borrow().clone();
            unsafe {
                paint_panel(hwnd, &view);
            }
            return 0;
        }
        WM_PRINTCLIENT => {
            let view = bridge.view.borrow().clone();
            if wparam != 0 {
                unsafe {
                    draw_panel(wparam as HDC, hwnd, &view);
                }
            }
            return 0;
        }
        WM_DPICHANGED => UiEvent::DpiChanged(hwnd, unsafe { *(lparam as *const RECT) }),
        WM_SETTINGCHANGE | WM_THEMECHANGED => UiEvent::SettingsChanged,
        WM_CTLCOLORBTN | WM_CTLCOLORSTATIC | WM_CTLCOLOREDIT => {
            let colors = panel_colors();
            unsafe {
                SetBkMode(wparam as HDC, TRANSPARENT as i32);
                SetTextColor(wparam as HDC, colors.text);
                SetBkColor(wparam as HDC, colors.paper);
                SetDCBrushColor(wparam as HDC, colors.paper);
                return GetStockObject(DC_BRUSH) as isize;
            }
        }
        WM_NCDESTROY => {
            unsafe {
                release_window_bridge(hwnd);
            }
            return unsafe { DefWindowProcW(hwnd, message, wparam, lparam) };
        }
        _ => return unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    };
    bridge.queue(event);
    0
}
fn cell_fill(percent: Option<u8>, cell: i32) -> i32 {
    percent.map_or(0, |p| (i32::from(p) - cell * 10).clamp(0, 10))
}
unsafe fn draw_battery(dc: HDC, y: i32, reading: Option<&Reading>, colors: &PanelColors) {
    unsafe {
        SetDCPenColor(dc, colors.outline);
        SetDCBrushColor(dc, colors.paper);
        RoundRect(dc, 210, y + 16, 337, y + 40, 3, 3);
        SetDCBrushColor(dc, colors.outline);
        Rectangle(dc, 339, y + 23, 342, y + 33);
        if let Some(reading) = reading {
            for cell in 0..10 {
                let x = 214 + cell * 12;
                SetDCBrushColor(dc, colors.empty);
                SetDCPenColor(dc, colors.empty);
                Rectangle(dc, x, y + 20, x + 10, y + 36);
                let fill = cell_fill(Some(reading.normalized_percent()), cell);
                if fill > 0 {
                    SetDCBrushColor(dc, colors.fill);
                    SetDCPenColor(dc, colors.fill);
                    Rectangle(dc, x, y + 20, x + fill, y + 36);
                }
            }
        } else {
            SetTextColor(dc, colors.secondary);
            panel_text(
                dc,
                "?",
                RECT {
                    left: 210,
                    top: y + 14,
                    right: 337,
                    bottom: y + 42,
                },
                19,
                700,
                DT_CENTER | DT_VCENTER,
            );
        }
        if reading.is_some_and(|r| r.power_state == crate::device::PowerState::Charging) {
            let points = [
                POINT { x: 354, y: y + 16 },
                POINT { x: 347, y: y + 29 },
                POINT { x: 353, y: y + 29 },
                POINT { x: 350, y: y + 40 },
                POINT { x: 360, y: y + 24 },
                POINT { x: 354, y: y + 24 },
            ];
            SetDCBrushColor(dc, colors.fill);
            SetDCPenColor(dc, colors.fill);
            Polygon(dc, points.as_ptr(), 6);
        }
    }
}
unsafe fn paint_panel(hwnd: HWND, view: &PanelView) {
    unsafe {
        let mut paint: PAINTSTRUCT = std::mem::zeroed();
        let dc = BeginPaint(hwnd, &mut paint);
        draw_panel(dc, hwnd, view);
        EndPaint(hwnd, &paint);
    }
}
unsafe fn draw_panel(dc: HDC, hwnd: HWND, view: &PanelView) {
    unsafe {
        let saved = SaveDC(dc);
        let mut client: RECT = std::mem::zeroed();
        GetClientRect(hwnd, &mut client);
        let colors = panel_colors();
        let paper = CreateSolidBrush(colors.paper);
        FillRect(dc, &client, paper);
        use_panel_coordinates(dc, hwnd);
        let old_brush = SelectObject(dc, GetStockObject(DC_BRUSH));
        let old_pen = SelectObject(dc, GetStockObject(DC_PEN));
        let dpi = GetDpiForWindow(hwnd).max(96) as i32;
        let height = client.bottom * 96 / dpi;
        if view.page != Page::Main {
            SetTextColor(dc, colors.text);
            panel_text(
                dc,
                if view.page == Page::Settings {
                    "设置"
                } else {
                    "添加设备"
                },
                RECT {
                    left: 52,
                    top: 12,
                    right: 360,
                    bottom: 50,
                },
                20,
                700,
                DT_LEFT | DT_VCENTER,
            );
        }
        if view.page == Page::Settings {
            let clip = SaveDC(dc);
            IntersectClipRect(dc, 0, 64, 560, height - 24);
            let offset = view.scroll;
            let edit = GetDlgItem(hwnd, THRESHOLD as i32);
            let focused = windows_sys::Win32::UI::Input::KeyboardAndMouse::GetFocus() == edit;
            SetDCBrushColor(dc, colors.paper);
            SetDCPenColor(
                dc,
                if (focused && focus_cues_visible(edit)) || view.threshold_error {
                    colors.fill
                } else {
                    colors.outline
                },
            );
            SelectObject(dc, GetStockObject(NULL_BRUSH));
            RoundRect(dc, 462, 167 - offset, 538, 201 - offset, 5, 5);
            SelectObject(dc, GetStockObject(DC_BRUSH));
            if !view.rows.is_empty() {
                SetDCPenColor(dc, colors.empty);
                MoveToEx(dc, 24, 218 - offset, null_mut());
                LineTo(dc, 536, 218 - offset);
                SetTextColor(dc, colors.secondary);
                for (text, left, right) in
                    [("设备", 24, 300), ("托盘", 334, 380), ("提醒", 410, 458)]
                {
                    panel_text(
                        dc,
                        text,
                        RECT {
                            left,
                            top: 228 - offset,
                            right,
                            bottom: 254 - offset,
                        },
                        13,
                        400,
                        DT_LEFT | DT_VCENTER,
                    );
                }
            }
            RestoreDC(dc, clip);
        }
        if view.page == Page::Add && view.rows.is_empty() {
            SetTextColor(dc, colors.secondary);
            panel_text(
                dc,
                "未发现可添加设备",
                RECT {
                    left: 24,
                    top: 82,
                    right: 536,
                    bottom: 138,
                },
                16,
                400,
                DT_CENTER | DT_VCENTER,
            );
        }
        let top = match view.page {
            Page::Main => 20,
            Page::Settings => 264,
            Page::Add => 76,
        };
        let footer = if view.page == Page::Main { 48 } else { 24 };
        for (index, state) in view.rows.iter().enumerate() {
            let y = top + index as i32 * 64 - view.scroll;
            if y < if view.page == Page::Main { 20 } else { 64 } || y + 56 > height - footer {
                continue;
            }
            if let Some(icon) = view.symbols.get(index) {
                let saved = SaveDC(dc);
                SetMapMode(dc, MM_TEXT);
                DrawIconEx(
                    dc,
                    24 * dpi / 96,
                    (y + 13) * dpi / 96,
                    icon.get(),
                    28 * dpi / 96,
                    28 * dpi / 96,
                    0,
                    null_mut(),
                    DI_NORMAL,
                );
                RestoreDC(dc, saved);
            }
            if view.page == Page::Main {
                draw_battery(dc, y, state.reading.as_ref(), &colors);
            }
            if view.page != Page::Settings && index + 1 < view.rows.len() {
                SetDCPenColor(dc, colors.empty);
                MoveToEx(dc, 24, y + 63, null_mut());
                LineTo(dc, 536, y + 63);
            }
        }
        if view.page == Page::Main && !view.rows.is_empty() {
            SetDCPenColor(dc, colors.empty);
            MoveToEx(dc, 24, height - 48, null_mut());
            LineTo(dc, 536, height - 48);
        }
        SelectObject(dc, old_brush);
        SelectObject(dc, old_pen);
        DeleteObject(paper);
        RestoreDC(dc, saved);
    }
}
pub fn run(mode: crate::launch::Mode) -> Result<(), String> {
    unsafe {
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let mutex = CreateMutexW(
            null(),
            1,
            wide("Local\\QuietBattery.G304.Tray.Mutex").as_ptr(),
        );
        if mutex.is_null() {
            return Err("无法创建单实例互斥锁".into());
        }
        if GetLastError() == ERROR_ALREADY_EXISTS {
            CloseHandle(mutex);
            for _ in 0..20 {
                let hwnd = find_window();
                if !hwnd.is_null() {
                    if PostMessageW(hwnd, CONTROL, mode.existing_command(), 0) == 0 {
                        return Err("无法通知已有实例打开窗口".into());
                    }
                    return Ok(());
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            return Err("已有实例正在启动".into());
        }
        let result = run_owned(mode);
        ReleaseMutex(mutex);
        CloseHandle(mutex);
        result
    }
}
unsafe fn run_owned(mode: crate::launch::Mode) -> Result<(), String> {
    // Keep the STA alive until after every native window and the message loop.
    // Last CoUninitialize may discard non-COM messages during its cleanup pump.
    let _com = startup::Com::init()?;
    crate::storage::initialize()?;
    if let Err(error) = startup::upgrade_background() {
        unsafe {
            MessageBoxW(
                null_mut(),
                wide(&format!(
                    "{error}\n\n软件将继续启动。请在设置中重新设置登录启动。"
                ))
                .as_ptr(),
                wide("登录启动设置更新失败").as_ptr(),
                MB_OK | MB_ICONWARNING,
            );
        }
    }
    unsafe {
        let instance = GetModuleHandleW(null());
        let init = INITCOMMONCONTROLSEX {
            dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_STANDARD_CLASSES,
        };
        InitCommonControlsEx(&init);
        for (name, proc) in [
            (
                CLASS,
                window_proc as unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT,
            ),
            (PANEL_CLASS, panel_proc),
        ] {
            let class_name = wide(name);
            let class = WNDCLASSW {
                lpfnWndProc: Some(proc),
                hInstance: instance,
                lpszClassName: class_name.as_ptr(),
                hCursor: LoadCursorW(null_mut(), IDC_ARROW),
                ..std::mem::zeroed()
            };
            if RegisterClassW(&class) == 0 {
                return Err("无法注册原生窗口".into());
            }
        }
        let hwnd = CreateWindowExW(
            0,
            wide(CLASS).as_ptr(),
            wide("Quiet Battery").as_ptr(),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            null_mut(),
            null_mut(),
            instance,
            null(),
        );
        if hwnd.is_null() {
            return Err("无法创建托盘窗口".into());
        }
        let (sender, requests) = mpsc::channel();
        let (results, receiver) = mpsc::channel();
        let target = hwnd as usize;
        let stop = Cancellation::default();
        let worker_stop = stop.clone();
        let worker = std::thread::Builder::new()
            .name("battery-reader".into())
            .spawn(move || {
                let _runtime = Registry::runtime();
                let event_results = results.clone();
                let event_stop = worker_stop.clone();
                Registry::set_change_handler(std::sync::Arc::new(move |id, value| {
                    if event_stop.check().is_ok()
                        && event_results.send(WorkerResult::Reading(id, value)).is_ok()
                    {
                        PostMessageW(target as HWND, RESULT, 0, 0);
                    }
                }));
                let refresh_results = results.clone();
                let refresh_stop = worker_stop.clone();
                Registry::set_refresh_handler(std::sync::Arc::new(move |id| {
                    if refresh_stop.check().is_ok()
                        && refresh_results
                            .send(WorkerResult::RefreshRequest(id))
                            .is_ok()
                    {
                        PostMessageW(target as HWND, RESULT, 0, 0);
                    }
                }));
                while let Ok(command) = requests.recv() {
                    if worker_stop.check().is_err() {
                        break;
                    }
                    match command {
                        WorkerCommand::Stop => break,
                        WorkerCommand::Discover(simulated) => {
                            let report = if simulated {
                                Err(simulated_error())
                            } else {
                                Registry::discover_cancelled(&worker_stop)
                            };
                            let _ = results.send(WorkerResult::Discovery(report));
                        }
                        WorkerCommand::Read(ids, simulated) => {
                            for id in ids {
                                if worker_stop.check().is_err() {
                                    break;
                                }
                                let result = if simulated {
                                    Err(simulated_error())
                                } else {
                                    Registry::read_cancelled(&id, &worker_stop)
                                };
                                if results.send(WorkerResult::Reading(id, result)).is_err() {
                                    return;
                                }
                                PostMessageW(target as HWND, RESULT, 0, 0);
                            }
                        }
                    }
                    if worker_stop.check().is_err() {
                        break;
                    }
                    if results.send(WorkerResult::Complete).is_err() {
                        break;
                    }
                    PostMessageW(target as HWND, RESULT, 0, 0);
                }
                Registry::shutdown();
            })
            .map_err(|e| e.to_string())?;
        let bridge = Rc::new(UiBridge {
            events: RefCell::new(VecDeque::new()),
            view: RefCell::new(Rc::new(PanelView::default())),
            taskbar_created: RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()),
            wake_window: Cell::new(hwnd),
            menu_active: Cell::new(false),
            menu_timing: Cell::new(MenuTiming::default()),
            activity_queued: Cell::new(false),
            activity_last: Cell::new(Instant::now() - std::time::Duration::from_secs(10)),
            activity_needed: Cell::new(true),
            activity_seen: Cell::new(Instant::now() - std::time::Duration::from_secs(30)),
            notification_events: Cell::new([0; 3]),
            status: RefCell::new(StatusView::default()),
        });
        attach_bridge(hwnd, &bridge);
        let mut app = App {
            #[cfg(test)]
            effects: TestEffects::default(),
            hwnd,
            panel: null_mut(),
            picker: null_mut(),
            manager: Manager::load(&manager::state_path()),
            trays: Vec::new(),
            fallback: None,
            rows: Vec::new(),
            sender,
            receiver,
            worker: Some(worker),
            stop,
            busy: false,
            pending: false,
            pending_discovery: false,
            simulated: false,
            refresh_count: 0,
            success_count: 0,
            bridge: bridge.clone(),
            panel_fonts: [null_mut(); 3],
            panel_icons: [null_mut(); 2],
            last_activity_refresh: Instant::now() - std::time::Duration::from_secs(10),
            picker_ids: Vec::new(),
            page: Page::Main,
            page_scroll: [0; 3],
            page_controls: Vec::new(),
            threshold_error: false,
            notification_submissions: 0,
            notification_last_accepted: false,
            scroll: 0,
        };
        let raw = [
            RAWINPUTDEVICE {
                usUsagePage: 1,
                usUsage: 2,
                dwFlags: RIDEV_INPUTSINK,
                hwndTarget: hwnd,
            },
            RAWINPUTDEVICE {
                usUsagePage: 1,
                usUsage: 6,
                dwFlags: RIDEV_INPUTSINK,
                hwndTarget: hwnd,
            },
        ];
        RegisterRawInputDevices(
            raw.as_ptr(),
            2,
            std::mem::size_of::<RAWINPUTDEVICE>() as u32,
        );
        if SetTimer(hwnd, 1, 120_000, None) == 0 || SetTimer(hwnd, 2, 30_000, None) == 0 {
            app.quit();
            return Err("无法启动刷新定时器".into());
        }
        app.update_icons(false);
        app.discover();
        if mode.show_window() {
            app.show_panel();
        }
        let mut message: MSG = std::mem::zeroed();
        while GetMessageW(&mut message, null_mut(), 0, 0) > 0 {
            input_focus_cues(app.panel, &message);
            if message.message == WM_KEYDOWN && message.wParam == 27 {
                if !app.picker.is_null()
                    && (message.hwnd == app.picker || IsChild(app.picker, message.hwnd) != 0)
                {
                    app.close_picker();
                    continue;
                }
                if !app.panel.is_null()
                    && (message.hwnd == app.panel || IsChild(app.panel, message.hwnd) != 0)
                {
                    if app.page == Page::Main {
                        app.close_panel();
                    } else {
                        app.switch_page(Page::Main);
                    }
                    continue;
                }
            }
            if message.message == WM_KEYDOWN
                && message.wParam == 9
                && !app.panel.is_null()
                && (message.hwnd == app.panel || IsChild(app.panel, message.hwnd) != 0)
            {
                app.tab_focus(windows_sys::Win32::UI::Input::KeyboardAndMouse::GetKeyState(16) < 0);
                app.process_events();
                continue;
            }
            if message.message == WM_KEYDOWN
                && message.wParam == 13
                && !app.panel.is_null()
                && GetDlgCtrlID(message.hwnd) as u32 == THRESHOLD
            {
                app.submit_threshold(true);
                app.process_events();
                continue;
            }
            if message.message == WM_KEYDOWN
                && message.wParam == 13
                && !app.panel.is_null()
                && IsChild(app.panel, message.hwnd) != 0
            {
                let mut class = [0u16; 16];
                let len = GetClassNameW(message.hwnd, class.as_mut_ptr(), 16);
                if String::from_utf16_lossy(&class[..len as usize]).eq_ignore_ascii_case("BUTTON") {
                    SendMessageW(message.hwnd, BM_CLICK, 0, 0);
                    app.process_events();
                    // BM_CLICK emulates mouse activation internally; retain
                    // the keyboard cue after an Enter-triggered page action.
                    if !app.panel.is_null() {
                        set_focus_cues(app.panel, true);
                    }
                    continue;
                }
            }
            if (app.picker.is_null() || IsDialogMessageW(app.picker, &message) == 0)
                && (app.panel.is_null() || IsDialogMessageW(app.panel, &message) == 0)
            {
                TranslateMessage(&message);
                DispatchMessageW(&message);
            }
            app.process_events();
        }
        if IsWindow(hwnd) != 0 {
            app.quit();
            app.process_events();
        }
        app.release_panel_resources();
        let _ = app.sender.send(WorkerCommand::Stop);
        if let Some(worker) = app.worker.take() {
            let _ = worker.join();
        }
        for name in [CLASS, PANEL_CLASS] {
            UnregisterClassW(wide(name).as_ptr(), instance);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn supported_descriptor_json_looks_up_both_cached_collections_by_physical_identity() {
        let candidate = crate::device::Registry::diagnostic_fixture();
        assert_ne!(candidate.descriptor.id, candidate.descriptor.physical_id);
        let cached =
            crate::device::Registry::diagnostic_information(&candidate.descriptor).unwrap();
        assert_eq!(cached.matches("\"endpoint\":").count(), 2);
        let json = super::descriptor_json(&candidate);
        assert!(json.contains("\"id\":\"fixture-protocol:slot-7\""));
        assert!(json.contains("\"supported\":true"));
        assert!(json.contains("\"diagnostics\":{\"mode\":\"cached-hid-metadata\""));
        assert!(json.contains("fixture-input"));
        assert!(json.contains("fixture-vendor"));
        assert_eq!(json.matches("\"endpoint\":").count(), 2);
    }
    use super::*;
    #[test]
    fn native_focus_cues_follow_mouse_keyboard_without_moving_focus() {
        unsafe {
            let parent = CreateWindowExW(
                0,
                wide("STATIC").as_ptr(),
                null(),
                0,
                0,
                0,
                560,
                300,
                null_mut(),
                null_mut(),
                GetModuleHandleW(null()),
                null(),
            );
            assert!(!parent.is_null());
            let button = create_child(parent, "BUTTON", "返回", WS_TABSTOP, 101);
            let edit = create_child(parent, "EDIT", "20", WS_TABSTOP, THRESHOLD);
            let foreign = CreateWindowExW(
                0,
                wide("STATIC").as_ptr(),
                null(),
                0,
                0,
                0,
                50,
                50,
                null_mut(),
                null_mut(),
                GetModuleHandleW(null()),
                null(),
            );
            let focused = windows_sys::Win32::UI::Input::KeyboardAndMouse::GetFocus();
            for control in [button, edit] {
                set_focus_cues(parent, true);
                input_focus_cues(
                    parent,
                    &MSG {
                        hwnd: control,
                        message: WM_LBUTTONDOWN,
                        ..Default::default()
                    },
                );
                assert!(!focus_cues_visible(parent));
                assert_ne!(
                    SendMessageW(parent, WM_QUERYUISTATE, 0, 0) & UISF_HIDEFOCUS as isize,
                    0
                );
                input_focus_cues(
                    parent,
                    &MSG {
                        hwnd: control,
                        message: WM_MOUSEMOVE,
                        ..Default::default()
                    },
                );
                assert!(!focus_cues_visible(control));
                for key in [9, 13, 32] {
                    input_focus_cues(
                        parent,
                        &MSG {
                            hwnd: control,
                            message: WM_KEYDOWN,
                            wParam: key,
                            ..Default::default()
                        },
                    );
                    assert!(focus_cues_visible(control));
                    set_focus_cues(parent, false);
                }
            }
            input_focus_cues(
                parent,
                &MSG {
                    hwnd: foreign,
                    message: WM_KEYDOWN,
                    wParam: 9,
                    ..Default::default()
                },
            );
            assert!(!focus_cues_visible(parent));
            assert_eq!(
                windows_sys::Win32::UI::Input::KeyboardAndMouse::GetFocus(),
                focused,
                "changing focus cues must not move keyboard focus"
            );
            DestroyWindow(foreign);
            DestroyWindow(parent);
        }
    }
    #[test]
    fn styled_pushbuttons_do_not_request_native_drawing_during_label_and_state_updates() {
        struct NativePaintTap {
            original: WNDPROC,
            requests: usize,
            invalid_dc: bool,
        }
        unsafe extern "system" fn parent_proc(
            hwnd: HWND,
            message: u32,
            wparam: WPARAM,
            lparam: LPARAM,
        ) -> LRESULT {
            unsafe {
                let tap = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut NativePaintTap;
                if message == WM_CTLCOLORBTN {
                    // A request from the original BUTTON's direct paint path,
                    // independently of our intercepted WM_PAINT/PRINTCLIENT.
                    (*tap).invalid_dc |= GetObjectType(wparam as HGDIOBJ) == 0;
                    (*tap).requests += 1;
                }
                CallWindowProcW((*tap).original, hwnd, message, wparam, lparam)
            }
        }
        unsafe {
            let foreground = GetForegroundWindow();
            let focused = windows_sys::Win32::UI::Input::KeyboardAndMouse::GetFocus();
            let parent = CreateWindowExW(
                WS_EX_NOACTIVATE,
                wide("STATIC").as_ptr(),
                null(),
                0,
                0,
                0,
                300,
                120,
                null_mut(),
                null_mut(),
                GetModuleHandleW(null()),
                null(),
            );
            assert!(!parent.is_null());
            assert_eq!(IsWindowVisible(parent), 0);
            // Establish an empty region before WS_VISIBLE is ever set: native
            // visibility branches execute without rendering desktop pixels.
            let region = CreateRectRgn(0, 0, 0, 0);
            assert!(!region.is_null());
            if SetWindowRgn(parent, region, 0) == 0 {
                DeleteObject(region);
                DestroyWindow(parent);
                panic!("empty test region could not be established");
            }
            let mut tap = NativePaintTap {
                original: None,
                requests: 0,
                invalid_dc: false,
            };
            SetWindowLongPtrW(parent, GWLP_USERDATA, &mut tap as *mut _ as isize);
            struct OwnedWindow(HWND);
            impl Drop for OwnedWindow {
                fn drop(&mut self) {
                    unsafe {
                        if IsWindow(self.0) != 0 {
                            DestroyWindow(self.0);
                        }
                    }
                }
            }
            let owned_parent = OwnedWindow(parent);
            let original =
                SetWindowLongPtrW(parent, GWLP_WNDPROC, parent_proc as *const () as isize);
            tap.original = std::mem::transmute::<isize, WNDPROC>(original);
            let buttons = ["复制信息 A", "复制信息 B"].map(|label| {
                let button = create_child(parent, "BUTTON", label, WS_TABSTOP, 0);
                assert!(!button.is_null());
                MoveWindow(button, 0, 0, 140, 36, 0);
                style_control(button, false);
                button
            });
            ShowWindow(parent, SW_SHOWNOACTIVATE);
            // Positive control: bypass only our subclass and invoke the same
            // native BUTTON messages. Even this empty-region window reaches
            // native direct painting, so zero below is not a hidden-window
            // false negative or merely a WM_PRINTCLIENT rendering assertion.
            let messages = [
                (WM_SETTEXT, 0, wide("原生基线")),
                (
                    WM_UPDATEUISTATE,
                    UIS_CLEAR as usize | ((UISF_HIDEFOCUS as usize) << 16),
                    Vec::new(),
                ),
                (BM_SETSTATE, 1, Vec::new()),
                (BM_SETSTATE, 0, Vec::new()),
                (
                    WM_UPDATEUISTATE,
                    UIS_SET as usize | ((UISF_HIDEFOCUS as usize) << 16),
                    Vec::new(),
                ),
            ];
            let mut baseline = Vec::new();
            for button in buttons {
                let state = GetWindowLongPtrW(button, GWLP_USERDATA) as *const StyledControl;
                for (message, wparam, label) in &messages {
                    let before = tap.requests;
                    CallWindowProcW(
                        (*state).original.get(),
                        button,
                        *message,
                        *wparam,
                        if label.is_empty() {
                            0
                        } else {
                            label.as_ptr() as isize
                        },
                    );
                    baseline.push((*message, tap.requests - before));
                }
            }
            tap.requests = 0;
            for button in buttons {
                SendMessageW(button, WM_SETTEXT, 0, wide("已复制").as_ptr() as isize);
                SendMessageW(
                    button,
                    WM_UPDATEUISTATE,
                    UIS_CLEAR as usize | ((UISF_HIDEFOCUS as usize) << 16),
                    0,
                );
                SendMessageW(button, BM_SETSTATE, 1, 0);
                assert_ne!(
                    SendMessageW(button, BM_GETSTATE, 0, 0) & BST_PUSHED as isize,
                    0
                );
                SendMessageW(button, BM_SETSTATE, 0, 0);
                assert_eq!(
                    SendMessageW(button, BM_GETSTATE, 0, 0) & BST_PUSHED as isize,
                    0
                );
                SendMessageW(
                    button,
                    WM_UPDATEUISTATE,
                    UIS_SET as usize | ((UISF_HIDEFOCUS as usize) << 16),
                    0,
                );
                SendMessageW(button, WM_SETFOCUS, 0, 0);
                SendMessageW(button, WM_KILLFOCUS, 0, 0);
                assert_eq!(window_text(button), "已复制");
                assert_ne!(GetUpdateRect(button, null_mut(), 0), 0);
            }
            let native_requests = tap.requests;
            // Hidden and caller-redraw-disabled controls must never be made
            // visible by an internal FALSE/TRUE redraw pair.
            ShowWindow(buttons[0], SW_HIDE);
            SendMessageW(buttons[1], WM_SETREDRAW, 0, 0);
            for button in buttons {
                SendMessageW(button, WM_SETTEXT, 0, wide("状态保留").as_ptr() as isize);
                SendMessageW(button, BM_SETSTATE, 1, 0);
                SendMessageW(button, BM_SETSTATE, 0, 0);
                assert_eq!(GetWindowLongPtrW(button, GWL_STYLE) as u32 & WS_VISIBLE, 0);
            }
            let state = GetWindowLongPtrW(buttons[1], GWLP_USERDATA) as *const StyledControl;
            assert!(!(*state).redraw_enabled.get());
            drop(owned_parent);
            assert_eq!(GetForegroundWindow(), foreground);
            assert_eq!(
                windows_sys::Win32::UI::Input::KeyboardAndMouse::GetFocus(),
                focused
            );
            eprintln!(
                "native BUTTON per-message direct-paint requests (two buttons): {baseline:?}; styled={native_requests}; foreground/focus unchanged; empty region established before show"
            );
            assert!(!tap.invalid_dc);
            assert!(
                baseline
                    .iter()
                    .any(|(message, count)| *message == WM_SETTEXT && *count > 0)
            );
            assert!(
                baseline
                    .iter()
                    .any(|(message, count)| *message == WM_UPDATEUISTATE && *count > 0)
            );
            assert!(
                baseline
                    .iter()
                    .any(|(message, count)| *message == BM_SETSTATE && *count > 0)
            );
            assert_eq!(
                native_requests, 0,
                "styled buttons reached original BUTTON drawing during label/state updates"
            );
        }
    }
    #[test]
    fn styled_control_nested_redraw_choice_and_synchronous_destruction_are_safe() {
        struct Forward {
            native: WNDPROC,
            destroy_on: u32,
        }
        unsafe extern "system" fn forwarding_proc(
            hwnd: HWND,
            message: u32,
            wparam: WPARAM,
            lparam: LPARAM,
        ) -> LRESULT {
            unsafe {
                let forward =
                    GetPropW(hwnd, wide("QuietBattery.Test.Forward").as_ptr()) as *const Forward;
                let native = (*forward).native;
                if message == (*forward).destroy_on {
                    DestroyWindow(hwnd);
                    return 1;
                }
                if message == WM_SETTEXT {
                    SendMessageW(hwnd, WM_SETREDRAW, 0, 0);
                }
                CallWindowProcW(native, hwnd, message, wparam, lparam)
            }
        }
        unsafe {
            let parent = CreateWindowExW(
                0,
                wide("STATIC").as_ptr(),
                null(),
                0,
                0,
                0,
                300,
                100,
                null_mut(),
                null_mut(),
                GetModuleHandleW(null()),
                null(),
            );
            assert!(!parent.is_null());
            for destroy_on in [0, WM_SETTEXT, WM_SETREDRAW] {
                let button = create_child(parent, "BUTTON", "初始", WS_TABSTOP, 0);
                style_control(button, false);
                let pointer = GetWindowLongPtrW(button, GWLP_USERDATA) as *const StyledControl;
                Rc::increment_strong_count(pointer);
                let state = Rc::from_raw(pointer);
                let weak = Rc::downgrade(&state);
                let forward = Forward {
                    native: state.original.get(),
                    destroy_on,
                };
                assert_ne!(
                    SetPropW(
                        button,
                        wide("QuietBattery.Test.Forward").as_ptr(),
                        &forward as *const _ as HANDLE
                    ),
                    0
                );
                state.original.set(Some(forwarding_proc));
                drop(state);
                SendMessageW(button, WM_SETTEXT, 0, wide("重入").as_ptr() as isize);
                if destroy_on != 0 {
                    assert_eq!(IsWindow(button), 0);
                    assert!(
                        weak.upgrade().is_none(),
                        "destroyed control state must be released after outer dispatch"
                    );
                } else {
                    let state = weak.upgrade().unwrap();
                    assert!(!state.redraw_enabled.get());
                    assert!(!state.native_redraw_suppressed.get());
                    assert_eq!(GetWindowLongPtrW(button, GWL_STYLE) as u32 & WS_VISIBLE, 0);
                    assert_eq!(window_text(button), "重入");
                    DestroyWindow(button);
                }
            }
            DestroyWindow(parent);
        }
    }
    #[test]
    fn styled_control_hover_erase_and_buffered_paint_stress_releases_resources() {
        use windows_sys::Win32::System::Threading::{
            GR_GDIOBJECTS, GetCurrentProcess, GetGuiResources,
        };
        unsafe {
            let parent = CreateWindowExW(
                0,
                wide("STATIC").as_ptr(),
                null(),
                0,
                0,
                0,
                512,
                100,
                null_mut(),
                null_mut(),
                GetModuleHandleW(null()),
                null(),
            );
            assert!(!parent.is_null());
            let controls = [(STARTUP, "开机自启"), (ALERTS, "低电提醒")].map(|(id, label)| {
                let h = create_child(
                    parent,
                    "BUTTON",
                    label,
                    WS_TABSTOP | BS_AUTOCHECKBOX as u32,
                    id,
                );
                MoveWindow(h, 0, 0, 512, 40, 0);
                style_control(h, true);
                h
            });
            let dc = CreateCompatibleDC(null_mut());
            let mut bits = null_mut();
            let info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: 40,
                    biWidth: 512,
                    biHeight: -40,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB,
                    ..Default::default()
                },
                ..Default::default()
            };
            let bitmap = CreateDIBSection(dc, &info, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
            assert!(!bitmap.is_null());
            let old = SelectObject(dc, bitmap);
            for control in controls {
                // A self-painted control must acknowledge erase without ever
                // exposing a stock BUTTON background in the supplied DC.
                let brush = CreateSolidBrush(0x00ff00ff);
                let bounds = RECT {
                    left: 0,
                    top: 0,
                    right: 512,
                    bottom: 40,
                };
                FillRect(dc, &bounds, brush);
                DeleteObject(brush);
                assert_eq!(SendMessageW(control, WM_ERASEBKGND, dc as usize, 0), 1);
                assert_eq!(GetPixel(dc, 0, 0), 0x00ff00ff);
                SendMessageW(control, WM_SETREDRAW, 0, 0);
                SendMessageW(control, WM_PRINTCLIENT, dc as usize, 0);
                SendMessageW(control, WM_PAINT, 0, 0);
                assert_eq!(
                    GetPixel(dc, 0, 0),
                    0x00ff00ff,
                    "explicit paint must preserve the caller's redraw-disabled choice"
                );
                SendMessageW(control, WM_SETREDRAW, 1, 0);
                ValidateRect(control, null());
                let state = GetWindowLongPtrW(control, GWLP_USERDATA) as *const StyledControl;
                let before_hover = (*state).invalidations.get();
                SendMessageW(control, WM_MOUSEMOVE, 0, 10 | (10 << 16));
                assert_eq!((*state).invalidations.get(), before_hover + 1);
                ValidateRect(control, null());
                for x in 10..500 {
                    SendMessageW(control, WM_MOUSEMOVE, 0, x | (20 << 16));
                }
                assert_eq!(
                    (*state).invalidations.get(),
                    before_hover + 1,
                    "moving inside the same hover region must not invalidate again"
                );
                SendMessageW(control, WM_MOUSELEAVE, 0, 0);
                assert_eq!((*state).invalidations.get(), before_hover + 2);
                ValidateRect(control, null());
                SendMessageW(control, WM_MOUSELEAVE, 0, 0);
                assert_eq!((*state).invalidations.get(), before_hover + 2);
                SendMessageW(control, BM_SETCHECK, 0, 0);
                SendMessageW(control, BM_CLICK, 0, 0);
                assert_eq!(
                    SendMessageW(control, BM_GETCHECK, 0, 0),
                    BST_CHECKED as isize,
                    "custom rendering must retain native checkbox activation"
                );
                SendMessageW(control, WM_PRINTCLIENT, dc as usize, 0);
                assert_eq!(GetPixel(dc, 0, 0), panel_colors().paper);
                assert_eq!(GetPixel(dc, 477, 20), panel_colors().fill);
            }
            let before = GetGuiResources(GetCurrentProcess(), GR_GDIOBJECTS);
            assert!(before > 0);
            for cycle in 0..300 {
                for control in controls {
                    SendMessageW(control, WM_MOUSEMOVE, 0, 12 | (12 << 16));
                    SendMessageW(control, WM_PRINTCLIENT, dc as usize, 0);
                    SendMessageW(control, WM_MOUSELEAVE, 0, 0);
                    InvalidateRect(control, null(), 1);
                    SendMessageW(control, WM_PAINT, 0, 0);
                    assert_eq!(GetUpdateRect(control, null_mut(), 0), 0);
                    SendMessageW(control, WM_PRINTCLIENT, dc as usize, 0);
                    assert_eq!(
                        GetPixel(dc, 0, 0),
                        panel_colors().paper,
                        "finished frame {cycle}"
                    );
                }
            }
            let after = GetGuiResources(GetCurrentProcess(), GR_GDIOBJECTS);
            assert!(
                after <= before,
                "buffer/DC/font resources grew across repeated paints: {before}->{after}"
            );
            assert_eq!(window_text(controls[0]), "开机自启");
            assert_eq!(window_text(controls[1]), "低电提醒");
            SelectObject(dc, old);
            assert_ne!(DeleteObject(bitmap), 0);
            assert_ne!(DeleteDC(dc), 0);
            assert_ne!(DestroyWindow(parent), 0);
            for h in controls {
                assert_eq!(IsWindow(h), 0);
            }
        }
    }
    fn headless_app(manager: Manager) -> (App, Sender<WorkerResult>) {
        let (sender, _requests) = mpsc::channel();
        let (results, receiver) = mpsc::channel();
        (
            App {
                effects: TestEffects::default(),
                hwnd: null_mut(),
                panel: null_mut(),
                picker: null_mut(),
                manager,
                trays: Vec::new(),
                fallback: None,
                rows: Vec::new(),
                sender,
                receiver,
                worker: None,
                stop: Cancellation::default(),
                busy: false,
                pending: false,
                pending_discovery: false,
                simulated: false,
                refresh_count: 0,
                success_count: 0,
                bridge: test_bridge(),
                panel_fonts: [null_mut(); 3],
                panel_icons: [null_mut(); 2],
                last_activity_refresh: Instant::now(),
                picker_ids: Vec::new(),
                page: Page::Main,
                page_scroll: [0; 3],
                page_controls: Vec::new(),
                threshold_error: false,
                notification_submissions: 0,
                notification_last_accepted: false,
                scroll: 0,
            },
            results,
        )
    }
    #[test]
    fn receive_persists_before_alert_delivery_and_retries_only_valid_results() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join(format!(".build/tmp/delivery-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        for scenario in ["accepted", "save-failed", "shell-rejected"] {
            let mut manager = Manager::default();
            let id = "delivery-fixture";
            manager.merge(
                vec![Candidate {
                    descriptor: crate::device::DeviceDescriptor {
                        id: id.into(),
                        physical_id: id.into(),
                        endpoint: None,
                        recoverable: true,
                        name: "delivery fixture".into(),
                        kind: DeviceKind::Mouse,
                        supported: true,
                        provider: "fixture".into(),
                    },
                    reading: Some(
                        Reading::from_percent(
                            "delivery fixture".into(),
                            90,
                            crate::device::PowerState::Discharging,
                        )
                        .unwrap(),
                    ),
                    error: None,
                }],
                1,
            );
            let path = root.join(format!("{scenario}.state"));
            let calls = Rc::new(RefCell::new(Vec::new()));
            let sink_calls = calls.clone();
            let sink_path = path.clone();
            let reject_first = scenario == "shell-rejected";
            let (mut app, results) = headless_app(manager);
            app.effects.state_path = Some(path.clone());
            app.effects.notification_sink = Some(Box::new(move |name, percent| {
                assert!(
                    Manager::load(&sink_path).devices[0].alert.notified,
                    "cycle must be durable before Shell submission"
                );
                let mut calls = sink_calls.borrow_mut();
                calls.push((name.to_owned(), percent));
                !reject_first || calls.len() > 1
            }));
            if scenario == "save-failed" {
                std::fs::create_dir_all(&path).unwrap();
            }
            let send = |percent| {
                results
                    .send(WorkerResult::Reading(
                        id.into(),
                        Ok(Reading::from_percent(
                            "delivery fixture".into(),
                            percent,
                            crate::device::PowerState::Discharging,
                        )
                        .unwrap()),
                    ))
                    .unwrap()
            };
            send(19);
            app.receive();
            if scenario == "save-failed" {
                assert!(calls.borrow().is_empty());
                assert_eq!(app.manager.pending_alerts.len(), 1);
                std::fs::remove_dir(&path).unwrap();
                send(18);
                app.receive();
            }
            if scenario == "shell-rejected" {
                assert_eq!(calls.borrow().len(), 1);
                assert_eq!(app.manager.pending_alerts.len(), 1);
                send(20);
                app.receive();
                assert_eq!(calls.borrow().len(), 1);
                assert_eq!(
                    app.manager.pending_alerts.len(),
                    1,
                    "equal threshold retains the pending cycle without triggering"
                );
                results.send(WorkerResult::Complete).unwrap();
                app.receive();
                assert_eq!(
                    calls.borrow().len(),
                    1,
                    "Complete must not immediately retry a rejected Shell submission"
                );
                send(18);
                app.receive();
            }
            assert!(app.manager.pending_alerts.is_empty());
            let delivered = calls.borrow().len();
            send(17);
            app.receive();
            assert_eq!(
                calls.borrow().len(),
                delivered,
                "repeated low value must not notify again"
            );
            // A pending rejected alert becomes obsolete as soon as new valid
            // data says recovered, charging, disconnected, disabled or removed.
            app.manager
                .pending_alerts
                .push((id.into(), "obsolete".into(), 17));
            app.manager.notifications_enabled = false;
            app.deliver_alerts();
            assert!(app.manager.pending_alerts.is_empty());
            app.manager.notifications_enabled = true;
            for obsolete in 0..4 {
                let original = app.manager.devices[0].clone();
                app.manager
                    .pending_alerts
                    .push((id.into(), "obsolete".into(), 17));
                match obsolete {
                    0 => app.manager.devices[0].selected = false,
                    1 => app.manager.devices[0].notify = false,
                    2 => {
                        app.manager.devices[0].connected = false;
                        app.manager.devices[0].connection_known = true;
                    }
                    _ => {
                        app.manager.devices[0].reading.as_mut().unwrap().power_state =
                            crate::device::PowerState::Charging
                    }
                }
                app.deliver_alerts();
                assert!(app.manager.pending_alerts.is_empty());
                assert_eq!(calls.borrow().len(), delivered);
                app.manager.devices[0] = original;
            }
            app.manager
                .pending_alerts
                .push((id.into(), "obsolete".into(), 17));
            send(21);
            app.receive();
            assert!(app.manager.pending_alerts.is_empty());
            assert_eq!(calls.borrow().len(), delivered);
            std::fs::remove_file(path).unwrap();
        }
        std::fs::remove_dir(root).unwrap();
    }
    fn test_bridge() -> Rc<UiBridge> {
        Rc::new(UiBridge {
            events: RefCell::new(VecDeque::new()),
            view: RefCell::new(Rc::new(PanelView::default())),
            taskbar_created: WM_APP + 100,
            wake_window: Cell::new(null_mut()),
            menu_active: Cell::new(false),
            menu_timing: Cell::new(MenuTiming::default()),
            activity_queued: Cell::new(false),
            activity_last: Cell::new(Instant::now() - std::time::Duration::from_secs(10)),
            activity_needed: Cell::new(true),
            activity_seen: Cell::new(Instant::now() - std::time::Duration::from_secs(30)),
            notification_events: Cell::new([0; 3]),
            status: RefCell::new(StatusView::default()),
        })
    }
    #[test]
    fn menu_visible_timing_requires_visible_menu_idle_and_is_one_shot() {
        unsafe {
            let owner = CreateWindowExW(
                0,
                wide("STATIC").as_ptr(),
                null(),
                0,
                0,
                0,
                0,
                0,
                null_mut(),
                null_mut(),
                null_mut(),
                null(),
            );
            let popup = CreateWindowExW(
                0,
                wide("STATIC").as_ptr(),
                null(),
                WS_POPUP,
                -1000,
                -1000,
                1,
                1,
                null_mut(),
                null_mut(),
                null_mut(),
                null(),
            );
            assert!(!owner.is_null() && !popup.is_null());
            let bridge = test_bridge();
            attach_bridge(owner, &bridge);
            bridge.menu_active.set(true);
            bridge.menu_timing.set(MenuTiming {
                requested: Some(Instant::now()),
                ..MenuTiming::default()
            });
            window_proc(owner, WM_ENTERMENULOOP, 1, 0);
            assert!(bridge.menu_timing.get().enter_loop_us.is_some());
            assert!(
                bridge.menu_timing.get().visible_idle_us.is_none(),
                "entry is not visibility"
            );
            window_proc(owner, WM_ENTERIDLE, MSGF_MENU as usize, 0);
            assert!(
                bridge.menu_timing.get().visible_idle_us.is_none(),
                "null HWND is not visible"
            );
            window_proc(owner, WM_ENTERIDLE, MSGF_MENU as usize, popup as isize);
            assert!(
                bridge.menu_timing.get().visible_idle_us.is_none(),
                "hidden HWND is not visible"
            );
            ShowWindow(popup, SW_SHOWNOACTIVATE);
            window_proc(owner, WM_ENTERIDLE, 0, popup as isize);
            assert!(
                bridge.menu_timing.get().visible_idle_us.is_none(),
                "only menu-idle counts"
            );
            window_proc(owner, WM_ENTERIDLE, MSGF_MENU as usize, popup as isize);
            let first = bridge.menu_timing.get();
            assert!(first.visible_idle_us.is_some());
            bridge.menu_timing.set(MenuTiming {
                requested: Some(Instant::now() - std::time::Duration::from_secs(1)),
                ..first
            });
            window_proc(owner, WM_ENTERMENULOOP, 1, 0);
            window_proc(owner, WM_ENTERIDLE, MSGF_MENU as usize, popup as isize);
            assert_eq!(bridge.menu_timing.get().enter_loop_us, first.enter_loop_us);
            assert_eq!(
                bridge.menu_timing.get().visible_idle_us,
                first.visible_idle_us
            );
            ShowWindow(popup, SW_HIDE);
            bridge.menu_timing.set(MenuTiming {
                requested: Some(Instant::now()),
                ..MenuTiming::default()
            });
            bridge.menu_active.set(false);
            ShowWindow(popup, SW_SHOWNOACTIVATE);
            window_proc(owner, WM_ENTERIDLE, MSGF_MENU as usize, popup as isize);
            assert!(
                bridge.menu_timing.get().visible_idle_us.is_none(),
                "inactive menus do not count"
            );
            release_window_bridge(owner);
            DestroyWindow(popup);
            DestroyWindow(owner);
        }
    }
    #[test]
    fn native_callbacks_only_queue_and_release_their_bridge() {
        unsafe {
            let bridge = test_bridge();
            let instance = GetModuleHandleW(null());
            let name = wide("QuietBattery.CallbackOwnership.Test");
            let class = WNDCLASSW {
                lpfnWndProc: Some(window_proc),
                hInstance: instance,
                lpszClassName: name.as_ptr(),
                ..std::mem::zeroed()
            };
            assert_ne!(RegisterClassW(&class), 0);
            let hwnd = CreateWindowExW(
                0,
                name.as_ptr(),
                name.as_ptr(),
                0,
                0,
                0,
                0,
                0,
                null_mut(),
                null_mut(),
                instance,
                null(),
            );
            assert!(!hwnd.is_null());
            attach_bridge(hwnd, &bridge);
            // Native dispatch can reenter while a caller retains an immutable view.
            let view = bridge.view.borrow().clone();
            SendMessageW(hwnd, WM_CLOSE, 0, 0);
            assert!(matches!(bridge.next(), Some(UiEvent::Control(QUIT))));
            assert_ne!(
                IsWindow(hwnd),
                0,
                "callback must leave mutation to outer loop"
            );
            assert_eq!(Rc::strong_count(&bridge), 2);
            DestroyWindow(hwnd);
            assert!(matches!(bridge.next(), Some(UiEvent::MainDestroyed)));
            assert_eq!(Rc::strong_count(&bridge), 1);
            assert!(view.rows.is_empty());
            UnregisterClassW(name.as_ptr(), instance);
        }
    }
    #[test]
    fn sent_callback_wakes_get_message_without_waiting_for_a_timer() {
        let (ready_tx, ready_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let worker = std::thread::spawn(move || unsafe {
            let instance = GetModuleHandleW(null());
            let name = wide("QuietBattery.SentCallbackWake.Test");
            let class = WNDCLASSW {
                lpfnWndProc: Some(window_proc),
                hInstance: instance,
                lpszClassName: name.as_ptr(),
                ..std::mem::zeroed()
            };
            assert_ne!(RegisterClassW(&class), 0);
            let hwnd = CreateWindowExW(
                0,
                name.as_ptr(),
                name.as_ptr(),
                0,
                0,
                0,
                0,
                0,
                null_mut(),
                null_mut(),
                instance,
                null(),
            );
            assert!(!hwnd.is_null());
            let bridge = test_bridge();
            bridge.wake_window.set(hwnd);
            attach_bridge(hwnd, &bridge);
            ready_tx.send(hwnd as usize).unwrap();
            let mut message: MSG = std::mem::zeroed();
            let mut woke = false;
            while GetMessageW(&mut message, null_mut(), 0, 0) > 0 {
                if message.message == EVENTS_READY {
                    while let Some(event) = bridge.next() {
                        woke |= matches!(event, UiEvent::Control(REFRESH));
                    }
                    if woke {
                        break;
                    }
                } else {
                    DispatchMessageW(&message);
                }
            }
            done_tx.send(woke).unwrap();
            DestroyWindow(hwnd);
            UnregisterClassW(name.as_ptr(), instance);
        });
        let hwnd = ready_rx
            .recv_timeout(std::time::Duration::from_secs(3))
            .unwrap() as HWND;
        let mut result = 0;
        assert_ne!(
            unsafe {
                SendMessageTimeoutW(
                    hwnd,
                    CONTROL,
                    REFRESH,
                    0,
                    SMTO_ABORTIFHUNG,
                    1000,
                    &mut result,
                )
            },
            0
        );
        let woke = done_rx.recv_timeout(std::time::Duration::from_millis(500));
        // Ensure a failing regression cannot strand the test thread in GetMessage.
        if woke.is_err() {
            unsafe {
                PostMessageW(hwnd, WM_QUIT, 0, 0);
            }
        }
        worker.join().unwrap();
        assert_eq!(
            woke.ok(),
            Some(true),
            "sent callback must wake the outer dispatcher immediately"
        );
    }

    #[test]
    fn full_range_meter_uses_reported_and_normalized_values() {
        for (percent, full, partial) in
            [(90, 9, 0), (75, 7, 5), (87, 8, 7), (0, 0, 0), (100, 10, 0)]
        {
            let fills: Vec<_> = (0..10).map(|i| cell_fill(Some(percent), i)).collect();
            assert_eq!(fills.iter().filter(|&&fill| fill == 10).count(), full);
            if full < 10 {
                assert_eq!(fills[full], partial);
            }
        }
        assert_eq!(cell_fill(None, 0), 0);
    }
    #[test]
    fn status_is_serialized_on_demand_without_files() {
        let view = StatusView {
            manager: Manager {
                threshold: 37,
                notifications_enabled: false,
                ..Manager::default()
            },
            ..StatusView::default()
        };
        let text = view.json(
            MenuTiming {
                count: 3,
                visible_idle_us: Some(7400),
                ..MenuTiming::default()
            },
            [1, 2, 3],
        );
        assert!(text.starts_with(&format!("{{\"pid\":{},", std::process::id())));
        assert!(text.contains("\"threshold\":37"));
        assert!(text.contains("\"notifications_enabled\":false"));
        assert!(text.contains("\"visible_idle_us\":7400"));
        assert!(text.contains("\"shown\":1"));
    }

    #[test]
    fn activity_wakes_after_idle_coalesces_bursts_and_bounds_error_retries() {
        let bridge = test_bridge();
        let now = Instant::now();
        bridge.activity_needed.set(false);
        bridge.activity(now);
        assert!(matches!(bridge.next(), Some(UiEvent::Activity)));
        for seconds in 1..20 {
            bridge.activity(now + std::time::Duration::from_secs(seconds));
        }
        assert!(bridge.next().is_none());
        bridge.activity_queued.set(false);
        bridge.activity(now + std::time::Duration::from_secs(40));
        assert!(matches!(bridge.next(), Some(UiEvent::Activity)));
        bridge.activity_queued.set(false);
        bridge.activity_needed.set(true);
        bridge.activity(now + std::time::Duration::from_secs(41));
        assert!(bridge.next().is_none());
        bridge.activity(now + std::time::Duration::from_secs(50));
        assert!(matches!(bridge.next(), Some(UiEvent::Activity)));
    }
    #[test]
    fn stable_tray_ids_do_not_depend_on_order() {
        assert_ne!(stable_tray_id("receiverA:1"), stable_tray_id("receiverB:1"));
        assert_eq!(stable_tray_id("receiverA:1"), stable_tray_id("receiverA:1"));
    }

    unsafe fn window_text(hwnd: HWND) -> String {
        let mut text = [0u16; 128];
        let count = unsafe { GetWindowTextW(hwnd, text.as_mut_ptr(), text.len() as i32) };
        String::from_utf16_lossy(&text[..count as usize])
    }
    unsafe fn hidden_native_bitmap(hwnd: HWND, name: &str) {
        unsafe {
            let mut client: RECT = std::mem::zeroed();
            GetClientRect(hwnd, &mut client);
            let dc = CreateCompatibleDC(null_mut());
            let mut pixels = null_mut();
            let info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: 40,
                    biWidth: client.right,
                    biHeight: -client.bottom,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB,
                    ..Default::default()
                },
                ..Default::default()
            };
            let bitmap = CreateDIBSection(dc, &info, DIB_RGB_COLORS, &mut pixels, null_mut(), 0);
            assert!(!bitmap.is_null());
            let old = SelectObject(dc, bitmap);
            let bridge = callback_bridge(hwnd).unwrap();
            draw_panel(dc, hwnd, &bridge.view.borrow());
            let mut origin = POINT { x: 0, y: 0 };
            ClientToScreen(hwnd, &mut origin);
            let mut child = GetWindow(hwnd, GW_CHILD);
            while !child.is_null() {
                if GetWindowLongPtrW(child, GWL_STYLE) & WS_VISIBLE as isize != 0 {
                    let mut rect: RECT = std::mem::zeroed();
                    GetWindowRect(child, &mut rect);
                    let saved = SaveDC(dc);
                    SetViewportOrgEx(dc, rect.left - origin.x, rect.top - origin.y, null_mut());
                    IntersectClipRect(dc, 0, 0, rect.right - rect.left, rect.bottom - rect.top);
                    SendMessageW(
                        child,
                        WM_PRINT,
                        dc as usize,
                        (PRF_CLIENT | PRF_ERASEBKGND) as isize,
                    );
                    RestoreDC(dc, saved);
                }
                child = GetWindow(child, GW_HWNDNEXT);
            }
            assert_eq!(GetPixel(dc, 0, 0), panel_colors().paper);
            let len = (client.right * client.bottom * 4) as usize;
            let mut bytes = Vec::new();
            bytes.extend(b"BM");
            bytes.extend((54 + len as u32).to_le_bytes());
            bytes.extend([0; 4]);
            bytes.extend(54u32.to_le_bytes());
            bytes.extend(40u32.to_le_bytes());
            bytes.extend(client.right.to_le_bytes());
            bytes.extend((-client.bottom).to_le_bytes());
            bytes.extend(1u16.to_le_bytes());
            bytes.extend(32u16.to_le_bytes());
            bytes.extend([0; 24]);
            bytes.extend(std::slice::from_raw_parts(pixels.cast::<u8>(), len));
            let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join(format!(".build/tmp/hidden-native-{name}.bmp"));
            std::fs::write(path, bytes).unwrap();
            SelectObject(dc, old);
            DeleteObject(bitmap);
            DeleteDC(dc);
        }
    }
    #[test]
    fn native_unified_pages_preferences_threshold_and_add() {
        unsafe {
            let instance = GetModuleHandleW(null());
            let name = wide(PANEL_CLASS);
            let class = WNDCLASSW {
                lpfnWndProc: Some(panel_proc),
                hInstance: instance,
                lpszClassName: name.as_ptr(),
                ..std::mem::zeroed()
            };
            assert_ne!(RegisterClassW(&class), 0);
            let (sender, _commands) = mpsc::channel();
            let (_results, receiver) = mpsc::channel();
            let mut app = App {
                #[cfg(test)]
                effects: TestEffects::default(),
                hwnd: null_mut(),
                panel: null_mut(),
                picker: null_mut(),
                manager: Manager::default(),
                trays: Vec::new(),
                fallback: None,
                rows: Vec::new(),
                sender,
                receiver,
                worker: None,
                stop: Cancellation::default(),
                busy: false,
                pending: false,
                pending_discovery: false,
                simulated: false,
                refresh_count: 0,
                success_count: 0,
                bridge: test_bridge(),
                panel_fonts: [null_mut(); 3],
                panel_icons: [null_mut(); 2],
                last_activity_refresh: Instant::now(),
                picker_ids: Vec::new(),
                page: Page::Main,
                page_scroll: [0; 3],
                page_controls: Vec::new(),
                threshold_error: false,
                notification_submissions: 0,
                notification_last_accepted: false,
                scroll: 0,
            };
            for (id, kind, supported) in [
                ("mouse", DeviceKind::Mouse, true),
                ("keyboard", DeviceKind::Keyboard, true),
                ("unsupported", DeviceKind::Generic, false),
            ] {
                app.manager.devices.push(DeviceState {
                    descriptor: crate::device::DeviceDescriptor {
                        id: id.into(),
                        physical_id: id.into(),
                        endpoint: None,
                        recoverable: supported,
                        name: id.into(),
                        kind,
                        supported,
                        provider: "fixture".into(),
                    },
                    selected: id == "mouse",
                    removed: false,
                    show_tray: true,
                    notify: true,
                    alert: Default::default(),
                    reading: None,
                    last_success: None,
                    error: None,
                    connected: true,
                    connection_known: true,
                });
            }
            app.show_panel();
            let window = app.panel;
            assert_eq!(IsWindowVisible(window), 0);
            assert_eq!(app.rows.len(), 1);
            assert_eq!(window_text(app.rows[0].percent), "—%");
            hidden_native_bitmap(window, "main-unknown-fixture");
            let mut client: RECT = std::mem::zeroed();
            GetClientRect(window, &mut client);
            assert_eq!(
                client.right,
                560 * GetDpiForWindow(window).max(96) as i32 / 96
            );
            app.switch_page(Page::Settings);
            let threshold = GetDlgItem(window, THRESHOLD as i32);
            let suffix = GetDlgItem(window, 114);
            let mut edit_rect: RECT = std::mem::zeroed();
            let mut suffix_rect: RECT = std::mem::zeroed();
            let mut format: RECT = std::mem::zeroed();
            GetWindowRect(threshold, &mut edit_rect);
            GetWindowRect(suffix, &mut suffix_rect);
            SendMessageW(threshold, EM_GETRECT, 0, &mut format as *mut RECT as isize);
            let dc = GetDC(threshold);
            let old_font = SelectObject(dc, SendMessageW(threshold, WM_GETFONT, 0, 0) as HFONT);
            let mut metrics: TEXTMETRICW = std::mem::zeroed();
            assert_ne!(GetTextMetricsW(dc, &mut metrics), 0);
            SelectObject(dc, old_font);
            ReleaseDC(threshold, dc);
            let text_center = 2 * (edit_rect.top + format.top) + metrics.tmHeight;
            let row_center = suffix_rect.top + suffix_rect.bottom;
            assert!(
                (text_center - row_center).abs() <= 2,
                "threshold text must align vertically with its percent suffix"
            );
            assert!(edit_rect.top >= suffix_rect.top);
            assert!(edit_rect.bottom <= suffix_rect.bottom);
            hidden_native_bitmap(window, "settings-fixture");
            assert_eq!(app.panel, window);
            assert_eq!(app.rows.len(), 1);
            assert_eq!(SendMessageW(app.rows[0].percent, BM_GETCHECK, 0, 0), 1);
            assert!(window_text(app.rows[0].percent).contains("mouse，显示托盘"));
            let anchor = app.anchor_for(app.rows[0].refresh).unwrap();
            let previous_model = app.rows[0].model;
            app.manager.devices[0].descriptor.name = "Renamed mouse".into();
            app.manager.devices[0].descriptor.kind = DeviceKind::Headset;
            app.rebuild_rows();
            assert_ne!(app.rows[0].model, previous_model);
            assert_eq!(window_text(app.rows[0].model), "Renamed mouse");
            assert!(window_text(app.rows[0].refresh).contains("Renamed mouse，低电提醒"));
            assert_eq!(app.rows[0].presentation.2, DeviceKind::Headset);
            assert_eq!(app.rows[0]._tooltip_text, wide("Renamed mouse"));
            assert_eq!(app.resolve_anchor(&anchor).unwrap().0, app.rows[0].refresh);
            assert!(app.anchor_for(null_mut()).is_none());
            let foreign = CreateWindowExW(
                0,
                wide("STATIC").as_ptr(),
                null(),
                0,
                0,
                0,
                0,
                0,
                null_mut(),
                null_mut(),
                instance,
                null(),
            );
            assert!(app.anchor_for(foreign).is_none());
            DestroyWindow(foreign);
            let mut duplicate = app.manager.devices[0].clone();
            duplicate.descriptor.id = "other-same-model".into();
            duplicate.selected = false;
            app.manager.devices.push(duplicate);
            app.rebuild_rows();
            assert!(window_text(app.rows[0].model).contains('·'));
            assert!(window_text(app.rows[0].refresh).contains('·'));
            app.manager.devices.pop();
            app.rebuild_rows();
            let dc = CreateCompatibleDC(null_mut());
            let mut bits = null_mut();
            let info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: 40,
                    biWidth: 64,
                    biHeight: -40,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB,
                    ..Default::default()
                },
                ..Default::default()
            };
            let bitmap = CreateDIBSection(dc, &info, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
            let old = SelectObject(dc, bitmap);
            let high_contrast = PanelColors {
                paper: 0,
                text: 0xffffff,
                secondary: 0xffffff,
                fill: 0xff0000,
                empty: 0x777777,
                outline: 0xffffff,
                high_contrast: true,
            };
            draw_control_colors(app.rows[0].percent, true, dc, &high_contrast);
            assert_eq!(GetPixel(dc, 4, 18), high_contrast.fill);
            assert_eq!(window_text(app.rows[0].percent), "Renamed mouse，显示托盘");
            SelectObject(dc, old);
            DeleteObject(bitmap);
            DeleteDC(dc);
            app.page_command(ALERTS);
            assert!(!app.manager.notifications_enabled);
            assert_eq!(IsWindowEnabled(app.rows[0].refresh), 0);
            assert_ne!(IsWindowEnabled(app.rows[0].percent), 0);
            app.page_command(ALERTS);
            let edit = GetDlgItem(window, THRESHOLD as i32);
            SetWindowTextW(edit, wide("101").as_ptr());
            app.submit_threshold(true);
            assert_eq!(app.manager.threshold, 20);
            assert!(app.threshold_error);
            SetWindowTextW(edit, wide("21").as_ptr());
            app.submit_threshold(true);
            assert_eq!(app.manager.threshold, 21);
            assert!(!app.threshold_error);
            SetWindowTextW(edit, wide("bad").as_ptr());
            app.submit_threshold(false);
            assert_eq!(window_text(edit), "21");
            app.show_picker();
            hidden_native_bitmap(window, "add-fixture");
            assert_eq!(app.panel, window);
            assert!(app.picker.is_null());
            assert_eq!(app.rows.len(), 2);
            let i = app.rows.iter().position(|r| r.id == "keyboard").unwrap();
            app.page_command(ADD_BASE + i as u32);
            assert_eq!(app.rows.len(), 2);
            assert_eq!(window_text(app.rows[i].refresh), "已添加");
            assert_eq!(IsWindowEnabled(app.rows[i].refresh), 0);
            app.switch_page(Page::Main);
            assert_eq!(app.rows.len(), 2);
            app.manager.apply("mouse", Err(DeviceError::NotFound), 100);
            app.rebuild_rows();
            assert_eq!(app.rows.len(), 1);
            assert!(app.manager.devices[0].selected);
            let template = app.manager.devices[1].clone();
            for index in 0..40 {
                let mut state = template.clone();
                state.descriptor.id = format!("scroll-{index}");
                state.selected = true;
                app.manager.devices.push(state);
            }
            app.rebuild_rows();
            let last = app.rows.len() - 1;
            let target = app.rows[last].refresh;
            app.focus_target(target, Some(last));
            assert!(app.scroll > 0);
            assert_eq!(
                GetWindowLongPtrW(target, GWL_STYLE) & WS_VISIBLE as isize,
                WS_VISIBLE as isize
            );
            let scroll = app.scroll;
            app.switch_page(Page::Settings);
            app.switch_page(Page::Main);
            assert_eq!(app.scroll, scroll);
            assert_eq!(
                app.focus_targets().last().unwrap().0,
                GetDlgItem(window, SETTINGS as i32)
            );
            app.close_panel();
            assert_eq!(IsWindow(window), 0);
            assert!(app.panel_fonts.iter().all(|f| f.is_null()));
            UnregisterClassW(name.as_ptr(), instance);
            let _ = std::fs::remove_file(manager::state_path());
        }
    }
    #[test]
    fn simulation_covers_pending_discovery_and_inflight_real_results() {
        let (sender, commands) = mpsc::channel();
        let (results, receiver) = mpsc::channel();
        let bridge = test_bridge();
        let mut manager = Manager::default();
        let descriptor = crate::device::DeviceDescriptor {
            id: "race-fixture".into(),
            physical_id: "race-connection".into(),
            endpoint: Some("1".into()),
            recoverable: true,
            name: "Race fixture".into(),
            kind: DeviceKind::Mouse,
            supported: true,
            provider: "fixture".into(),
        };
        manager.merge(
            vec![Candidate {
                descriptor: descriptor.clone(),
                reading: Some(
                    Reading::from_percent(
                        "Race fixture".into(),
                        90,
                        crate::device::PowerState::Discharging,
                    )
                    .unwrap(),
                ),
                error: None,
            }],
            100,
        );
        let mut app = App {
            #[cfg(test)]
            effects: TestEffects::default(),
            hwnd: null_mut(),
            panel: null_mut(),
            picker: null_mut(),
            manager,
            trays: Vec::new(),
            fallback: None,
            rows: Vec::new(),
            sender,
            receiver,
            worker: None,
            stop: Cancellation::default(),
            busy: true,
            pending: false,
            pending_discovery: true,
            simulated: false,
            refresh_count: 0,
            success_count: 0,
            bridge,
            panel_fonts: [null_mut(); 3],
            panel_icons: [null_mut(); 2],
            last_activity_refresh: Instant::now(),
            picker_ids: Vec::new(),
            page: Page::Main,
            page_scroll: [0; 3],
            page_controls: Vec::new(),
            threshold_error: false,
            notification_submissions: 0,
            notification_last_accepted: false,
            scroll: 0,
        };
        app.bridge.queue(UiEvent::Control(SIMULATE_ERROR));
        app.process_events();
        assert!(app.simulated);
        assert!(app.pending);
        results
            .send(WorkerResult::Reading(
                "race-fixture".into(),
                Ok(Reading::from_percent(
                    "Race fixture".into(),
                    87,
                    crate::device::PowerState::Discharging,
                )
                .unwrap()),
            ))
            .unwrap();
        app.receive();
        assert_eq!(app.manager.devices[0].last_success, Some(100));
        assert_eq!(
            app.manager.devices[0]
                .reading
                .as_ref()
                .unwrap()
                .reported_percent,
            90
        );
        results
            .send(WorkerResult::Discovery(Ok(DiscoveryReport {
                candidates: vec![Candidate {
                    descriptor,
                    reading: Some(
                        Reading::from_percent(
                            "Race fixture".into(),
                            87,
                            crate::device::PowerState::Discharging,
                        )
                        .unwrap(),
                    ),
                    error: None,
                }],
                failures: Vec::new(),
            })))
            .unwrap();
        results.send(WorkerResult::Complete).unwrap();
        app.receive();
        assert!(matches!(
            commands.try_recv(),
            Ok(WorkerCommand::Discover(true))
        ));
        assert_eq!(app.success_count, 0);
        assert_eq!(app.manager.devices[0].last_success, Some(100));
        assert_eq!(
            app.manager.devices[0]
                .reading
                .as_ref()
                .unwrap()
                .reported_percent,
            90
        );
        results
            .send(WorkerResult::Discovery(Err(simulated_error())))
            .unwrap();
        results.send(WorkerResult::Complete).unwrap();
        app.receive();
        assert!(matches!(
            commands.try_recv(),
            Ok(WorkerCommand::Read(_, true))
        ));
        results
            .send(WorkerResult::Reading(
                "race-fixture".into(),
                Err(simulated_error()),
            ))
            .unwrap();
        results.send(WorkerResult::Complete).unwrap();
        app.receive();
        assert!(!app.busy);
        assert!(app.simulated);
        assert!(app.manager.devices[0].error.is_some());
        assert_eq!(app.manager.devices[0].last_success, Some(100));
        app.discover();
        assert!(matches!(
            commands.try_recv(),
            Ok(WorkerCommand::Discover(true))
        ));
        results
            .send(WorkerResult::Discovery(Err(simulated_error())))
            .unwrap();
        results.send(WorkerResult::Complete).unwrap();
        app.receive();
        assert!(!app.busy);
        assert!(app.manager.devices[0].error.is_some());
        app.bridge.queue(UiEvent::Control(RECOVER));
        app.process_events();
        assert!(matches!(
            commands.try_recv(),
            Ok(WorkerCommand::Read(_, false))
        ));
        results
            .send(WorkerResult::Reading(
                "race-fixture".into(),
                Ok(Reading::from_percent(
                    "Race fixture".into(),
                    90,
                    crate::device::PowerState::Discharging,
                )
                .unwrap()),
            ))
            .unwrap();
        results.send(WorkerResult::Complete).unwrap();
        app.receive();
        assert!(!app.simulated);
        assert!(!app.busy);
        assert!(app.manager.devices[0].error.is_none());
        assert_eq!(app.success_count, 1);
        assert_ne!(app.manager.devices[0].last_success, Some(100));
        let _ = std::fs::remove_file(manager::state_path());
    }
    #[test]
    fn ui_apartment_preserves_completion_messages_across_real_startup_status() {
        unsafe {
            let _com = startup::Com::init().unwrap();
            let instance = GetModuleHandleW(null());
            let name = wide("QuietBattery.StartupStatusWake.Test");
            let class = WNDCLASSW {
                lpfnWndProc: Some(window_proc),
                hInstance: instance,
                lpszClassName: name.as_ptr(),
                ..std::mem::zeroed()
            };
            assert_ne!(RegisterClassW(&class), 0);
            let hwnd = CreateWindowExW(
                0,
                name.as_ptr(),
                null(),
                0,
                0,
                0,
                0,
                0,
                null_mut(),
                null_mut(),
                instance,
                null(),
            );
            assert!(!hwnd.is_null());
            let bridge = test_bridge();
            bridge.wake_window.set(hwnd);
            attach_bridge(hwnd, &bridge);
            for _ in 0..40 {
                while bridge.next().is_some() {}
                let mut message: MSG = std::mem::zeroed();
                while PeekMessageW(&mut message, null_mut(), 0, 0, PM_REMOVE) != 0 {
                    DispatchMessageW(&message);
                }
                let (raw_tx, raw_rx) = mpsc::channel();
                let target = hwnd as usize;
                let worker = std::thread::spawn(move || {
                    raw_tx.send(()).unwrap();
                    assert_ne!(PostMessageW(target as HWND, RESULT, 0, 0), 0);
                });
                raw_rx.recv().unwrap();
                let _ = startup::status();
                worker.join().unwrap();
                let mut received = false;
                let deadline = Instant::now() + std::time::Duration::from_millis(500);
                while Instant::now() < deadline && !received {
                    while PeekMessageW(&mut message, null_mut(), 0, 0, PM_REMOVE) != 0 {
                        DispatchMessageW(&message);
                    }
                    while let Some(event) = bridge.next() {
                        received |= matches!(event, UiEvent::Result);
                    }
                    if !received {
                        std::thread::yield_now();
                    }
                }
                assert!(
                    received,
                    "real startup status must not consume worker completion messages"
                );
            }
            DestroyWindow(hwnd);
            UnregisterClassW(name.as_ptr(), instance);
        }
    }
    #[test]
    fn actual_gdi_meter_partial_last_cell() {
        unsafe {
            let dc = CreateCompatibleDC(null_mut());
            let mut bits: *mut std::ffi::c_void = null_mut();
            let info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: 384,
                    biHeight: -64,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB,
                    ..Default::default()
                },
                ..Default::default()
            };
            let bitmap = CreateDIBSection(dc, &info, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
            assert!(!bitmap.is_null());
            let old = SelectObject(dc, bitmap);
            let old_pen = SelectObject(dc, GetStockObject(DC_PEN));
            let old_brush = SelectObject(dc, GetStockObject(DC_BRUSH));
            let colors = panel_colors();
            for (percent, cell, width) in [(90, 9, 0), (75, 7, 5), (87, 8, 7)] {
                let reading = Reading::from_percent(
                    "fixture".into(),
                    percent,
                    crate::device::PowerState::Discharging,
                )
                .unwrap();
                draw_battery(dc, 0, Some(&reading), &colors);
                let x = 214 + cell * 12;
                if width > 0 {
                    assert_eq!(GetPixel(dc, x + width - 1, 25), colors.fill);
                }
                assert_eq!(GetPixel(dc, x + width, 25), colors.empty);
            }
            SelectObject(dc, old_brush);
            SelectObject(dc, old_pen);
            SelectObject(dc, old);
            DeleteObject(bitmap);
            DeleteDC(dc);
        }
    }
}
