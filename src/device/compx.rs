//! Compx 17-byte battery exchange; independently implemented from the wire contract.
//! Sources and profile provenance: docs/DEVICE-SUPPORT.md (no imported driver code).
use super::{
    Cancellation, DeviceError, DeviceKind, PowerState, Reading,
    diagnostics::Collection,
    vendor_hid::{Adapter, Profile, Transport, number},
};

// Explicit receiver/cable pairs. A shared retail name does not imply shared firmware.
const MODELS: &[(&str, u16, u16, u16)] = &[
    ("ATK F1 Ultimate", 0x373b, 0x1031, 0x102e),
    ("ATK A9 Ultimate", 0x373b, 0x11d9, 0x11b6),
    ("VXE MAD R", 0x373b, 0x104d, 0x103f),
    ("VXE MAD R Major Plus", 0x373b, 0x1040, 0x104c),
    ("VXE R1 Pro Max", 0x3554, 0xf58a, 0xf58c),
    ("VXE R1 SE+", 0x3554, 0xf58e, 0xf58f),
    ("VGN F1 Pro", 0x3554, 0xf503, 0xf502),
    ("VGN F2 Pro Max", 0x3554, 0xfb3e, 0xfb3d),
];
fn identify(c: &Collection) -> Option<Profile> {
    let vid = number(c, "vid")?;
    let pid = number(c, "pid")?;
    MODELS.iter().find_map(|&(name, v, receiver, cable)| {
        (vid == v && (pid == receiver || pid == cable)).then_some(Profile {
            name,
            vid,
            pid,
            wireless: pid == receiver,
        })
    })
}
fn accepts(c: &Collection) -> bool {
    number(c, "usage_page") == Some(0xff02)
        && number(c, "usage") == Some(2)
        && number(c, "input_report_bytes") == Some(17)
        && number(c, "output_report_bytes") == Some(17)
}
pub(super) const ADAPTER: Adapter = Adapter {
    name: "compx",
    identify,
    accepts,
    feature_only: false,
    kind: DeviceKind::Mouse,
    query,
};
fn request() -> [u8; 17] {
    let mut b = [0; 17];
    b[0] = 8;
    b[1] = 4;
    b[16] = 0x49;
    b
}
fn parse(p: Profile, b: &[u8]) -> Result<Option<Reading>, DeviceError> {
    if b.len() != 17 || b[0] != 8 || b[1] != 4 {
        return Ok(None);
    }
    if b.iter().fold(0u8, |sum, x| sum.wrapping_add(*x)) != 0x55 {
        return Err(DeviceError::Protocol("Compx电量回复校验和不匹配".into()));
    }
    // The query can be echoed; it has no live power information. Never turn it into 0%.
    if b == request() {
        return Ok(None);
    }
    // Decode the protocol's power flag; never infer it from the cable PID or percent.
    // Preserve unknown for future flag values.
    let power = match b[7] {
        0 => PowerState::Discharging,
        1 => PowerState::Charging,
        n => PowerState::Unknown(n),
    };
    Reading::from_percent(p.name.into(), b[6], power).map(Some)
}
fn query(p: Profile, io: &dyn Transport, cancel: &Cancellation) -> Result<Reading, DeviceError> {
    cancel.check()?;
    // No sequence field exists: drain stale replies on the dedicated vendor collection.
    io.drain(17)?;
    io.output(&request())?;
    for _ in 0..32 {
        cancel.check()?;
        let Some(b) = io.input(17)? else {
            break;
        };
        if let Some(reading) = parse(p, &b)? {
            return Ok(reading);
        }
    }
    Err(DeviceError::Unavailable(
        "Compx鼠标休眠、未连接或未收到匹配电量回复".into(),
    ))
}
#[cfg(test)]
mod tests {
    use super::super::{
        diagnostics::Field,
        vendor_hid::tests::{Script, Step, collection},
    };
    use super::*;
    fn profile() -> Profile {
        identify(&collection(0x3554, 0xf58a, "fixture")).unwrap()
    }
    fn reply(percent: u8, charge: u8) -> Vec<u8> {
        let mut b = vec![
            8, 4, 0, 0, 0, 2, percent, charge, 0x0e, 0x94, 0, 0, 0, 0, 0, 0, 0,
        ];
        b[16] = 0x55u8.wrapping_sub(b[..16].iter().fold(0u8, |sum, x| sum.wrapping_add(*x)));
        b
    }
    #[test]
    fn all_profiles_require_vendor_endpoint_and_correct_lengths() {
        for &(name, vid, receiver, cable) in MODELS {
            for (pid, wireless) in [(receiver, true), (cable, false)] {
                let mut c = collection(vid, pid, "fixture");
                let p = identify(&c).unwrap();
                assert_eq!((p.name, p.wireless), (name, wireless));
                for (k, v) in [
                    ("usage_page", 0xff02),
                    ("usage", 2),
                    ("input_report_bytes", 17),
                    ("output_report_bytes", 17),
                ] {
                    c.fields.insert(k, Field::number(v, "fixture"));
                }
                assert!(accepts(&c));
                for k in [
                    "usage_page",
                    "usage",
                    "input_report_bytes",
                    "output_report_bytes",
                ] {
                    let previous = c.fields.insert(k, Field::number(0, "fixture")).unwrap();
                    assert!(!accepts(&c));
                    c.fields.insert(k, previous);
                }
            }
        }
        assert!(identify(&collection(0x373b, 0xffff, "fixture")).is_none());
        assert!(identify(&collection(0x3555, 0xf58a, "fixture")).is_none());
    }
    #[test]
    fn published_35_percent_wire_sample_matches_independent_contract() {
        // Published Nordic52 transcript, no local device identifiers. This was
        // captured upstream, not on a model physically tested by this project.
        let packet = [
            0x08, 0x04, 0, 0, 0, 0x02, 0x23, 0, 0x0e, 0x94, 0, 0, 0, 0, 0, 0, 0x82,
        ];
        let r = parse(profile(), &packet).unwrap().unwrap();
        assert_eq!(
            (r.normalized_percent(), r.power_state),
            (35, PowerState::Discharging)
        );
    }
    #[test]
    fn wire_contract_boundaries_checksum_echo_and_unrelated_packets() {
        assert_eq!(
            request(),
            [8, 4, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x49]
        );
        for p in 0..=100 {
            assert_eq!(
                parse(profile(), &reply(p, 0))
                    .unwrap()
                    .unwrap()
                    .normalized_percent(),
                p
            );
        }
        for p in 101..=255 {
            assert!(parse(profile(), &reply(p, 0)).is_err());
        }
        assert!(parse(profile(), &request()).unwrap().is_none());
        let b = reply(35, 0);
        for len in 0..17 {
            assert!(parse(profile(), &b[..len]).unwrap().is_none());
        }
        for index in 2..17 {
            let mut bad = b.clone();
            bad[index] ^= 1;
            assert!(parse(profile(), &bad).is_err());
        }
        let mut foreign = b.clone();
        foreign[1] = 5;
        assert!(parse(profile(), &foreign).unwrap().is_none());
        assert_eq!(
            parse(profile(), &reply(35, 1))
                .unwrap()
                .unwrap()
                .power_state,
            PowerState::Charging
        );
        assert_eq!(
            parse(profile(), &reply(35, 9))
                .unwrap()
                .unwrap()
                .power_state,
            PowerState::Unknown(9)
        );
    }
    #[test]
    fn transaction_filters_echo_and_foreign_then_reads_valid_reply() {
        let io = Script::new(vec![
            Step::Drain,
            Step::Output(request().to_vec()),
            Step::Input(Some(request().to_vec())),
            Step::Input(Some(vec![8, 5])),
            Step::Input(Some(reply(35, 0))),
        ]);
        assert_eq!(
            query(profile(), &io, &Cancellation::default())
                .unwrap()
                .normalized_percent(),
            35
        );
        io.done();
        for steps in [
            vec![Step::Drain, Step::Failure],
            vec![
                Step::Drain,
                Step::Output(request().to_vec()),
                Step::Input(None),
            ],
        ] {
            let io = Script::new(steps);
            assert!(query(profile(), &io, &Cancellation::default()).is_err());
            io.done();
        }
        let cancel = Cancellation::default();
        cancel.cancel();
        let io = Script::new(vec![]);
        assert!(matches!(
            query(profile(), &io, &cancel),
            Err(DeviceError::Cancelled)
        ));
        io.done();
    }
}
