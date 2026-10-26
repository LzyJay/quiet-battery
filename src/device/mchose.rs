//! MCHOSE HUB Protocol V2 (`0x4d` output report) battery exchange.
//! Independently implemented from the wire contract; sources and provenance:
//! docs/DEVICE-SUPPORT.md. No vendor driver code or binary is used at runtime.
use super::{
    Cancellation, DeviceError, DeviceKind, PowerState, Reading,
    diagnostics::Collection,
    vendor_hid::{Adapter, Profile, Transport, number},
};

/// Verified route first: the 8K 2.4G receiver. The mouse reports its own cable
/// PID in the same reply, so the cable entry reuses one profile shape without a
/// separate firmware assumption.
const MODELS: &[(&str, u16, u16, u16)] = &[("MCHOSE A5 V3 Ultra", 0x3837, 0x1014, 0x4034)];
const VENDOR_USAGE_PAGE: u16 = 0xff01;
const VENDOR_USAGE: u16 = 1;
const REPORT_ID: u8 = 0x4d;
/// Windows HID caps count the report-ID slot, so the wire frame is 1 + 63 bytes.
const REPORT_LEN: usize = 64;
const PROTOCOL_VERSION: u8 = 1;
const CMD_STATUS: u16 = 0x0900;
/// Header length before the command payload starts.
const HEADER_LEN: usize = 8;
/// Status replies carry connection, charging and percent at these payload offsets.
const PAYLOAD_MIN: usize = 13;

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
    number(c, "usage_page") == Some(VENDOR_USAGE_PAGE)
        && number(c, "usage") == Some(VENDOR_USAGE)
        && number(c, "input_report_bytes") == Some(REPORT_LEN as u16)
        && number(c, "output_report_bytes") == Some(REPORT_LEN as u16)
}
pub(super) const ADAPTER: Adapter = Adapter {
    name: "mchose",
    identify,
    accepts,
    feature_only: false,
    kind: DeviceKind::Mouse,
    query,
};
/// XOR of bytes 2..=end: the frame's own checksum contract.
fn checksum(frame: &[u8], end: usize) -> u8 {
    frame[2..=end].iter().fold(0u8, |acc, byte| acc ^ byte)
}
fn request() -> [u8; REPORT_LEN] {
    let mut b = [0; REPORT_LEN];
    b[0] = REPORT_ID;
    b[1] = PROTOCOL_VERSION;
    b[2] = 1; // flags
    b[3] = 0; // empty command payload
    b[4] = (CMD_STATUS & 0xff) as u8;
    b[5] = (CMD_STATUS >> 8) as u8;
    b[6] = 0; // business code
    b[7] = 0; // sequence
    b[HEADER_LEN] = checksum(&b, HEADER_LEN - 1);
    b
}
fn parse(p: Profile, b: &[u8]) -> Result<Option<Reading>, DeviceError> {
    if b.len() <= HEADER_LEN || b[0] != REPORT_ID || b[1] != PROTOCOL_VERSION {
        return Ok(None);
    }
    let length = usize::from(b[3]);
    let end = HEADER_LEN - 1 + length;
    if end + 1 >= b.len() {
        return Err(DeviceError::Protocol("MCHOSE回复长度不足".into()));
    }
    // Integrity first: a damaged frame is a protocol error, never a silent skip.
    if b[end + 1] != checksum(b, end) {
        return Err(DeviceError::Protocol("MCHOSE电量回复校验和不匹配".into()));
    }
    // Well-formed frames for other commands (echo, busy, vendor software) are
    // acknowledged and left for the next read.
    if (u16::from(b[4]) | (u16::from(b[5]) << 8)) != CMD_STATUS {
        return Ok(None);
    }
    if length < PAYLOAD_MIN {
        return Ok(None);
    }
    let payload = &b[HEADER_LEN..HEADER_LEN + length];
    // A reply from another receiver model is not this device's reading.
    if u16::from_le_bytes([payload[0], payload[1]]) != p.vid {
        return Ok(None);
    }
    let percent = payload[12];
    if percent > 100 {
        return Err(DeviceError::Protocol("MCHOSE电量百分比超出范围".into()));
    }
    // Decode only the documented charging field; everything else stays unknown.
    let power = match payload[11] {
        0 => PowerState::Discharging,
        1 => PowerState::Charging,
        2 => PowerState::Full,
        value => PowerState::Unknown(value),
    };
    Reading::from_percent(p.name.into(), percent, power).map(Some)
}
fn query(p: Profile, io: &dyn Transport, cancel: &Cancellation) -> Result<Reading, DeviceError> {
    cancel.check()?;
    // The frame has no per-query identifier beyond the echoed command, so stale
    // replies on the dedicated vendor collection are drained first.
    io.drain(REPORT_LEN)?;
    io.output(&request())?;
    for _ in 0..32 {
        cancel.check()?;
        let Some(b) = io.input(REPORT_LEN)? else {
            break;
        };
        if let Some(reading) = parse(p, &b)? {
            return Ok(reading);
        }
    }
    Err(DeviceError::Unavailable(
        "MCHOSE设备休眠、未连接或未收到匹配电量回复".into(),
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
        identify(&collection(0x3837, 0x1014, "fixture")).unwrap()
    }
    fn reply(percent: u8, charge: u8, connection: u8) -> Vec<u8> {
        let payload = [
            0x37, 0x38, 0x34, 0x40, 0x04, 0x00, 0x00, 0x00, 0x00, 0x10, connection, charge,
            percent, 0x00, 0x08, 0xe4,
        ];
        let mut b = vec![REPORT_ID, PROTOCOL_VERSION, 1, payload.len() as u8];
        b.extend_from_slice(&[(CMD_STATUS & 0xff) as u8, (CMD_STATUS >> 8) as u8]);
        b.extend_from_slice(&[0, 0]);
        b.extend_from_slice(&payload);
        let end = HEADER_LEN - 1 + payload.len();
        b.push(checksum(&b, end));
        b.resize(REPORT_LEN, 0);
        b
    }
    #[test]
    fn routes_require_vendor_endpoint_and_expected_lengths() {
        for &(name, vid, receiver, cable) in MODELS {
            for (pid, wireless) in [(receiver, true), (cable, false)] {
                let mut c = collection(vid, pid, "fixture");
                let p = identify(&c).unwrap();
                assert_eq!((p.name, p.wireless), (name, wireless));
                for (k, v) in [
                    ("usage_page", 0xff01),
                    ("usage", 1),
                    ("input_report_bytes", 64),
                    ("output_report_bytes", 64),
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
        // The mouse's generic keyboard and consumer collections share the VID.
        assert!(identify(&collection(0x3837, 0x1012, "fixture")).is_none());
        assert!(identify(&collection(0x3837, 0x6011, "fixture")).is_none());
        assert!(identify(&collection(0x3836, 0x1014, "fixture")).is_none());
    }
    #[test]
    fn captured_receiver_frame_matches_the_independent_contract() {
        // Captured locally from the 8K receiver; contains no host identifiers.
        let frame = [
            0x4d, 0x01, 0x01, 0x10, 0x00, 0x09, 0x00, 0x00, 0x37, 0x38, 0x34, 0x40, 0x04, 0x00,
            0x00, 0x00, 0x00, 0x10, 0x02, 0x00, 0x45, 0x00, 0x08, 0xe4, 0xdc,
        ];
        let r = parse(profile(), &frame).unwrap().unwrap();
        assert_eq!(r.normalized_percent(), 69);
        assert_eq!(r.power_state, PowerState::Discharging);
        assert_eq!(r.device_name, "MCHOSE A5 V3 Ultra");
    }
    #[test]
    fn wire_contract_boundaries_checksum_echo_and_unrelated_packets() {
        let expected = {
            let mut r = [0u8; REPORT_LEN];
            r[0] = 0x4d;
            r[1] = 1;
            r[2] = 1;
            r[5] = 0x09;
            r[8] = 0x08;
            r
        };
        assert_eq!(request(), expected);
        for p in 0..=100 {
            assert_eq!(
                parse(profile(), &reply(p, 0, 2))
                    .unwrap()
                    .unwrap()
                    .normalized_percent(),
                p
            );
        }
        for p in 101..=255 {
            assert!(parse(profile(), &reply(p, 0, 2)).is_err());
        }
        // The echo carries no payload and must never become 0%.
        assert!(parse(profile(), &request()).unwrap().is_none());
        let b = reply(35, 0, 2);
        for len in 0..9 {
            assert!(parse(profile(), &b[..len]).unwrap().is_none());
        }
        // A truncated frame keeps its length claim but loses the checksum byte.
        assert!(parse(profile(), &b[..HEADER_LEN + 16]).is_err());
        for index in 2..25 {
            let mut bad = b.clone();
            bad[index] ^= 1;
            assert!(parse(profile(), &bad).is_err());
        }
        let mut foreign = b.clone();
        foreign[5] = 0x08;
        foreign[24] = checksum(&foreign, HEADER_LEN - 1 + 16);
        assert!(parse(profile(), &foreign).unwrap().is_none());
        let mut other_vendor = b.clone();
        other_vendor[8] = 0x36;
        other_vendor[24] = checksum(&other_vendor, HEADER_LEN - 1 + 16);
        assert!(parse(profile(), &other_vendor).unwrap().is_none());
        assert_eq!(
            parse(profile(), &reply(35, 1, 2))
                .unwrap()
                .unwrap()
                .power_state,
            PowerState::Charging
        );
        assert_eq!(
            parse(profile(), &reply(100, 2, 1))
                .unwrap()
                .unwrap()
                .power_state,
            PowerState::Full
        );
        assert_eq!(
            parse(profile(), &reply(35, 9, 3))
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
            Step::Input(Some(vec![0x4d, 0x01])),
            Step::Input(Some(reply(69, 0, 2))),
        ]);
        assert_eq!(
            query(profile(), &io, &Cancellation::default())
                .unwrap()
                .normalized_percent(),
            69
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
