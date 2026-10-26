//! Standard BLE Battery Service only. No pairing, private requests or estimates.
use super::*;
use std::{
    cell::RefCell,
    collections::BTreeMap,
    rc::Rc,
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};
use windows::{
    Devices::{
        Bluetooth::GenericAttributeProfile::*,
        Bluetooth::{BluetoothCacheMode, BluetoothConnectionStatus, BluetoothLEDevice},
        Enumeration::{
            DeviceAccessInformation, DeviceAccessStatus, DeviceInformation, DeviceInformationKind,
        },
    },
    Foundation::{IPropertyValue, TypedEventHandler},
    Storage::Streams::DataReader,
    core::{GUID, HSTRING, Interface, RuntimeType},
};
use windows_future::{AsyncStatus, IAsyncOperation};

/// Outlives every retained WinRT object on the serial query/diagnostic thread.
pub struct Runtime(std::marker::PhantomData<Rc<()>>);
impl Runtime {
    pub fn init() -> Result<Self, DeviceError> {
        unsafe {
            windows::Win32::System::WinRT::RoInitialize(
                windows::Win32::System::WinRT::RO_INIT_MULTITHREADED,
            )
        }
        .map_err(error)?;
        Ok(Self(std::marker::PhantomData))
    }
}
impl Drop for Runtime {
    fn drop(&mut self) {
        shutdown();
        unsafe {
            windows::Win32::System::WinRT::RoUninitialize();
        }
    }
}

