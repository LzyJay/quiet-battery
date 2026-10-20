//! Bounded native HID I/O. A cancelled IRP owns its memory until kernel completion.
use super::{Cancellation, DeviceError};
use std::{
    cell::UnsafeCell,
    marker::PhantomData,
    ptr::{null, null_mut},
    rc::Rc,
    sync::{Arc, Mutex, OnceLock},
    thread::ThreadId,
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::*,
    Storage::FileSystem::*,
    System::{IO::*, Threading::*},
};

const MAX_PENDING: usize = 8;
const GET_FEATURE: u32 = 0x000b0192;
const SET_FEATURE: u32 = 0x000b0191;
struct OwnedHandle(HANDLE);
// Kernel handles can be used across threads; closure is exclusively owned by this RAII value.
unsafe impl Send for OwnedHandle {}
unsafe impl Sync for OwnedHandle {}
impl Drop for OwnedHandle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}
struct Device {
    handle: OwnedHandle,
    key: String,
    owner: ThreadId,
    _lease: QueryGuard,
}
struct Operation {
    device: Arc<Device>,
    event: OwnedHandle,
    // Windows updates this asynchronously; UnsafeCell prevents Rust references
    // to the owning Operation from promising immutability/exclusivity here.
    overlapped: UnsafeCell<OVERLAPPED>,
    buffer: Vec<u8>,
    transferred: u32,
}
// Only the issuing thread or the locked quarantine accesses an operation. Its Box and
// Vec allocations never move while Windows owns their addresses.
unsafe impl Send for Operation {}
#[derive(Default)]
struct Pending {
    // Box keeps OVERLAPPED stable when the bounded vector grows or reaps entries.
    #[allow(clippy::vec_box)]
    operations: Vec<Box<Operation>>,
    reserved: usize,
}
static PENDING: OnceLock<Mutex<Pending>> = OnceLock::new();
fn pending() -> &'static Mutex<Pending> {
    PENDING.get_or_init(|| Mutex::new(Pending::default()))
}
fn io(operation: &str, code: u32) -> DeviceError {
    DeviceError::Io(format!("{operation}: Win32 {code}"))
}
fn lock_pending() -> Result<std::sync::MutexGuard<'static, Pending>, DeviceError> {
    pending()
        .lock()
        .map_err(|_| DeviceError::Unavailable("HID cleanup state unavailable".into()))
}
impl Operation {
    fn complete(&mut self) -> Result<bool, DeviceError> {
        if unsafe {
            GetOverlappedResult(
                self.device.handle.0,
                self.overlapped.get(),
                &mut self.transferred,
                0,
            )
        } != 0
        {
            return Ok(true);
        }
        let error = unsafe { GetLastError() };
        if error == ERROR_IO_INCOMPLETE {
            Ok(false)
        } else {
            Err(io("HID completion", error))
        }
    }
}
fn reap(p: &mut Pending) {
    // Any completion status other than IO_INCOMPLETE means the IRP no longer owns memory.
    p.operations.retain_mut(|op| {
        op.device.owner != std::thread::current().id() || matches!(op.complete(), Ok(false))
    });
}
struct Reservation(bool);
impl Reservation {
    fn acquire(key: &str) -> Result<Self, DeviceError> {
        let mut p = lock_pending()?;
        reap(&mut p);
        if p.operations.iter().any(|op| op.device.key == key) {
            return Err(DeviceError::Unavailable(
                "HID previous request still cancelling".into(),
            ));
        }
        if p.operations.len() + p.reserved >= MAX_PENDING {
            return Err(DeviceError::Unavailable(
                "HID cancellation queue full".into(),
            ));
        }
        p.reserved += 1;
        Ok(Self(true))
    }
    fn quarantine(mut self, operation: Box<Operation>) {
        // Lock poisoning cannot arise from OS status errors; preserve ownership even if
        // another thread panicked rather than freeing a live IRP buffer.
        let mut p = pending().lock().unwrap_or_else(|error| error.into_inner());
        p.reserved -= 1;
        p.operations.push(operation);
        self.0 = false;
    }
}
impl Drop for Reservation {
    fn drop(&mut self) {
        if self.0 {
            pending().lock().unwrap_or_else(|e| e.into_inner()).reserved -= 1;
        }
    }
}
pub(super) struct Session {
    device: Arc<Device>,
}
impl Session {
    pub fn open(
        path: &str,
        key: &str,
        lease: QueryGuard,
        cancel: &Cancellation,
    ) -> Result<Self, DeviceError> {
        Self::open_access(path, key, lease, cancel, false)
    }
    pub fn open_access(
        path: &str,
        key: &str,
        lease: QueryGuard,
        cancel: &Cancellation,
        feature_only: bool,
    ) -> Result<Self, DeviceError> {
        cancel.check()?;
        {
            let mut p = lock_pending()?;
            reap(&mut p);
            if p.operations.iter().any(|op| op.device.key == key) {
                return Err(DeviceError::Unavailable(
                    "HID previous request still cancelling".into(),
                ));
            }
        }
        let wide = path.encode_utf16().chain(Some(0)).collect::<Vec<_>>();
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                if feature_only {
                    0
                } else {
                    GENERIC_READ | GENERIC_WRITE
                },
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                null(),
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED,
                null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io("HID open", unsafe { GetLastError() }));
        }
        // Arc preserves the receiver lease across a quarantined IRP. Session and
        // QueryGuard are !Send; quarantine reaping only drops on this owning thread.
        #[allow(clippy::arc_with_non_send_sync)]
        let device = Arc::new(Device {
            handle: OwnedHandle(handle),
            key: key.into(),
            owner: std::thread::current().id(),
            _lease: lease,
        });
        Ok(Self { device })
    }
    pub fn feature(
        &self,
        command: Option<u8>,
        deadline: Instant,
        cancel: &Cancellation,
    ) -> Result<Vec<u8>, DeviceError> {
        let mut buffer = vec![0u8; 1001];
        buffer[0] = 0x51;
        if let Some(command) = command {
            buffer[1] = command;
        }
        self.perform(
            buffer,
            Some(if command.is_some() {
                SET_FEATURE
            } else {
                GET_FEATURE
            }),
            deadline,
            cancel,
        )?
        .ok_or_else(|| DeviceError::Unavailable("HID feature request timed out".into()))
    }
    pub fn input(
        &self,
        deadline: Instant,
        cancel: &Cancellation,
    ) -> Result<Option<Vec<u8>>, DeviceError> {
        self.input_sized(33, deadline, cancel)
    }
    pub fn input_sized(
        &self,
        length: usize,
        deadline: Instant,
        cancel: &Cancellation,
    ) -> Result<Option<Vec<u8>>, DeviceError> {
        self.perform(vec![0; checked_length(length)?], None, deadline, cancel)
    }
    pub fn output(
        &self,
        report: &[u8],
        deadline: Instant,
        cancel: &Cancellation,
    ) -> Result<(), DeviceError> {
        checked_length(report.len())?;
        self.perform(report.to_vec(), Some(0), deadline, cancel)?
            .map(|_| ())
            .ok_or_else(|| DeviceError::Unavailable("HID output timed out".into()))
    }
    pub fn set_feature(
        &self,
        report: &[u8],
        deadline: Instant,
        cancel: &Cancellation,
    ) -> Result<(), DeviceError> {
        checked_length(report.len())?;
        self.perform(report.to_vec(), Some(SET_FEATURE), deadline, cancel)?
            .map(|_| ())
            .ok_or_else(|| DeviceError::Unavailable("HID set feature timed out".into()))
    }
    pub fn get_feature(
        &self,
        id: u8,
        length: usize,
        deadline: Instant,
        cancel: &Cancellation,
    ) -> Result<Vec<u8>, DeviceError> {
        let mut buffer = vec![0; checked_length(length)?];
        buffer[0] = id;
        self.perform(buffer, Some(GET_FEATURE), deadline, cancel)?
            .ok_or_else(|| DeviceError::Unavailable("HID get feature timed out".into()))
    }
    fn perform(
        &self,
        buffer: Vec<u8>,
        code: Option<u32>,
        deadline: Instant,
        cancel: &Cancellation,
    ) -> Result<Option<Vec<u8>>, DeviceError> {
        cancel.check()?;
        if Instant::now() >= deadline {
            return Ok(None);
        }
        let reservation = Reservation::acquire(&self.device.key)?;
        let event = unsafe { CreateEventW(null(), 1, 0, null()) };
        if event.is_null() {
            return Err(io("HID event", unsafe { GetLastError() }));
        }
        let mut op = Box::new(Operation {
            device: self.device.clone(),
            event: OwnedHandle(event),
            overlapped: UnsafeCell::new(OVERLAPPED {
                hEvent: event,
                ..Default::default()
            }),
            buffer,
            transferred: 0,
        });
        let ok = unsafe {
            match code {
                // SET_FEATURE receives its report through the input buffer;
                // GET_FEATURE uses the output buffer, initialized with report-ID.
                // https://learn.microsoft.com/windows-hardware/drivers/ddi/hidclass/ni-hidclass-ioctl_hid_set_feature
                Some(0) => WriteFile(
                    op.device.handle.0,
                    op.buffer.as_ptr(),
                    op.buffer.len() as u32,
                    null_mut(),
                    op.overlapped.get(),
                ),
                Some(code) => DeviceIoControl(
                    op.device.handle.0,
                    code,
                    if code == SET_FEATURE {
                        op.buffer.as_mut_ptr().cast()
                    } else {
                        null_mut()
                    },
                    if code == SET_FEATURE {
                        op.buffer.len() as u32
                    } else {
                        0
                    },
                    if code == GET_FEATURE {
                        op.buffer.as_mut_ptr().cast()
                    } else {
                        null_mut()
                    },
                    if code == GET_FEATURE {
                        op.buffer.len() as u32
                    } else {
                        0
                    },
                    null_mut(),
                    op.overlapped.get(),
                ),
                None => ReadFile(
                    op.device.handle.0,
                    op.buffer.as_mut_ptr(),
                    op.buffer.len() as u32,
                    null_mut(),
                    op.overlapped.get(),
                ),
            }
        };
        if ok == 0 {
            let error = unsafe { GetLastError() };
            if error != ERROR_IO_PENDING {
                return Err(io("HID submit", error));
            }
        }
        let outcome = loop {
            match op.complete() {
                Ok(true) => break Ok(true),
                Err(e) => break Err(e),
                Ok(false) => {}
            }
            if let Err(e) = cancel.check() {
                break Err(e);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break Ok(false);
            }
            let wait = unsafe {
                WaitForSingleObject(op.event.0, remaining.as_millis().clamp(1, 20) as u32)
            };
            if wait != WAIT_OBJECT_0 && wait != WAIT_TIMEOUT {
                break Err(io("HID wait", unsafe { GetLastError() }));
            }
        };
        if !matches!(outcome, Ok(true)) {
            unsafe {
                CancelIoEx(op.device.handle.0, op.overlapped.get());
            }
            let grace = Instant::now() + Duration::from_millis(100);
            loop {
                if !matches!(op.complete(), Ok(false)) {
                    break;
                }
                if Instant::now() >= grace {
                    reservation.quarantine(op);
                    return outcome.map(|_| None);
                }
                unsafe {
                    WaitForSingleObject(op.event.0, 5);
                }
            }
            return outcome.map(|_| None);
        }
        if code.is_none() || code == Some(GET_FEATURE) {
            if op.transferred as usize > op.buffer.len() {
                return Err(DeviceError::Protocol(
                    "HID returned impossible length".into(),
                ));
            }
            op.buffer.truncate(op.transferred as usize);
        } else if code == Some(0) && op.transferred as usize != op.buffer.len() {
            return Err(DeviceError::Protocol(
                "HID output was not fully transferred".into(),
            ));
        }
        cancel.check()?;
        Ok(Some(std::mem::take(&mut op.buffer)))
    }
}

