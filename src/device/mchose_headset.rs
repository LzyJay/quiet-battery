//! MCHOSE V9 Pro 2 receiver vendor channel (`0x55` report, command `0x01`, parameter `0x0b`).
//! Independently implemented from the wire contract; provenance and open questions:
//! docs/DEVICE-SUPPORT.md. No vendor driver code or binary is used at runtime.
use super::{
    Cancellation, DeviceError, DeviceKind, PowerState, Reading,
    diagnostics::Collection,
    vendor_hid::{Adapter, Profile, Transport, number},
};

/// Only the verified 2.4G receiver. The wired cable PID is reported by the
/// device itself and has never been queried, so it is deliberately absent.
const MODELS: &[(&str, u16, u16)] = &[("MCHOSE V9 Pro 2", 0x3837, 0x6011)];
const VENDOR_USAGE_PAGE: u16 = 0xff07;
const VENDOR_USAGE: u16 = 0x0222;
const REPORT_ID: u8 = 0x55;
const MAGIC: u8 = 0xaa;
const VERSION: u8 = 0x01;
/// Windows HID caps count the report-ID slot, so the wire frame is 1 + 63 bytes.
const REPORT_LEN: usize = 64;
/// Report layout after the report ID: `AA | version | flags | len LE | cmd | payload | XOR`.
const CMD_PARAM_READ: u8 = 0x01;
const PARAM_BATTERY: u8 = 0x0b;
/// Parameter echo, status byte, then the value.
const REPLY_MIN: usize = 3;

