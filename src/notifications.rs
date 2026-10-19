//! New valid readings alone advance this durable per-device low-battery cycle.
use crate::device::{PowerState, Reading};
use windows_sys::Win32::{Foundation::HWND, UI::Shell::*};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AlertRecord {
    pub notified: bool,
    pub last_power: Option<PowerState>,
}
impl AlertRecord {
    pub fn observe(&mut self, reading: &Reading, threshold: u8, enabled: bool) -> bool {
        let percent = reading.normalized_percent();
        let power = reading.power_state;
        let left_charging = self.last_power == Some(PowerState::Charging)
            && matches!(power, PowerState::Discharging | PowerState::Full);
        // Retain the last confirmed charging state across unknown samples.
        // The current Reading still controls the UI bolt independently.
        if !matches!(power, PowerState::Unknown(_)) {
            self.last_power = Some(power);
        }
        if percent > threshold {
            self.notified = false;
        }
        if percent >= threshold || self.last_power == Some(PowerState::Charging) {
            return false;
        }
        let eligible = !self.notified || left_charging;
        if enabled && eligible {
            self.notified = true;
            true
        } else {
            false
        }
    }
    pub fn encode(&self) -> String {
        let power = match self.last_power {
            None => "-".into(),
            Some(PowerState::Discharging) => "d".into(),
            Some(PowerState::Charging) => "c".into(),
            Some(PowerState::Full) => "f".into(),
            Some(PowerState::Unknown(n)) => format!("u{n}"),
        };
        format!("{},{}", u8::from(self.notified), power)
    }
    pub fn decode(value: &str) -> Option<Self> {
        let (notified, power) = value.split_once(',')?;
        Some(Self {
            notified: match notified {
                "0" => false,
                "1" => true,
                _ => return None,
            },
            last_power: match power {
                "-" => None,
                "d" => Some(PowerState::Discharging),
                "c" => Some(PowerState::Charging),
                "f" => Some(PowerState::Full),
                value => Some(PowerState::Unknown(value.strip_prefix('u')?.parse().ok()?)),
            },
        })
    }
}

/// Quiet-time-aware Shell notification. Windows owns presentation and suppression.
/// Never substitutes a modal dialog if the user/system suppresses notifications.
pub fn show(hwnd: HWND, shell_id: u32, model: &str, percent: u8, self_test: bool) -> bool {
    let mut data: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
    data.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    data.hWnd = hwnd;
    data.uID = shell_id;
    data.uFlags = NIF_INFO;
    data.dwInfoFlags = NIIF_INFO | NIIF_RESPECT_QUIET_TIME;
    let title = if self_test {
        "Quiet Battery 通知自测"
    } else {
        "外设电量低"
    };
    let message = if self_test {
        "这是一条隔离的通知测试，不代表任何设备的实际电量。".to_owned()
    } else {
        format!("{model} · {percent}%")
    };
    for (dst, src) in data
        .szInfoTitle
        .iter_mut()
        .take(63)
        .zip(title.encode_utf16())
    {
        *dst = src;
    }
    for (dst, src) in data.szInfo.iter_mut().take(255).zip(message.encode_utf16()) {
        *dst = src;
    }
    unsafe { Shell_NotifyIconW(NIM_MODIFY, &data) != 0 }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn read(percent: u8, power: PowerState) -> Reading {
        Reading::from_percent("fixture".into(), percent, power).unwrap()
    }
    #[test]
    fn strict_boundary_persistence_and_switches() {
        let mut record = AlertRecord::default();
        let events = [19, 18, 20, 21, 19]
            .map(|p| record.observe(&read(p, PowerState::Discharging), 20, true));
        assert_eq!(events, [true, false, false, false, true]);
        let mut restored = AlertRecord::decode(&record.encode()).unwrap();
        assert!(!restored.observe(&read(18, PowerState::Discharging), 20, false));
        assert!(!restored.observe(&read(18, PowerState::Discharging), 20, true));
        assert!(!restored.observe(&read(20, PowerState::Discharging), 20, true));
    }
    #[test]
    fn charging_exit_once_unknown_is_not_an_exit() {
        let mut record = AlertRecord::default();
        for (power, expected) in [
            (PowerState::Charging, false),
            (PowerState::Unknown(255), false),
            (PowerState::Discharging, true),
            (PowerState::Discharging, false),
            (PowerState::Charging, false),
            (PowerState::Unknown(255), false),
            (PowerState::Discharging, true),
            (PowerState::Charging, false),
            (PowerState::Full, true),
        ] {
            assert_eq!(record.observe(&read(19, power), 20, true), expected);
        }
    }
    #[test]
    fn disabled_period_keeps_consumed_cycle_and_high_value_can_reset() {
        let mut record = AlertRecord::default();
        assert!(record.observe(&read(19, PowerState::Unknown(255)), 20, true));
        assert!(!record.observe(&read(21, PowerState::Charging), 20, false));
        assert!(!record.observe(&read(19, PowerState::Charging), 20, true));
        assert!(record.observe(&read(19, PowerState::Discharging), 20, true));
    }
}
