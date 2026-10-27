//! Verified legacy Darmoshark N3 queries, from the manufacturer's web driver.
//! https://www.darmoshark.cc/main.c13d27c0c38ce98a.js, class Bt and transceiver g.
//! Receiver slot identity cannot distinguish a replacement N3 paired to the same dongle.
use super::{
    Cancellation, Candidate, DeviceDescriptor, DeviceError, DeviceKind, DiscoveryReport,
    PowerState, ProtocolAdapter, Reading,
    diagnostics::{self, Collection},
    hid_io::{QueryGuard, Session},
};
use std::{
    collections::BTreeSet,
    time::{Duration, Instant},
};
const NAME: &str = "Darmoshark N3";
const ENDPOINT: &str = "paired-248a-ff10";
pub(super) struct DarmosharkAdapter;
fn number(c: &Collection, key: &str) -> Option<u16> {
    c.fields.get(key)?.value.as_deref()?.parse().ok()
}
pub(super) fn target(c: &Collection) -> bool {
    [
        ("vid", 0x248a),
        ("pid", 0xff30),
        ("usage_page", 0x008c),
        ("usage", 1),
        ("interface", 1),
        ("input_report_bytes", 33),
        ("feature_report_bytes", 1001),
    ]
    .into_iter()
    .all(|(key, value)| number(c, key) == Some(value))
}
fn endpoints(cancel: &Cancellation) -> Result<Vec<Collection>, DeviceError> {
    let inventory = diagnostics::inventory(cancel)?;
    if !inventory.is_complete() {
        return Err(DeviceError::Unavailable(
            "Darmoshark HID inventory incomplete".into(),
        ));
    }
    for c in &inventory.collections {
        let path = c.path.to_ascii_lowercase();
        if path.contains("vid_248a&pid_ff30&mi_01") {
            // A present, potentially relevant endpoint with unavailable caps is
            // unconfirmed, never proof that a cached N3 disconnected.
            for key in [
                "vid",
                "pid",
                "usage_page",
                "usage",
                "interface",
                "input_report_bytes",
                "feature_report_bytes",
            ] {
                if number(c, key).is_none() {
                    return Err(DeviceError::Unavailable(
                        "Darmoshark endpoint metadata incomplete".into(),
                    ));
                }
            }
        }
    }
    Ok(inventory.collections.into_iter().filter(target).collect())
}
fn descriptor(c: &Collection) -> DeviceDescriptor {
    let key = c.key();
    DeviceDescriptor {
        id: format!("darmoshark:{key}:{ENDPOINT}"),
        physical_id: key,
        endpoint: Some(ENDPOINT.into()),
        recoverable: true,
        name: NAME.into(),
        kind: DeviceKind::Mouse,
        supported: true,
        provider: "darmoshark".into(),
        // The claimed endpoint is the 248A:FF30 2.4G receiver.
        connection: super::Connection::Receiver,
    }
}
fn reply(raw: &[u8], command: u8, minimum: usize) -> Result<&[u8], DeviceError> {
    if raw.len() < minimum || raw[0] != 0x51 || raw[1] != command {
        return Err(DeviceError::Protocol(
            "Darmoshark reply report/command/length mismatch".into(),
        ));
    }
    Ok(raw)
}
fn receiver(raw: &[u8]) -> Result<(), DeviceError> {
    let b = reply(raw, 3, 8)?;
    if b[3..7] != [0x8a, 0x24, 0x10, 0xff] {
        return Err(DeviceError::Unsupported(
            "Darmoshark paired mouse identity unverified".into(),
        ));
    }
    if b[7] != 1 {
        return Err(DeviceError::Unavailable(
            "Darmoshark mouse not connected".into(),
        ));
    }
    Ok(())
}
fn battery(raw: &[u8]) -> Result<Reading, DeviceError> {
    let b = reply(raw, 6, 14)?;
    if b[4..8] != [0x8a, 0x24, 0x10, 0xff] {
        return Err(DeviceError::Protocol(
            "Darmoshark paired identity changed".into(),
        ));
    }
    let mut reading = Reading::from_percent(
        NAME.into(),
        b[11],
        if b[12] == 0 {
            PowerState::Discharging
        } else {
            PowerState::Unknown(b[12])
        },
    )?;
    reading.protocol = (b[3], b[2]);
    Ok(reading)
}
#[derive(Debug, PartialEq)]
enum Ack {
    Unrelated,
    Progress,
    Ready,
    Busy,
}
fn ack(raw: &[u8]) -> Ack {
    if raw.len() < 3 || !matches!(raw[0], 0x54 | 0xb1) || raw[1] != 0xe4 {
        return Ack::Unrelated;
    }
    match raw[2] {
        0 => Ack::Progress,
        1 => Ack::Ready,
        _ => Ack::Busy,
    }
}
fn drain(s: &Session, deadline: Instant, cancel: &Cancellation) -> Result<(), DeviceError> {
    for _ in 0..8 {
        cancel.check()?;
        if Instant::now() >= deadline {
            return Err(DeviceError::Unavailable(
                "Darmoshark input drain budget exhausted".into(),
            ));
        }
        if s.input(
            (Instant::now() + Duration::from_millis(2)).min(deadline),
            cancel,
        )?
        .is_none()
        {
            return Ok(());
        }
    }
    Err(DeviceError::Unavailable(
        "Darmoshark vendor input queue busy".into(),
    ))
}
#[derive(Debug)]
struct QueryFailure {
    error: DeviceError,
    identity_confirmed: bool,
}
impl QueryFailure {
    fn identity_changed(&self) -> bool {
        // The receiver parser is the only source of Unsupported in this query:
        // a structurally valid reply explicitly reports a different paired ID.
        !self.identity_confirmed && matches!(self.error, DeviceError::Unsupported(_))
    }
    fn into_read_error(self) -> DeviceError {
        if self.identity_changed() {
            DeviceError::NotFound
        } else {
            self.error
        }
    }
}
fn unavailable(c: &Collection, failure: QueryFailure) -> Candidate {
    let mut d = descriptor(c);
    d.supported = false;
    d.recoverable = !failure.identity_changed();
    if !failure.identity_confirmed {
        d.endpoint = None;
        d.id = format!("darmoshark:{}:unavailable", c.key());
        d.name = "Darmoshark 外设".into();
        d.kind = DeviceKind::Generic;
    }
    Candidate {
        descriptor: d,
        reading: None,
        error: Some(failure.error),
    }
}
fn query(c: &Collection, cancel: &Cancellation) -> Result<Reading, QueryFailure> {
    let mut identity_confirmed = false;
    let result = (|| {
        cancel.check()?;
        let deadline = Instant::now() + Duration::from_millis(1500);
        let key = c.key();
        let guard = QueryGuard::acquire(&key, deadline, cancel)?;
        let s = Session::open(&c.path, &key, guard, cancel)?;
        drain(&s, deadline, cancel)?;
        s.feature(Some(3), deadline, cancel)?;
        let state = s.feature(None, deadline, cancel)?;
        identity_confirmed = reply(&state, 3, 8).is_ok() && state[3..7] == [0x8a, 0x24, 0x10, 0xff];
        receiver(&state)?;
        drain(&s, deadline, cancel)?;
        s.feature(Some(6), deadline, cancel)?;
        for _ in 0..64 {
            let b = s.input(deadline, cancel)?.ok_or_else(|| {
                DeviceError::Unavailable("Darmoshark battery acknowledgement timed out".into())
            })?;
            match ack(&b) {
                Ack::Ready => return battery(&s.feature(None, deadline, cancel)?),
                Ack::Busy => {
                    return Err(DeviceError::Unavailable(
                        "Darmoshark receiver busy/error".into(),
                    ));
                }
                _ => {}
            }
        }
        Err(DeviceError::Unavailable(
            "Darmoshark acknowledgement count limit".into(),
        ))
    })();
    result.map_err(|error| QueryFailure {
        error,
        identity_confirmed,
    })
}
impl ProtocolAdapter for DarmosharkAdapter {
    fn name(&self) -> &'static str {
        "darmoshark"
    }
    fn owns(&self, id: &str) -> bool {
        id.starts_with("darmoshark:")
    }
    fn discover(&self, cancel: &Cancellation) -> Result<Vec<Candidate>, DeviceError> {
        self.discover_report(cancel).map(|r| r.candidates)
    }
    fn discover_report(&self, cancel: &Cancellation) -> Result<DiscoveryReport, DeviceError> {
        let endpoints = endpoints(cancel)?;
        let start = Instant::now();
        let mut report = DiscoveryReport::default();
        let mut seen = BTreeSet::new();
        for c in &endpoints {
            cancel.check()?;
            let key = c.key();
            if !seen.insert(key.clone()) {
                continue;
            }
            if seen.len() > 8 || start.elapsed() > Duration::from_secs(12) {
                report.failures.push((
                    "darmoshark".into(),
                    "Darmoshark discovery budget exhausted".into(),
                ));
                break;
            }
            let result = query(c, cancel);
            if matches!(
                result,
                Err(QueryFailure {
                    error: DeviceError::Cancelled,
                    ..
                })
            ) {
                return Err(DeviceError::Cancelled);
            }
            match result {
                Ok(reading) => report.candidates.push(Candidate {
                    descriptor: descriptor(c),
                    reading: Some(reading),
                    error: None,
                }),
                Err(failure) => report.candidates.push(unavailable(c, failure)),
            }
        }
        Ok(report)
    }
    fn read(&self, id: &str, cancel: &Cancellation) -> Result<Reading, DeviceError> {
        cancel.check()?;
        for c in &endpoints(cancel)? {
            if descriptor(c).id == id {
                return query(c, cancel).map_err(QueryFailure::into_read_error);
            }
        }
        Err(DeviceError::NotFound)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    const RECEIVER: &[u8] = &[0x51, 3, 1, 0x8a, 0x24, 0x10, 0xff, 1];
    const BATTERY: &[u8] = &[0x51, 6, 1, 0, 0x8a, 0x24, 0x10, 0xff, 3, 1, 9, 9, 0, 0];
    #[test]
    fn verified_battery_and_boundaries() {
        for p in [0, 9, 100] {
            let mut b = BATTERY.to_vec();
            b[11] = p;
            let r = battery(&b).unwrap();
            assert_eq!(r.normalized_percent(), p);
            assert_eq!(r.protocol, (0, 1));
            assert_eq!(r.power_state, PowerState::Discharging);
        }
        let mut b = BATTERY.to_vec();
        b[11] = 101;
        assert!(battery(&b).is_err());
    }
    #[test]
    fn reject_unrelated_truncated_and_changed_identity() {
        for index in [0, 1, 4, 5, 6, 7] {
            let mut b = BATTERY.to_vec();
            b[index] ^= 1;
            assert!(battery(&b).is_err());
        }
        for len in 0..14 {
            assert!(battery(&BATTERY[..len]).is_err());
        }
        assert!(receiver(RECEIVER).is_ok());
        let mut b = RECEIVER.to_vec();
        b[7] = 0;
        assert!(matches!(receiver(&b), Err(DeviceError::Unavailable(_))));
        b[7] = 1;
        b[5] ^= 1;
        assert!(receiver(&b).is_err());
    }
    #[test]
    fn positive_power_is_not_claimed_charging() {
        let mut b = BATTERY.to_vec();
        b[12] = 1;
        assert_eq!(battery(&b).unwrap().power_state, PowerState::Unknown(1));
    }
    #[test]
    fn acknowledgement_requires_vendor_report_and_ready() {
        assert_eq!(ack(&[0x54, 0xe4, 0]), Ack::Progress);
        assert_eq!(ack(&[0x54, 0xe4, 1]), Ack::Ready);
        assert_eq!(ack(&[0x54, 0xe4, 4]), Ack::Busy);
        for b in [&[0x54, 0xe4][..], &[3, 0xe4, 1], &[0x54, 0xe2, 1]] {
            assert_eq!(ack(b), Ack::Unrelated);
        }
    }
    #[test]
    fn cancelled_query_does_not_open_device() {
        let cancel = Cancellation::default();
        cancel.cancel();
        assert!(matches!(
            query(&Collection::default(), &cancel),
            Err(QueryFailure {
                error: DeviceError::Cancelled,
                ..
            })
        ));
    }
    #[test]
    fn endpoint_filter_requires_verified_collection_and_preserves_physical_identity() {
        use super::super::diagnostics::Field;
        let mut c = Collection {
            container: Some("container-a".into()),
            ..Default::default()
        };
        for (key, value) in [
            ("vid", 0x248a),
            ("pid", 0xff30),
            ("usage_page", 0x008c),
            ("usage", 1),
            ("interface", 1),
            ("input_report_bytes", 33),
            ("feature_report_bytes", 1001),
        ] {
            c.fields.insert(key, Field::number(value, "fixture"));
        }
        assert!(target(&c));
        let first = descriptor(&c);
        c.container = Some("container-b".into());
        assert_ne!(first.id, descriptor(&c).id);
        for key in [
            "vid",
            "pid",
            "usage_page",
            "usage",
            "interface",
            "input_report_bytes",
            "feature_report_bytes",
        ] {
            let previous = c.fields.insert(key, Field::number(0, "fixture")).unwrap();
            assert!(!target(&c));
            c.fields.insert(key, previous);
        }
    }
    #[test]
    fn silent_receiver_keeps_the_value_then_hides_without_losing_health() {
        let a = Collection {
            container: Some("receiver-a".into()),
            ..Default::default()
        };
        let b = Collection {
            container: Some("receiver-b".into()),
            ..Default::default()
        };
        let success = |c: &Collection| Candidate {
            descriptor: descriptor(c),
            reading: Some(battery(BATTERY).unwrap()),
            error: None,
        };
        let mut manager = crate::manager::Manager::default();
        manager.merge(vec![success(&a), success(&b)], 100);
        // A receiver stays on the bus while the mouse behind it sleeps or is off,
        // so one silent pass keeps the cached value and a second one hides the
        // device. Neither pass may drop the cache or disturb the other device.
        for (pass, identity_confirmed) in [true, false].into_iter().enumerate() {
            let candidate = unavailable(
                &a,
                QueryFailure {
                    error: DeviceError::Unavailable("asleep".into()),
                    identity_confirmed,
                },
            );
            assert_eq!(candidate.descriptor.endpoint.is_some(), identity_confirmed);
            manager.merge(vec![candidate, success(&b)], 200 + pass as u64 * 100);
            assert_eq!(manager.devices.len(), 2);
            let old = manager
                .devices
                .iter()
                .find(|s| s.descriptor.physical_id == a.key())
                .unwrap();
            assert_eq!(old.last_success, Some(100));
            assert!(old.selected && old.descriptor.supported);
            assert_eq!(old.connected, pass == 0);
            assert_eq!(old.reading.as_ref().unwrap().normalized_percent(), 9);
            assert!(old.error.is_some());
            assert_eq!(manager.visible().count(), 2 - pass);
            let healthy = manager
                .devices
                .iter()
                .find(|s| s.descriptor.physical_id == b.key())
                .unwrap();
            assert_eq!(healthy.last_success, Some(200 + pass as u64 * 100));
            assert!(healthy.error.is_none());
        }
        // A reply brings the hidden device back with its old value intact.
        manager.merge(vec![success(&a), success(&b)], 400);
        assert_eq!(manager.visible().count(), 2);
        let restored = manager
            .devices
            .iter()
            .find(|s| s.descriptor.physical_id == a.key())
            .unwrap();
        assert!(restored.connected && restored.error.is_none());
        assert_eq!(restored.reading.as_ref().unwrap().normalized_percent(), 9);
    }
    #[test]
    fn changed_pairing_hides_old_battery_without_inheriting_it() {
        let collection = Collection {
            container: Some("receiver-a".into()),
            ..Default::default()
        };
        let known = descriptor(&collection);
        let mut manager = crate::manager::Manager::default();
        manager.merge(
            vec![Candidate {
                descriptor: known.clone(),
                reading: Some(battery(BATTERY).unwrap()),
                error: None,
            }],
            100,
        );
        let changed_failure = || {
            let mut changed = RECEIVER.to_vec();
            changed[5] = 0x11;
            QueryFailure {
                error: receiver(&changed).unwrap_err(),
                identity_confirmed: false,
            }
        };
        let candidate = unavailable(&collection, changed_failure());
        assert!(!candidate.descriptor.recoverable);
        assert!(!candidate.descriptor.supported);
        assert!(candidate.reading.is_none());
        manager.merge(vec![candidate], 200);
        let old = manager
            .devices
            .iter()
            .find(|s| s.descriptor.id == known.id)
            .unwrap();
        assert!(old.selected);
        assert!(!old.connected);
        assert_eq!(old.last_success, Some(100));
        assert_eq!(old.reading.as_ref().unwrap().normalized_percent(), 9);
        assert_eq!(manager.visible().count(), 0);
        let replacement = manager
            .devices
            .iter()
            .find(|s| s.descriptor.id != known.id)
            .unwrap();
        assert!(replacement.reading.is_none());
        assert!(replacement.last_success.is_none());
        assert!(matches!(
            changed_failure().into_read_error(),
            DeviceError::NotFound
        ));
        manager.apply(&known.id, Err(changed_failure().into_read_error()), 201);
        assert_eq!(manager.visible().count(), 0);
    }
}