fn identify(c: &Collection) -> Option<Profile> {
    let vid = number(c, "vid")?;
    let pid = number(c, "pid")?;
    MODELS.iter().find_map(|&(name, v, p)| {
        (vid == v && pid == p).then_some(Profile {
            name,
            vid,
            pid,
            wireless: true,
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
    name: "mchose-hs",
    identify,
    accepts,
    feature_only: false,
    kind: DeviceKind::Headset,
    query,
};
/// XOR of flags..=end of payload. The version byte is excluded; both the
/// captured push frames and the query replies agree on this start offset.
fn checksum(frame: &[u8], end: usize) -> u8 {
    frame[3..=end].iter().fold(0u8, |acc, byte| acc ^ byte)
}
fn request() -> [u8; REPORT_LEN] {
    let mut b = [0; REPORT_LEN];
    b[0] = REPORT_ID;
    b[1] = MAGIC;
    b[2] = VERSION;
    b[3] = 1; // flags
    b[4] = 1; // payload length LE
    b[5] = 0;
    b[6] = CMD_PARAM_READ;
    b[7] = PARAM_BATTERY;
    b[8] = checksum(&b, 7);
    b
}
fn parse(p: Profile, b: &[u8]) -> Result<Option<Reading>, DeviceError> {
    if b.len() <= 8 || b[0] != REPORT_ID || b[1] != MAGIC {
        return Ok(None);
    }
    // A different protocol revision is not this contract; do not guess at it.
    if b[2] != VERSION {
        return Ok(None);
    }
    let length = usize::from(b[4]) | (usize::from(b[5]) << 8);
    let end = 6 + length;
    if end + 1 >= b.len() {
        return Err(DeviceError::Protocol("迈从耳机回复长度不足".into()));
    }
    // Integrity first: a damaged frame is a protocol error, never a silent skip.
    if b[end + 1] != checksum(b, end) {
        return Err(DeviceError::Protocol("迈从耳机回复校验和不匹配".into()));
    }
    // Well-formed frames for other commands (equalizer presets, status pushes,
    // vendor software) are acknowledged and left for the next read.
    if b[6] != CMD_PARAM_READ || length < REPLY_MIN {
        return Ok(None);
    }
    let payload = &b[7..7 + length];
    if payload[0] != PARAM_BATTERY {
        return Ok(None);
    }
    let percent = payload[2];
    if percent > 100 {
        return Err(DeviceError::Protocol("迈从耳机电量百分比超出范围".into()));
    }
    // The status byte has no confirmed meaning, so charging stays unknown.
    Reading::from_percent(p.name.into(), percent, PowerState::Unknown(255)).map(Some)
}
fn query(p: Profile, io: &dyn Transport, cancel: &Cancellation) -> Result<Reading, DeviceError> {
    cancel.check()?;
    // The frame carries no per-query identifier beyond the echoed parameter, so
    // stale replies on the dedicated vendor collection are drained first.
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
        "迈从耳机休眠、未连接或未收到匹配电量回复".into(),
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
        identify(&collection(0x3837, 0x6011, "fixture")).unwrap()
    }
    fn accept(c: &mut Collection) {
        for (k, v) in [
            ("usage_page", 0xff07),
            ("usage", 0x0222),
            ("input_report_bytes", 64),
            ("output_report_bytes", 64),
        ] {
            c.fields.insert(k, Field::number(v, "fixture"));
        }
    }
    fn reply(percent: u8) -> Vec<u8> {
        let mut b = vec![REPORT_ID, MAGIC, VERSION, 1, 3, 0, CMD_PARAM_READ];
        b.extend_from_slice(&[PARAM_BATTERY, 0x01, percent]);
        b.push(checksum(&b, 9));
        b.resize(REPORT_LEN, 0);
        b
    }
    #[test]
    fn routes_require_vendor_endpoint_and_expected_lengths() {
        let mut c = collection(0x3837, 0x6011, "fixture");
        assert_eq!(identify(&c).unwrap().name, "MCHOSE V9 Pro 2");
        assert!(identify(&c).unwrap().wireless);
        assert!(!accepts(&c)); // IDs alone never permit traffic.
        accept(&mut c);
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
        // The mouse receiver and the wired PID must not be claimed here.
        assert!(identify(&collection(0x3837, 0x1014, "fixture")).is_none());
        assert!(identify(&collection(0x3837, 0x6010, "fixture")).is_none());
        assert!(identify(&collection(0x3836, 0x6011, "fixture")).is_none());
    }
    #[test]
    fn captured_replies_yield_the_battery_byte() {
        // Captured locally from the 2.4G receiver; contains no host identifiers.
        let current = [
            0x55, 0xaa, 0x01, 0x01, 0x03, 0x00, 0x01, 0x0b, 0x01, 0x59, 0x50,
        ];
        assert_eq!(
            parse(profile(), &current)
                .unwrap()
                .unwrap()
                .normalized_percent(),
            89
        );
        // The same parameter read earlier in the day, one point lower.
        let earlier = [
            0x55, 0xaa, 0x01, 0x01, 0x03, 0x00, 0x01, 0x0b, 0x01, 0x58, 0x51,
        ];
        assert_eq!(
            parse(profile(), &earlier)
                .unwrap()
                .unwrap()
                .normalized_percent(),
            88
        );
        assert_eq!(
            parse(profile(), &reply(89)).unwrap().unwrap().power_state,
            PowerState::Unknown(255)
        );
    }
    #[test]
    fn wire_contract_boundaries_checksum_and_unrelated_packets() {
        let expected = {
            let mut r = [0u8; REPORT_LEN];
            r[0] = 0x55;
            r[1] = 0xaa;
            r[2] = 0x01;
            r[3] = 0x01;
            r[4] = 0x01;
            r[6] = 0x01;
            r[7] = 0x0b;
            r[8] = 0x0a;
            r
        };
        assert_eq!(request(), expected);
        for p in 0..=100 {
            assert_eq!(
                parse(profile(), &reply(p))
                    .unwrap()
                    .unwrap()
                    .normalized_percent(),
                p
            );
        }
        for p in 101..=255 {
            assert!(parse(profile(), &reply(p)).is_err());
        }
        // The echo carries a one-byte payload and must never become 0%.
        assert!(parse(profile(), &request()).unwrap().is_none());
        let b = reply(35);
        for len in 0..9 {
            assert!(parse(profile(), &b[..len]).unwrap().is_none());
        }
        // A truncated frame keeps its length claim but loses the checksum byte.
        assert!(parse(profile(), &b[..10]).is_err());
        // Integrity precedes the command filter: a corrupted command byte is an
        // error, not an unrelated packet.
        for index in 3..11 {
            let mut bad = b.clone();
            bad[index] ^= 1;
            assert!(parse(profile(), &bad).is_err(), "index {index} accepted");
        }
        // A well-formed frame for another command is acknowledged and skipped.
        let mut other = vec![REPORT_ID, MAGIC, VERSION, 1, 3, 0, 0x06];
        other.extend_from_slice(&[0x01, 0x80, 0x33]);
        other.push(checksum(&other, 9));
        other.resize(REPORT_LEN, 0);
        assert!(parse(profile(), &other).unwrap().is_none());
        // A well-formed reply for another parameter is not this reading.
        let mut other_param = vec![REPORT_ID, MAGIC, VERSION, 1, 3, 0, CMD_PARAM_READ];
        other_param.extend_from_slice(&[0x0e, 0x00, 0x2d]);
        other_param.push(checksum(&other_param, 9));
        other_param.resize(REPORT_LEN, 0);
        assert!(parse(profile(), &other_param).unwrap().is_none());
        // An unknown protocol revision is not guessed at.
        let mut other_version = reply(35);
        other_version[2] = 0x02;
        assert!(parse(profile(), &other_version).unwrap().is_none());
    }
    #[test]
    fn transaction_filters_echo_and_pushes_then_reads_valid_reply() {
        let mut push = vec![REPORT_ID, MAGIC, VERSION, 1, 3, 0, 0x06];
        push.extend_from_slice(&[0x01, 0x80, 0x33]);
        push.push(checksum(&push, 9));
        push.resize(REPORT_LEN, 0);
        let io = Script::new(vec![
            Step::Drain,
            Step::Output(request().to_vec()),
            Step::Input(Some(request().to_vec())),
            Step::Input(Some(push)),
            Step::Input(Some(reply(89))),
        ]);
        assert_eq!(
            query(profile(), &io, &Cancellation::default())
                .unwrap()
                .normalized_percent(),
            89
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
