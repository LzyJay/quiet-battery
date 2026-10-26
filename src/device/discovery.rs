//! Shared physical discovery. Enumeration alone never claims battery support.
use super::diagnostics::{self, Collection, Field};
use super::{Cancellation, Candidate, DeviceDescriptor, DeviceError, DeviceKind};
use hidapi::HidApi;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
static DIAGNOSTICS: OnceLock<Mutex<BTreeMap<String, String>>> = OnceLock::new();
pub(super) fn diagnostic_information(id: &str) -> Option<String> {
    DIAGNOSTICS.get()?.lock().ok()?.get(id).cloned()
}
#[derive(Clone)]
struct InputCollection {
    metadata: Collection,
    name: Option<String>,
    kind: Option<DeviceKind>,
    usage_page: u16,
    usage: u16,
    interface: i32,
}
fn input_kind(page: u16, usage: u16) -> Option<DeviceKind> {
    match (page, usage) {
        (1, 2) => Some(DeviceKind::Mouse),
        (1, 6) => Some(DeviceKind::Keyboard),
        (1, 4) => Some(DeviceKind::Generic),
        (1, 5) => Some(DeviceKind::Gamepad),
        (0x0b, 5) => Some(DeviceKind::Headset),
        _ => None,
    }
}
fn grouped_candidates(
    collections: Vec<InputCollection>,
    claimed: &BTreeSet<String>,
    cancel: &Cancellation,
) -> Result<(Vec<Candidate>, BTreeMap<String, String>), DeviceError> {
    let mut groups: BTreeMap<String, Vec<InputCollection>> = BTreeMap::new();
    for collection in collections {
        cancel.check()?;
        groups
            .entry(collection.metadata.key())
            .or_default()
            .push(collection);
    }
    let mut found = Vec::new();
    let mut diagnostics = BTreeMap::new();
    for (key, mut siblings) in groups {
        cancel.check()?;
        // Prefer input collections, then a stable usage/interface/path ordering.
        // Group before filtering so vendor siblings remain available as evidence.
        siblings.sort_by(|a, b| {
            (
                a.kind.is_none(),
                a.usage_page,
                a.usage,
                a.interface,
                &a.metadata.path,
            )
                .cmp(&(
                    b.kind.is_none(),
                    b.usage_page,
                    b.usage,
                    b.interface,
                    &b.metadata.path,
                ))
                .then(a.name.cmp(&b.name))
        });
        let rows = siblings
            .iter()
            .enumerate()
            .map(|(i, c)| {
                c.metadata
                    .json("group-1", &format!("endpoint-{}", i + 1), false)
            })
            .collect::<Vec<_>>()
            .join(",");
        diagnostics.insert(key.clone(), format!("{{\"mode\":\"cached-hid-metadata\",\"collections\":[{rows}],\"note\":\"Enumeration is not battery support. Report lengths require --diagnose. USB bus does not prove a 2.4 GHz radio.\"}}"));
        if claimed.contains(&key) {
            continue;
        }
        let input: Vec<_> = siblings.iter().filter(|c| c.kind.is_some()).collect();
        let Some(representative) = input.first() else {
            continue;
        };
        let first_kind = representative.kind.unwrap();
        let kind = if input.iter().all(|c| c.kind == Some(first_kind)) {
            first_kind
        } else {
            DeviceKind::Generic
        };
        let name = input
            .iter()
            .find_map(|c| c.name.as_deref())
            .or_else(|| siblings.iter().find_map(|c| c.name.as_deref()))
            .unwrap_or("HID 外设")
            .to_owned();
        let id = format!("hid:{key}");
        found.push(Candidate {
            descriptor: DeviceDescriptor {
                id,
                physical_id: key,
                endpoint: None,
                recoverable: false,
                name,
                kind,
                supported: false,
                provider: "unsupported".into(),
                connection: super::Connection::Unknown,
            },
            reading: None,
            error: Some(DeviceError::Unsupported("未确认电量查询协议".into())),
        });
    }
    Ok((found, diagnostics))
}
pub(super) fn generic_candidates(
    claimed: &BTreeSet<String>,
    cancel: &Cancellation,
) -> Result<Vec<Candidate>, DeviceError> {
    cancel.check()?;
    let start = Instant::now();
    let api =
        HidApi::new().map_err(|_| DeviceError::Io("hidapi metadata enumeration failed".into()))?;
    let mut collections = Vec::new();
    for info in api.device_list() {
        cancel.check()?;
        if collections.len() >= diagnostics::MAX_COLLECTIONS
            || start.elapsed() >= Duration::from_secs(8)
        {
            return Err(DeviceError::Unavailable(
                "HID metadata discovery incomplete: collection/time limit".into(),
            ));
        }
        let mut metadata = diagnostics::for_path(&info.path().to_string_lossy());
        for (name, value) in [
            ("vid", info.vendor_id()),
            ("pid", info.product_id()),
            ("release", info.release_number()),
            ("usage_page", info.usage_page()),
            ("usage", info.usage()),
        ] {
            metadata.fields.insert(name, Field::hidapi_number(value));
        }
        metadata.fields.insert(
            "interface",
            if info.interface_number() >= 0 {
                Field::number(
                    info.interface_number(),
                    "hidapi.DeviceInfo.interface_number",
                )
            } else {
                Field::text(None, "hidapi.DeviceInfo.interface_number")
            },
        );
        metadata.fields.insert(
            "manufacturer",
            Field::text(
                info.manufacturer_string(),
                "hidapi.manufacturer_string (may use Windows fallback)",
            ),
        );
        metadata.fields.insert(
            "product",
            Field::text(
                info.product_string(),
                "hidapi.product_string (may use Windows fallback)",
            ),
        );
        store_hidapi_bus(&mut metadata, info.bus_type());
        let name = metadata
            .fields
            .get("product")
            .and_then(|f| f.value.clone())
            .or_else(|| {
                metadata
                    .fields
                    .get("friendly_name")
                    .and_then(|f| f.value.clone())
            })
            .or_else(|| {
                metadata
                    .fields
                    .get("description")
                    .and_then(|f| f.value.clone())
            });
        collections.push(InputCollection {
            metadata,
            name,
            kind: input_kind(info.usage_page(), info.usage()),
            usage_page: info.usage_page(),
            usage: info.usage(),
            interface: info.interface_number(),
        });
    }
    let (found, diagnostics) = grouped_candidates(collections, claimed, cancel)?;
    cancel.check()?;
    *DIAGNOSTICS
        .get_or_init(|| Mutex::new(BTreeMap::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = diagnostics;
    Ok(found)
}
fn store_hidapi_bus(metadata: &mut Collection, bus: hidapi::BusType) {
    metadata.fields.insert(
        "hidapi_bus_type",
        match bus {
            hidapi::BusType::Unknown => Field::text(None, "hidapi.bus_type"),
            bus => Field::text(Some(&format!("{bus:?}")), "hidapi.bus_type"),
        },
    );
}
#[cfg(test)]
pub(super) fn publish_diagnostic_fixture() -> Candidate {
    let mut rows = Vec::new();
    for (path, page, usage, product) in [
        ("input-fixture", 1, 2, "fixture-input"),
        ("vendor-fixture", 0xff00, 1, "fixture-vendor"),
    ] {
        let mut metadata = Collection {
            path: path.into(),
            container: Some("fixture-physical-lookup".into()),
            ..Default::default()
        };
        metadata
            .fields
            .insert("product", Field::text(Some(product), "fixture"));
        rows.push(InputCollection {
            metadata,
            name: Some(product.into()),
            kind: input_kind(page, usage),
            usage_page: page,
            usage,
            interface: 0,
        });
    }
    let (mut candidates, cache) =
        grouped_candidates(rows, &BTreeSet::new(), &Cancellation::default()).unwrap();
    DIAGNOSTICS
        .get_or_init(|| Mutex::new(BTreeMap::new()))
        .lock()
        .unwrap()
        .extend(cache);
    let mut candidate = candidates.remove(0);
    candidate.descriptor.id = "fixture-protocol:slot-7".into();
    candidate.descriptor.provider = "fixture-protocol".into();
    candidate.descriptor.supported = true;
    candidate.error = None;
    candidate
}
/// Windows container IDs group sibling input/vendor interfaces without merging
/// physically separate receivers; retain conservative path identity on failure.
pub(super) fn physical_key(path: &str) -> String {
    diagnostics::for_path(path).key()
}
pub(super) fn receiver_key(path: &str) -> String {
    let normalized = path.to_ascii_lowercase();
    let mut parts: Vec<String> = normalized.split('#').map(str::to_owned).collect();
    // Windows HID paths encode a collection in two places: &Col01/02 in
    // the hardware ID, and &0000/0001 at the end of its instance ID.
    // Preserve the physical instance root (e.g. 7&1ce076a8&0) so two receivers
    // at different USB locations never share a transport. This topology is
    // deliberately scoped to the verified C53F MI_02 vendor collections.
    if parts.len() >= 4 && parts[1].contains("&col") {
        if let Some(start) = parts[1].find("&col") {
            if parts[1].len() >= start + 6 {
                parts[1].replace_range(start..start + 6, "");
            }
        }
        if let Some((root, collection)) = parts[2].rsplit_once('&') {
            if collection.len() == 4 && collection.bytes().all(|c| c.is_ascii_hexdigit()) {
                parts[2] = root.to_owned();
            }
        }
    }
    parts.join("#")
}

#[cfg(test)]
mod tests {
    use super::*;
    fn collection(
        container: &str,
        path: &str,
        page: u16,
        usage: u16,
        name: Option<&str>,
    ) -> InputCollection {
        InputCollection {
            metadata: Collection {
                path: path.into(),
                container: Some(container.into()),
                ..Default::default()
            },
            name: name.map(str::to_owned),
            kind: input_kind(page, usage),
            usage_page: page,
            usage,
            interface: 0,
        }
    }
    #[test]
    fn groups_all_siblings_before_selecting_and_is_order_independent() {
        let mut rows = vec![
            collection("a", "mouse-a", 1, 2, Some("mouse")),
            collection("a", "vendor-a", 0xff00, 1, Some("vendor")),
            collection("b", "mouse-b", 1, 2, Some("mouse")),
        ];
        let (first, cache) =
            grouped_candidates(rows.clone(), &BTreeSet::new(), &Cancellation::default()).unwrap();
        rows.reverse();
        let (second, second_cache) =
            grouped_candidates(rows, &BTreeSet::new(), &Cancellation::default()).unwrap();
        assert_eq!(first.len(), 2);
        assert_eq!(
            first.iter().map(|c| &c.descriptor).collect::<Vec<_>>(),
            second.iter().map(|c| &c.descriptor).collect::<Vec<_>>()
        );
        assert_eq!(cache, second_cache);
        assert_eq!(cache["a"].matches("\"endpoint\":").count(), 2);
        assert!(!cache["a"].contains("vendor-a"));
        assert_eq!(first[0].descriptor.name, "mouse");
    }
    #[test]
    fn mixed_inputs_are_generic_vendor_only_is_not_candidate_and_claimed_metadata_remains() {
        let rows = vec![
            collection("a", "mouse", 1, 2, Some("Virtual fixture")),
            collection("a", "keyboard", 1, 6, None),
            collection("vendor-only", "vendor", 0xff00, 1, None),
        ];
        let (candidates, cache) =
            grouped_candidates(rows.clone(), &BTreeSet::new(), &Cancellation::default()).unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].descriptor.kind, DeviceKind::Generic);
        assert_eq!(candidates[0].descriptor.name, "Virtual fixture");
        assert!(cache.contains_key("vendor-only"));
        let (claimed, cache) = grouped_candidates(
            rows,
            &BTreeSet::from(["a".into()]),
            &Cancellation::default(),
        )
        .unwrap();
        assert!(claimed.is_empty());
        assert_eq!(cache["a"].matches("\"endpoint\":").count(), 2);
    }
    #[test]
    fn missing_names_and_cancelled_scans_do_not_fabricate_evidence() {
        let rows = vec![collection("a", "mouse", 1, 2, None)];
        let (candidates, _) =
            grouped_candidates(rows.clone(), &BTreeSet::new(), &Cancellation::default()).unwrap();
        assert_eq!(candidates[0].descriptor.name, "HID 外设");
        assert!(candidates[0].reading.is_none());
        let cancel = Cancellation::default();
        cancel.cancel();
        assert!(matches!(
            grouped_candidates(rows, &BTreeSet::new(), &cancel),
            Err(DeviceError::Cancelled)
        ));
    }
    #[test]
    fn nameless_input_uses_stably_selected_vendor_sibling_name() {
        let mut rows = vec![
            collection("a", "input", 1, 2, None),
            collection("a", "vendor-z", 0xff00, 1, Some("Z")),
            collection("a", "vendor-a", 0xff00, 1, Some("A")),
        ];
        let (first, _) =
            grouped_candidates(rows.clone(), &BTreeSet::new(), &Cancellation::default()).unwrap();
        rows.reverse();
        let (second, _) =
            grouped_candidates(rows, &BTreeSet::new(), &Cancellation::default()).unwrap();
        assert_eq!(first[0].descriptor.name, "A");
        assert_eq!(first[0].descriptor, second[0].descriptor);
    }
    #[test]
    fn hidapi_bus_and_uncertain_zero_do_not_replace_windows_evidence() {
        let mut metadata = Collection::default();
        metadata.fields.insert(
            "bus",
            Field::text(
                Some("windows-bus-guid"),
                "Windows.DEVPKEY_Device_BusTypeGuid",
            ),
        );
        for bus in [hidapi::BusType::Usb, hidapi::BusType::Unknown] {
            store_hidapi_bus(&mut metadata, bus);
            assert_eq!(
                metadata.fields["bus"].value.as_deref(),
                Some("windows-bus-guid")
            );
            assert!(metadata.fields.contains_key("hidapi_bus_type"));
        }
        assert!(metadata.fields["hidapi_bus_type"].value.is_none());
        for key in ["vid", "pid", "release", "usage_page", "usage"] {
            metadata.fields.insert(key, Field::hidapi_number(0));
        }
        for key in ["vid", "pid", "release", "usage_page", "usage"] {
            assert!(metadata.fields[key].value.is_none());
        }
        assert!(
            metadata
                .json("group-1", "endpoint-1", false)
                .contains("zero is unconfirmed")
        );
    }
}