type ChangeHandler = Arc<dyn Fn(String, Result<Reading, DeviceError>) + Send + Sync>;
static CHANGES: OnceLock<Mutex<Option<ChangeHandler>>> = OnceLock::new();
type RefreshHandler = Arc<dyn Fn(String) + Send + Sync>;
static RECHECK: OnceLock<Mutex<Option<RefreshHandler>>> = OnceLock::new();
pub(super) fn set_refresh_handler(handler: RefreshHandler) {
    *RECHECK
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = Some(handler);
}
fn recheck(id: String) {
    let handler = RECHECK
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    if let Some(handler) = handler {
        handler(id);
    }
}
pub(super) fn set_change_handler(handler: ChangeHandler) {
    *CHANGES
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = Some(handler);
}
fn emit(id: String, value: Result<Reading, DeviceError>) {
    let handler = CHANGES
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    if let Some(handler) = handler {
        handler(id, value);
    }
}
fn uuid(short: u32) -> GUID {
    GUID::from_u128((u128::from(short) << 96) | 0x00001000800000805f9b34fb)
}
fn error(e: windows::core::Error) -> DeviceError {
    DeviceError::Io(format!("BAS: {e}"))
}
fn poll_until(
    cancel: &Cancellation,
    deadline: Instant,
    mut pending: impl FnMut() -> Result<bool, DeviceError>,
    mut abort: impl FnMut(),
) -> Result<(), DeviceError> {
    loop {
        if let Err(e) = cancel.check() {
            abort();
            return Err(e);
        }
        if Instant::now() >= deadline {
            abort();
            return Err(DeviceError::Unavailable("BAS 查询超时".into()));
        }
        if !pending()? {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}
fn wait<T: RuntimeType + 'static>(
    op: IAsyncOperation<T>,
    cancel: &Cancellation,
    deadline: Instant,
) -> Result<T, DeviceError> {
    poll_until(
        cancel,
        deadline,
        || {
            op.Status()
                .map(|s| s == AsyncStatus::Started)
                .map_err(error)
        },
        || {
            let _ = op.Cancel();
        },
    )?;
    op.GetResults().map_err(error)
}
fn parse(bytes: &[u8], name: &str) -> Result<Reading, DeviceError> {
    if bytes.len() != 1 {
        return Err(DeviceError::Protocol("BAS 2A19 长度异常".into()));
    }
    Reading::from_percent(name.into(), bytes[0], PowerState::Unknown(255))
}
fn from_buffer(
    value: &windows::Storage::Streams::IBuffer,
    name: &str,
) -> Result<Reading, DeviceError> {
    let reader = DataReader::FromBuffer(value).map_err(error)?;
    let size = reader.UnconsumedBufferLength().map_err(error)?;
    if size != 1 {
        return Err(DeviceError::Protocol("BAS 2A19 长度异常".into()));
    }
    parse(&[reader.ReadByte().map_err(error)?], name)
}
fn kind(raw: u16) -> DeviceKind {
    // Assigned Numbers: HID appearance category 15, standard subcategories.
    match raw {
        0x03c1 => DeviceKind::Keyboard,
        0x03c2 => DeviceKind::Mouse,
        0x03c3 | 0x03c4 => DeviceKind::Gamepad,
        0x03c5 => DeviceKind::Tablet,
        0x03c7 => DeviceKind::Pen,
        0x03c9 => DeviceKind::Touchpad,
        0x03ca => DeviceKind::Presenter,
        _ => DeviceKind::Generic,
    }
}
/// GSS 3.31 Battery Level Status (2BED), not a vendor charge flag.
fn parse_status(bytes: &[u8]) -> Result<PowerState, DeviceError> {
    if bytes.len() < 3 || bytes[0] & !7 != 0 {
        return Err(DeviceError::Protocol("BAS 2BED 标志或长度异常".into()));
    }
    let flags = bytes[0];
    let expected = 3
        + usize::from(flags & 1 != 0) * 2
        + usize::from(flags & 2 != 0)
        + usize::from(flags & 4 != 0);
    if bytes.len() != expected {
        return Err(DeviceError::Protocol("BAS 2BED 可选字段不完整".into()));
    }
    let power = u16::from_le_bytes([bytes[1], bytes[2]]);
    if power & 1 == 0 {
        return Err(DeviceError::Unavailable(
            "标准电池状态报告未安装电池".into(),
        ));
    }
    if flags & 2 != 0 {
        let index = 3 + usize::from(flags & 1 != 0) * 2;
        if bytes[index] > 100 {
            return Err(DeviceError::Protocol("BAS 2BED 百分比异常".into()));
        }
    }
    Ok(match (power >> 5) & 3 {
        1 => PowerState::Charging,
        2 | 3 => PowerState::Discharging,
        _ => PowerState::Unknown(255),
    })
}
fn status_buffer(value: &windows::Storage::Streams::IBuffer) -> Result<PowerState, DeviceError> {
    let reader = DataReader::FromBuffer(value).map_err(error)?;
    let size = reader.UnconsumedBufferLength().map_err(error)?;
    if !(3..=7).contains(&size) {
        return Err(DeviceError::Protocol("BAS 2BED 长度异常".into()));
    }
    let mut bytes = vec![0; size as usize];
    reader.ReadBytes(&mut bytes).map_err(error)?;
    parse_status(&bytes)
}
fn optional_status(
    service: &GattDeviceService,
    id: &str,
    cancel: &Cancellation,
    deadline: Instant,
) -> Result<Option<(GattCharacteristic, Option<i64>)>, DeviceError> {
    let result = wait(
        service
            .GetCharacteristicsForUuidAsync(uuid(0x2bed))
            .map_err(error)?,
        cancel,
        deadline,
    )?;
    if result.Status().map_err(error)? != GattCommunicationStatus::Success {
        return Ok(None);
    }
    let chars = result.Characteristics().map_err(error)?;
    if chars.Size().map_err(error)? != 1 {
        return Ok(None);
    }
    let status = chars.GetAt(0).map_err(error)?;
    let props = status.CharacteristicProperties().map_err(error)?;
    let mode = if props.contains(GattCharacteristicProperties::Notify) {
        Some(GattClientCharacteristicConfigurationDescriptorValue::Notify)
    } else if props.contains(GattCharacteristicProperties::Indicate) {
        Some(GattClientCharacteristicConfigurationDescriptorValue::Indicate)
    } else {
        None
    };
    let mut token = None;
    if let Some(mode) = mode {
        let status_id = id.to_owned();
        token = status
            .ValueChanged(&TypedEventHandler::<
                GattCharacteristic,
                GattValueChangedEventArgs,
            >::new(move |_, args| {
                if args.as_ref().is_some() {
                    recheck(status_id.clone());
                }
                Ok(())
            }))
            .ok();
        if token.is_some() {
            let subscribed = status
                .WriteClientCharacteristicConfigurationDescriptorAsync(mode)
                .map_err(error)
                .and_then(|op| wait(op, cancel, deadline))
                .is_ok_and(|s| s == GattCommunicationStatus::Success);
            if !subscribed {
                if let Some(value) = token.take() {
                    let _ = status.RemoveValueChanged(value);
                }
            }
        }
    }
    if let Err(e) = cancel.check() {
        if let Some(token) = token {
            let _ = status.RemoveValueChanged(token);
        }
        return Err(e);
    }
    Ok(Some((status, token)))
}
struct DeviceHandle {
    device: BluetoothLEDevice,
    id: String,
}
impl Drop for DeviceHandle {
    fn drop(&mut self) {
        let _ = self.device.Close();
    }
}
struct Session {
    device: Rc<DeviceHandle>,
    service: GattDeviceService,
    characteristic: GattCharacteristic,
    value_token: Option<i64>,
    connection_token: Option<i64>,
    status: Option<GattCharacteristic>,
    status_token: Option<i64>,
    name: String,
    configured: bool,
}
impl Drop for Session {
    fn drop(&mut self) {
        if let Some(token) = self.value_token {
            let _ = self.characteristic.RemoveValueChanged(token);
        }
        if let Some(token) = self.connection_token {
            let _ = self.device.device.RemoveConnectionStatusChanged(token);
        }
        if let (Some(status), Some(token)) = (&self.status, self.status_token) {
            let _ = status.RemoveValueChanged(token);
        }
        let _ = self.service.Close();
    }
}
thread_local! { static SESSIONS: RefCell<BTreeMap<String,Session>> = const { RefCell::new(BTreeMap::new()) }; }
pub(super) fn shutdown() {
    SESSIONS.with(|s| s.borrow_mut().clear());
    if let Some(handler) = CHANGES.get() {
        *handler.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
    if let Some(handler) = RECHECK.get() {
        *handler.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}
fn read_mandatory(
    session: &Session,
    cancel: &Cancellation,
    deadline: Instant,
) -> Result<Reading, DeviceError> {
    if session.device.device.ConnectionStatus().map_err(error)?
        != BluetoothConnectionStatus::Connected
    {
        return Err(DeviceError::NotFound);
    }
    let result = wait(
        session
            .characteristic
            .ReadValueWithCacheModeAsync(BluetoothCacheMode::Uncached)
            .map_err(error)?,
        cancel,
        deadline,
    )?;
    if session.device.device.ConnectionStatus().map_err(error)?
        != BluetoothConnectionStatus::Connected
    {
        return Err(DeviceError::NotFound);
    }
    if result.Status().map_err(error)? != GattCommunicationStatus::Success {
        return Err(DeviceError::Unavailable(
            "BAS 读取被拒绝或连接不可用".into(),
        ));
    }
    from_buffer(&result.Value().map_err(error)?, &session.name)
}
fn enrich_status(
    mut reading: Reading,
    session: &Session,
    cancel: &Cancellation,
    deadline: Instant,
) -> Result<Reading, DeviceError> {
    if let Some(status) = &session.status {
        if Instant::now() < deadline {
            let result = status
                .ReadValueWithCacheModeAsync(BluetoothCacheMode::Uncached)
                .map_err(error)
                .and_then(|op| {
                    wait(
                        op,
                        cancel,
                        deadline.min(Instant::now() + Duration::from_millis(500)),
                    )
                });
            match result {
                Err(DeviceError::Cancelled) => return Err(DeviceError::Cancelled),
                Ok(result)
                    if result
                        .Status()
                        .is_ok_and(|s| s == GattCommunicationStatus::Success) =>
                {
                    match result
                        .Value()
                        .map_err(error)
                        .and_then(|b| status_buffer(&b))
                    {
                        Ok(power) => reading.power_state = power,
                        Err(e @ DeviceError::Unavailable(_)) => return Err(e),
                        _ => {}
                    }
                }
                _ => {}
            }
        }
    }
    cancel.check()?;
    if session.device.device.ConnectionStatus().map_err(error)?
        != BluetoothConnectionStatus::Connected
    {
        return Err(DeviceError::NotFound);
    }
    Ok(reading)
}
fn read_session(
    session: &Session,
    cancel: &Cancellation,
    deadline: Instant,
) -> Result<Reading, DeviceError> {
    enrich_status(
        read_mandatory(session, cancel, deadline)?,
        session,
        cancel,
        deadline,
    )
}
fn same_session(existing_win_id: &str, win_id: &str) -> bool {
    existing_win_id == win_id
}
fn physical_id(guid: Option<GUID>, win_id: &str) -> String {
    if let Some(guid) = guid.filter(|g| *g != GUID::zeroed()) {
        let mut bytes = Vec::from(guid.data1.to_le_bytes());
        bytes.extend(guid.data2.to_le_bytes());
        bytes.extend(guid.data3.to_le_bytes());
        bytes.extend(guid.data4);
        format!(
            "container:{}",
            bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()
        )
    } else {
        format!("aep:{win_id}")
    }
}
fn container_for(info: &DeviceInformation, win_id: &str) -> String {
    let value = info
        .Properties()
        .and_then(|p| p.Lookup(&HSTRING::from("System.Devices.Aep.ContainerId")))
        .and_then(|v| v.cast::<IPropertyValue>())
        .and_then(|v| v.GetGuid())
        .ok();
    physical_id(value, win_id)
}
fn configure_session(
    session: &mut Session,
    id: &str,
    cancel: &Cancellation,
    deadline: Instant,
) -> Result<(), DeviceError> {
    let props = session
        .characteristic
        .CharacteristicProperties()
        .map_err(error)?;
    let callback_id = id.to_owned();
    let callback_device = session.device.device.clone();
    session.value_token = session
        .characteristic
        .ValueChanged(&TypedEventHandler::<
            GattCharacteristic,
            GattValueChangedEventArgs,
        >::new(move |_, args| {
            if args.as_ref().is_some()
                && callback_device
                    .ConnectionStatus()
                    .is_ok_and(|s| s == BluetoothConnectionStatus::Connected)
            {
                recheck(callback_id.clone());
            }
            Ok(())
        }))
        .ok();
    let mode = if props.contains(GattCharacteristicProperties::Notify) {
        Some(GattClientCharacteristicConfigurationDescriptorValue::Notify)
    } else if props.contains(GattCharacteristicProperties::Indicate) {
        Some(GattClientCharacteristicConfigurationDescriptorValue::Indicate)
    } else {
        None
    };
    let subscribed = if let Some(mode) = mode.filter(|_| session.value_token.is_some()) {
        session
            .characteristic
            .WriteClientCharacteristicConfigurationDescriptorAsync(mode)
            .map_err(error)
            .and_then(|op| {
                wait(
                    op,
                    cancel,
                    deadline.min(Instant::now() + Duration::from_millis(500)),
                )
            })
            .is_ok_and(|s| s == GattCommunicationStatus::Success)
    } else {
        false
    };
    if !subscribed {
        if let Some(token) = session.value_token.take() {
            let _ = session.characteristic.RemoveValueChanged(token);
        }
    }
    cancel.check()?;
    if Instant::now() < deadline {
        match optional_status(
            &session.service,
            id,
            cancel,
            deadline.min(Instant::now() + Duration::from_millis(500)),
        ) {
            Ok(Some((status, token))) => {
                session.status = Some(status);
                session.status_token = token;
            }
            Err(DeviceError::Cancelled) => return Err(DeviceError::Cancelled),
            _ => {}
        }
    }
    let disconnect_id = id.to_owned();
    session.connection_token = session
        .device
        .device
        .ConnectionStatusChanged(&TypedEventHandler::<
            BluetoothLEDevice,
            windows::core::IInspectable,
        >::new(move |sender, _| {
            if sender.as_ref().is_some_and(|d| {
                d.ConnectionStatus()
                    .is_ok_and(|s| s == BluetoothConnectionStatus::Disconnected)
            }) {
                emit(disconnect_id.clone(), Err(DeviceError::NotFound));
            } else {
                recheck(disconnect_id.clone());
            }
            Ok(())
        }))
        .ok();
    session.configured = true;
    Ok(())
}
#[derive(Default)]
struct Collector {
    report: DiscoveryReport,
    active: std::collections::BTreeSet<String>,
}
impl Collector {
    fn record(
        &mut self,
        result: Result<Option<Candidate>, DeviceError>,
    ) -> Result<(), DeviceError> {
        match result {
            Ok(Some(candidate)) => {
                self.active.insert(candidate.descriptor.id.clone());
                self.report.candidates.push(candidate);
            }
            Ok(None) | Err(DeviceError::NotFound) => {}
            Err(DeviceError::Cancelled) => return Err(DeviceError::Cancelled),
            Err(error) => self.report.failures.push(("bas".into(), error.to_string())),
        }
        Ok(())
    }
}
struct DiscoveryIdentity<'a> {
    name: &'a str,
    physical: &'a str,
    kind: DeviceKind,
    address: u64,
}
fn discover_service(
    service: GattDeviceService,
    owner: Rc<DeviceHandle>,
    identity: &DiscoveryIdentity<'_>,
    cancel: &Cancellation,
    deadline: Instant,
) -> Result<Option<Candidate>, DeviceError> {
    let name = identity.name;
    let physical = identity.physical;
    let category = identity.kind;
    let address = identity.address;
    let handle = service.AttributeHandle().map_err(error)?;
    let id = format!("bas:{address:012x}:{handle:04x}");
    SESSIONS.with(|s| {
        let mut s = s.borrow_mut();
        if s.get(&id)
            .is_some_and(|old| !same_session(&old.device.id, &owner.id))
        {
            s.remove(&id);
        }
    });
    if !SESSIONS.with(|s| s.borrow().contains_key(&id)) {
        let chars = wait(
            service
                .GetCharacteristicsForUuidAsync(uuid(0x2a19))
                .map_err(error)?,
            cancel,
            deadline,
        )?;
        if chars.Status().map_err(error)? != GattCommunicationStatus::Success {
            let _ = service.Close();
            return Err(DeviceError::Unavailable("BAS 特征访问被拒绝或超时".into()));
        }
        let chars = chars.Characteristics().map_err(error)?;
        if chars.Size().map_err(error)? != 1 {
            let _ = service.Close();
            return Ok(None);
        }
        let characteristic = chars.GetAt(0).map_err(error)?;
        SESSIONS.with(|s| {
            s.borrow_mut().insert(
                id.clone(),
                Session {
                    device: owner,
                    service,
                    characteristic,
                    value_token: None,
                    connection_token: None,
                    status: None,
                    status_token: None,
                    name: name.into(),
                    configured: false,
                },
            )
        });
    }
    // Always read mandatory 2A19 before spending remaining budget on setup.
    let reading = SESSIONS.with(|s| {
        let mut sessions = s.borrow_mut();
        let session = sessions.get_mut(&id).unwrap();
        session.name = name.into();
        let reading = read_mandatory(session, cancel, deadline)?;
        if !session.configured
            && deadline.saturating_duration_since(Instant::now()) >= Duration::from_millis(100)
        {
            if let Err(DeviceError::Cancelled) = configure_session(session, &id, cancel, deadline) {
                return Err(DeviceError::Cancelled);
            }
        }
        enrich_status(reading, session, cancel, deadline)
    });
    let (reading, error) = match reading {
        Ok(value) => (Some(value), None),
        Err(DeviceError::Cancelled) => {
            SESSIONS.with(|s| s.borrow_mut().remove(&id));
            return Err(DeviceError::Cancelled);
        }
        Err(DeviceError::NotFound) => {
            SESSIONS.with(|s| s.borrow_mut().remove(&id));
            emit(id, Err(DeviceError::NotFound));
            return Ok(None);
        }
        Err(error) => (None, Some(error)),
    };
    Ok(Some(Candidate {
        descriptor: DeviceDescriptor {
            id,
            physical_id: physical.into(),
            endpoint: Some(format!("{handle:04x}")),
            recoverable: true,
            name: name.into(),
            kind: category,
            supported: true,
            provider: "bas".into(),
            connection: super::Connection::BluetoothLe,
        },
        reading,
        error,
    }))
}
fn discover_device(
    info: DeviceInformation,
    collector: &mut Collector,
    cancel: &Cancellation,
    deadline: Instant,
) -> Result<(), DeviceError> {
    if !info.Pairing().map_err(error)?.IsPaired().map_err(error)? {
        return Ok(());
    }
    let win_id = info.Id().map_err(error)?.to_string();
    if DeviceAccessInformation::CreateFromId(&HSTRING::from(&win_id))
        .and_then(|a| a.CurrentStatus())
        .map_err(error)?
        != DeviceAccessStatus::Allowed
    {
        return Err(DeviceError::Unavailable("标准蓝牙服务尚无访问许可".into()));
    }
    let existing = SESSIONS.with(|s| {
        s.borrow()
            .values()
            .find(|s| same_session(&s.device.id, &win_id))
            .map(|s| s.device.clone())
    });
    let owner = if let Some(owner) = existing {
        owner
    } else {
        Rc::new(DeviceHandle {
            device: wait(
                BluetoothLEDevice::FromIdAsync(&HSTRING::from(&win_id)).map_err(error)?,
                cancel,
                deadline,
            )?,
            id: win_id.clone(),
        })
    };
    if owner.device.ConnectionStatus().map_err(error)? != BluetoothConnectionStatus::Connected {
        SESSIONS.with(|s| {
            s.borrow_mut().retain(|id, s| {
                if s.device.id == win_id {
                    emit(id.clone(), Err(DeviceError::NotFound));
                    false
                } else {
                    true
                }
            });
        });
        return Ok(());
    }
    let address = owner.device.BluetoothAddress().map_err(error)?;
    let name = info.Name().map_err(error)?.to_string();
    let name = if name.trim().is_empty() {
        "蓝牙外设".into()
    } else {
        name
    };
    let physical = container_for(&info, &win_id);
    let category = owner
        .device
        .Appearance()
        .and_then(|a| a.RawValue())
        .map(kind)
        .unwrap_or(DeviceKind::Generic);
    let services = wait(
        owner
            .device
            .GetGattServicesForUuidWithCacheModeAsync(uuid(0x180f), BluetoothCacheMode::Cached)
            .map_err(error)?,
        cancel,
        deadline,
    )?;
    if services.Status().map_err(error)? != GattCommunicationStatus::Success {
        return Err(DeviceError::Unavailable("BAS 服务访问被拒绝或超时".into()));
    }
    for service in services.Services().map_err(error)? {
        cancel.check()?;
        collector.record(discover_service(
            service,
            owner.clone(),
            &DiscoveryIdentity {
                name: &name,
                physical: &physical,
                kind: category,
                address,
            },
            cancel,
            deadline,
        ))?;
    }
    Ok(())
}
pub(super) fn read(id: &str, cancel: &Cancellation) -> Result<Reading, DeviceError> {
    let deadline = Instant::now() + Duration::from_secs(3);
    if let Some(value) = SESSIONS.with(|s| {
        s.borrow()
            .get(id)
            .map(|s| read_session(s, cancel, deadline))
    }) {
        if matches!(value, Err(DeviceError::NotFound)) {
            SESSIONS.with(|s| s.borrow_mut().remove(id));
        }
        return value;
    }
    let report = discover_until(cancel, deadline)?;
    if let Some(candidate) = report
        .candidates
        .into_iter()
        .find(|c| c.descriptor.id == id)
    {
        return candidate.reading.ok_or_else(|| {
            candidate
                .error
                .unwrap_or_else(|| DeviceError::Unavailable("BAS 暂无有效读数".into()))
        });
    }
    if report.failures.is_empty() {
        Err(DeviceError::NotFound)
    } else {
        Err(DeviceError::Unavailable("BAS 发现未完整完成".into()))
    }
}
fn discover_until(
    cancel: &Cancellation,
    deadline: Instant,
) -> Result<DiscoveryReport, DeviceError> {
    let selector = BluetoothLEDevice::GetDeviceSelectorFromPairingState(true).map_err(error)?;
    let selector = HSTRING::from(format!(
        "({selector}) AND System.Devices.Aep.IsConnected:=System.StructuredQueryType.Boolean#True"
    ));
    let properties = windows_collections::IIterable::<HSTRING>::from(vec![HSTRING::from(
        "System.Devices.Aep.ContainerId",
    )]);
    let found = wait(
        DeviceInformation::FindAllAsyncWithKindAqsFilterAndAdditionalProperties(
            &selector,
            &properties,
            DeviceInformationKind::AssociationEndpoint,
        )
        .map_err(error)?,
        cancel,
        deadline,
    )?;
    let count = found.Size().map_err(error)?;
    if count > 32 {
        return Err(DeviceError::Unavailable(
            "BAS 设备数超出本轮有界发现上限".into(),
        ));
    }
    let mut collector = Collector::default();
    for index in 0..count {
        cancel.check()?;
        if Instant::now() >= deadline {
            collector.record(Err(DeviceError::Unavailable("BAS 发现时间已达上限".into())))?;
            break;
        }
        let result = found
            .GetAt(index)
            .map_err(error)
            .and_then(|info| discover_device(info, &mut collector, cancel, deadline));
        if let Err(error) = result {
            collector.record(Err(error))?;
        }
    }
    cancel.check()?;
    // Incomplete enumeration never proves another known device disconnected.
    if collector.report.failures.is_empty() {
        SESSIONS.with(|s| s.borrow_mut().retain(|id, _| collector.active.contains(id)));
    }
    Ok(collector.report)
}
pub(super) fn discover(cancel: &Cancellation) -> Result<Vec<Candidate>, DeviceError> {
    discover_until(cancel, Instant::now() + Duration::from_secs(8)).map(|r| r.candidates)
}
pub(super) struct BasAdapter;
impl ProtocolAdapter for BasAdapter {
    fn name(&self) -> &'static str {
        "bas"
    }
    fn owns(&self, id: &str) -> bool {
        id.starts_with("bas:")
    }
    fn discover(&self, cancel: &Cancellation) -> Result<Vec<Candidate>, DeviceError> {
        discover(cancel)
    }
    fn discover_report(&self, cancel: &Cancellation) -> Result<DiscoveryReport, DeviceError> {
        discover_until(cancel, Instant::now() + Duration::from_secs(8))
    }
    fn read(&self, id: &str, cancel: &Cancellation) -> Result<Reading, DeviceError> {
        read(id, cancel)
    }
}
fn support_characteristic(
    service: &GattDeviceService,
    short: u32,
    cancel: &Cancellation,
    deadline: Instant,
) -> Result<String, DeviceError> {
    use crate::app::json_string as q;
    let chars = wait(
        service
            .GetCharacteristicsForUuidAsync(uuid(short))
            .map_err(error)?,
        cancel,
        deadline,
    )?;
    let cs = chars.Status().map_err(error)?;
    if cs != GattCommunicationStatus::Success {
        return Ok(format!(
            "{{\"uuid\":\"{short:04X}\",\"lookup_status\":{},\"present\":null}}",
            cs.0
        ));
    }
    let chars = chars.Characteristics().map_err(error)?;
    let count = chars.Size().map_err(error)?;
    if count == 0 {
        return Ok(format!("{{\"uuid\":\"{short:04X}\",\"present\":false}}"));
    }
    // Ambiguous duplicate standard characteristics are reported, not chosen arbitrarily.
    if count != 1 {
        return Ok(format!(
            "{{\"uuid\":\"{short:04X}\",\"present\":true,\"count\":{count},\"read\":null}}"
        ));
    }
    let characteristic = chars.GetAt(0).map_err(error)?;
    let props = characteristic.CharacteristicProperties().map_err(error)?;
    let value = if props.contains(GattCharacteristicProperties::Read) {
        let r = wait(
            characteristic
                .ReadValueWithCacheModeAsync(BluetoothCacheMode::Uncached)
                .map_err(error)?,
            cancel,
            deadline,
        )?;
        if r.Status().map_err(error)? == GattCommunicationStatus::Success {
            if short == 0x2a19 {
                match from_buffer(&r.Value().map_err(error)?, "diagnostic") {
                    Ok(reading) => format!(
                        "{{\"status\":\"success\",\"percent\":{}}}",
                        reading.normalized_percent()
                    ),
                    Err(e) => format!(
                        "{{\"status\":\"invalid_value\",\"reason\":{}}}",
                        q(&e.to_string())
                    ),
                }
            } else {
                match status_buffer(&r.Value().map_err(error)?) {
                    Ok(power) => format!(
                        "{{\"status\":\"success\",\"power_state\":{}}}",
                        q(&format!("{power:?}"))
                    ),
                    Err(e) => format!(
                        "{{\"status\":\"invalid_value\",\"reason\":{}}}",
                        q(&e.to_string())
                    ),
                }
            }
        } else {
            format!(
                "{{\"status\":\"gatt_error\",\"code\":{}}}",
                r.Status().map_err(error)?.0
            )
        }
    } else {
        "{\"status\":\"not_readable\"}".into()
    };
    Ok(format!(
        "{{\"uuid\":\"{short:04X}\",\"present\":true,\"properties\":{{\"read\":{},\"notify\":{},\"indicate\":{}}},\"read\":{value}}}",
        props.contains(GattCharacteristicProperties::Read),
        props.contains(GattCharacteristicProperties::Notify),
        props.contains(GattCharacteristicProperties::Indicate)
    ))
}

fn support_fields(
    mut query: impl FnMut(u32) -> Result<String, DeviceError>,
) -> Result<Vec<String>, DeviceError> {
    let mut rows = Vec::new();
    for short in [0x2a19, 0x2bed] {
        rows.push(match query(short) {
            Ok(row) => row,
            Err(DeviceError::Cancelled) => return Err(DeviceError::Cancelled),
            Err(error) => format!(
                "{{\"uuid\":\"{short:04X}\",\"status\":\"query_failed\",\"error\":{}}}",
                super::support::error_json(Some(&error))
            ),
        });
    }
    Ok(rows)
}

/// Selected-device, read-only BAS diagnostic. Does not pair, request consent,
/// subscribe/unsubscribe or update monitor caches/alert cycles.
pub(super) fn support_information(
    target: &DeviceDescriptor,
    cancel: &Cancellation,
) -> Result<String, DeviceError> {
    let deadline = Instant::now() + Duration::from_secs(5);
    cancel.check()?;
    let selector = BluetoothLEDevice::GetDeviceSelectorFromPairingState(true).map_err(error)?;
    let properties = windows_collections::IIterable::<HSTRING>::from(vec![HSTRING::from(
        "System.Devices.Aep.ContainerId",
    )]);
    let found = wait(
        DeviceInformation::FindAllAsyncWithKindAqsFilterAndAdditionalProperties(
            &selector,
            &properties,
            DeviceInformationKind::AssociationEndpoint,
        )
        .map_err(error)?,
        cancel,
        deadline,
    )?;
    let count = found.Size().map_err(error)?;
    if count > 32 {
        return Err(DeviceError::Unavailable("BLE诊断候选数量超限".into()));
    }
    for index in 0..count {
        cancel.check()?;
        let info = found.GetAt(index).map_err(error)?;
        let id = info.Id().map_err(error)?.to_string();
        // Identity only: never match a BLE peer by its product/display name.
        if container_for(&info, &id) != target.physical_id {
            continue;
        }
        let permission = DeviceAccessInformation::CreateFromId(&HSTRING::from(&id))
            .and_then(|a| a.CurrentStatus())
            .map_err(error)?;
        if permission != DeviceAccessStatus::Allowed {
            return Ok(format!(
                "{{\"status\":\"permission_not_allowed\",\"access_status\":{},\"paired\":true,\"services\":null}}",
                permission.0
            ));
        }
        let owner = if let Some(owner) = SESSIONS.with(|s| {
            s.borrow()
                .values()
                .find(|s| s.device.id == id)
                .map(|s| s.device.clone())
        }) {
            owner
        } else {
            Rc::new(DeviceHandle {
                device: wait(
                    BluetoothLEDevice::FromIdAsync(&HSTRING::from(&id)).map_err(error)?,
                    cancel,
                    deadline,
                )?,
                id: id.clone(),
            })
        };
        if owner.device.ConnectionStatus().map_err(error)? != BluetoothConnectionStatus::Connected {
            return Ok("{\"status\":\"disconnected\",\"paired\":true,\"access_status\":\"allowed\",\"services\":null}".into());
        }
        let result = wait(
            owner
                .device
                .GetGattServicesForUuidWithCacheModeAsync(
                    uuid(0x180f),
                    BluetoothCacheMode::Uncached,
                )
                .map_err(error)?,
            cancel,
            deadline,
        )?;
        let status = result.Status().map_err(error)?;
        if status != GattCommunicationStatus::Success {
            return Ok(format!(
                "{{\"status\":\"service_lookup_failed\",\"gatt_status\":{},\"services\":null}}",
                status.0
            ));
        }
        let services = result.Services().map_err(error)?;
        let length = services.Size().map_err(error)?;
        let mut rows = Vec::new();
        for n in 0..length.min(4) {
            cancel.check()?;
            let service = services.GetAt(n).map_err(error)?;
            let handle = service.AttributeHandle().map_err(error)?;
            let existing = SESSIONS.with(|s| {
                s.borrow()
                    .values()
                    .find(|s| s.device.id == id && s.service.AttributeHandle().ok() == Some(handle))
                    .map(|s| s.value_token.is_some())
            });
            // If Windows returned a service already retained by monitoring,
            // do not Close an aliased service and invalidate that session.
            struct ServiceGuard(GattDeviceService, bool);
            impl Drop for ServiceGuard {
                fn drop(&mut self) {
                    if self.1 {
                        let _ = self.0.Close();
                    }
                }
            }
            let guard = ServiceGuard(service, existing.is_none());
            let characteristics =
                support_fields(|short| support_characteristic(&guard.0, short, cancel, deadline))?;
            rows.push(format!("{{\"uuid\":\"180F\",\"attribute_handle\":{handle},\"monitor_subscription_active\":{},\"characteristics\":[{}]}}",
                existing.map_or_else(||"null".into(),|v|v.to_string()),characteristics.join(",")));
        }
        cancel.check()?;
        return Ok(format!(
            "{{\"status\":\"success\",\"paired\":true,\"connected\":true,\"access_status\":\"allowed\",\"battery_service_present\":{},\"truncated\":{},\"services\":[{}]}}",
            length > 0,
            length > 4,
            rows.join(",")
        ));
    }
    Ok("{\"status\":\"no_matching_paired_ble_endpoint\",\"battery_service_present\":null,\"note\":\"No physical-identity match; this does not prove BAS absence or rule out classic Bluetooth\"}".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn support_optional_failure_keeps_mandatory_evidence_and_cancel_is_not_partial_success() {
        let mut calls = Vec::new();
        let rows = support_fields(|short| {
            calls.push(short);
            if short == 0x2a19 {
                Ok("{\"percent\":35}".into())
            } else {
                Err(DeviceError::Io(r"permission C:\Users\PRIVATE\file".into()))
            }
        })
        .unwrap();
        assert_eq!(calls, [0x2a19, 0x2bed]);
        assert_eq!(rows[0], "{\"percent\":35}");
        assert!(rows[1].contains("query_failed") && !rows[1].contains("PRIVATE"));
        assert!(matches!(
            support_fields(|_| Err(DeviceError::Cancelled)),
            Err(DeviceError::Cancelled)
        ));
    }
    #[test]
    fn pending_wait_times_out_and_cancels_after_wait_started() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let aborted = AtomicBool::new(false);
        let mut polls = 0;
        let start = Instant::now();
        assert!(matches!(
            poll_until(
                &Cancellation::default(),
                start + Duration::from_millis(35),
                || {
                    polls += 1;
                    Ok(true)
                },
                || {
                    aborted.store(true, Ordering::SeqCst);
                }
            ),
            Err(DeviceError::Unavailable(_))
        ));
        assert!(aborted.load(Ordering::SeqCst));
        assert!(polls > 0);
        assert!(start.elapsed() < Duration::from_millis(300));
        let cancel = Cancellation::default();
        let child_cancel = cancel.clone();
        let aborted = Arc::new(AtomicBool::new(false));
        let child_aborted = aborted.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let waiter = std::thread::spawn(move || {
            let mut announced = false;
            let start = Instant::now();
            let result = poll_until(
                &child_cancel,
                start + Duration::from_secs(2),
                || {
                    if !announced {
                        announced = true;
                        tx.send(()).unwrap();
                    }
                    Ok(true)
                },
                || {
                    child_aborted.store(true, Ordering::SeqCst);
                },
            );
            (result, start.elapsed())
        });
        rx.recv_timeout(Duration::from_millis(300)).unwrap();
        cancel.cancel();
        let (result, elapsed) = waiter.join().unwrap();
        assert!(matches!(result, Err(DeviceError::Cancelled)));
        assert!(aborted.load(Ordering::SeqCst));
        assert!(elapsed < Duration::from_millis(300));
    }
    fn fixture(id: &str, percent: u8) -> Candidate {
        Candidate {
            descriptor: DeviceDescriptor {
                id: id.into(),
                physical_id: id.into(),
                endpoint: None,
                recoverable: true,
                name: id.into(),
                kind: DeviceKind::Mouse,
                supported: true,
                provider: "bas".into(),
                connection: super::Connection::BluetoothLe,
            },
            reading: Some(
                Reading::from_percent(id.into(), percent, PowerState::Unknown(255)).unwrap(),
            ),
            error: None,
        }
    }
    #[test]
    fn device_and_service_failures_preserve_healthy_and_unresolved_values() {
        let mut manager = crate::manager::Manager::default();
        manager.merge(vec![fixture("healthy", 80), fixture("denied", 60)], 100);
        let mut collector = Collector::default();
        collector
            .record(Err(DeviceError::Unavailable("one denied device".into())))
            .unwrap();
        collector.record(Ok(Some(fixture("healthy", 79)))).unwrap();
        collector
            .record(Err(DeviceError::Io("one failed service".into())))
            .unwrap();
        manager.merge_discovery(std::mem::take(&mut collector.report), 101);
        let healthy = &manager.devices[0];
        assert_eq!(healthy.last_success, Some(101));
        assert!(healthy.error.is_none());
        assert_eq!(healthy.reading.as_ref().unwrap().reported_percent, 79);
        let denied = &manager.devices[1];
        assert_eq!(denied.last_success, Some(100));
        assert_eq!(denied.reading.as_ref().unwrap().reported_percent, 60);
        assert!(denied.connected);
        assert!(denied.error.is_some());
        assert!(matches!(
            collector.record(Err(DeviceError::Cancelled)),
            Err(DeviceError::Cancelled)
        ));
        let mut collector = Collector::default();
        collector.record(Err(DeviceError::NotFound)).unwrap();
        assert!(collector.report.candidates.is_empty());
        assert!(collector.active.is_empty());
    }
    #[test]
    fn aep_container_guid_matches_hid_byte_identity_and_pairing_is_not_reused() {
        let guid = GUID::from_u128(0x00112233445566778899aabbccddeeff);
        assert_eq!(
            physical_id(Some(guid), "aep"),
            "container:33221100554477668899aabbccddeeff"
        );
        assert_eq!(physical_id(None, "aep-id"), "aep:aep-id");
        assert_eq!(physical_id(Some(GUID::zeroed()), "aep-id"), "aep:aep-id");
        assert!(same_session("paired-one", "paired-one"));
        assert!(!same_session("paired-one", "paired-two"));
    }
    #[test]
    fn standard_percent_and_unknown_power_only() {
        for p in [0, 1, 19, 20, 87, 100] {
            let r = parse(&[p], "BAS fixture").unwrap();
            assert_eq!(r.reported_percent, p);
            assert!(matches!(r.power_state, PowerState::Unknown(_)));
        }
        for b in [&[][..], &[101][..], &[255][..], &[20, 1][..]] {
            assert!(parse(b, "fixture").is_err());
        }
    }
    #[test]
    fn classification_uses_assigned_appearance_not_name() {
        assert_eq!(kind(0x3c2), DeviceKind::Mouse);
        assert_eq!(kind(0x3c1), DeviceKind::Keyboard);
        assert_eq!(kind(0), DeviceKind::Generic);
        assert_eq!(
            uuid(0x180f),
            GUID::from_u128(0x0000180f00001000800000805f9b34fb)
        );
    }
    #[test]
    fn standard_battery_status_checks_optional_fields_and_charge_bits() {
        for (bits, power) in [
            (0, PowerState::Unknown(255)),
            (1, PowerState::Charging),
            (2, PowerState::Discharging),
            (3, PowerState::Discharging),
        ] {
            assert_eq!(parse_status(&[0, 1 | (bits << 5), 0]).unwrap(), power);
        }
        assert_eq!(
            parse_status(&[7, 0x21, 0, 0x34, 0x12, 19, 0]).unwrap(),
            PowerState::Charging
        );
        for bytes in [
            &[][..],
            &[0, 0][..],
            &[0, 0, 0][..],
            &[2, 0x21, 0][..],
            &[2, 0x21, 0, 101][..],
            &[8, 1, 0][..],
        ] {
            assert!(parse_status(bytes).is_err());
        }
        assert_eq!(kind(0x03c6), DeviceKind::Generic);
        assert_eq!(kind(0x03c7), DeviceKind::Pen);
        assert_eq!(kind(0x03c9), DeviceKind::Touchpad);
    }
}
