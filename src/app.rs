//! Native multi-device shell. Device access is confined to the serial worker.
use crate::{
    device::{
        Cancellation, Candidate, DeviceError, DeviceKind, DiscoveryReport, Reading, Registry,
        SupportContext,
    },
    icons::{BatteryVisual, SymbolIcon, TrayIcon},
    manager::{self, DeviceState, Manager},
    shapes::{Rect, Surface},
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
/// Link check cadence. Separate from the battery refresh interval: one query per
/// monitored device, no enumeration and no BLE scan. The interval is re-armed
/// from the session state on every tick, so an idle machine probes less often.
const PRESENCE_TIMER: usize = 4;
/// One-shot guard for the first paint: the panel waits for the first discovery
/// pass so it never opens at the height of devices that are no longer there.
const REVEAL_TIMER: usize = 5;
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
const CONNECTIONS: u32 = 115;
const AUTO_REFRESH: u32 = 116;
const REFRESH_INTERVAL: u32 = 117;
const INTERVAL_LABEL: u32 = 118;
const INTERVAL_UNIT: u32 = 119;
/// The one line under the interval field that explains what the number costs.
const INTERVAL_HINT: u32 = 121;
/// "Window pinned": keeps the panel nailed to the bottom-right corner.
const WINDOW_PIN: u32 = 123;
/// The main header link into the usage history page.
const HISTORY: u32 = 122;
const ADD_BASE: u32 = 8000;
/// Actions per add-page row: 0 add, 1 copy, 2 remove.
const ADD_STRIDE: u32 = 3;
/// Per-device switches live on the main page; the id encodes the row index.
const MAIN_TRAY_BASE: u32 = 12000;
const MAIN_NOTIFY_BASE: u32 = 14000;
/// The single settings-page switch that flips every device at once.
const ALL_SWITCHES: u32 = 120;
/// One corner radius for every custom-drawn rounded control (96-dpi units).
/// Buttons, input frames and focus rings share it so the chrome reads as one
/// system; the toggle track is a full pill and the battery keeps its own scale.
const RADIUS_CONTROL: f32 = 6.0;
const RADIUS_BATTERY: f32 = 4.0;
/// Device symbol edge in 96-dpi units and its offset inside a 64px row: the
/// 28px glyph is centred against the two-line name/connection block.
const SYMBOL_SIZE: i32 = 28;
const SYMBOL_LEFT: i32 = 24;
const SYMBOL_TOP: i32 = 12;
/// Row pitch. Tall enough for the switch captions above the pair.
const ROW_HEIGHT: i32 = 64;
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Page {
    #[default]
    Main,
    Settings,
    Add,
    History,
}
impl Page {
    fn index(self) -> usize {
        match self {
            Self::Main => 0,
            Self::Settings => 1,
            Self::Add => 2,
            Self::History => 3,
        }
    }
    fn key(self) -> &'static str {
        match self {
            Self::Main => "main",
            Self::Settings => "settings",
            Self::Add => "add",
            Self::History => "history",
        }
    }
}
enum WorkerCommand {
    Discover(bool),
    Read(Vec<String>, bool),
    /// Link-only probe: the reply is used for connection state, not for the
    /// displayed percentage.
    Link(Vec<String>, bool),
    Support(u64, Box<SupportContext>, SupportDestination, Cancellation),
    Stop,
}
enum WorkerResult {
    Discovery(Result<DiscoveryReport, DeviceError>),
    Reading(String, Result<Reading, DeviceError>),
    LinkRead(String, Result<Reading, DeviceError>),
    Complete,
    RefreshRequest(String),
    Support(u64, Result<Option<String>, DeviceError>),
}
#[derive(Clone)]
enum SupportDestination {
    Clipboard,
    File(PathBuf),
    #[cfg(not(test))]
    FileDialog(usize),
}
struct SupportJob {
    token: u64,
    id: String,
    cancel: Cancellation,
    file: bool,
}
#[derive(Clone, Default)]
struct PanelView {
    scroll: i32,
    page: Page,
    /// Mirrored for the window procedure: a pinned panel has no drag handle.
    pinned: bool,
    threshold_error: bool,
    interval_error: bool,
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
    TogglePanel,
    RevealPanel,
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
    Presence,
    Threshold(bool),
    ThresholdEdited,
    Interval(bool),
    IntervalEdited,
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
    presentation: (String, String, DeviceKind, bool, String),
    /// Main page only: per-device tray and low-battery switches.
    tray: HWND,
    notify: HWND,
}
impl RowControls {
    /// Focusable children in tab order. Null holes are dropped on both the
    /// capture and the restore side, so anchors stay stable within a page.
    fn focusable(&self) -> Vec<HWND> {
        [self.percent, self.tray, self.notify, self.refresh]
            .into_iter()
            .chain(self.extra.iter().copied())
            .filter(|hwnd| !hwnd.is_null())
            .collect()
    }
    fn switches(&self) -> impl Iterator<Item = HWND> {
        [self.tray, self.notify]
            .into_iter()
            .filter(|hwnd| !hwnd.is_null())
    }
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
    connection_toasts: Vec<(String, bool)>,
    support_copies: Vec<String>,
    support_path: Option<PathBuf>,
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
    support_job: Option<SupportJob>,
    support_sequence: u64,
    support_feedback: Option<(String, bool)>,
    simulated: bool,
    refresh_count: u64,
    success_count: u64,
    bridge: Rc<UiBridge>,
    panel_fonts: [HFONT; 4],
    panel_icons: [HICON; 2],
    last_activity_refresh: Instant,
    /// Interval last handed to timer 1. A cadence re-check leaves an unchanged
    /// countdown alone instead of restarting it on every heartbeat.
    refresh_timer_seconds: Cell<u32>,
    /// Same for timer 4. Without it every burst of input would push the next
    /// link check a full interval into the future, and a second device that died
    /// while the user kept typing would go unnoticed.
    presence_timer_seconds: Cell<u32>,
    /// The window exists but is still hidden, waiting for the first discovery
    /// pass to settle the row count (and therefore the height).
    awaiting_first_pass: bool,
    picker_ids: Vec<String>,
    page: Page,
    page_scroll: [i32; 4],
    page_controls: Vec<HWND>,
    threshold_error: bool,
    interval_error: bool,
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
        // A charge state is an observation, not protocol metadata: a device that
        // only reports a percentage can still say it is charging.
        if reading.metadata_known
            || !matches!(reading.power_state, crate::device::PowerState::Unknown(_))
        {
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
/// Second line of a device row: how the device is attached and how old the
/// shown percentage is. Times are derived from the last confirmed reading, never
/// from an estimate.
fn row_detail(state: &DeviceState) -> String {
    let link = state.descriptor.connection.short_label();
    if !state.connected {
        return if state.connection_known {
            format!("{link} · 未连接")
        } else {
            format!("{link} · 连接状态未知")
        };
    }
    // The reading age lives in the header summary; repeating it on every row
    // just duplicated the same string.
    let mut text = format!("{link} · {}", usage_text(state));
    // History rides at the end of the same line: the last completed full-to-low
    // run is the number the user asked to see, and the tooltip carries the rest.
    if let Some(cycle) = state.usage_cycles.last() {
        text.push_str(&format!(" · 满电续航{}", cycle_duration(cycle.seconds)));
    }
    text
}
/// One-line summary for the main header: how many devices are listed, the lowest
/// value among them and how fresh the numbers are. Everything comes from the
/// rows on screen, so the bar can never disagree with the list below it.
fn main_summary(view: &PanelView) -> String {
    if view.rows.is_empty() {
        return "暂无设备".into();
    }
    let mut parts = vec![format!("{} 台设备", view.rows.len())];
    if let Some(percent) = view
        .rows
        .iter()
        .filter_map(|state| {
            state
                .reading
                .as_ref()
                .map(|reading| reading.normalized_percent())
        })
        .min()
    {
        parts.push(format!("最低 {percent}%"));
    }
    parts.push(updated_age(
        view.rows
            .iter()
            .filter_map(|state| state.last_success)
            .max(),
    ));
    parts.join(" · ")
}

/// Longest completed run, or `None` when the pack has not finished one yet.
fn longest_cycle(state: &DeviceState) -> Option<&manager::UsageCycle> {
    state.usage_cycles.iter().max_by_key(|cycle| cycle.seconds)
}
/// One line per device: the longest and the shortest completed full charge. A
/// single record states its length once; no record says so plainly instead of
/// borrowing the run in progress.
fn history_span(state: &DeviceState) -> String {
    let mut seconds: Vec<u64> = state
        .usage_cycles
        .iter()
        .map(|cycle| cycle.seconds)
        .collect();
    if seconds.is_empty() {
        return "尚无完整满电记录".into();
    }
    seconds.sort_unstable();
    let longest = cycle_duration(*seconds.last().unwrap());
    if seconds.len() == 1 {
        return format!("最长 {longest}");
    }
    format!("最长 {longest} · 最短 {}", cycle_duration(seconds[0]))
}
fn percent_text(percent: u8) -> String {
    format!("{percent}%")
}
/// Local `MM-DD HH:MM`, with the year when the stamp is not from this year.
fn stamp_text(unix: u64) -> String {
    let Some((year, month, day, hour, minute)) = local_stamp(unix) else {
        return "—".into();
    };
    let this_year = local_stamp(now()).map_or(year, |parts| parts.0);
    if year == this_year {
        format!("{month:02}-{day:02} {hour:02}:{minute:02}")
    } else {
        format!("{year}-{month:02}-{day:02} {hour:02}:{minute:02}")
    }
}
/// Unix seconds to local wall-clock parts. `None` when the value is out of
/// range or the time-zone conversion fails, so the caller prints a dash rather
/// than a wrong clock.
fn local_stamp(unix: u64) -> Option<(u16, u16, u16, u16, u16)> {
    use windows_sys::Win32::{
        Foundation::{FILETIME, SYSTEMTIME},
        Storage::FileSystem::FileTimeToLocalFileTime,
        System::Time::FileTimeToSystemTime,
    };
    unsafe {
        let ticks = unix
            .saturating_add(11_644_473_600)
            .checked_mul(10_000_000)?;
        let file = FILETIME {
            dwLowDateTime: ticks as u32,
            dwHighDateTime: (ticks >> 32) as u32,
        };
        let mut local: FILETIME = std::mem::zeroed();
        if FileTimeToLocalFileTime(&file, &mut local) == 0 {
            return None;
        }
        let mut parts: SYSTEMTIME = std::mem::zeroed();
        if FileTimeToSystemTime(&local, &mut parts) == 0 {
            return None;
        }
        Some((
            parts.wYear,
            parts.wMonth,
            parts.wDay,
            parts.wHour,
            parts.wMinute,
        ))
    }
}
/// The discharge in progress: when it started, where the value went and how
/// long it has run. Runs that began before the starting percentage was stored
/// are counted from 100%, the value a full charge starts from.
fn history_current_text(state: &DeviceState) -> String {
    if state
        .reading
        .as_ref()
        .is_some_and(|reading| reading.power_state == crate::device::PowerState::Charging)
    {
        return "当前 · 充电中".into();
    }
    let Some(since) = state.usage_since else {
        return "当前 · 尚未开始记录".into();
    };
    let current = state
        .reading
        .as_ref()
        .map_or(100, |reading| reading.normalized_percent());
    format!(
        "当前 {} → 现在 · {} → {} · 已用{}",
        stamp_text(since),
        percent_text(state.usage_from.unwrap_or(100)),
        percent_text(current),
        short_duration(now().saturating_sub(since))
    )
}
/// Duration of one discharge run. Unlike `short_duration` this keeps minutes for
/// multi-hour runs, because the point is comparing one cycle against another.
fn cycle_duration(seconds: u64) -> String {
    if seconds < 60 {
        return "不到1分钟".into();
    }
    if seconds < 3_600 {
        return format!("{}分钟", seconds / 60);
    }
    if seconds < 86_400 {
        let hours = seconds / 3_600;
        let minutes = seconds % 3_600 / 60;
        return if minutes == 0 {
            format!("{hours}小时")
        } else {
            format!("{hours}小时{minutes}分")
        };
    }
    let days = seconds / 86_400;
    let hours = seconds % 86_400 / 3_600;
    if hours == 0 || days >= 10 {
        format!("{days}天")
    } else {
        format!("{days}天{hours}小时")
    }
}
/// Hover text: full model name plus the recent full-to-low history.
fn tooltip_text(state: &DeviceState) -> String {
    let mut text = format!(
        "{} · {}",
        state.descriptor.name,
        state.descriptor.connection.label()
    );
    let Some(latest) = state.usage_cycles.last() else {
        return text;
    };
    text.push_str(&format!("\n满电续航 {}", cycle_duration(latest.seconds)));
    let recent: Vec<String> = state
        .usage_cycles
        .iter()
        .rev()
        .take(3)
        .map(|cycle| cycle_duration(cycle.seconds))
        .collect();
    if recent.len() > 1 {
        text.push_str(&format!("\n最近{}次：{}", recent.len(), recent.join(" / ")));
    }
    text
}
/// How long the current discharge cycle has been running. A charge is only
/// claimed when the device said so, or when the value came back higher than the
/// one recorded when the link dropped.
fn usage_text(state: &DeviceState) -> String {
    if state
        .reading
        .as_ref()
        .is_some_and(|reading| reading.power_state == crate::device::PowerState::Charging)
    {
        return "充电中".into();
    }
    let Some(since) = state.usage_since else {
        return "已使用—".into();
    };
    format!("已使用{}", short_duration(now().saturating_sub(since)))
}
fn short_duration(seconds: u64) -> String {
    if seconds < 60 {
        "不到1分钟".into()
    } else if seconds < 3_600 {
        format!("{}分钟", seconds / 60)
    } else if seconds < 86_400 {
        format!("{}小时", seconds / 3_600)
    } else {
        let days = seconds / 86_400;
        let hours = seconds % 86_400 / 3_600;
        if hours == 0 || days >= 10 {
            format!("{days}天")
        } else {
            format!("{days}天{hours}小时")
        }
    }
}
fn updated_age(last: Option<u64>) -> String {
    let Some(time) = last else {
        return "尚未读取".into();
    };
    let seconds = now().saturating_sub(time);
    if seconds < 60 {
        "刚刚更新".into()
    } else if seconds < 3_600 {
        format!("{} 分钟前", seconds / 60)
    } else if seconds < 86_400 {
        format!("{} 小时前", seconds / 3_600)
    } else {
        format!("{} 天前", seconds / 86_400)
    }
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
        "{{\"id\":{},\"name\":{},\"kind\":{},\"supported\":{},\"provider\":{},\"connection\":{},\"reading\":{},\"error\":{},\"diagnostics\":{}}}",
        json_string(&candidate.descriptor.id),
        json_string(&candidate.descriptor.name),
        json_string(candidate.descriptor.kind.key()),
        candidate.descriptor.supported,
        json_string(&candidate.descriptor.provider),
        json_string(candidate.descriptor.connection.key()),
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
        let devices=self.manager.devices.iter().map(|s|format!("{{\"id\":{},\"name\":{},\"kind\":{},\"connection\":{},\"supported\":{},\"selected\":{},\"removed\":{},\"show_tray\":{},\"tray_id\":{},\"connected\":{},\"last_success_unix\":{},\"usage_since_unix\":{},\"reading\":{},\"error\":{}}}",json_string(&s.descriptor.id),json_string(&s.descriptor.name),json_string(s.descriptor.kind.key()),json_string(s.descriptor.connection.key()),s.descriptor.supported,s.selected,s.removed,s.show_tray,self.tray_ids.iter().find(|(id,_)|*id==s.descriptor.id).map_or_else(||"null".into(),|(_,id)|id.to_string()),s.connected,opt_number(s.last_success),opt_number(s.usage_since),s.reading.as_ref().map_or_else(||"null".into(),reading_json),s.error.as_ref().map_or_else(||"null".into(),|e|json_string(&e.to_string())))).collect::<Vec<_>>().join(",");
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
                    "{v},\"page\":{},\"notifications_enabled\":{},\"threshold\":{},\"connection_notifications\":{},\"auto_refresh\":{},\"refresh_seconds\":{}}}",
                    json_string(self.page.key()),
                    self.manager.notifications_enabled,
                    self.manager.threshold,
                    self.manager.connection_notifications,
                    self.manager.auto_refresh,
                    self.manager.refresh_seconds
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
            pinned: self.manager.window_pinned,
            threshold_error: self.threshold_error,
            interval_error: self.interval_error,
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
                        if self.page == Page::Add {
                            SetWindowTextW(
                                row.refresh,
                                wide(if state.selected {
                                    "已添加"
                                } else if state.descriptor.supported {
                                    "添加"
                                } else {
                                    "—"
                                })
                                .as_ptr(),
                            );
                            EnableWindow(
                                row.refresh,
                                i32::from(state.descriptor.supported && !state.selected),
                            );
                            for (index, h) in row.extra.iter().enumerate() {
                                if index > 0 {
                                    // The second action is removal; its label and
                                    // enabled state do not follow the capture job.
                                    continue;
                                }
                                let file = index == 1;
                                let active = self
                                    .support_job
                                    .as_ref()
                                    .is_some_and(|job| job.id == row.id && job.file == file);
                                let done = self
                                    .support_feedback
                                    .as_ref()
                                    .is_some_and(|(id, f)| *id == row.id && *f == file);
                                SetWindowTextW(
                                    *h,
                                    wide(if active {
                                        "采集中"
                                    } else if done {
                                        if file { "已导出" } else { "已复制" }
                                    } else if file {
                                        "导出文件"
                                    } else {
                                        "复制信息"
                                    })
                                    .as_ptr(),
                                );
                                EnableWindow(
                                    *h,
                                    i32::from(!self.busy && self.support_job.is_none()),
                                );
                            }
                        }
                    }
                }
                InvalidateRect(self.panel, null(), 1);
            }
        }
    }
    /// `rescan` lets an explicit user or power event rediscover when nothing is
    /// currently present; the input-driven path must stay cheap.
    /// Cheap link check: query every monitored device once, including the ones
    /// currently hidden, so a power switch shows up in seconds instead of at the
    /// next battery refresh. No enumeration, no BLE scan, no writes.
    fn presence(&mut self) {
        if self.busy {
            return;
        }
        let ids: Vec<_> = self
            .manager
            .selected()
            .map(|s| s.descriptor.id.clone())
            .collect();
        if !ids.is_empty() {
            self.read_ids(ids, true);
        }
    }
    /// `rescan` lets an explicit user or power event rediscover when nothing is
    /// currently present; the input-driven path has to stay cheap.
    fn refresh(&mut self, rescan: bool) {
        // Only present devices are queried. A confirmed disconnect is left to the
        // next discovery pass instead of being read again on every cycle.
        let ids: Vec<_> = self
            .manager
            .visible()
            .map(|s| s.descriptor.id.clone())
            .collect();
        if ids.is_empty() {
            if rescan || self.manager.selected().next().is_none() {
                self.discover();
            }
            return;
        }
        self.refresh_ids(ids);
    }
    fn refresh_ids(&mut self, ids: Vec<String>) {
        self.read_ids(ids, false);
    }
    /// `link` selects the link-only probe, which never replaces the displayed
    /// percentage or its timestamp.
    fn read_ids(&mut self, ids: Vec<String>, link: bool) {
        if self.busy {
            // A probe is cheap to skip; a value refresh must not be dropped.
            self.pending |= !link;
            return;
        }
        if ids.is_empty() {
            if !link {
                self.discover();
            }
            return;
        }
        self.busy = true;
        self.refresh_count += 1;
        let command = if link {
            WorkerCommand::Link(ids, self.simulated)
        } else {
            WorkerCommand::Read(ids, self.simulated)
        };
        if self.sender.send(command).is_err() {
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
                    WorkerResult::LinkRead(id, _) => {
                        WorkerResult::LinkRead(id, Err(simulated_error()))
                    }
                    WorkerResult::Complete => WorkerResult::Complete,
                    WorkerResult::RefreshRequest(id) => WorkerResult::RefreshRequest(id),
                    result @ WorkerResult::Support(_, _) => result,
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
                        for state in self.manager.devices.iter().filter(|s| s.connected) {
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
                    // The row count is settled: this is the moment the panel may
                    // appear, at its final height.
                    self.reveal_panel();
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
                // Link probes never touch the cached value, so they need no save.
                WorkerResult::LinkRead(id, result) => {
                    self.manager.apply_link(&id, result, now());
                    self.rebuild_rows();
                }
                WorkerResult::Complete => {
                    self.busy = false;
                    if self.pending_discovery {
                        self.pending_discovery = false;
                        self.discover();
                    } else if self.pending {
                        self.pending = false;
                        self.refresh(true);
                    }
                }
                WorkerResult::RefreshRequest(id) => self.refresh_ids(vec![id]),
                WorkerResult::Support(token, result) => self.complete_support(token, result),
            }
            self.update_icons(false);
            if fresh_result {
                self.deliver_alerts();
            } else {
                self.prune_alerts();
            }
            // A link transition can be queued by a failed query as well, so it is
            // not tied to a successful reading.
            if !self.manager.pending_connections.is_empty() {
                self.deliver_connections();
            }
            self.publish_view();
        }
    }
    fn cancel_support(&mut self) {
        if let Some(job) = self.support_job.take() {
            job.cancel.cancel();
        }
        self.support_feedback = None;
    }
    fn request_support(&mut self, id: &str, file: bool) {
        if self.busy || self.support_job.is_some() || self.page != Page::Add {
            return;
        }
        let Some(state) = self.manager.devices.iter().find(|s| s.descriptor.id == id) else {
            return;
        };
        let context = SupportContext {
            descriptor: state.descriptor.clone(),
            reading: state.reading.clone(),
            last_success: state.last_success,
            error: state.error.clone(),
            connected: state.connected,
            connection_known: state.connection_known,
        };
        let destination = if file {
            #[cfg(test)]
            {
                let Some(path) = self.effects.support_path.clone() else {
                    return;
                };
                SupportDestination::File(path)
            }
            #[cfg(not(test))]
            {
                SupportDestination::FileDialog(self.panel as usize)
            }
        } else {
            SupportDestination::Clipboard
        };
        self.support_sequence = self.support_sequence.wrapping_add(1);
        let token = self.support_sequence;
        let cancel = Cancellation::default();
        if self
            .sender
            .send(WorkerCommand::Support(
                token,
                Box::new(context),
                destination,
                cancel.clone(),
            ))
            .is_err()
        {
            self.operation_error("无法启动适配信息采集");
            return;
        }
        self.support_feedback = None;
        self.support_job = Some(SupportJob {
            token,
            id: id.into(),
            cancel,
            file,
        });
        self.busy = true;
        self.publish_view();
    }
    fn complete_support(&mut self, token: u64, result: Result<Option<String>, DeviceError>) {
        if self
            .support_job
            .as_ref()
            .is_none_or(|job| job.token != token)
        {
            return;
        }
        let job = self.support_job.take().unwrap();
        if job.cancel.check().is_err() || self.page != Page::Add || self.panel.is_null() {
            return;
        }
        match result {
            Ok(json) => {
                if let Some(json) = json {
                    #[cfg(test)]
                    self.effects.support_copies.push(json);
                    #[cfg(not(test))]
                    if let Err(error) = copy_text(self.panel, &json) {
                        self.operation_error(&error);
                        return;
                    }
                }
                self.support_feedback = Some((job.id, job.file));
            }
            Err(DeviceError::Cancelled) => {}
            Err(error) => self.operation_error(&format!("适配信息采集失败：{error}")),
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
    /// Deliver confirmed link transitions. The manager only queues transitions
    /// whose previous state was known, so startup and first discovery stay quiet.
    fn deliver_connections(&mut self) {
        if self.manager.persistence_error.is_some() {
            return;
        }
        for (id, connected) in std::mem::take(&mut self.manager.pending_connections) {
            let Some(state) = self
                .manager
                .devices
                .iter()
                .find(|s| s.descriptor.id == id && s.selected && !s.removed)
            else {
                continue;
            };
            let name = Manager::display_name(&self.manager.devices, &id, true, true);
            let percent = state
                .reading
                .as_ref()
                .map(|reading| reading.normalized_percent());
            let shell_id = self
                .trays
                .iter()
                .find(|t| t.id == id)
                .or(self.fallback.as_ref())
                .or(self.trays.first())
                .map(|t| t.shell_id);
            #[cfg(test)]
            let shell_id = shell_id.or(Some(1));
            if let Some(shell_id) = shell_id {
                let accepted = crate::notifications::show_connection(
                    self.hwnd, shell_id, &name, connected, percent,
                );
                self.notification_last_accepted = accepted;
                self.notification_submissions += 1;
            }
            #[cfg(test)]
            self.effects
                .connection_toasts
                .push((name.clone(), connected));
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
                MENU_REFRESH => self.refresh(true),
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
            // The settings page is a fixed form now: switches live on the main
            // page and removal moved to the add page.
            Page::Settings => Vec::new(),
            Page::Add => self
                .picker_ids
                .iter()
                .filter_map(|id| self.manager.devices.iter().find(|s| s.descriptor.id == *id))
                .cloned()
                .collect(),
            // Every device that is connected right now, longest run first. A
            // pack without a completed run is not excluded: its row still says
            // when the current discharge started and where the value went, which
            // is the question this page answers for a new device.
            Page::History => {
                let mut states: Vec<DeviceState> = self
                    .manager
                    .selected()
                    .filter(|state| state.connected)
                    .cloned()
                    .collect();
                states.sort_by(|a, b| {
                    longest_cycle(b)
                        .map_or(0, |cycle| cycle.seconds)
                        .cmp(&longest_cycle(a).map_or(0, |cycle| cycle.seconds))
                        .then_with(|| a.descriptor.name.cmp(&b.descriptor.name))
                });
                states
            }
        }
    }
    fn row_top(&self) -> i32 {
        match self.page {
            Page::Main => 68,
            Page::Settings => 320,
            Page::Add | Page::History => 76,
        }
    }
    /// True when every device in the settings list has both per-device switches
    /// on. An empty list counts as "off" so the master switch cannot claim a
    /// state it does not govern.
    fn all_switches_on(&self) -> bool {
        let mut any = false;
        for state in self.manager.selected() {
            any = true;
            if !(state.show_tray && state.notify) {
                return false;
            }
        }
        any
    }
    fn viewport_top(&self) -> i32 {
        if self.page == Page::Main { 68 } else { 64 }
    }
    /// Keep the master switch's tick and label in step with the device rows.
    fn sync_all_switch(&self) {
        if self.panel.is_null() {
            return;
        }
        unsafe {
            let all = GetDlgItem(self.panel, ALL_SWITCHES as i32);
            if all.is_null() {
                return;
            }
            let on = self.all_switches_on();
            let before = SendMessageW(all, BM_GETCHECK, 0, 0);
            if before != usize::from(on) as isize {
                SendMessageW(all, BM_SETCHECK, usize::from(on), 0);
                InvalidateRect(all, null(), 0);
            }
        }
    }
    fn content_height(&self) -> i32 {
        if self.rows.is_empty() {
            0
        } else {
            self.row_top() - self.viewport_top() + self.rows.len() as i32 * ROW_HEIGHT
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
                Page::Main => 200,
                Page::Settings => 304,
                Page::Add | Page::History => 180,
            }
        } else {
            self.row_top() + self.rows.len() as i32 * ROW_HEIGHT + self.footer_height()
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
            if matches!(GetDlgCtrlID(hwnd) as u32, THRESHOLD | REFRESH_INTERVAL) {
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
            for (index, (height, weight)) in [(15, 400), (17, 700), (26, 700), (12, 400)]
                .into_iter()
                .enumerate()
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
                        112
                    } else {
                        self.panel_height() - 38
                    },
                    if self.rows.is_empty() { 180 } else { 130 },
                    32,
                    0,
                );
                self.place(get(SETTINGS), 486, self.panel_height() - 38, 56, 32, 0);
                // Header link, left of the two column captions, so it never
                // competes with the row content below.
                self.place(get(HISTORY), 366, 18, 84, 24, 0);
            } else {
                self.place(get(BACK), 18, 12, 32, 36, 0);
                if self.page == Page::Add {
                    self.place(get(PICK_REFRESH), 450, 14, 94, 32, 0);
                }
                if self.page == Page::Settings {
                    for (id, x, y, w, h) in [
                        (STARTUP, 24, 68, 248, 40),
                        (CONNECTIONS, 288, 68, 248, 40),
                        (ALERTS, 24, 116, 248, 40),
                        (113, 288, 120, 168, 32),
                        (THRESHOLD, 464, 122, 56, 28),
                        (114, 524, 122, 12, 28),
                        (AUTO_REFRESH, 24, 164, 248, 40),
                        (INTERVAL_LABEL, 288, 168, 120, 32),
                        (REFRESH_INTERVAL, 436, 170, 56, 28),
                        (INTERVAL_UNIT, 496, 170, 40, 28),
                        (INTERVAL_HINT, 24, 202, 512, 22),
                        (ALL_SWITCHES, 24, 232, 248, 40),
                        (WINDOW_PIN, 288, 232, 248, 40),
                    ] {
                        let y = y - self.scroll;
                        self.place(get(id), x, y, w, h, if id == INTERVAL_HINT { 3 } else { 0 });
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
                let y = self.row_top() + index as i32 * ROW_HEIGHT - self.scroll;
                let visible = y >= self.viewport_top()
                    && y + 56 <= self.panel_height() - self.footer_height();
                for control in [row.model, row.percent, row.refresh]
                    .into_iter()
                    .chain(row.switches())
                    .chain(row.extra.iter().copied())
                {
                    if !control.is_null() {
                        ShowWindow(control, if visible { SW_SHOW } else { SW_HIDE });
                    }
                }
                match self.page {
                    Page::Main => {
                        // Two storeys: name/battery/percentage on top, the
                        // connection line plus both switches underneath. The
                        // switches sit at the right edge of the second line so
                        // the model name keeps almost its full width.
                        self.place(row.model, 64, y + 8, 178, 26, 1);
                        self.place(row.percent, 378, y + 8, 72, 28, 2);
                        // One pill per control keeps a 4px gap between the
                        // pair while both stay flush with the 536 right margin.
                        self.place(row.tray, 456, y + 8, 38, 26, 0);
                        self.place(row.notify, 498, y + 8, 38, 26, 0);
                    }
                    Page::Settings => {
                        // The switches moved to the main page; only the row's
                        // own removal action remains here.
                        self.place(row.model, 64, y + 6, 400, 40, 1);
                        if let Some(remove) = row.extra.first() {
                            self.place(*remove, 494, y + 8, 50, 36, 0);
                        }
                    }
                    Page::History => {
                        // Two lines like the main page: name plus the two
                        // completed-run durations on top, the run in progress
                        // underneath (painted by the panel).
                        self.place(row.model, 64, y + 8, 232, 26, 1);
                    }
                    Page::Add => {
                        self.place(row.model, 64, y + 6, 232, 40, 1);
                        self.place(row.percent, 302, y + 6, 46, 40, 0);
                        self.place(row.refresh, 428, y + 6, 52, 36, 0);
                        if !row.presentation.3 {
                            ShowWindow(row.refresh, SW_HIDE);
                        }
                        if let Some(copy) = row.extra.first() {
                            self.place(*copy, 358, y + 6, 66, 36, 0);
                        }
                        if let Some(remove) = row.extra.get(1) {
                            self.place(*remove, 484, y + 6, 52, 36, 0);
                            let monitored = self
                                .manager
                                .devices
                                .iter()
                                .any(|s| s.descriptor.id == row.id && s.selected);
                            ShowWindow(
                                *remove,
                                if monitored && visible {
                                    SW_SHOW
                                } else {
                                    SW_HIDE
                                },
                            );
                        }
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
            // The bottom edge is the fixed one: the panel rests on the work-area
            // bottom, so a page with more rows grows upwards instead of pushing
            // its footer behind the taskbar.
            let left = position
                .left
                .clamp(work.left, (work.right - width).max(work.left));
            let top =
                (position.bottom - height).clamp(work.top, (work.bottom - height).max(work.top));
            SetWindowPos(
                self.panel,
                null_mut(),
                left,
                top,
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
                    .chain(row.switches())
                    .chain(row.extra.iter().copied())
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
                            Manager::display_name(
                                &states,
                                &s.descriptor.id,
                                true,
                                self.page != Page::Main,
                            ),
                            s.descriptor.name.clone(),
                            s.descriptor.kind,
                            s.descriptor.supported,
                            tooltip_text(s),
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
                let name = Manager::display_name(
                    &states,
                    &state.descriptor.id,
                    true,
                    self.page != Page::Main,
                );
                let model = create_child(
                    self.panel,
                    "STATIC",
                    &name,
                    SS_LEFT | SS_NOPREFIX | SS_CENTERIMAGE | 0x4000,
                    ROW_BASE + index as u32 * 3,
                );
                let mut extra = Vec::new();
                let mut tray = null_mut();
                let mut notify = null_mut();
                let (percent, refresh) = match self.page {
                    // The main page carries the value and both per-device
                    // switches; the manual refresh still lives in the tray menu.
                    Page::Main => {
                        tray = create_child(
                            self.panel,
                            "BUTTON",
                            &format!("{name}，显示托盘"),
                            WS_TABSTOP | BS_AUTOCHECKBOX as u32,
                            MAIN_TRAY_BASE + index as u32,
                        );
                        notify = create_child(
                            self.panel,
                            "BUTTON",
                            &format!("{name}，低电提醒"),
                            WS_TABSTOP | BS_AUTOCHECKBOX as u32,
                            MAIN_NOTIFY_BASE + index as u32,
                        );
                        style_control(tray, true);
                        style_control(notify, true);
                        SendMessageW(tray, BM_SETCHECK, usize::from(state.show_tray), 0);
                        SendMessageW(notify, BM_SETCHECK, usize::from(state.notify), 0);
                        EnableWindow(notify, i32::from(self.manager.notifications_enabled));
                        (
                            create_child(
                                self.panel,
                                "STATIC",
                                "—%",
                                SS_LEFT | SS_NOPREFIX | SS_CENTERIMAGE,
                                ROW_BASE + index as u32 * 3 + 1,
                            ),
                            null_mut(),
                        )
                    }
                    // No device rows on this page: it is the fixed preferences
                    // form plus the one master switch.
                    Page::Settings => (null_mut(), null_mut()),
                    // Read-only rows: the name is the only control. The two
                    // durations are painted by the panel, which also gives them
                    // the secondary colour a static would not.
                    Page::History => (null_mut(), null_mut()),
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
                                "—"
                            },
                            WS_TABSTOP | BS_PUSHBUTTON as u32,
                            ADD_BASE + index as u32 * ADD_STRIDE,
                        ),
                    ),
                };
                if self.page == Page::Add {
                    style_control(refresh, false);
                }
                if self.page == Page::Add {
                    // Copy and remove only: export wrote the same report to a
                    // file instead of the clipboard, so it duplicated this one.
                    for (action, label) in [(1, "复制信息"), (2, "移除")] {
                        let accessible = if action == 2 {
                            format!("{name}，移除监控")
                        } else {
                            label.to_string()
                        };
                        let h = create_child(
                            self.panel,
                            "BUTTON",
                            &accessible,
                            WS_TABSTOP | BS_PUSHBUTTON as u32,
                            ADD_BASE + index as u32 * ADD_STRIDE + action,
                        );
                        style_control(h, false);
                        extra.push(h);
                    }
                }
                let mut text = wide(&tooltip_text(state));
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
                if let Ok(symbol) = SymbolIcon::new(SYMBOL_SIZE * dpi / 96, state.descriptor.kind) {
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
                            tooltip_text(state),
                        ),
                        tray,
                        notify,
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
            for (action, control) in row.focusable().into_iter().enumerate() {
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
                    let controls = row.focusable();
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
            let all_on = self.all_switches_on();
            let mut add = |class: &str, label: &str, style: u32, id: u32| {
                let h = create_child(self.panel, class, label, style, id);
                self.page_controls.push(h);
                h
            };
            if self.page == Page::Main {
                for (id, label) in [
                    (ADD, "＋ 添加设备"),
                    (SETTINGS, "设置"),
                    (HISTORY, "历史使用"),
                ] {
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
                        (
                            CONNECTIONS,
                            "连接提示",
                            self.manager.connection_notifications,
                        ),
                        (AUTO_REFRESH, "自动刷新", self.manager.auto_refresh),
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
                    add(
                        "STATIC",
                        "刷新间隔",
                        SS_LEFT | SS_CENTERIMAGE,
                        INTERVAL_LABEL,
                    );
                    let interval = add(
                        "EDIT",
                        &(self.manager.refresh_seconds / 60).to_string(),
                        WS_TABSTOP | ES_CENTER as u32 | ES_AUTOHSCROLL as u32,
                        REFRESH_INTERVAL,
                    );
                    SendMessageW(interval, EM_SETLIMITTEXT, 8, 0);
                    EnableWindow(interval, i32::from(self.manager.auto_refresh));
                    add("STATIC", "分钟", SS_LEFT | SS_CENTERIMAGE, INTERVAL_UNIT);
                    add(
                        "STATIC",
                        "调长间隔可延长外设续航；支持电量通知的外设会优先用通知更新。",
                        SS_LEFT,
                        INTERVAL_HINT,
                    );
                    // One switch for every device. The per-device pair lives on
                    // the main page, next to the device it belongs to.
                    let all = add(
                        "BUTTON",
                        "所有设备的托盘与提醒",
                        WS_TABSTOP | BS_AUTOCHECKBOX as u32,
                        ALL_SWITCHES,
                    );
                    style_control(all, true);
                    SendMessageW(all, BM_SETCHECK, usize::from(all_on), 0);
                    let pin = add(
                        "BUTTON",
                        "窗口固定",
                        WS_TABSTOP | BS_AUTOCHECKBOX as u32,
                        WINDOW_PIN,
                    );
                    style_control(pin, true);
                    SendMessageW(pin, BM_SETCHECK, usize::from(self.manager.window_pinned), 0);
                }
            }
        }
    }
    fn switch_page(&mut self, page: Page) {
        self.cancel_support();
        self.page_scroll[self.page.index()] = self.scroll;
        self.threshold_error = false;
        self.interval_error = false;
        self.clear_rows();
        self.page = page;
        self.scroll = self.page_scroll[page.index()];
        if page == Page::Add {
            self.picker_ids = self
                .manager
                .devices
                .iter()
                .filter(|s| s.connected)
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
                    // Tool window: no taskbar button and no Alt-Tab entry, the
                    // way the shell's own flyouts behave.
                    WS_EX_CONTROLPARENT | WS_EX_TOOLWINDOW,
                    wide(PANEL_CLASS).as_ptr(),
                    wide("外设电量").as_ptr(),
                    // Frameless, like the clock flyout: no caption to drag, so
                    // the header band handles dragging instead (WM_NCHITTEST).
                    WS_POPUP | WS_CLIPCHILDREN | WS_VSCROLL,
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
                // Only after the first layout: the window height follows the
                // content, and anchoring before that would be off by the delta.
                self.apply_panel_position();
                // Rows restored from disk are not proof of a live link. Without
                // this the panel opens at the height of every saved device and
                // then collapses to the connected ones — a window that visibly
                // jumps and hangs over the desktop. Wait for the first pass.
                self.awaiting_first_pass = !cfg!(test)
                    && self
                        .manager
                        .devices
                        .iter()
                        .any(|state| !state.connection_known);
                if self.awaiting_first_pass {
                    SetTimer(self.hwnd, REVEAL_TIMER, 500, None);
                }
            }
            self.publish_view();
            // Opening the window is the moment the user looks, so refresh the
            // inventory now instead of showing a row that may be minutes old.
            // A pass already in flight will update the same rows anyway.
            if !self.busy {
                self.discover();
            }
            if !self.awaiting_first_pass {
                ShowWindow(self.panel, if cfg!(test) { SW_HIDE } else { SW_RESTORE });
                if !cfg!(test) {
                    SetForegroundWindow(self.panel);
                }
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
        self.cancel_support();
        self.close_picker();
        self.remember_panel_position();
        self.awaiting_first_pass = false;
        unsafe {
            KillTimer(self.hwnd, REVEAL_TIMER);
        }
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
            HISTORY => self.switch_page(Page::History),
            WINDOW_PIN => {
                self.manager.window_pinned = !self.manager.window_pinned;
                self.persist();
                // Pinning snaps the window back to the corner immediately;
                // unpinning leaves it where it is so it can be dragged.
                if self.manager.window_pinned {
                    self.apply_panel_position();
                }
            }
            ALL_SWITCHES => {
                // Flip every device the settings list shows. A mixed state turns
                // everything on, which is what a half-lit master switch promises.
                let target = !self.all_switches_on();
                for state in &mut self.manager.devices {
                    if state.selected && !state.removed {
                        state.show_tray = target;
                        state.notify = target;
                    }
                }
                self.persist();
                unsafe {
                    for row in &self.rows {
                        for control in row.switches() {
                            SendMessageW(control, BM_SETCHECK, usize::from(target), 0);
                        }
                    }
                }
                self.update_icons(false);
            }
            PICK_REFRESH => {
                self.picker_ids = self
                    .manager
                    .devices
                    .iter()
                    .filter(|s| s.connected)
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
                        if !row.notify.is_null() {
                            EnableWindow(row.notify, i32::from(self.manager.notifications_enabled));
                        }
                    }
                }
            }
            CONNECTIONS => {
                self.manager.connection_notifications = !self.manager.connection_notifications;
                self.persist();
            }
            AUTO_REFRESH => {
                self.manager.auto_refresh = !self.manager.auto_refresh;
                self.persist();
                unsafe {
                    EnableWindow(
                        GetDlgItem(self.panel, REFRESH_INTERVAL as i32),
                        i32::from(self.manager.auto_refresh),
                    );
                }
            }
            id if id >= ADD_BASE => {
                let index = ((id - ADD_BASE) / ADD_STRIDE) as usize;
                let action = (id - ADD_BASE) % ADD_STRIDE;
                if let Some(row) = self.rows.get(index) {
                    let device_id = row.id.clone();
                    match action {
                        0 => {
                            if self.manager.devices.iter().any(|s| {
                                s.descriptor.id == device_id
                                    && s.descriptor.supported
                                    && !s.selected
                            }) {
                                self.manager.select(&device_id);
                                self.persist();
                                self.update_icons(false);
                                self.publish_view();
                                self.refresh_ids(vec![device_id]);
                            }
                        }
                        1 => self.request_support(&device_id, false),
                        _ => {
                            // Removal moved here from the settings list.
                            self.manager.remove(&device_id);
                            self.persist();
                            self.rebuild_rows();
                            self.update_icons(false);
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
        self.apply_refresh_timer();
        self.sync_preferences();
        self.publish_view();
    }
    /// A per-device switch on the main page. The button already flipped itself,
    /// so the stored preference follows the click and a rejected write puts both
    /// the model and the button back.
    fn main_switch(&mut self, index: usize, action: usize) {
        let Some(device_id) = self.rows.get(index).map(|row| row.id.clone()) else {
            return;
        };
        let previous = self.manager.clone();
        if action == 0 {
            self.manager.toggle_tray(&device_id);
        } else if let Some(state) = self
            .manager
            .devices
            .iter_mut()
            .find(|s| s.descriptor.id == device_id)
        {
            state.notify = !state.notify;
        }
        self.persist();
        if let Some(error) = self.manager.persistence_error.clone() {
            self.manager = previous;
            self.manager.persistence_error = Some(error.clone());
            self.sync_preferences();
            self.operation_error(&format!("无法保存设置：{error}"));
            return;
        }
        self.update_icons(false);
        self.sync_preferences();
        self.publish_view();
    }
    /// Idle means no keyboard or mouse input has reached this window for a
    /// while. Both background timers slow down then, and a single key press
    /// brings the active cadence back immediately.
    fn idle(&self) -> bool {
        Instant::now()
            .saturating_duration_since(self.bridge.activity_seen.get())
            .as_secs()
            >= manager::IDLE_AFTER_SECONDS
    }
    /// The refresh cadence is owned by the tray window timer, so every settings
    /// change has to restate it. Disabling the setting stops the periodic pass
    /// entirely instead of leaving a hidden timer running. The interval itself
    /// comes from the manager — stored value plus the adaptive rules — and an
    /// unchanged interval leaves the running countdown alone.
    fn apply_refresh_timer(&self) -> bool {
        if self.hwnd.is_null() {
            return false;
        }
        if !self.manager.auto_refresh {
            unsafe {
                KillTimer(self.hwnd, 1);
            }
            self.refresh_timer_seconds.set(0);
            return true;
        }
        let seconds = self.manager.refresh_cadence(self.idle());
        if self.refresh_timer_seconds.get() == seconds {
            return true;
        }
        unsafe {
            KillTimer(self.hwnd, 1);
            let millis = seconds.saturating_mul(1000).max(30_000);
            if SetTimer(self.hwnd, 1, millis, None) == 0 {
                return false;
            }
        }
        self.refresh_timer_seconds.set(seconds);
        true
    }
    /// Re-arms the link-check timer from the state this session is in now. Only
    /// a changed cadence touches the timer: an unchanged one keeps counting, so
    /// continuous input cannot starve the probe. Input arriving after an idle
    /// stretch does change it, which restores the active cadence immediately.
    fn apply_presence_timer(&self) -> bool {
        if self.hwnd.is_null() {
            return false;
        }
        let seconds = self.manager.presence_cadence(self.idle());
        if self.presence_timer_seconds.get() == seconds {
            return true;
        }
        let armed = unsafe {
            SetTimer(
                self.hwnd,
                PRESENCE_TIMER,
                seconds.saturating_mul(1000),
                None,
            ) != 0
        };
        if armed {
            self.presence_timer_seconds.set(seconds);
        }
        armed
    }
    /// Put the panel where it belongs: the remembered left edge and the bottom
    /// it rested on, or — the first time — centred horizontally on the work-area
    /// bottom, the way the clock flyout hangs off the taskbar. A remembered spot
    /// that no longer touches a monitor is dropped instead of stranding it.
    fn apply_panel_position(&self) {
        if self.panel.is_null() {
            return;
        }
        unsafe {
            let mut rect: RECT = std::mem::zeroed();
            if GetWindowRect(self.panel, &mut rect) == 0 {
                return;
            }
            let width = rect.right - rect.left;
            let height = rect.bottom - rect.top;
            // Pinned: the bottom-right corner of the monitor under the pointer,
            // the way the shell's own flyouts sit above the tray. Unpinned: the
            // spot the window was dragged to, and that corner when there is none.
            let corner = || {
                let mut cursor = POINT { x: 0, y: 0 };
                if GetCursorPos(&mut cursor) == 0 {
                    cursor = POINT { x: 0, y: 0 };
                }
                let work = work_area_at(cursor);
                ((work.right - width).max(work.left), work.bottom)
            };
            let (left, bottom) = if self.manager.window_pinned {
                corner()
            } else {
                self.manager
                    .panel_anchor
                    .filter(|(left, bottom)| anchor_on_some_monitor(*left, *bottom, width))
                    .unwrap_or_else(corner)
            };
            // Clamp against the monitor that holds the anchor, then derive the
            // top from the bottom edge: a taller page grows upwards.
            let work = work_area_at(POINT {
                x: left,
                y: (bottom - 1).max(0),
            });
            let left = left.clamp(work.left, (work.right - width).max(work.left));
            let bottom = bottom.clamp(work.top, work.bottom);
            let top = (bottom - height).max(work.top);
            SetWindowPos(
                self.panel,
                null_mut(),
                left,
                top,
                width,
                height,
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
    }
    /// Show the panel once the row count is settled: after the first discovery
    /// pass, or after the 500 ms guard if the worker is slow. Called twice is
    /// harmless — the second call returns immediately.
    fn reveal_panel(&mut self) {
        unsafe {
            KillTimer(self.hwnd, REVEAL_TIMER);
        }
        if !self.awaiting_first_pass {
            return;
        }
        self.awaiting_first_pass = false;
        if self.panel.is_null() {
            return;
        }
        self.layout_panel();
        if !cfg!(test) {
            unsafe {
                ShowWindow(self.panel, SW_RESTORE);
                SetForegroundWindow(self.panel);
            }
        }
    }
    /// Remember the left edge and the bottom edge for the next run. Called
    /// before the window is destroyed, so a moved window survives a restart.
    fn remember_panel_position(&mut self) {
        if self.panel.is_null() || self.manager.window_pinned {
            return;
        }
        unsafe {
            let mut rect: RECT = std::mem::zeroed();
            if GetWindowRect(self.panel, &mut rect) == 0 {
                return;
            }
            let anchor = Some((rect.left, rect.bottom));
            if self.manager.panel_anchor != anchor {
                self.manager.panel_anchor = anchor;
                self.persist();
            }
        }
    }
    fn sync_preferences(&self) {
        if self.panel.is_null() {
            return;
        }
        unsafe {
            match self.page {
                Page::Settings => {
                    for (id, checked) in [
                        (ALERTS, self.manager.notifications_enabled),
                        (CONNECTIONS, self.manager.connection_notifications),
                        (AUTO_REFRESH, self.manager.auto_refresh),
                        (WINDOW_PIN, self.manager.window_pinned),
                    ] {
                        SendMessageW(
                            GetDlgItem(self.panel, id as i32),
                            BM_SETCHECK,
                            usize::from(checked),
                            0,
                        );
                    }
                    EnableWindow(
                        GetDlgItem(self.panel, THRESHOLD as i32),
                        i32::from(self.manager.notifications_enabled),
                    );
                    EnableWindow(
                        GetDlgItem(self.panel, REFRESH_INTERVAL as i32),
                        i32::from(self.manager.auto_refresh),
                    );
                    self.sync_all_switch();
                }
                Page::Main => {
                    for row in &self.rows {
                        let Some(state) = self
                            .manager
                            .devices
                            .iter()
                            .find(|s| s.descriptor.id == row.id)
                        else {
                            continue;
                        };
                        if !row.tray.is_null() {
                            SendMessageW(row.tray, BM_SETCHECK, usize::from(state.show_tray), 0);
                        }
                        if !row.notify.is_null() {
                            SendMessageW(row.notify, BM_SETCHECK, usize::from(state.notify), 0);
                            EnableWindow(row.notify, i32::from(self.manager.notifications_enabled));
                        }
                    }
                }
                // Read-only pages keep no switches to synchronise.
                Page::Add | Page::History => {}
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
            if self.page == Page::Settings {
                targets.push((row.refresh, Some(index)));
            }
            for h in &row.extra {
                targets.push((*h, Some(index)));
            }
            if self.page == Page::Add {
                targets.push((row.refresh, Some(index)));
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
            .map(|index| {
                (
                    self.row_top() - self.viewport_top() + index as i32 * ROW_HEIGHT,
                    56,
                )
            })
            .or_else(|| {
                if self.page == Page::Settings {
                    match unsafe { GetDlgCtrlID(hwnd) } as u32 {
                        STARTUP => Some((4, 40)),
                        ALERTS | CONNECTIONS => Some((52, 40)),
                        THRESHOLD => Some((104, 32)),
                        AUTO_REFRESH => Some((152, 40)),
                        REFRESH_INTERVAL => Some((156, 32)),
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
    fn submit_refresh_interval(&mut self, enter: bool) {
        if self.page != Page::Settings {
            return;
        }
        unsafe {
            let edit = GetDlgItem(self.panel, REFRESH_INTERVAL as i32);
            let mut text = [0u16; 32];
            let len = GetWindowTextW(edit, text.as_mut_ptr(), 32);
            let text = String::from_utf16_lossy(&text[..len as usize]);
            let parsed = if !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()) {
                text.parse::<u32>()
                    .ok()
                    .filter(|n| manager::REFRESH_MINUTES.contains(n))
            } else {
                None
            };
            if let Some(minutes) = parsed {
                let previous = self.manager.refresh_seconds;
                self.manager.refresh_seconds = minutes * 60;
                self.interval_error = false;
                SendMessageW(edit, EM_HIDEBALLOONTIP, 0, 0);
                self.persist();
                if let Some(error) = &self.manager.persistence_error {
                    self.manager.refresh_seconds = previous;
                    SetWindowTextW(edit, wide(&(previous / 60).to_string()).as_ptr());
                    self.operation_error(&format!("无法保存刷新间隔：{error}"));
                }
            } else {
                self.interval_error = true;
                if enter {
                    SendMessageW(edit, EM_SETSEL, 0, -1);
                } else {
                    SetWindowTextW(
                        edit,
                        wide(&(self.manager.refresh_seconds / 60).to_string()).as_ptr(),
                    );
                }
                if !cfg!(test) {
                    let title = wide("刷新间隔");
                    let message = wide("请输入1–60的整数（分钟）");
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
        self.apply_refresh_timer();
        self.publish_view();
    }
    fn process_events(&mut self) {
        while let Some(event) = self.bridge.next() {
            match event {
                UiEvent::Control(command) => match command {
                    REFRESH => self.refresh(true),
                    QUIT => self.quit(),
                    SIMULATE_ERROR => {
                        self.simulated = true;
                        self.refresh(true);
                    }
                    RECOVER => {
                        self.simulated = false;
                        self.refresh(true);
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
                    // A new reading can move the cadence (a pack inside the
                    // alert band, or the session having gone idle). Restating it
                    // here costs nothing while the interval is unchanged.
                    self.apply_refresh_timer();
                    self.update_icons(false);
                    if !self.manager.pending_alerts.is_empty() {
                        self.persist();
                        self.deliver_alerts();
                    }
                    if !self.manager.pending_connections.is_empty() {
                        self.deliver_connections();
                    }
                    self.publish_view();
                }
                UiEvent::Refresh => self.refresh(true),
                UiEvent::Presence => {
                    self.presence();
                    // The tick just observed the session state, so this is the
                    // right moment to decide how long the next gap should be.
                    self.apply_presence_timer();
                }
                UiEvent::Discover => self.discover(),
                UiEvent::Activity => {
                    self.bridge.activity_queued.set(false);
                    // Input ends an idle stretch: the next link check is due in
                    // the active interval again, and the fallback poll returns to
                    // the active cadence too.
                    self.apply_presence_timer();
                    self.apply_refresh_timer();
                    if self.last_activity_refresh.elapsed().as_secs() >= 10 {
                        self.last_activity_refresh = Instant::now();
                        self.refresh(false);
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
                UiEvent::Interval(enter) => self.submit_refresh_interval(enter),
                UiEvent::IntervalEdited => {
                    self.interval_error = false;
                    self.publish_view();
                }
                UiEvent::RevealPanel => self.reveal_panel(),
                UiEvent::TogglePanel => {
                    if self.panel.is_null() {
                        self.show_panel();
                    } else {
                        self.close_panel();
                    }
                }
                UiEvent::ClosePanel => self.close_panel(),
                UiEvent::RowRefresh(id) => {
                    if self.manager.selected().any(|s| s.descriptor.id == id) {
                        self.refresh_ids(vec![id]);
                    }
                }
                UiEvent::DeviceCommand(page, action, id) => {
                    if self.page == page {
                        if let Some(index) = self.rows.iter().position(|r| r.id == id) {
                            match page {
                                Page::Main => self.main_switch(index, action as usize),
                                Page::Settings | Page::History => {}
                                Page::Add => {
                                    self.page_command(
                                        ADD_BASE + index as u32 * ADD_STRIDE + action,
                                    );
                                }
                            }
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
                        self.cancel_support();
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
            // The panel's own pixels are painted from `panel_colors()`, not from
            // any child, so a theme change has to invalidate it explicitly.
            unsafe {
                InvalidateRect(self.panel, null(), 1);
            }
        }
    }
    fn quit(&mut self) {
        self.stop.cancel();
        unsafe {
            KillTimer(self.hwnd, 1);
            KillTimer(self.hwnd, 2);
            KillTimer(self.hwnd, PRESENCE_TIMER);
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
#[cfg(not(test))]
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
/// Whether the shell follows the light theme. Windows stores this per user and
/// only broadcasts a change; a plain Win32 window does not switch by itself, so
/// the shell reads the same value the clock flyout does.
fn system_uses_light_theme() -> bool {
    use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};
    let mut light: u32 = 1;
    let mut size = std::mem::size_of::<u32>() as u32;
    let result = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            wide("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize").as_ptr(),
            wide("AppsUseLightTheme").as_ptr(),
            RRF_RT_REG_DWORD,
            null_mut(),
            (&mut light as *mut u32).cast(),
            &mut size,
        )
    };
    // A missing value means light: that is the Windows 10 default.
    result != windows_sys::Win32::Foundation::ERROR_SUCCESS || light != 0
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
    } else if system_uses_light_theme() {
        PanelColors {
            paper: 0x00f5f8fa,
            text: 0x00292d30,
            secondary: 0x00636b71,
            fill: 0x003e59a9,
            empty: 0x00cbd7df,
            outline: 0x00646e75,
            high_contrast: false,
        }
    } else {
        // The Windows 10 clock flyout's own palette, so the panel sits next to
        // it without looking like a different era.
        PanelColors {
            paper: 0x001f1f1f,
            text: 0x00f2f2f2,
            secondary: 0x00a0a0a0,
            fill: 0x00d47800,
            empty: 0x003a3a3a,
            outline: 0x005a5a5a,
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
thread_local! {
    /// Whether keyboard focus cues are drawn. Windows keeps this in a per-window
    /// UI state that `WM_CHANGEUISTATE` only applies when it feels like it: this
    /// shell is a plain window, not a dialog, and on this machine the request is
    /// dropped — which left a 250px focus ring behind after every mouse click on
    /// a switch. The shell therefore owns the flag and mirrors the request to the
    /// system for the native parts it does honour.
    static FOCUS_CUES: Cell<bool> = const { Cell::new(true) };
}
fn focus_cues_visible() -> bool {
    FOCUS_CUES.with(Cell::get)
}
unsafe fn set_focus_cues(hwnd: HWND, visible: bool) {
    unsafe {
        if FOCUS_CUES.with(|cues| cues.replace(visible)) == visible {
            return;
        }
        let root = GetAncestor(hwnd, GA_ROOT);
        let root = if root.is_null() { hwnd } else { root };
        // Mirror it for the native controls: whether the system applies this is
        // optional, agreeing with our own flag is not.
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
/// Work area of the monitor holding `point`, falling back to the primary one.
unsafe fn work_area_at(point: POINT) -> RECT {
    unsafe {
        let monitor = MonitorFromPoint(point, MONITOR_DEFAULTTOPRIMARY);
        let mut info: MONITORINFO = std::mem::zeroed();
        info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        if !monitor.is_null() && GetMonitorInfoW(monitor, &mut info) != 0 {
            return info.rcWork;
        }
        let mut work: RECT = std::mem::zeroed();
        SystemParametersInfoW(SPI_GETWORKAREA, 0, (&mut work as *mut RECT).cast(), 0);
        work
    }
}
/// True when an anchor at `(left, bottom)` still leaves a reachable corner on a
/// monitor that exists right now — a spot on an unplugged screen does not.
unsafe fn anchor_on_some_monitor(left: i32, bottom: i32, width: i32) -> bool {
    unsafe {
        let probe = RECT {
            left,
            right: left + width.min(160),
            top: bottom - 48,
            bottom,
        };
        !MonitorFromRect(&probe, MONITOR_DEFAULTTONULL).is_null()
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
        let control_id = GetDlgCtrlID(hwnd) as u32;
        let mut text = [0u16; 512];
        let len = GetWindowTextW(hwnd, text.as_mut_ptr(), 512);
        let text = String::from_utf16_lossy(&text[..len as usize]);
        let control_state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const StyledControl;
        let hovered = enabled && !control_state.is_null() && (*control_state).hovered.get();
        let scale = dpi as f32 / 96.0;
        if checkbox {
            let checked = SendMessageW(hwnd, BM_GETCHECK, 0, 0) == BST_CHECKED as isize;
            let x = if bounds.right > 100 {
                bounds.right - 38
            } else {
                (bounds.right - 38) / 2
            };
            let y = (bounds.bottom - 22) / 2;
            let track = if checked && enabled {
                colors.fill
            } else {
                colors.outline
            };
            let knob_color = if checked && enabled && colors.high_contrast {
                GetSysColor(COLOR_HIGHLIGHTTEXT)
            } else {
                colors.paper
            };
            // A GDI RoundRect with radius 22 on a 22-tall track collapses into
            // an aliased octagon, so the track and knob are composited at
            // device resolution instead.
            draw_toggle(dc, dpi, x, y, checked, track, knob_color);
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
            let primary = control_id >= ADD_BASE && (control_id - ADD_BASE) % ADD_STRIDE == 0;
            if primary || hovered || pushed {
                blend_round_rect(
                    dc,
                    Rect::new(
                        scale,
                        scale,
                        (bounds.right - 1) as f32 * scale,
                        (bounds.bottom - 1) as f32 * scale,
                    ),
                    RADIUS_CONTROL * scale,
                    Some(if primary && enabled {
                        colors.fill
                    } else {
                        colors.empty
                    }),
                    Some((if primary { colors.fill } else { colors.empty }, scale)),
                );
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
            } else if control_id >= ADD_BASE && (control_id - ADD_BASE) % ADD_STRIDE == 2 {
                "移除"
            } else {
                &text
            };
            panel_text(
                dc,
                visible,
                bounds,
                if control_id == BACK {
                    22
                } else if control_id == HISTORY {
                    // Same size as the column captions it sits beside, so the
                    // header row reads as one band instead of three sizes.
                    11
                } else {
                    15
                },
                400,
                DT_VCENTER | DT_CENTER,
            );
        }
        if focused && focus_cues_visible() {
            blend_round_rect(
                dc,
                Rect::new(
                    2.0 * scale,
                    2.0 * scale,
                    (bounds.right - 2) as f32 * scale,
                    (bounds.bottom - 2) as f32 * scale,
                ),
                RADIUS_CONTROL * scale,
                None,
                Some((colors.fill, 2.0 * scale)),
            );
        }
        DeleteObject(paper);
        RestoreDC(dc, saved);
    }
}

/// Light `RoundRect` replacement: the shape is rasterized into an antialiased
/// surface at device resolution and blended, so a 1px border stays one crisp
/// pixel on the grid instead of GDI's fractional, jagged strokes.
fn blend_round_rect(
    dc: HDC,
    rect: Rect,
    radius: f32,
    fill: Option<u32>,
    stroke: Option<(u32, f32)>,
) {
    if rect.right <= rect.left || rect.bottom <= rect.top {
        return;
    }
    let pad = 1.0;
    let origin_x = rect.left - pad;
    let origin_y = rect.top - pad;
    let mut surface = Surface::new(
        (rect.right - rect.left + pad * 2.0).ceil() as usize,
        (rect.bottom - rect.top + pad * 2.0).ceil() as usize,
    );
    let local = Rect::new(
        pad,
        pad,
        rect.right - rect.left + pad,
        rect.bottom - rect.top + pad,
    );
    match (fill, stroke) {
        (None, None) => {}
        (Some(color), None) => {
            surface.fill_round_rect(local, radius, color);
        }
        (None, Some((color, width))) => {
            surface.stroke_round_rect(local, radius, width, color);
        }
        (Some(color), Some((edge, width))) => {
            // The ring is painted first, then the interior is restored so the
            // fill colour survives the inside stroke.
            surface.fill_round_rect(local, radius, edge);
            surface.fill_round_rect(local.inset(width), (radius - width).max(0.0), color);
        }
    }
    surface.blend_onto(dc, origin_x.round() as i32, origin_y.round() as i32);
}

/// 38x22 logical switch: a true pill track with a 16px knob inset 3px on the
/// active side. Both shapes are antialiased circles, so the knob sits evenly
/// at either end instead of the old 3px/2px mismatch.
fn draw_toggle(dc: HDC, dpi: i32, x: i32, y: i32, checked: bool, track: u32, knob: u32) {
    let scale = dpi as f32 / 96.0;
    let width = 38.0 * scale;
    let height = 22.0 * scale;
    let diameter = 16.0 * scale;
    let inset = 3.0 * scale;
    let mut surface = Surface::new(width.ceil() as usize, height.ceil() as usize);
    surface.fill_round_rect(Rect::new(0.0, 0.0, width, height), height * 0.5, track);
    let left = if checked {
        width - inset - diameter
    } else {
        inset
    };
    let top = (height - diameter) * 0.5;
    surface.fill_circle(
        (left + diameter * 0.5, top + diameter * 0.5),
        diameter * 0.5,
        knob,
    );
    surface.blend_onto(
        dc,
        (x as f32 * scale).round() as i32,
        (y as f32 * scale).round() as i32,
    );
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
        WM_TIMER if wparam == PRESENCE_TIMER => UiEvent::Presence,
        WM_TIMER if wparam == REVEAL_TIMER => UiEvent::RevealPanel,
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
        // A frameless panel has no close button of its own, so the tray icon
        // does what the clock does: the same click that opens it closes it.
        TRAY if lparam as u32 == WM_LBUTTONUP => UiEvent::TogglePanel,
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
            // Keep the original full-height click target around each shorter
            // text field; never focus a disabled one.
            for (id, left, right, top, bottom) in [
                (THRESHOLD, 464, 520, 168, 200),
                (REFRESH_INTERVAL, 436, 492, 220, 252),
            ] {
                if page == Page::Settings
                    && x >= left * dpi / 96
                    && x < right * dpi / 96
                    && y >= (top - scroll) * dpi / 96
                    && y < (bottom - scroll) * dpi / 96
                {
                    unsafe {
                        let edit = GetDlgItem(hwnd, id as i32);
                        if IsWindowEnabled(edit) != 0 {
                            windows_sys::Win32::UI::Input::KeyboardAndMouse::SetFocus(edit);
                        }
                    }
                    return 0;
                }
            }
            return unsafe { DefWindowProcW(hwnd, message, wparam, lparam) };
        }
        WM_COMMAND if (wparam >> 16) as u32 == BN_CLICKED => match (wparam & 0xffff) as u32 {
            ADD => UiEvent::Control(ADD as usize),
            SETTINGS => UiEvent::Settings,
            id if (MAIN_TRAY_BASE..MAIN_TRAY_BASE + 512).contains(&id) => {
                let view = bridge.view.borrow();
                let Some(row) = view.rows.get((id - MAIN_TRAY_BASE) as usize) else {
                    return 0;
                };
                UiEvent::DeviceCommand(Page::Main, 0, row.descriptor.id.clone())
            }
            id if (MAIN_NOTIFY_BASE..MAIN_NOTIFY_BASE + 512).contains(&id) => {
                let view = bridge.view.borrow();
                let Some(row) = view.rows.get((id - MAIN_NOTIFY_BASE) as usize) else {
                    return 0;
                };
                UiEvent::DeviceCommand(Page::Main, 1, row.descriptor.id.clone())
            }
            id if id >= ADD_BASE => {
                let view = bridge.view.borrow();
                let index = ((id - ADD_BASE) / ADD_STRIDE) as usize;
                let action = (id - ADD_BASE) % ADD_STRIDE;
                let Some(row) = view.rows.get(index) else {
                    return 0;
                };
                UiEvent::DeviceCommand(view.page, action, row.descriptor.id.clone())
            }
            BACK | STARTUP | ALERTS | CONNECTIONS | AUTO_REFRESH | PICK_REFRESH | HISTORY
            | WINDOW_PIN => UiEvent::PageCommand((wparam & 0xffff) as u32),
            ALL_SWITCHES => UiEvent::PageCommand(ALL_SWITCHES),
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
            if matches!((wparam & 0xffff) as u32, THRESHOLD | REFRESH_INTERVAL)
                && (wparam >> 16) as u32 == EN_SETFOCUS =>
        {
            unsafe {
                InvalidateRect(hwnd, null(), 1);
            }
            return 0;
        }
        WM_COMMAND
            if matches!((wparam & 0xffff) as u32, THRESHOLD | REFRESH_INTERVAL)
                && (wparam >> 16) as u32 == EN_KILLFOCUS =>
        {
            if (wparam & 0xffff) as u32 == THRESHOLD {
                UiEvent::Threshold(false)
            } else {
                UiEvent::Interval(false)
            }
        }
        WM_COMMAND
            if matches!((wparam & 0xffff) as u32, THRESHOLD | REFRESH_INTERVAL)
                && (wparam >> 16) as u32 == EN_CHANGE =>
        {
            unsafe {
                SendMessageW(lparam as HWND, EM_HIDEBALLOONTIP, 0, 0);
                InvalidateRect(hwnd, null(), 1);
            }
            if (wparam & 0xffff) as u32 == THRESHOLD {
                UiEvent::ThresholdEdited
            } else {
                UiEvent::IntervalEdited
            }
        }
        WM_VSCROLL => UiEvent::Scroll(match (wparam & 0xffff) as i32 {
            SB_LINEUP => -64,
            SB_LINEDOWN => ROW_HEIGHT,
            SB_PAGEUP => -ROW_HEIGHT * 3,
            SB_PAGEDOWN => ROW_HEIGHT * 3,
            SB_THUMBTRACK | SB_THUMBPOSITION => {
                ((wparam >> 16) as i32 / ROW_HEIGHT) * ROW_HEIGHT - bridge.view.borrow().scroll
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
        // The caption is gone, so the header band is the drag handle. Child
        // controls (the back arrow, the history link) keep their own hit test.
        WM_NCHITTEST => {
            let mut point = POINT {
                x: (lparam & 0xffff) as i16 as i32,
                y: ((lparam >> 16) & 0xffff) as i16 as i32,
            };
            unsafe {
                ScreenToClient(hwnd, &mut point);
            }
            let view = bridge.view.borrow();
            let band = if view.page == Page::Main { 68 } else { 56 };
            // A pinned panel is not a handle: dragging it would only fight the
            // corner it is nailed to.
            if !view.pinned && point.y < band {
                return HTCAPTION as isize;
            }
            drop(view);
            return unsafe { DefWindowProcW(hwnd, message, wparam, lparam) };
        }
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
unsafe fn draw_battery(
    dc: HDC,
    dpi: i32,
    left: i32,
    y: i32,
    reading: Option<&Reading>,
    colors: &PanelColors,
) {
    unsafe {
        let scale = dpi as f32 / 96.0;
        // Body, terminal, cells and bolt share one device-resolution surface:
        // GDI's Rectangle/RoundRect/Polygon alias and land on fractional
        // pixels as soon as the panel mapping leaves 96 dpi.
        let origin_x = (left as f32 - 1.0) * scale;
        let origin_y = (y as f32 + 14.0) * scale;
        let mut surface = Surface::new(
            (133.0 * scale).ceil() as usize,
            (29.0 * scale).ceil() as usize,
        );
        let px = |value: i32| (value - left + 1) as f32 * scale;
        let py = |value: i32| (value - y - 14) as f32 * scale;
        let edge = scale.max(1.0);
        let radius = RADIUS_BATTERY * scale;
        // Ten cells on an 11px pitch keep the gauge inside the narrower column.
        let body = Rect::new(px(left), py(y + 16), px(left + 107), py(y + 40));
        surface.fill_round_rect(body, radius, colors.outline);
        surface.fill_round_rect(body.inset(edge), (radius - edge).max(0.0), colors.paper);
        surface.fill_rect(
            Rect::new(px(left + 109), py(y + 23), px(left + 113), py(y + 34)),
            colors.outline,
        );
        if let Some(reading) = reading {
            for cell in 0..10 {
                let x = left + 4 + cell * 10;
                let background = Rect::new(px(x), py(y + 20), px(x + 9), py(y + 37));
                surface.fill_rect(background, colors.empty);
                let fill = cell_fill(Some(reading.normalized_percent()), cell);
                if fill > 0 {
                    surface.fill_rect(
                        Rect::new(px(x), py(y + 20), px(x + fill), py(y + 37)),
                        colors.fill,
                    );
                }
            }
        }
        if reading.is_some_and(|r| r.power_state == crate::device::PowerState::Charging) {
            let bolt = [
                (px(left + 124), py(y + 16)),
                (px(left + 117), py(y + 29)),
                (px(left + 123), py(y + 29)),
                (px(left + 120), py(y + 40)),
                (px(left + 130), py(y + 24)),
                (px(left + 124), py(y + 24)),
            ];
            surface.fill_polygon(&bolt, colors.fill);
        }
        surface.blend_onto(dc, origin_x.round() as i32, origin_y.round() as i32);
        if reading.is_none() {
            SetTextColor(dc, colors.secondary);
            panel_text(
                dc,
                "?",
                RECT {
                    left,
                    top: y + 14,
                    right: left + 107,
                    bottom: y + 42,
                },
                19,
                700,
                DT_CENTER | DT_VCENTER,
            );
        }
    }
}

/// One-pixel separator. A GDI pen of one logical pixel scales to 1.5 device
/// pixels under the panel mapping and renders as alternating 1px/2px steps;
/// the rule is width-snapped instead.
fn blend_rule(dc: HDC, dpi: i32, left: i32, right: i32, y: i32, color: u32) {
    let scale = dpi as f32 / 96.0;
    let width = right - left;
    let thickness = scale.max(1.0);
    if width <= 0 {
        return;
    }
    let mut surface = Surface::new(
        (width as f32 * scale).ceil() as usize,
        thickness.ceil() as usize,
    );
    surface.fill_rect(Rect::new(0.0, 0.0, width as f32 * scale, thickness), color);
    surface.blend_onto(
        dc,
        (left as f32 * scale).round() as i32,
        (y as f32 * scale).round() as i32,
    );
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
        // Frameless window: a hairline keeps the edge readable even where the
        // system shadow is turned off in performance settings.
        let edge = CreateSolidBrush(colors.empty);
        FrameRect(dc, &client, edge);
        DeleteObject(edge);
        use_panel_coordinates(dc, hwnd);
        let old_brush = SelectObject(dc, GetStockObject(DC_BRUSH));
        let old_pen = SelectObject(dc, GetStockObject(DC_PEN));
        let dpi = GetDpiForWindow(hwnd).max(96) as i32;
        let scale = dpi as f32 / 96.0;
        let height = client.bottom * 96 / dpi;
        if view.page == Page::Main {
            // The summary bar keeps the same band as the other pages' titles, so
            // switching pages does not jump the eye.
            SetTextColor(dc, colors.text);
            panel_text(
                dc,
                "外设名称",
                RECT {
                    left: 24,
                    top: 10,
                    right: 228,
                    bottom: 50,
                },
                20,
                700,
                DT_LEFT | DT_VCENTER | DT_END_ELLIPSIS,
            );
            SetTextColor(dc, colors.secondary);
            // Right-aligned against the history link, so a long device name and
            // the summary can never claim the same pixels.
            panel_text(
                dc,
                &main_summary(view),
                RECT {
                    left: 120,
                    top: 14,
                    right: 360,
                    bottom: 46,
                },
                12,
                400,
                DT_RIGHT | DT_VCENTER | DT_END_ELLIPSIS,
            );
            if !view.rows.is_empty() {
                // Column captions use the empty space at the right of the header
                // band instead of taking a line of their own above the list.
                for (text, left, right) in [("托盘", 456, 494), ("提醒", 498, 536)] {
                    panel_text(
                        dc,
                        text,
                        RECT {
                            left,
                            top: 18,
                            right,
                            bottom: 42,
                        },
                        11,
                        400,
                        DT_CENTER | DT_VCENTER,
                    );
                }
            }
            // Sit the header rule exactly one pixel above the first row, the same
            // way the row dividers behave, so every row band is 64px tall and the
            // first row is not given extra breathing room the others lack.
            blend_rule(dc, dpi, 24, 536, 67, colors.empty);
        }
        if view.page != Page::Main {
            SetTextColor(dc, colors.text);
            panel_text(
                dc,
                if view.page == Page::Settings {
                    "设置"
                } else if view.page == Page::Add {
                    "添加设备"
                } else {
                    "历史使用"
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
            for (id, left, top, right, bottom, error) in [
                (THRESHOLD, 462, 119, 538, 153, view.threshold_error),
                (REFRESH_INTERVAL, 434, 167, 538, 201, view.interval_error),
            ] {
                let edit = GetDlgItem(hwnd, id as i32);
                let focused = windows_sys::Win32::UI::Input::KeyboardAndMouse::GetFocus() == edit;
                blend_round_rect(
                    dc,
                    Rect::new(
                        left as f32 * scale,
                        (top - offset) as f32 * scale,
                        right as f32 * scale,
                        (bottom - offset) as f32 * scale,
                    ),
                    RADIUS_CONTROL * scale,
                    None,
                    Some((
                        if (focused && focus_cues_visible()) || error {
                            colors.fill
                        } else {
                            colors.outline
                        },
                        scale,
                    )),
                );
            }
            RestoreDC(dc, clip);
        }
        if view.page == Page::History && view.rows.is_empty() {
            SetTextColor(dc, colors.secondary);
            panel_text(
                dc,
                "暂无已连接的设备",
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
        if view.page == Page::Main && view.rows.is_empty() {
            SetTextColor(dc, colors.secondary);
            panel_text(
                dc,
                "暂无已连接的设备",
                RECT {
                    left: 24,
                    top: 78,
                    right: 536,
                    bottom: 104,
                },
                13,
                400,
                DT_CENTER | DT_VCENTER,
            );
        }
        let top = match view.page {
            Page::Main => 68,
            Page::Settings => 320,
            Page::Add | Page::History => 76,
        };
        let footer = if view.page == Page::Main { 48 } else { 24 };
        for (index, state) in view.rows.iter().enumerate() {
            let y = top + index as i32 * ROW_HEIGHT - view.scroll;
            if y < if view.page == Page::Main { 68 } else { 64 } || y + 56 > height - footer {
                continue;
            }
            if let Some(icon) = view.symbols.get(index) {
                let saved = SaveDC(dc);
                SetMapMode(dc, MM_TEXT);
                DrawIconEx(
                    dc,
                    SYMBOL_LEFT * dpi / 96,
                    (y + SYMBOL_TOP) * dpi / 96,
                    icon.get(),
                    SYMBOL_SIZE * dpi / 96,
                    SYMBOL_SIZE * dpi / 96,
                    0,
                    null_mut(),
                    DI_NORMAL,
                );
                RestoreDC(dc, saved);
            }
            if view.page == Page::Main {
                draw_battery(dc, dpi, 248, y - 6, state.reading.as_ref(), &colors);
                SetTextColor(dc, colors.secondary);
                panel_text(
                    dc,
                    &row_detail(state),
                    RECT {
                        left: 64,
                        top: y + 36,
                        right: 536,
                        bottom: y + 56,
                    },
                    12,
                    400,
                    DT_LEFT | DT_VCENTER | DT_END_ELLIPSIS,
                );
            }
            if view.page == Page::History {
                SetTextColor(dc, colors.secondary);
                panel_text(
                    dc,
                    &history_span(state),
                    RECT {
                        left: 300,
                        top: y + 8,
                        right: 536,
                        bottom: y + 34,
                    },
                    15,
                    400,
                    DT_RIGHT | DT_VCENTER | DT_END_ELLIPSIS,
                );
                panel_text(
                    dc,
                    &history_current_text(state),
                    RECT {
                        left: 64,
                        top: y + 36,
                        right: 536,
                        bottom: y + 56,
                    },
                    12,
                    400,
                    DT_LEFT | DT_VCENTER | DT_END_ELLIPSIS,
                );
            }
            if view.page != Page::Settings && index + 1 < view.rows.len() {
                blend_rule(dc, dpi, 24, 536, y + ROW_HEIGHT - 1, colors.empty);
            }
        }
        if view.page == Page::Main && !view.rows.is_empty() {
            blend_rule(dc, dpi, 24, 536, height - 48, colors.empty);
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
            // The mutex is the authority: a second copy never starts, even if the
            // first one is still building its window or cannot answer. Surface the
            // running instance when we can, and stay silent otherwise — a login
            // autostart collision must not raise a dialog.
            let deadline = Instant::now() + std::time::Duration::from_secs(5);
            loop {
                let hwnd = find_window();
                if !hwnd.is_null() {
                    let _ = PostMessageW(hwnd, CONTROL, mode.existing_command(), 0);
                    return Ok(());
                }
                if Instant::now() >= deadline {
                    return Ok(());
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
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
    // Fail before creating windows, worker threads or changing a legacy shortcut.
    let manager = Manager::load(&manager::state_path())?;
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
        for (name, proc, style) in [
            (
                CLASS,
                window_proc as unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT,
                0u32,
            ),
            // The drop shadow is what keeps a captionless panel from floating on
            // a same-coloured desktop without an edge.
            (PANEL_CLASS, panel_proc, CS_DROPSHADOW),
        ] {
            let class_name = wide(name);
            let class = WNDCLASSW {
                style,
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
                    let link_batch = matches!(&command, WorkerCommand::Link(..));
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
                        WorkerCommand::Support(token, context, destination, cancel) => {
                            let result = (|| {
                                let destination = match destination {
                                    #[cfg(not(test))]
                                    SupportDestination::FileDialog(owner) => {
                                        match crate::save_dialog::choose(owner, &cancel)? {
                                            Some(path) => SupportDestination::File(path),
                                            None => return Err(DeviceError::Cancelled),
                                        }
                                    }
                                    destination => destination,
                                };
                                let json = crate::device::support::collect(&context, &cancel)?;
                                cancel.check()?;
                                worker_stop.check()?;
                                match destination {
                                    SupportDestination::Clipboard => Ok(Some(json)),
                                    SupportDestination::File(path) => {
                                        crate::device::support::save(&path, &json, &cancel)
                                            .map(|_| None)
                                    }
                                    #[cfg(not(test))]
                                    SupportDestination::FileDialog(_) => unreachable!(),
                                }
                            })();
                            let _ = results.send(WorkerResult::Support(token, result));
                        }
                        WorkerCommand::Read(ids, simulated)
                        | WorkerCommand::Link(ids, simulated) => {
                            for id in ids {
                                if worker_stop.check().is_err() {
                                    break;
                                }
                                let result = if simulated {
                                    Err(simulated_error())
                                } else {
                                    Registry::read_cancelled(&id, &worker_stop)
                                };
                                let result = if link_batch {
                                    WorkerResult::LinkRead(id, result)
                                } else {
                                    WorkerResult::Reading(id, result)
                                };
                                if results.send(result).is_err() {
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
            manager,
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
            support_job: None,
            support_sequence: 0,
            support_feedback: None,
            simulated: false,
            refresh_count: 0,
            success_count: 0,
            bridge: bridge.clone(),
            panel_fonts: [null_mut(); 4],
            panel_icons: [null_mut(); 2],
            last_activity_refresh: Instant::now() - std::time::Duration::from_secs(10),
            refresh_timer_seconds: Cell::new(0),
            presence_timer_seconds: Cell::new(0),
            awaiting_first_pass: false,
            picker_ids: Vec::new(),
            page: Page::Main,
            page_scroll: [0; 4],
            page_controls: Vec::new(),
            threshold_error: false,
            interval_error: false,
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
        if SetTimer(hwnd, 2, 30_000, None) == 0
            || !app.apply_presence_timer()
            || !app.apply_refresh_timer()
        {
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
                && matches!(
                    GetDlgCtrlID(message.hwnd) as u32,
                    THRESHOLD | REFRESH_INTERVAL
                )
            {
                if GetDlgCtrlID(message.hwnd) as u32 == THRESHOLD {
                    app.submit_threshold(true);
                } else {
                    app.submit_refresh_interval(true);
                }
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
                assert!(!focus_cues_visible());
                // The shell keeps its own flag instead of reading the window's UI
                // state back: `WM_CHANGEUISTATE` is only a request, and a plain
                // window cannot rely on the system applying it. On this machine it
                // is dropped, which is exactly why a mouse click used to leave a
                // 250px focus ring around the switch it landed on.
                assert!(!focus_cues_visible());
                input_focus_cues(
                    parent,
                    &MSG {
                        hwnd: control,
                        message: WM_MOUSEMOVE,
                        ..Default::default()
                    },
                );
                assert!(!focus_cues_visible());
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
                    assert!(focus_cues_visible());
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
            assert!(!focus_cues_visible());
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
                support_job: None,
                support_sequence: 0,
                support_feedback: None,
                simulated: false,
                refresh_count: 0,
                success_count: 0,
                bridge: test_bridge(),
                panel_fonts: [null_mut(); 4],
                panel_icons: [null_mut(); 2],
                last_activity_refresh: Instant::now(),
                refresh_timer_seconds: Cell::new(0),
                presence_timer_seconds: Cell::new(0),
                awaiting_first_pass: false,
                picker_ids: Vec::new(),
                page: Page::Main,
                page_scroll: [0; 4],
                page_controls: Vec::new(),
                threshold_error: false,
                interval_error: false,
                notification_submissions: 0,
                notification_last_accepted: false,
                scroll: 0,
            },
            results,
        )
    }
    #[test]
    fn support_actions_queue_once_keep_reading_unchanged_and_discard_late_copy() {
        let mut manager = Manager::default();
        manager.merge(
            vec![Candidate {
                descriptor: crate::device::DeviceDescriptor {
                    id: "support-device".into(),
                    physical_id: "local-physical".into(),
                    endpoint: None,
                    recoverable: false,
                    name: "support fixture".into(),
                    kind: DeviceKind::Mouse,
                    supported: false,
                    provider: "unsupported".into(),
                    connection: crate::device::Connection::Unknown,
                },
                reading: Some(
                    Reading::from_percent(
                        "support fixture".into(),
                        35,
                        crate::device::PowerState::Discharging,
                    )
                    .unwrap(),
                ),
                error: None,
            }],
            100,
        );
        let (mut app, _) = headless_app(manager);
        let (sender, commands) = mpsc::channel();
        app.sender = sender;
        let panel = unsafe {
            CreateWindowExW(
                0,
                wide("STATIC").as_ptr(),
                null(),
                WS_OVERLAPPED,
                0,
                0,
                560,
                240,
                null_mut(),
                null_mut(),
                GetModuleHandleW(null()),
                null(),
            )
        };
        assert!(!panel.is_null());
        app.panel = panel;
        app.page = Page::Add;
        app.request_support("support-device", false);
        let (token, cancel) = match commands.try_recv().unwrap() {
            WorkerCommand::Support(token, context, SupportDestination::Clipboard, cancel) => {
                assert_eq!(context.descriptor.id, "support-device");
                assert_eq!(context.last_success, Some(100));
                assert_eq!(context.reading.unwrap().normalized_percent(), 35);
                (token, cancel)
            }
            _ => panic!("wrong worker action"),
        };
        assert!(app.busy);
        app.request_support("support-device", false);
        assert!(commands.try_recv().is_err());
        app.complete_support(token + 1, Ok(Some("wrong".into())));
        assert!(app.effects.support_copies.is_empty());
        app.complete_support(token, Ok(Some("{\"schema\":1}".into())));
        assert_eq!(app.effects.support_copies, ["{\"schema\":1}"]);
        assert_eq!(app.manager.devices[0].last_success, Some(100));
        assert!(cancel.check().is_ok());
        app.busy = false;
        // Cancelling the save picker never queues work or creates an output file.
        app.request_support("support-device", true);
        assert!(!app.busy && commands.try_recv().is_err());
        app.request_support("support-device", false);
        let (late, cancellation) = match commands.try_recv().unwrap() {
            WorkerCommand::Support(t, _, _, c) => (t, c),
            _ => panic!(),
        };
        app.cancel_support();
        assert!(matches!(cancellation.check(), Err(DeviceError::Cancelled)));
        app.complete_support(late, Ok(Some("late".into())));
        assert_eq!(app.effects.support_copies.len(), 1);
        app.panel = null_mut();
        unsafe {
            DestroyWindow(panel);
        }
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
                        connection: crate::device::Connection::Receiver,
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
                    Manager::load(&sink_path).unwrap().devices[0].alert.notified,
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
    fn a_reported_charge_state_is_serialized_without_protocol_metadata() {
        let charging = Reading::from_percent(
            "Charging fixture".into(),
            97,
            crate::device::PowerState::Charging,
        )
        .unwrap();
        let json = reading_json(&charging);
        assert!(json.contains("\"power_state\":\"Charging\""), "{json}");
        assert!(json.contains("\"metadata_known\":false"), "{json}");
        // An unobserved state stays null instead of inventing a value.
        let unknown = Reading::from_percent(
            "Fixture".into(),
            50,
            crate::device::PowerState::Unknown(255),
        )
        .unwrap();
        assert!(reading_json(&unknown).contains("\"power_state\":null"));
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
            // A frameless panel draws a hairline frame, so the very edge is the
            // border colour and the paper starts one pixel in.
            assert_eq!(GetPixel(dc, 0, 0), panel_colors().empty);
            assert_eq!(GetPixel(dc, 1, 1), panel_colors().paper);
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
                support_job: None,
                support_sequence: 0,
                support_feedback: None,
                simulated: false,
                refresh_count: 0,
                success_count: 0,
                bridge: test_bridge(),
                panel_fonts: [null_mut(); 4],
                panel_icons: [null_mut(); 2],
                last_activity_refresh: Instant::now(),
                refresh_timer_seconds: Cell::new(0),
                presence_timer_seconds: Cell::new(0),
                awaiting_first_pass: false,
                picker_ids: Vec::new(),
                page: Page::Main,
                page_scroll: [0; 4],
                page_controls: Vec::new(),
                threshold_error: false,
                interval_error: false,
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
                        connection: crate::device::Connection::Receiver,
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
                    silent_since: None,
                    usage_since: None,
                    usage_from: None,
                    last_seen: None,
                    usage_cycles: Vec::new(),
                    full_since: None,
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
            // The settings page is a fixed form now: no device rows at all, and
            // the master switch is the only device-related control left.
            assert!(app.rows.is_empty());
            let master = GetDlgItem(window, ALL_SWITCHES as i32);
            assert!(!master.is_null());
            assert_eq!(SendMessageW(master, BM_GETCHECK, 0, 0), 1);
            assert_eq!(window_text(master), "所有设备的托盘与提醒");

            // Renaming, tooltips and focus anchors are exercised on the main
            // page, which is the one that lists devices.
            app.switch_page(Page::Main);
            assert_eq!(app.rows.len(), 1);
            let anchor = app.anchor_for(app.rows[0].tray).unwrap();
            let previous_model = app.rows[0].model;
            app.manager.devices[0].descriptor.name = "Renamed mouse".into();
            app.manager.devices[0].descriptor.kind = DeviceKind::Headset;
            app.rebuild_rows();
            assert_ne!(app.rows[0].model, previous_model);
            assert_eq!(window_text(app.rows[0].model), "Renamed mouse");
            assert!(window_text(app.rows[0].tray).contains("Renamed mouse，显示托盘"));
            assert_eq!(app.rows[0].presentation.2, DeviceKind::Headset);
            assert_eq!(
                app.rows[0]._tooltip_text,
                wide("Renamed mouse · 2.4G 接收器")
            );
            assert_eq!(app.resolve_anchor(&anchor).unwrap().0, app.rows[0].tray);
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
            // The main page never decorates a name; only the mixed lists do.
            assert!(!window_text(app.rows[0].model).contains('·'));
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
            // The per-device switches live on the main page now, so the
            // high-contrast render and the reminder gating are checked there.
            app.switch_page(Page::Main);
            draw_control_colors(app.rows[0].tray, true, dc, &high_contrast);
            let mut bb: RECT = std::mem::zeroed();
            GetClientRect(app.rows[0].tray, &mut bb);
            assert_eq!(
                GetPixel(dc, bb.right / 4, bb.bottom / 2),
                high_contrast.fill
            );
            assert!(window_text(app.rows[0].tray).contains("Renamed mouse，显示托盘"));
            SelectObject(dc, old);
            DeleteObject(bitmap);
            DeleteDC(dc);
            app.page_command(ALERTS);
            assert!(!app.manager.notifications_enabled);
            assert_eq!(IsWindowEnabled(app.rows[0].notify), 0);
            assert_ne!(IsWindowEnabled(app.rows[0].tray), 0);
            app.page_command(ALERTS);
            app.switch_page(Page::Settings);
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
            assert_eq!(app.rows.len(), 3);
            for row in &app.rows {
                assert_eq!(row.extra.len(), 2);
                assert_eq!(window_text(row.extra[0]), "复制信息");
                assert!(window_text(row.extra[1]).contains("移除监控"));
                let bounds = [row.extra[0], row.refresh, row.extra[1]].map(|h| {
                    let mut r = RECT::default();
                    GetWindowRect(h, &mut r);
                    r
                });
                assert!(bounds.windows(2).all(|pair| pair[0].right <= pair[1].left));
                assert!(bounds.iter().all(|r| r.bottom - r.top >= 32));
            }
            let i = app.rows.iter().position(|r| r.id == "keyboard").unwrap();
            app.page_command(ADD_BASE + i as u32 * ADD_STRIDE);
            assert_eq!(app.rows.len(), 3);
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
            // The main page's per-row control is the tray switch; the add page
            // is the one that owns the action buttons.
            let target = app.rows[last].tray;
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
            // The header link is created after the two footer buttons, so it
            // closes the page's tab order.
            assert_eq!(
                app.focus_targets().last().unwrap().0,
                GetDlgItem(window, HISTORY as i32)
            );
            app.close_panel();
            assert_eq!(IsWindow(window), 0);
            assert!(app.panel_fonts.iter().all(|f| f.is_null()));
            UnregisterClassW(name.as_ptr(), instance);
            let _ = std::fs::remove_file(manager::state_path());
        }
    }
    fn usage_state(name: &str, cycles: &[(u64, u64)]) -> DeviceState {
        DeviceState {
            descriptor: crate::device::DeviceDescriptor {
                id: name.to_lowercase(),
                physical_id: format!("physical-{name}"),
                endpoint: None,
                recoverable: true,
                name: name.into(),
                kind: DeviceKind::Mouse,
                supported: true,
                provider: "fixture".into(),
                connection: crate::device::Connection::Receiver,
            },
            selected: true,
            removed: false,
            show_tray: true,
            notify: true,
            alert: crate::notifications::AlertRecord::default(),
            reading: Some(
                Reading::from_percent(name.into(), 98, crate::device::PowerState::Discharging)
                    .unwrap(),
            ),
            last_success: Some(now()),
            error: None,
            connected: true,
            connection_known: true,
            silent_since: None,
            usage_since: Some(now().saturating_sub(3_600)),
            usage_from: Some(98),
            last_seen: Some((98, crate::device::PowerState::Discharging)),
            usage_cycles: cycles
                .iter()
                .map(|(ended, seconds)| manager::UsageCycle {
                    ended: *ended,
                    seconds: *seconds,
                    ended_percent: Some(9),
                })
                .collect(),
            full_since: None,
        }
    }
    #[test]
    fn history_page_lists_connected_devices_longest_first_without_controls() {
        unsafe {
            let mut manager = Manager::default();
            manager
                .devices
                .push(usage_state("Short run", &[(now() - 86_400 * 3, 3_600)]));
            // Two runs: the page shows the longest one, not the latest.
            manager.devices.push(usage_state(
                "Long run",
                &[(now() - 86_400 * 5, 3 * 86_400), (now() - 86_400, 7_200)],
            ));
            // No completed run yet: still listed, with the run in progress.
            manager.devices.push(usage_state("No run", &[]));
            let (mut app, _) = headless_app(manager);
            let window = CreateWindowExW(
                0,
                wide("STATIC").as_ptr(),
                null(),
                WS_OVERLAPPED,
                0,
                0,
                560,
                600,
                null_mut(),
                null_mut(),
                GetModuleHandleW(null()),
                null(),
            );
            assert!(!window.is_null());
            attach_bridge(window, &app.bridge);
            app.panel = window;
            app.page_command(HISTORY);
            assert_eq!(app.page, Page::History);
            assert_eq!(app.rows.len(), 3);
            assert_eq!(app.rows[0].id, "long run");
            assert_eq!(app.rows[1].id, "short run");
            // A device without a completed run keeps a row of its own.
            assert_eq!(app.rows[2].id, "no run");
            // One line per device: the name is the only control, the two
            // durations are painted by the panel.
            assert!(app.rows[0].percent.is_null());
            assert!(app.rows[0].tray.is_null());
            assert!(app.rows[0].notify.is_null());
            assert!(app.rows[0].refresh.is_null());
            assert!(app.rows[0].extra.is_empty());
            hidden_native_bitmap(window, "history-fixture");
            app.page_command(BACK);
            assert_eq!(app.page, Page::Main);
            hidden_native_bitmap(window, "main-title-fixture");
            release_window_bridge(window);
            assert_ne!(DestroyWindow(window), 0);
        }
    }
    #[test]
    fn main_summary_counts_devices_and_names_the_lowest() {
        let mut view = PanelView::default();
        let state = usage_state("MCHOSE A5 V3 Ultra", &[]);
        view.rows.push(state.clone());
        assert_eq!(main_summary(&view), "1 台设备 · 最低 98% · 刚刚更新");
        let mut lower = state;
        lower.reading = Some(
            Reading::from_percent("Other".into(), 42, crate::device::PowerState::Discharging)
                .unwrap(),
        );
        view.rows.push(lower);
        assert_eq!(main_summary(&view), "2 台设备 · 最低 42% · 刚刚更新");
        view.rows.clear();
        assert_eq!(main_summary(&view), "暂无设备");
    }
    #[test]
    fn history_current_text_counts_an_unrecorded_start_from_a_full_charge() {
        let state = usage_state("MCHOSE A5 V3 Ultra", &[]);
        let text = history_current_text(&state);
        assert!(text.starts_with("当前 "), "{text}");
        assert!(text.contains("→ 现在 · 98% → 98%"), "{text}");
        assert!(text.ends_with("已用1小时"), "{text}");
        // A run that began before the starting percentage was stored counts
        // from 100%, the value a full charge starts from.
        let mut legacy = state.clone();
        legacy.usage_from = None;
        assert!(
            history_current_text(&legacy).contains("100% → 98%"),
            "{}",
            history_current_text(&legacy)
        );
        let mut idle = state.clone();
        idle.usage_since = None;
        assert_eq!(history_current_text(&idle), "当前 · 尚未开始记录");
        let mut charging = state;
        charging.reading = Some(
            Reading::from_percent("X".into(), 98, crate::device::PowerState::Charging).unwrap(),
        );
        assert_eq!(history_current_text(&charging), "当前 · 充电中");
    }
    #[test]
    fn panel_height_follows_the_connected_rows_and_keeps_its_bottom() {
        unsafe {
            let mut manager = Manager::default();
            for index in 0..9 {
                let mut state = usage_state(&format!("Device {index}"), &[]);
                // Only the first is connected; the rest look like rows loaded
                // from disk that no probe has confirmed yet.
                state.connected = index == 0;
                state.connection_known = index == 0;
                manager.devices.push(state);
            }
            let (mut app, _) = headless_app(manager);
            let name = wide(PANEL_CLASS);
            let instance = GetModuleHandleW(null());
            let class = WNDCLASSW {
                lpfnWndProc: Some(panel_proc),
                hInstance: instance,
                lpszClassName: name.as_ptr(),
                ..std::mem::zeroed()
            };
            assert_ne!(RegisterClassW(&class), 0);
            let window = CreateWindowExW(
                WS_EX_CONTROLPARENT,
                name.as_ptr(),
                null(),
                WS_POPUP | WS_CLIPCHILDREN | WS_VSCROLL,
                0,
                0,
                560,
                180,
                null_mut(),
                null_mut(),
                instance,
                null(),
            );
            assert!(!window.is_null());
            attach_bridge(window, &app.bridge);
            app.panel = window;
            app.create_page_controls();
            app.rebuild_rows();
            app.layout_panel();
            let mut tall: RECT = std::mem::zeroed();
            GetWindowRect(window, &mut tall);
            assert_eq!(app.rows.len(), 9, "unconfirmed rows are listed first");

            // Every row but the first is now confirmed disconnected.
            for state in app.manager.devices.iter_mut().skip(1) {
                state.connected = false;
                state.connection_known = true;
            }
            app.rebuild_rows();
            app.layout_panel();
            let mut short: RECT = std::mem::zeroed();
            GetWindowRect(window, &mut short);
            assert_eq!(app.rows.len(), 1);
            assert!(
                short.bottom - short.top < tall.bottom - tall.top,
                "window did not shrink: tall {} short {}",
                tall.bottom - tall.top,
                short.bottom - short.top
            );
            // Bottom-anchored: the footer must not move, the top comes down.
            assert_eq!(short.bottom, tall.bottom, "bottom edge moved");
            assert!(short.top > tall.top, "top edge did not move up");

            release_window_bridge(window);
            let _ = DestroyWindow(window);
            UnregisterClassW(name.as_ptr(), instance);
        }
    }
    #[test]
    fn window_pin_switch_is_on_by_default_and_owns_the_second_column() {
        unsafe {
            let (mut app, _) = headless_app(Manager::default());
            let window = CreateWindowExW(
                0,
                wide("STATIC").as_ptr(),
                null(),
                WS_OVERLAPPED,
                0,
                0,
                560,
                600,
                null_mut(),
                null_mut(),
                GetModuleHandleW(null()),
                null(),
            );
            assert!(!window.is_null());
            attach_bridge(window, &app.bridge);
            app.panel = window;
            app.page = Page::Settings;
            app.create_page_controls();
            app.rebuild_rows();
            let pin = GetDlgItem(window, WINDOW_PIN as i32);
            let all = GetDlgItem(window, ALL_SWITCHES as i32);
            assert!(!pin.is_null() && !all.is_null());
            assert_eq!(SendMessageW(pin, BM_GETCHECK, 0, 0), 1, "pinned by default");
            // The last settings row is two columns, like the rows above it.
            let mut left: RECT = std::mem::zeroed();
            let mut right: RECT = std::mem::zeroed();
            GetWindowRect(all, &mut left);
            GetWindowRect(pin, &mut right);
            assert!(left.right <= right.left, "columns overlap");
            assert_eq!(left.top, right.top, "same row band");
            // Both directions keep the button and the model in step.
            app.page_command(WINDOW_PIN);
            assert!(!app.manager.window_pinned);
            assert_eq!(SendMessageW(pin, BM_GETCHECK, 0, 0), 0);
            app.page_command(WINDOW_PIN);
            assert!(app.manager.window_pinned);
            assert_eq!(SendMessageW(pin, BM_GETCHECK, 0, 0), 1);
            release_window_bridge(window);
            assert_ne!(DestroyWindow(window), 0);
            let _ = std::fs::remove_file(manager::state_path());
        }
    }
    #[test]
    fn panel_sits_in_the_bottom_right_corner_and_remembers_a_dragged_spot() {
        unsafe {
            let (mut app, _) = headless_app(Manager::default());
            let window = CreateWindowExW(
                0,
                wide("STATIC").as_ptr(),
                null(),
                WS_POPUP,
                40,
                40,
                560,
                300,
                null_mut(),
                null_mut(),
                GetModuleHandleW(null()),
                null(),
            );
            assert!(!window.is_null());
            attach_bridge(window, &app.bridge);
            app.panel = window;

            // Pinned by default: bottom-right corner of the work area, ignoring
            // any remembered spot.
            app.manager.panel_anchor = Some((120, 440));
            app.apply_panel_position();
            let mut rect: RECT = std::mem::zeroed();
            GetWindowRect(window, &mut rect);
            let work = work_area_at(POINT {
                x: (rect.right - 1).max(0),
                y: (rect.bottom - 1).max(0),
            });
            assert!(
                (rect.right - work.right).abs() <= 2,
                "right {} vs work right {}",
                rect.right,
                work.right
            );
            assert!(
                (rect.bottom - work.bottom).abs() <= 2,
                "bottom {} vs work bottom {}",
                rect.bottom,
                work.bottom
            );

            // Unpinned: a moved window is remembered on close, and restored.
            app.manager.window_pinned = false;
            assert_ne!(
                SetWindowPos(
                    window,
                    null_mut(),
                    120,
                    140,
                    560,
                    300,
                    SWP_NOZORDER | SWP_NOACTIVATE
                ),
                0
            );
            app.remember_panel_position();
            assert_eq!(app.manager.panel_anchor, Some((120, 440)));
            assert_ne!(
                SetWindowPos(
                    window,
                    null_mut(),
                    300,
                    220,
                    560,
                    300,
                    SWP_NOZORDER | SWP_NOACTIVATE
                ),
                0
            );
            app.apply_panel_position();
            GetWindowRect(window, &mut rect);
            assert_eq!((rect.left, rect.top), (120, 140));

            // A remembered spot no monitor covers (screen unplugged) is dropped
            // rather than leaving the window unreachable.
            app.manager.panel_anchor = Some((-30_000, -30_000));
            app.apply_panel_position();
            GetWindowRect(window, &mut rect);
            assert!(anchor_on_some_monitor(
                rect.left,
                rect.bottom,
                rect.right - rect.left
            ));

            release_window_bridge(window);
            assert_ne!(DestroyWindow(window), 0);
            let _ = std::fs::remove_file(manager::state_path());
        }
    }
    #[test]
    fn history_span_reports_the_longest_and_shortest_completed_runs() {
        let two = usage_state("two", &[(now() - 600, 3_600), (now() - 100, 7_200)]);
        assert_eq!(history_span(&two), "最长 2小时 · 最短 1小时");
        // A single record states its length once instead of repeating it.
        let one = usage_state("one", &[(now() - 600, 3_600)]);
        assert_eq!(history_span(&one), "最长 1小时");
        // No completed run: say so, never borrow the run in progress.
        let none = usage_state("none", &[]);
        assert_eq!(history_span(&none), "尚无完整满电记录");
        // The shortest wins over the order the records arrived in.
        let unordered = usage_state("unordered", &[(now() - 100, 9_000), (now() - 200, 600)]);
        assert_eq!(history_span(&unordered), "最长 2小时30分 · 最短 10分钟");
    }
    #[test]
    fn row_detail_shows_link_state_and_reading_age() {
        let mut state = crate::manager::DeviceState {
            descriptor: crate::device::DeviceDescriptor {
                id: "detail".into(),
                physical_id: "detail".into(),
                endpoint: None,
                recoverable: true,
                name: "Detail".into(),
                kind: DeviceKind::Mouse,
                supported: true,
                provider: "fixture".into(),
                connection: crate::device::Connection::Receiver,
            },
            selected: true,
            removed: false,
            show_tray: true,
            notify: true,
            alert: crate::notifications::AlertRecord::default(),
            reading: None,
            last_success: None,
            error: None,
            connected: false,
            connection_known: false,
            silent_since: None,
            usage_since: None,
            usage_from: None,
            last_seen: None,
            usage_cycles: Vec::new(),
            full_since: None,
        };
        // Before the first discovery the link is genuinely unknown, never "down".
        assert_eq!(row_detail(&state), "2.4G · 连接状态未知");
        state.connection_known = true;
        assert_eq!(row_detail(&state), "2.4G · 未连接");
        state.connected = true;
        // The reading age moved to the header summary; the row no longer
        // repeats it, whatever the age is.
        assert_eq!(row_detail(&state), "2.4G · 已使用—");
        state.usage_since = Some(now().saturating_sub(3 * 86_400 + 5 * 3_600));
        for age in [0u64, 61, 7_200, 180_000] {
            state.last_success = Some(now().saturating_sub(age));
            assert_eq!(row_detail(&state), "2.4G · 已使用3天5小时", "age {age}");
        }
        state.usage_since = Some(now().saturating_sub(42));
        assert!(row_detail(&state).contains("已使用不到1分钟"));
        state.usage_since = Some(now().saturating_sub(2 * 3_600 + 600));
        assert!(row_detail(&state).contains("已使用2小时"));
        // A connected device on the charger reports the charging state instead.
        state.reading = Some(
            Reading::from_percent("Detail".into(), 40, crate::device::PowerState::Charging)
                .unwrap(),
        );
        assert!(row_detail(&state).contains("充电中"));
        state.descriptor.connection = crate::device::Connection::BluetoothClassic;
        assert!(row_detail(&state).starts_with("蓝牙 · "));
    }
    #[test]
    fn auto_refresh_setting_gates_its_interval_and_rejects_bad_minutes() {
        unsafe {
            let mut manager = Manager::default();
            manager.merge(
                vec![Candidate {
                    descriptor: crate::device::DeviceDescriptor {
                        id: "mouse".into(),
                        physical_id: "mouse".into(),
                        endpoint: Some("1".into()),
                        recoverable: true,
                        name: "Settings mouse".into(),
                        kind: DeviceKind::Mouse,
                        supported: true,
                        provider: "fixture".into(),
                        connection: crate::device::Connection::Receiver,
                    },
                    reading: Some(
                        Reading::from_percent(
                            "Settings mouse".into(),
                            65,
                            crate::device::PowerState::Discharging,
                        )
                        .unwrap(),
                    ),
                    error: None,
                }],
                100,
            );
            let (mut app, _) = headless_app(manager);
            let window = CreateWindowExW(
                0,
                wide("STATIC").as_ptr(),
                null(),
                WS_OVERLAPPED,
                0,
                0,
                560,
                600,
                null_mut(),
                null_mut(),
                GetModuleHandleW(null()),
                null(),
            );
            assert!(!window.is_null());
            attach_bridge(window, &app.bridge);
            app.panel = window;
            app.page = Page::Settings;
            app.create_page_controls();
            app.rebuild_rows();
            let interval = GetDlgItem(window, REFRESH_INTERVAL as i32);
            assert_eq!(window_text(interval), "5");
            assert_ne!(IsWindowEnabled(interval), 0);
            assert_eq!(
                SendMessageW(GetDlgItem(window, AUTO_REFRESH as i32), BM_GETCHECK, 0, 0),
                1
            );
            // Turning the setting off disables its interval field and vice versa.
            app.page_command(AUTO_REFRESH);
            assert!(!app.manager.auto_refresh);
            assert_eq!(IsWindowEnabled(interval), 0);
            assert_eq!(
                SendMessageW(GetDlgItem(window, AUTO_REFRESH as i32), BM_GETCHECK, 0, 0),
                0
            );
            app.page_command(AUTO_REFRESH);
            assert!(app.manager.auto_refresh);
            assert_ne!(IsWindowEnabled(interval), 0);
            // The connection switch is independent of the low-battery one.
            app.page_command(CONNECTIONS);
            assert!(!app.manager.connection_notifications);
            app.page_command(CONNECTIONS);
            assert!(app.manager.connection_notifications);
            for (typed, seconds, error) in [
                ("0", 300, true),
                ("61", 300, true),
                ("bad", 300, true),
                ("", 300, true),
                ("1", 60, false),
                ("5", 300, false),
            ] {
                SetWindowTextW(interval, wide(typed).as_ptr());
                app.submit_refresh_interval(true);
                assert_eq!(app.manager.refresh_seconds, seconds, "typed {typed}");
                assert_eq!(app.interval_error, error, "typed {typed}");
            }
            // A confirmed value clears the error and resets the timer.
            SetWindowTextW(interval, wide("7").as_ptr());
            app.submit_refresh_interval(false);
            assert_eq!(app.manager.refresh_seconds, 420);
            assert!(!app.interval_error);
            hidden_native_bitmap(window, "settings-refresh-fixture");
            // The row shows the link type and reading age on the main page.
            app.switch_page(Page::Main);
            assert_eq!(app.rows.len(), 1);
            hidden_native_bitmap(window, "main-connection-fixture");
            release_window_bridge(window);
            assert_ne!(DestroyWindow(window), 0);
            let _ = std::fs::remove_file(manager::state_path());
        }
    }
    #[test]
    fn idle_follows_raw_input_recency_and_drives_both_cadences() {
        let (app, _) = headless_app(Manager::default());
        app.bridge.activity_seen.set(Instant::now());
        assert!(!app.idle());
        assert_eq!(
            app.manager.presence_cadence(app.idle()),
            manager::PRESENCE_ACTIVE_SECONDS
        );
        // One second past the idle threshold: both timers take the slow cadence.
        app.bridge
            .activity_seen
            .set(Instant::now() - std::time::Duration::from_secs(manager::IDLE_AFTER_SECONDS + 1));
        assert!(app.idle());
        assert_eq!(
            app.manager.presence_cadence(app.idle()),
            manager::PRESENCE_IDLE_SECONDS
        );
        assert_eq!(
            app.manager.refresh_cadence(app.idle()),
            manager::IDLE_REFRESH_SECONDS
        );
        // Input arriving again restores the active cadence immediately.
        app.bridge.activity_seen.set(Instant::now());
        assert_eq!(
            app.manager.refresh_cadence(app.idle()),
            manager::DEFAULT_REFRESH_SECONDS
        );
    }
    #[test]
    fn settings_hint_keeps_the_interval_row_inside_the_panel() {
        unsafe {
            let (mut app, _) = headless_app(Manager::default());
            let window = CreateWindowExW(
                0,
                wide("STATIC").as_ptr(),
                null(),
                WS_OVERLAPPED,
                0,
                0,
                560,
                600,
                null_mut(),
                null_mut(),
                GetModuleHandleW(null()),
                null(),
            );
            assert!(!window.is_null());
            attach_bridge(window, &app.bridge);
            app.panel = window;
            app.page = Page::Settings;
            app.create_page_controls();
            app.rebuild_rows();
            app.layout_panel();
            let rect = |id: u32| {
                let mut value: RECT = std::mem::zeroed();
                let control = GetDlgItem(window, id as i32);
                assert!(!control.is_null(), "control {id}");
                assert_ne!(GetWindowRect(control, &mut value), 0, "control {id}");
                value
            };
            let interval = rect(REFRESH_INTERVAL);
            let hint = rect(INTERVAL_HINT);
            let all = rect(ALL_SWITCHES);
            // The line explains the field above it and must not touch it…
            assert!(hint.top >= interval.bottom, "hint overlaps the field row");
            // …nor crowd the master switch below it.
            assert!(hint.bottom <= all.top, "hint overlaps the master switch");
            // Everything the layout places below the footer bound is hidden, so
            // a visible style here is the same statement as "inside the page".
            for id in [INTERVAL_HINT, ALL_SWITCHES, REFRESH_INTERVAL] {
                assert_ne!(
                    GetWindowLongPtrW(GetDlgItem(window, id as i32), GWL_STYLE)
                        & WS_VISIBLE as isize,
                    0,
                    "control {id} was laid out outside the page"
                );
            }
            release_window_bridge(window);
            assert_ne!(DestroyWindow(window), 0);
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
            connection: crate::device::Connection::Receiver,
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
            support_job: None,
            support_sequence: 0,
            support_feedback: None,
            simulated: false,
            refresh_count: 0,
            success_count: 0,
            bridge,
            panel_fonts: [null_mut(); 4],
            panel_icons: [null_mut(); 2],
            last_activity_refresh: Instant::now(),
            refresh_timer_seconds: Cell::new(0),
            presence_timer_seconds: Cell::new(0),
            awaiting_first_pass: false,
            picker_ids: Vec::new(),
            page: Page::Main,
            page_scroll: [0; 4],
            page_controls: Vec::new(),
            threshold_error: false,
            interval_error: false,
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
                draw_battery(dc, 96, 210, 0, Some(&reading), &colors);
                let x = 214 + cell * 10;
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
