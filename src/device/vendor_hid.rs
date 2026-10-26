//! Shared bounded transport and discovery for explicitly profiled vendor protocols.
//! Profiles are endpoint rules, never proof of a successful battery query.
use super::{
    Cancellation, Candidate, DeviceDescriptor, DeviceError, DeviceKind, DiscoveryReport,
    ProtocolAdapter, Reading,
    diagnostics::{self, Collection},
    hid_io::{QueryGuard, Session},
};
use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug)]
pub(super) struct Profile {
    pub name: &'static str,
    pub vid: u16,
    pub pid: u16,
    pub wireless: bool,
}
impl Profile {
    fn endpoint(self) -> String {
        format!(
            "{:04x}-{:04x}-{}",
            self.vid,
            self.pid,
            if self.wireless { "w" } else { "c" }
        )
    }
}
pub(super) fn number(c: &Collection, key: &str) -> Option<u16> {
    c.fields.get(key)?.value.as_deref()?.parse().ok()
}
pub(super) trait Transport {
    fn drain(&self, length: usize) -> Result<(), DeviceError>;
    fn output(&self, report: &[u8]) -> Result<(), DeviceError>;
    fn input(&self, length: usize) -> Result<Option<Vec<u8>>, DeviceError>;
    fn set_feature(&self, report: &[u8]) -> Result<(), DeviceError>;
    fn get_feature(&self, report_id: u8, length: usize) -> Result<Vec<u8>, DeviceError>;
    fn pause(&self, duration: Duration) -> Result<(), DeviceError>;
}
struct Native<'a> {
    session: Session,
    deadline: Instant,
    cancel: &'a Cancellation,
}
impl Transport for Native<'_> {
    fn drain(&self, length: usize) -> Result<(), DeviceError> {
        for _ in 0..8 {
            self.cancel.check()?;
            if self
                .session
                .input_sized(
                    length,
                    (Instant::now() + Duration::from_millis(2)).min(self.deadline),
                    self.cancel,
                )?
                .is_none()
            {
                return Ok(());
            }
        }
        Err(DeviceError::Unavailable("厂商输入队列繁忙".into()))
    }
    fn output(&self, report: &[u8]) -> Result<(), DeviceError> {
        self.session.output(report, self.deadline, self.cancel)
    }
    fn input(&self, length: usize) -> Result<Option<Vec<u8>>, DeviceError> {
        self.session.input_sized(length, self.deadline, self.cancel)
    }
    fn set_feature(&self, report: &[u8]) -> Result<(), DeviceError> {
        self.session.set_feature(report, self.deadline, self.cancel)
    }
    fn get_feature(&self, id: u8, length: usize) -> Result<Vec<u8>, DeviceError> {
        self.session
            .get_feature(id, length, self.deadline, self.cancel)
    }
    fn pause(&self, duration: Duration) -> Result<(), DeviceError> {
        let until = (Instant::now() + duration).min(self.deadline);
        while Instant::now() < until {
            self.cancel.check()?;
            std::thread::sleep(
                Duration::from_millis(5).min(until.saturating_duration_since(Instant::now())),
            );
        }
        self.cancel.check()?;
        if Instant::now() >= self.deadline {
            Err(DeviceError::Unavailable("厂商查询超时".into()))
        } else {
            Ok(())
        }
    }
}
pub(super) struct Adapter {
    pub name: &'static str,
    pub identify: fn(&Collection) -> Option<Profile>,
    pub accepts: fn(&Collection) -> bool,
    pub feature_only: bool,
    pub query: fn(Profile, &dyn Transport, &Cancellation) -> Result<Reading, DeviceError>,
}
impl Adapter {
    fn descriptor(&self, p: Profile, key: &str) -> DeviceDescriptor {
        let endpoint = p.endpoint();
        DeviceDescriptor {
            id: format!("{}:{key}:{endpoint}", self.name),
            physical_id: key.into(),
            endpoint: Some(endpoint),
            recoverable: true,
            name: p.name.into(),
            kind: DeviceKind::Mouse,
            supported: true,
            provider: self.name.into(),
        }
    }
    fn candidate(
        &self,
        p: Profile,
        key: &str,
        result: Result<Reading, DeviceError>,
    ) -> Result<Candidate, DeviceError> {
        let mut descriptor = self.descriptor(p, key);
        let (reading, error) = match result {
            Ok(r) => (Some(r), None),
            Err(DeviceError::Cancelled) => return Err(DeviceError::Cancelled),
            Err(e) => {
                descriptor.supported = false;
                (None, Some(e))
            }
        };
        Ok(Candidate {
            descriptor,
            reading,
            error,
        })
    }
    fn inventory(&self, cancel: &Cancellation) -> Result<Vec<Collection>, DeviceError> {
        let inventory = diagnostics::inventory(cancel)?;
        if !inventory.is_complete() {
            return Err(DeviceError::Unavailable(format!(
                "{} HID清单不完整",
                self.name
            )));
        }
        Ok(inventory.collections)
    }
    fn query_endpoints(
        &self,
        p: Profile,
        endpoints: &[&Collection],
        cancel: &Cancellation,
    ) -> Result<Reading, DeviceError> {
        let deadline = Instant::now() + Duration::from_millis(1600);
        let key = endpoints[0].key();
        let mut error = DeviceError::Unavailable("未找到符合协议的厂商集合或元数据不可用".into());
        // Stable order, shared budget, at most four eligible control collections.
        for c in endpoints.iter().filter(|c| (self.accepts)(c)).take(4) {
            cancel.check()?;
            if Instant::now() >= deadline {
                break;
            }
            let result = (|| {
                let guard = QueryGuard::acquire(&key, deadline, cancel)?;
                let session =
                    Session::open_access(&c.path, &key, guard, cancel, self.feature_only)?;
                (self.query)(
                    p,
                    &Native {
                        session,
                        deadline,
                        cancel,
                    },
                    cancel,
                )
            })();
            match result {
                Ok(reading) => return Ok(reading),
                Err(DeviceError::Cancelled) => return Err(DeviceError::Cancelled),
                Err(e) => error = e,
            }
        }
        Err(error)
    }
}
impl ProtocolAdapter for Adapter {
    fn name(&self) -> &'static str {
        self.name
    }
    fn owns(&self, id: &str) -> bool {
        id.strip_prefix(self.name)
            .is_some_and(|tail| tail.starts_with(':'))
    }
    fn discover(&self, cancel: &Cancellation) -> Result<Vec<Candidate>, DeviceError> {
        self.discover_report(cancel).map(|r| r.candidates)
    }
    fn discover_report(&self, cancel: &Cancellation) -> Result<DiscoveryReport, DeviceError> {
        cancel.check()?;
        let inventory = self.inventory(cancel)?;
        let mut groups: BTreeMap<(String, u16), (Profile, Vec<&Collection>)> = BTreeMap::new();
        for c in &inventory {
            if let Some(p) = (self.identify)(c) {
                groups
                    .entry((c.key(), p.pid))
                    .or_insert_with(|| (p, Vec::new()))
                    .1
                    .push(c);
            }
        }
        let start = Instant::now();
        let mut report = DiscoveryReport::default();
        for (position, ((key, _), (p, mut endpoints))) in groups.into_iter().enumerate() {
            cancel.check()?;
            if position >= 8 || start.elapsed() >= Duration::from_secs(12) {
                report
                    .failures
                    .push((self.name.into(), "厂商发现预算用尽".into()));
                break;
            }
            endpoints.sort_by(|a, b| a.path.cmp(&b.path));
            report.candidates.push(self.candidate(
                p,
                &key,
                self.query_endpoints(p, &endpoints, cancel),
            )?);
        }
        cancel.check()?;
        Ok(report)
    }
    fn read(&self, id: &str, cancel: &Cancellation) -> Result<Reading, DeviceError> {
        cancel.check()?;
        let inventory = self.inventory(cancel)?;
        let mut endpoints = Vec::new();
        let mut profile = None;
        for c in &inventory {
            if let Some(p) = (self.identify)(c) {
                if self.descriptor(p, &c.key()).id == id {
                    profile = Some(p);
                    endpoints.push(c);
                }
            }
        }
        endpoints.sort_by(|a, b| a.path.cmp(&b.path));
        let p = profile.ok_or_else(|| {
            // An endpoint with unreadable IDs is not proof of physical disconnection.
            if inventory
                .iter()
                .any(|c| id.starts_with(&format!("{}:{}:", self.name, c.key())))
            {
                DeviceError::Unavailable("设备身份元数据不可用".into())
            } else {
                DeviceError::NotFound
            }
        })?;
        self.query_endpoints(p, &endpoints, cancel)
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use std::cell::RefCell;
    pub enum Step {
        Output(Vec<u8>),
        Input(Option<Vec<u8>>),
        Set(Vec<u8>),
        Get(Vec<u8>),
        Failure,
        Pause,
        Drain,
    }
    pub struct Script(pub RefCell<std::collections::VecDeque<Step>>);
    impl Script {
        pub fn new(steps: Vec<Step>) -> Self {
            Self(RefCell::new(steps.into()))
        }
        fn next(&self) -> Step {
            self.0
                .borrow_mut()
                .pop_front()
                .expect("unexpected protocol operation")
        }
        pub fn done(&self) {
            assert!(self.0.borrow().is_empty(), "unconsumed protocol operations");
        }
    }
    impl Transport for Script {
        fn drain(&self, length: usize) -> Result<(), DeviceError> {
            assert!(
                matches!(length, 17 | 64),
                "unexpected drain length {length}"
            );
            match self.next() {
                Step::Drain => Ok(()),
                _ => panic!("expected drain"),
            }
        }
        fn output(&self, report: &[u8]) -> Result<(), DeviceError> {
            match self.next() {
                Step::Output(expected) => {
                    assert_eq!(report, expected);
                    Ok(())
                }
                Step::Failure => Err(DeviceError::Unavailable("injected output failure".into())),
                _ => panic!("expected output"),
            }
        }
        fn input(&self, length: usize) -> Result<Option<Vec<u8>>, DeviceError> {
            match self.next() {
                Step::Input(value) => {
                    assert!(value.as_ref().is_none_or(|v| v.len() <= length));
                    Ok(value)
                }
                _ => panic!("expected input"),
            }
        }
        fn set_feature(&self, report: &[u8]) -> Result<(), DeviceError> {
            match self.next() {
                Step::Set(expected) => {
                    assert_eq!(report, expected);
                    Ok(())
                }
                _ => panic!("expected set feature"),
            }
        }
        fn get_feature(&self, id: u8, length: usize) -> Result<Vec<u8>, DeviceError> {
            assert_eq!((id, length), (0, 91));
            match self.next() {
                Step::Get(value) => Ok(value),
                Step::Failure => Err(DeviceError::Io("injected feature failure".into())),
                _ => panic!("expected get feature"),
            }
        }
        fn pause(&self, _: Duration) -> Result<(), DeviceError> {
            match self.next() {
                Step::Pause => Ok(()),
                _ => panic!("expected pause"),
            }
        }
    }
    pub fn collection(vid: u16, pid: u16, key: &str) -> Collection {
        let mut c = Collection {
            container: Some(key.into()),
            ..Default::default()
        };
        for (k, v) in [("vid", vid), ("pid", pid)] {
            c.fields.insert(k, diagnostics::Field::number(v, "fixture"));
        }
        c
    }
    #[test]
    fn profiled_identity_is_physical_and_route_specific() {
        let a = super::super::compx::ADAPTER;
        let c = collection(0x3554, 0xf58a, "receiver-a");
        let p = (a.identify)(&c).unwrap();
        let d = a.descriptor(p, &c.key());
        assert!(a.owns(&d.id));
        assert!(!super::super::razer::ADAPTER.owns(&d.id));
        assert_ne!(d.id, a.descriptor(p, "receiver-b").id);
        assert_ne!(
            d.id,
            a.descriptor(Profile { pid: 0xf58c, ..p }, &c.key()).id
        );
        assert!(!(a.accepts)(&c)); // IDs alone never permit traffic.
    }
    #[test]
    fn vendor_sleep_failure_recovery_and_multi_receiver_cache_are_isolated() {
        use super::super::PowerState;
        let adapter = super::super::compx::ADAPTER;
        let p = (adapter.identify)(&collection(0x3554, 0xf58a, "unused")).unwrap();
        let success = |key: &str, percent| {
            adapter
                .candidate(
                    p,
                    key,
                    Reading::from_percent(p.name.into(), percent, PowerState::Discharging),
                )
                .unwrap()
        };
        let mut manager = crate::manager::Manager::default();
        manager.merge(vec![success("a", 35), success("b", 80)], 100);
        let failed = adapter
            .candidate(p, "a", Err(DeviceError::Unavailable("asleep".into())))
            .unwrap();
        manager.merge(vec![failed, success("b", 79)], 200);
        let a = manager
            .devices
            .iter()
            .find(|s| s.descriptor.physical_id == "a")
            .unwrap();
        assert_eq!(a.reading.as_ref().unwrap().normalized_percent(), 35);
        assert_eq!(a.last_success, Some(100));
        assert!(a.connected && a.selected && a.error.is_some());
        let b = manager
            .devices
            .iter()
            .find(|s| s.descriptor.physical_id == "b")
            .unwrap();
        assert_eq!(b.last_success, Some(200));
        assert!(b.error.is_none());
        manager.merge(vec![success("a", 34), success("b", 79)], 300);
        let a = manager
            .devices
            .iter()
            .find(|s| s.descriptor.physical_id == "a")
            .unwrap();
        assert_eq!(a.last_success, Some(300));
        assert!(a.error.is_none());
        assert!(matches!(
            adapter.candidate(p, "a", Err(DeviceError::Cancelled)),
            Err(DeviceError::Cancelled)
        ));
    }
    #[test]
    fn cancellation_precedes_inventory_or_open() {
        let cancel = Cancellation::default();
        cancel.cancel();
        for a in [&super::super::compx::ADAPTER, &super::super::razer::ADAPTER] {
            assert!(matches!(a.discover(&cancel), Err(DeviceError::Cancelled)));
            assert!(matches!(
                a.read("anything", &cancel),
                Err(DeviceError::Cancelled)
            ));
        }
    }
}
