//! DELUX Beken status reports. Wire facts independently checked against the
//! manufacturer's 20251112 driver and published captures; see DEVICE-SUPPORT.
//! No feature/output commands and no normal mouse/keyboard input collection.
use super::{
    Cancellation, Candidate, DeviceDescriptor, DeviceError, DeviceKind, DiscoveryReport,
    PowerState, ProtocolAdapter, Reading,
    diagnostics::{self, Collection},
    hid_io::{QueryGuard, Session},
    vendor_hid::number,
};
use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};
const PROVIDER: &str = "delux";
const WINDOW: Duration = Duration::from_millis(2300);
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Model {
    code: u8,
    event: u8,
    name: &'static str,
}
// Frame codes are essential: FA60 is also used by non-DELUX OEM mice. Do not
// add generic 0x10 (Attack Shark R1/M600) or 0x55 (X11) by VID/PID alone.
const MODELS: &[Model] = &[
    Model {
        code: 0x20,
        event: 0x40,
        name: "DELUX M600 Pro",
    },
    Model {
        code: 0x21,
        event: 0x40,
        name: "DELUX M600 Pro 8K",
    },
    Model {
        code: 0x30,
        event: 0x40,
        name: "DELUX M900 Pro",
    },
    Model {
        code: 0x31,
        event: 0x40,
        name: "DELUX M900",
    },
    Model {
        code: 0x32,
        event: 0x40,
        name: "DELUX M800 Pro 3395",
    },
    Model {
        code: 0x33,
        event: 0x40,
        name: "DELUX M800 Pro 3311",
    },
    Model {
        code: 0x34,
        event: 0x40,
        name: "DELUX M800 Pro+",
    },
    Model {
        code: 0x35,
        event: 0x40,
        name: "DELUX M800 Pro 3370",
    },
    Model {
        code: 0x37,
        event: 0x40,
        name: "DELUX M500 Pro",
    },
    Model {
        code: 0x38,
        event: 0x40,
        name: "DELUX M300 Pro",
    },
    Model {
        code: 0x39,
        event: 0x40,
        name: "DELUX M700 Pro",
    },
    Model {
        code: 0x3d,
        event: 0x40,
        name: "DELUX M900 Mini",
    },
    Model {
        code: 0x3f,
        event: 0x40,
        name: "DELUX M900 Pro",
    },
    Model {
        code: 0x29,
        event: 0x40,
        name: "DELUX M800 Mini",
    },
    Model {
        code: 0x2a,
        event: 0x40,
        name: "DELUX M800 Mini",
    },
    Model {
        code: 0x2b,
        event: 0x40,
        name: "DELUX M800 Mini",
    },
    // FA65 capture cross-checked with the manufacturer's 0x4A0A40 callback:
    // this variant uses event 0x41, with the same flag/percent byte order.
    Model {
        code: 0x50,
        event: 0x41,
        name: "DELUX M900 Pro",
    },
];
pub(super) struct DeluxAdapter;
pub(super) fn receiver(c: &Collection) -> bool {
    number(c, "vid") == Some(0x1d57) && matches!(number(c, "pid"), Some(0xfa60 | 0xfa65))
}
pub(super) fn target(c: &Collection) -> bool {
    receiver(c)
        && [
            ("usage_page", 0x000a),
            ("usage", 0),
            ("input_report_bytes", 5),
            ("output_report_bytes", 0),
            ("feature_report_bytes", 0),
        ]
        .into_iter()
        .all(|(k, v)| number(c, k) == Some(v))
}
fn parse(bytes: &[u8]) -> Result<Option<(Model, Reading)>, DeviceError> {
    if bytes.len() != 5 || bytes[0] != 3 {
        return Ok(None);
    }
    let Some(model) = MODELS
        .iter()
        .find(|m| m.code == bytes[1] && m.event == bytes[2])
        .copied()
    else {
        return Ok(None);
    };
    let power = match bytes[3] {
        1 => PowerState::Discharging,
        2 => PowerState::Full,
        3 => PowerState::Charging,
        n => PowerState::Unknown(n),
    };
    // Keep the actual value, including 0 and charging values; no dock filter,
    // guessed 100%, smoothing, discharge timer, or invented firmware version.
    Reading::from_percent(model.name.into(), bytes[4], power).map(|r| Some((model, r)))
}
trait StatusInput {
    fn read(&self) -> Result<Option<Vec<u8>>, DeviceError>;
}
struct Native<'a> {
    session: Session,
    deadline: Instant,
    cancel: &'a Cancellation,
}
impl StatusInput for Native<'_> {
    fn read(&self) -> Result<Option<Vec<u8>>, DeviceError> {
        self.session.input_sized(5, self.deadline, self.cancel)
    }
}
fn listen(
    io: &dyn StatusInput,
    expected: Option<Model>,
    cancel: &Cancellation,
) -> Result<(Model, Reading), DeviceError> {
    for _ in 0..64 {
        cancel.check()?;
        let Some(packet) = io.read()? else {
            break;
        };
        if let Some((model, reading)) = parse(&packet)? {
            if expected.is_some_and(|m| m != model) {
                return Err(DeviceError::NotFound);
            }
            return Ok((model, reading));
        }
    }
    Err(DeviceError::Unavailable(
        "多彩状态通道暂未上报电量；休眠或静止时保留上次读数".into(),
    ))
}
fn inventory(cancel: &Cancellation) -> Result<Vec<Collection>, DeviceError> {
    let inv = diagnostics::inventory(cancel)?;
    if !inv.is_complete() {
        return Err(DeviceError::Unavailable("多彩HID清单不完整".into()));
    }
    if inv.collections.iter().any(identity_unavailable) {
        return Err(DeviceError::Unavailable(
            "多彩接收器身份字段暂不可用".into(),
        ));
    }
    Ok(inv.collections)
}
fn identity_unavailable(c: &Collection) -> bool {
    let path = c.path.to_ascii_lowercase();
    (path.contains("vid_1d57&pid_fa60") || path.contains("vid_1d57&pid_fa65"))
        && (number(c, "vid").is_none() || number(c, "pid").is_none())
}
fn descriptor(c: &Collection, model: Model) -> DeviceDescriptor {
    let key = c.key();
    let route = format!(
        "{:04x}-{:02x}-{:02x}",
        number(c, "pid").unwrap(),
        model.code,
        model.event
    );
    DeviceDescriptor {
        id: format!("{PROVIDER}:{key}:{route}"),
        physical_id: key,
        endpoint: Some(route),
        recoverable: true,
        name: model.name.into(),
        kind: DeviceKind::Mouse,
        supported: true,
        provider: PROVIDER.into(),
        // Only the 1D57:FA60/FA65 receivers are claimed by this adapter.
        connection: super::Connection::Receiver,
    }
}
fn query(
    endpoints: &[&Collection],
    expected: Option<Model>,
    deadline: Instant,
    cancel: &Cancellation,
) -> Result<(Model, Reading), DeviceError> {
    let mut error = DeviceError::Unavailable("多彩专用状态集合不存在或元数据不可用".into());
    for c in endpoints.iter().filter(|c| target(c)).take(4) {
        cancel.check()?;
        if Instant::now() >= deadline {
            break;
        }
        let key = c.key();
        let result = (|| {
            let lease = QueryGuard::acquire(&key, deadline, cancel)?;
            let session = Session::open_readonly(&c.path, &key, lease, cancel)?;
            listen(
                &Native {
                    session,
                    deadline,
                    cancel,
                },
                expected,
                cancel,
            )
        })();
        match result {
            Ok(v) => return Ok(v),
            Err(e @ (DeviceError::Cancelled | DeviceError::NotFound)) => return Err(e),
            Err(e) => error = e,
        }
    }
    Err(error)
}
impl ProtocolAdapter for DeluxAdapter {
    fn name(&self) -> &'static str {
        PROVIDER
    }
    fn owns(&self, id: &str) -> bool {
        id.starts_with("delux:")
    }
    fn discover(&self, cancel: &Cancellation) -> Result<Vec<Candidate>, DeviceError> {
        self.discover_report(cancel).map(|r| r.candidates)
    }
    fn discover_report(&self, cancel: &Cancellation) -> Result<DiscoveryReport, DeviceError> {
        cancel.check()?;
        let inv = inventory(cancel)?;
        let mut groups: BTreeMap<(String, u16), Vec<&Collection>> = BTreeMap::new();
        for c in &inv {
            if receiver(c) {
                groups
                    .entry((c.key(), number(c, "pid").unwrap()))
                    .or_default()
                    .push(c);
            }
        }
        let start = Instant::now();
        let mut report = DiscoveryReport::default();
        for (index, (_, mut endpoints)) in groups.into_iter().enumerate() {
            cancel.check()?;
            if index >= 8 || start.elapsed() >= Duration::from_secs(12) {
                report
                    .failures
                    .push((PROVIDER.into(), "多彩发现预算用尽".into()));
                break;
            }
            endpoints.sort_by(|a, b| a.path.cmp(&b.path));
            let deadline = (Instant::now() + WINDOW).min(start + Duration::from_secs(12));
            match query(&endpoints, None, deadline, cancel) {
                Ok((model, reading)) => report.candidates.push(Candidate {
                    descriptor: descriptor(endpoints[0], model),
                    reading: Some(reading),
                    error: None,
                }),
                Err(DeviceError::Cancelled) => return Err(DeviceError::Cancelled),
                Err(e) => report.failures.push((PROVIDER.into(), e.to_string())),
            }
            // No valid model frame means no claim. Generic discovery still
            // lists this physical device; do not label every FA60 as DELUX.
        }
        cancel.check()?;
        Ok(report)
    }
    fn read(&self, id: &str, cancel: &Cancellation) -> Result<Reading, DeviceError> {
        cancel.check()?;
        let inv = inventory(cancel)?;
        let mut found = None;
        for c in inv.iter().filter(|c| receiver(c)) {
            for &model in MODELS {
                if descriptor(c, model).id == id {
                    found = Some((c.key(), number(c, "pid").unwrap(), model));
                    break;
                }
            }
            if found.is_some() {
                break;
            }
        }
        let Some((key, pid, model)) = found else {
            if inv
                .iter()
                .any(|c| id.starts_with(&format!("delux:{}:", c.key())))
            {
                return Err(DeviceError::Unavailable("多彩设备身份元数据不可用".into()));
            }
            return Err(DeviceError::NotFound);
        };
        let mut endpoints: Vec<_> = inv
            .iter()
            .filter(|c| c.key() == key && number(c, "pid") == Some(pid))
            .collect();
        endpoints.sort_by(|a, b| a.path.cmp(&b.path));
        query(&endpoints, Some(model), Instant::now() + WINDOW, cancel).map(|(_, r)| r)
    }
}
#[cfg(test)]
mod tests {
    use super::super::{diagnostics::Field, vendor_hid::tests::collection};
    use super::*;
    use std::cell::RefCell;
    struct Input(RefCell<std::collections::VecDeque<Option<Vec<u8>>>>);
    impl Input {
        fn new(frames: Vec<Option<Vec<u8>>>) -> Self {
            Self(RefCell::new(frames.into()))
        }
    }
    impl StatusInput for Input {
        fn read(&self) -> Result<Option<Vec<u8>>, DeviceError> {
            Ok(self
                .0
                .borrow_mut()
                .pop_front()
                .expect("unexpected extra input read"))
        }
    }
    fn endpoint() -> Collection {
        let mut c = collection(0x1d57, 0xfa65, "receiver-a");
        for (k, v) in [
            ("usage_page", 10),
            ("usage", 0),
            ("input_report_bytes", 5),
            ("output_report_bytes", 0),
            ("feature_report_bytes", 0),
        ] {
            c.fields.insert(k, Field::number(v, "fixture"));
        }
        c
    }
    #[test]
    fn dedicated_status_collection_only_never_mouse_keyboard_or_config() {
        let mut c = endpoint();
        assert!(target(&c));
        for pid in [0xfa60, 0xfa65] {
            c.fields.insert("pid", Field::number(pid, "fixture"));
            assert!(target(&c));
        }
        for (k, v) in [
            ("vid", 0x1d58),
            ("pid", 0xfa66),
            ("usage_page", 1),
            ("usage_page", 0xff00),
            ("usage", 1),
            ("input_report_bytes", 6),
            ("output_report_bytes", 1),
            ("feature_report_bytes", 1),
        ] {
            let before = c.fields.insert(k, Field::number(v, "fixture")).unwrap();
            assert!(!target(&c));
            c.fields.insert(k, before);
        }
        c.fields.remove("input_report_bytes");
        assert!(!target(&c));
    }
    #[test]
    fn original_m900_and_m600_captures_and_alternate_frame() {
        for (packet, name, percent, power) in [
            (
                [3, 0x30, 0x40, 1, 0x59],
                "DELUX M900 Pro",
                89,
                PowerState::Discharging,
            ),
            (
                [3, 0x30, 0x40, 3, 0x59],
                "DELUX M900 Pro",
                89,
                PowerState::Charging,
            ),
            (
                [3, 0x20, 0x40, 1, 0x64],
                "DELUX M600 Pro",
                100,
                PowerState::Discharging,
            ),
            (
                [3, 0x50, 0x41, 1, 0x55],
                "DELUX M900 Pro",
                85,
                PowerState::Discharging,
            ),
        ] {
            let (_, r) = parse(&packet).unwrap().unwrap();
            assert_eq!(
                (
                    r.device_name.as_str(),
                    r.normalized_percent(),
                    r.power_state
                ),
                (name, percent, power)
            );
            assert!(!r.metadata_known);
        }
    }
    #[test]
    fn all_model_percent_boundaries_power_flags_and_no_dock_smoothing() {
        for model in MODELS {
            for percent in 0..=100 {
                let (_, r) = parse(&[3, model.code, model.event, 3, percent])
                    .unwrap()
                    .unwrap();
                assert_eq!(r.normalized_percent(), percent);
            }
            for percent in 101..=255 {
                assert!(parse(&[3, model.code, model.event, 1, percent]).is_err());
            }
        }
        for (flag, power) in [
            (1, PowerState::Discharging),
            (2, PowerState::Full),
            (3, PowerState::Charging),
            (9, PowerState::Unknown(9)),
        ] {
            let (_, r) = parse(&[3, 0x30, 0x40, flag, 19]).unwrap().unwrap();
            assert_eq!(r.power_state, power);
            assert_eq!(r.normalized_percent(), 19);
        }
        assert_eq!(
            parse(&[3, 0x30, 0x40, 9, 100])
                .unwrap()
                .unwrap()
                .1
                .power_state,
            PowerState::Unknown(9)
        );
    }
    #[test]
    fn unknown_oem_wrong_events_truncation_and_padding_are_not_batteries() {
        let b = [3, 0x30, 0x40, 1, 35];
        for n in 0..5 {
            assert!(parse(&b[..n]).unwrap().is_none());
        }
        for bad in [
            &[3, 0x55, 0x40, 1, 35][..],
            &[3, 0x10, 0x40, 1, 35],
            &[4, 0x30, 0x40, 1, 35],
            &[3, 0x30, 0x50, 1, 35],
            &[3, 0x50, 0x40, 1, 35],
            &[3, 0x30, 0x40, 1, 35, 0],
        ] {
            assert!(parse(bad).unwrap().is_none());
        }
    }
    #[test]
    fn complete_passive_transaction_ignores_unrelated_silence_and_cancel_preserve_cache() {
        let c = Cancellation::default();
        let m = MODELS.iter().find(|m| m.code == 0x30).copied().unwrap();
        let io = Input::new(vec![
            Some(vec![3, 0x30, 0x50, 0, 4]),
            Some(vec![3, 0x30, 0x40, 1, 35]),
        ]);
        assert_eq!(listen(&io, Some(m), &c).unwrap().1.normalized_percent(), 35);
        assert!(io.0.borrow().is_empty());
        let io = Input::new(vec![None]);
        assert!(matches!(
            listen(&io, Some(m), &c),
            Err(DeviceError::Unavailable(_))
        ));
        let io = Input::new(vec![Some(vec![3, 0x20, 0x40, 1, 50])]);
        assert!(matches!(
            listen(&io, Some(m), &c),
            Err(DeviceError::NotFound)
        ));
        let c = Cancellation::default();
        c.cancel();
        let io = Input::new(vec![]);
        assert!(matches!(listen(&io, None, &c), Err(DeviceError::Cancelled)));
        let io = Input::new(vec![Some(vec![3, 0x55, 0x40, 1, 35]); 64]);
        assert!(matches!(
            listen(&io, None, &Cancellation::default()),
            Err(DeviceError::Unavailable(_))
        ));
        assert!(io.0.borrow().is_empty());
    }
    #[test]
    fn identity_distinguishes_models_receivers_and_routes_without_serial_guesses() {
        let a = endpoint();
        let mut b = a.clone();
        b.container = Some("receiver-b".into());
        assert_ne!(descriptor(&a, MODELS[0]).id, descriptor(&a, MODELS[1]).id);
        assert_ne!(descriptor(&a, MODELS[0]).id, descriptor(&b, MODELS[0]).id);
        b = a.clone();
        b.fields.insert("pid", Field::number(0xfa60, "fixture"));
        assert_ne!(descriptor(&a, MODELS[0]).id, descriptor(&b, MODELS[0]).id);
        assert!(DeluxAdapter.owns(&descriptor(&a, MODELS[0]).id));
        assert!(!DeluxAdapter.owns("delux-other:fixture"));
    }
    #[test]
    fn sleeping_device_retains_its_own_cache_and_different_model_starts_fresh() {
        use crate::manager::Manager;
        let endpoint = endpoint();
        let model = MODELS.iter().find(|m| m.code == 0x30).copied().unwrap();
        let mut m = Manager::default();
        m.merge(
            vec![Candidate {
                descriptor: descriptor(&endpoint, model),
                reading: Some(
                    Reading::from_percent(model.name.into(), 89, PowerState::Discharging).unwrap(),
                ),
                error: None,
            }],
            100,
        );
        let id = m.devices[0].descriptor.id.clone();
        m.merge_discovery(
            DiscoveryReport {
                candidates: vec![],
                failures: vec![(PROVIDER.into(), "silent receiver".into())],
            },
            101,
        );
        m.apply(&id, Err(DeviceError::Unavailable("no status".into())), 102);
        assert_eq!(m.devices[0].last_success, Some(100));
        assert_eq!(
            m.devices[0].reading.as_ref().unwrap().normalized_percent(),
            89
        );
        assert_eq!(m.visible().count(), 1);
        let model = MODELS.iter().find(|m| m.code == 0x20).copied().unwrap();
        m.merge(
            vec![Candidate {
                descriptor: descriptor(&endpoint, model),
                reading: Some(
                    Reading::from_percent(model.name.into(), 35, PowerState::Charging).unwrap(),
                ),
                error: None,
            }],
            103,
        );
        assert!(
            !m.devices
                .iter()
                .find(|d| d.descriptor.id == id)
                .unwrap()
                .connected
        );
        let active = m.visible().next().unwrap();
        assert_eq!(active.reading.as_ref().unwrap().normalized_percent(), 35);
        assert_eq!(active.last_success, Some(103));
    }
    #[test]
    fn present_receiver_with_missing_identity_is_not_proof_of_disconnect() {
        let mut c = endpoint();
        c.path = r"\\?\hid#vid_1d57&pid_fa65&mi_02&col03#fixture".into();
        assert!(!identity_unavailable(&c));
        for key in ["vid", "pid"] {
            let saved = c.fields.remove(key).unwrap();
            assert!(identity_unavailable(&c));
            c.fields.insert(key, saved);
        }
        c.path = r"\\?\hid#vid_046d&pid_c53f#fixture".into();
        c.fields.remove("pid");
        assert!(!identity_unavailable(&c));
    }
}