fn checked_length(length: usize) -> Result<usize, DeviceError> {
    if (1..=4096).contains(&length) {
        Ok(length)
    } else {
        Err(DeviceError::Protocol(
            "HID report length out of bounds".into(),
        ))
    }
}

// ReleaseMutex is thread-affine. This !Send lease stays on its issuing thread;
// quarantined operations retain it until same-thread reaping or OS process teardown.
pub(super) struct QueryGuard(OwnedHandle, PhantomData<Rc<()>>);
impl QueryGuard {
    pub fn acquire(
        key: &str,
        deadline: Instant,
        cancel: &Cancellation,
    ) -> Result<Self, DeviceError> {
        // Stable FNV key avoids exposing local endpoint strings in a kernel object name.
        let hash = key.bytes().fold(0xcbf29ce484222325u64, |h, b| {
            (h ^ u64::from(b)).wrapping_mul(0x100000001b3)
        });
        let name = format!("Local\\QuietBatteryDarmoshark{hash:016X}")
            .encode_utf16()
            .chain(Some(0))
            .collect::<Vec<_>>();
        let raw = unsafe { CreateMutexW(null(), 0, name.as_ptr()) };
        if raw.is_null() {
            return Err(io("HID mutex", unsafe { GetLastError() }));
        }
        let handle = OwnedHandle(raw);
        loop {
            cancel.check()?;
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(DeviceError::Unavailable("HID receiver busy".into()));
            }
            match unsafe {
                WaitForSingleObject(handle.0, remaining.as_millis().clamp(1, 20) as u32)
            } {
                WAIT_OBJECT_0 | WAIT_ABANDONED => return Ok(Self(handle, PhantomData)),
                WAIT_TIMEOUT => {}
                _ => return Err(io("HID mutex wait", unsafe { GetLastError() })),
            }
        }
    }
}
impl Drop for QueryGuard {
    fn drop(&mut self) {
        unsafe {
            ReleaseMutex(self.0.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::c_void;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CreateNamedPipeW(
            name: *const u16,
            open_mode: u32,
            pipe_mode: u32,
            max_instances: u32,
            out_size: u32,
            in_size: u32,
            timeout: u32,
            security: *const c_void,
        ) -> HANDLE;
    }
    #[test]
    fn pending_native_read_cancels_without_releasing_live_memory() {
        let name = format!(r"\\.\pipe\quiet-battery-hid-cancel-{}", std::process::id());
        let wide = name.encode_utf16().chain(Some(0)).collect::<Vec<_>>();
        let server = unsafe {
            CreateNamedPipeW(
                wide.as_ptr(),
                3 | FILE_FLAG_OVERLAPPED,
                0,
                1,
                64,
                64,
                0,
                null(),
            )
        };
        assert_ne!(server, INVALID_HANDLE_VALUE);
        #[allow(clippy::arc_with_non_send_sync)]
        let device = Arc::new(Device {
            handle: OwnedHandle(server),
            key: name.clone(),
            owner: std::thread::current().id(),
            _lease: QueryGuard::acquire(
                &name,
                Instant::now() + Duration::from_secs(1),
                &Cancellation::default(),
            )
            .unwrap(),
        });
        let raw_client = unsafe {
            CreateFileW(
                wide.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                0,
                null(),
                OPEN_EXISTING,
                0,
                null_mut(),
            )
        };
        assert_ne!(raw_client, INVALID_HANDLE_VALUE);
        let _client = OwnedHandle(raw_client);
        let session = Session { device };
        let cancel = Cancellation::default();
        let began = Instant::now();
        assert!(
            session
                .input(Instant::now() + Duration::from_millis(5), &cancel)
                .unwrap()
                .is_none()
        );
        assert!(began.elapsed() < Duration::from_secs(1));
        let signal = cancel.clone();
        let thread = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(10));
            signal.cancel();
        });
        assert!(matches!(
            session.input(Instant::now() + Duration::from_secs(1), &cancel),
            Err(DeviceError::Cancelled)
        ));
        thread.join().unwrap();
        let cancel = Cancellation::default();
        let frame = [8u8; 17];
        session
            .output(&frame, Instant::now() + Duration::from_secs(1), &cancel)
            .unwrap();
        let mut received = [0u8; 17];
        let mut count = 0;
        assert_ne!(
            unsafe {
                ReadFile(
                    _client.0,
                    received.as_mut_ptr(),
                    received.len() as u32,
                    &mut count,
                    null_mut(),
                )
            },
            0
        );
        assert_eq!((received, count), (frame, 17));
        assert_ne!(
            unsafe {
                WriteFile(
                    _client.0,
                    frame.as_ptr(),
                    frame.len() as u32,
                    &mut count,
                    null_mut(),
                )
            },
            0
        );
        assert_eq!(
            session
                .input_sized(17, Instant::now() + Duration::from_secs(1), &cancel)
                .unwrap()
                .unwrap(),
            frame
        );
        for length in [0, 4097, usize::MAX] {
            assert!(
                session
                    .input_sized(length, Instant::now(), &cancel)
                    .is_err()
            );
            assert!(
                session
                    .get_feature(0, length, Instant::now(), &cancel)
                    .is_err()
            );
        }
        let mut p = lock_pending().unwrap();
        reap(&mut p);
        assert!(!p.operations.iter().any(|op| op.device.key == name));
    }
}
