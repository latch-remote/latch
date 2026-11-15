//! Entitlement-gated user-space HID pads. No driver or privilege escalation.
//! Creation is probed before advertising slots. See docs/MACOS.md for signing.

mod reports;

use super::ffi::{self, Owned, Ref};
use crate::gamepad::{Feedback, FeedbackFn, Opened, PadKind, PadState, Pads, VirtualBus};
use anyhow::{ensure, Result};
use block2::{Block, RcBlock};
use dispatch2::{DispatchQueue, DispatchRetained};
use parking_lot::Mutex;
use std::{
    ffi::c_void,
    ptr,
    sync::{
        atomic::{AtomicBool, AtomicU32, Ordering},
        Arc, OnceLock,
    },
    time::{Duration, Instant},
};

pub const UNAVAILABLE: &str = "Mac host controllers unavailable. Apple requires entitlement com.apple.developer.hid.virtual.device and Apple-authorized signing. Ad-hoc Latch builds cannot create virtual controllers. See docs/MACOS.md.";

const CREATE_ON_ACTIVATE: u32 = 1 << 0;
static CAPABILITY: OnceLock<std::result::Result<(), String>> = OnceLock::new();

pub fn open(feedback: FeedbackFn) -> Result<Opened> {
    capability(&CAPABILITY, || {
        // CreateOnActivate may defer the kernel's entitlement check. Check the
        // effective signing entitlement first, then create one inactive object.
        ensure!(has_entitlement(), UNAVAILABLE);
        probe_inactive(|options| device(0, PadKind::Xbox, options))
    })?;
    // Pads::opened allocates slots only. Actual devices are published by plug.
    Ok(Pads::opened(Bus, feedback))
}

fn capability(
    cached: &OnceLock<std::result::Result<(), String>>,
    probe: impl FnOnce() -> Result<()>,
) -> Result<()> {
    cached
        .get_or_init(|| probe().map_err(|error| error.to_string()))
        .clone()
        .map_err(anyhow::Error::msg)
}

fn probe_inactive<T>(create: impl FnOnce(u32) -> Result<T>) -> Result<()> {
    drop(create(CREATE_ON_ACTIVATE)?);
    Ok(())
}

fn has_entitlement() -> bool {
    // SAFETY: Security returns owned CF references. Only Boolean true grants
    // capability, including builds whose profile strips an unauthorized key.
    unsafe {
        let Ok(task) = Owned::new(SecTaskCreateFromSelf(ptr::null())) else {
            return false;
        };
        let key = string("com.apple.developer.hid.virtual.device");
        let Ok(value) = Owned::new(SecTaskCopyValueForEntitlement(
            task.0,
            key.0,
            ptr::null_mut(),
        )) else {
            return false;
        };
        CFGetTypeID(value.0) == CFBooleanGetTypeID() && CFBooleanGetValue(value.0)
    }
}

const CANCEL_TIMEOUT: Duration = Duration::from_secs(2);

fn wait_for_cancel(receiver: &std::sync::mpsc::Receiver<()>, deadline: Instant) -> bool {
    receiver
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .is_ok()
}

fn cancel_all<T>(
    pads: &mut [T],
    timeout: Duration,
    mut start: impl FnMut(&mut T),
    mut finish: impl FnMut(&mut T, Instant),
) {
    let deadline = Instant::now() + timeout;
    for pad in pads.iter_mut() {
        start(pad);
    }
    for pad in pads {
        finish(pad, deadline);
    }
}

struct Bus;
struct Target {
    device: Option<Owned>,
    queue: Option<DispatchRetained<DispatchQueue>>,
    kind: PadKind,
    state: Arc<Mutex<Vec<u8>>>,
    stopped: Arc<AtomicBool>,
    cancelled: std::sync::mpsc::Receiver<()>,
}

impl Target {
    fn cancel(&mut self) {
        self.stopped.store(true, Ordering::Release);
        if let Some(device) = self.device.take() {
            // SAFETY: the cancel handler owns the Create reference until all
            // in-flight callbacks finish, including after our deadline expires.
            unsafe { IOHIDUserDeviceCancel(device.0) };
            std::mem::forget(device);
        }
    }
    fn finish_cancel(&mut self, deadline: Instant) {
        if let Some(queue) = self.queue.take() {
            if !wait_for_cancel(&self.cancelled, deadline) {
                tracing::warn!("virtual controller cancellation timed out or disconnected; retaining device and queue to avoid blocking teardown");
                std::mem::forget(queue);
            }
        }
    }
}
impl Drop for Target {
    fn drop(&mut self) {
        self.cancel();
        self.finish_cancel(Instant::now() + CANCEL_TIMEOUT);
    }
}

fn string(s: &str) -> Owned {
    // SAFETY: CFString copies the UTF-8 bytes.
    Owned::new(unsafe {
        CFStringCreateWithBytes(ptr::null(), s.as_ptr(), s.len() as isize, 0x08000100, false)
    })
    .expect("CFString allocation")
}

fn device(slot: u8, kind: PadKind, options: u32) -> Result<Owned> {
    // Distinct identities avoid consumers reusing a previous pad's state.
    static NEXT_DEVICE: AtomicU32 = AtomicU32::new(0x4c410000);
    let identity = NEXT_DEVICE.fetch_add(1, Ordering::Relaxed);
    let descriptor = reports::descriptor(kind);
    // SAFETY: CFData copies the descriptor before returning.
    let data = Owned::new(unsafe {
        CFDataCreate(ptr::null(), descriptor.as_ptr(), descriptor.len() as isize)
    })?;
    let (vendor, product) = reports::identity(kind);
    let values = [
        ("ReportDescriptor", data),
        ("VendorID", ffi::number(vendor)),
        ("ProductID", ffi::number(product)),
        ("Product", string("Latch remote controller")),
        ("Transport", string(reports::transport(kind))),
        (
            "SerialNumber",
            string(&format!("latch-{}-{identity}-{slot}", std::process::id())),
        ),
        ("LocationID", ffi::number(i64::from(identity))),
    ];
    let keys: Vec<_> = values.iter().map(|(key, _)| string(key)).collect();
    let entries: Vec<_> = keys
        .iter()
        .zip(&values)
        .map(|(key, (_, value))| (key.0, value.0))
        .collect();
    let properties = ffi::dictionary(&entries);
    // SAFETY: the immutable dictionary contains a valid HID descriptor.
    // Callers use CreateOnActivate, so creation never publishes a device.
    // The probe releases its inactive object. plug installs callbacks first.
    let raw = unsafe { IOHIDUserDeviceCreateWithProperties(ptr::null(), properties.0, options) };
    ensure!(!raw.is_null(), UNAVAILABLE);
    Owned::new(raw)
}

impl VirtualBus for Bus {
    type Pad = Target;
    const SLOTS: u8 = latch_proto::gamepad::MAX_PADS as u8;

    fn kinds(&self) -> &'static [PadKind] {
        &[PadKind::Xbox, PadKind::PlayStation]
    }

    fn plug(&mut self, slot: u8, kind: PadKind, feedback: FeedbackFn) -> Result<Target> {
        // Defer kernel publication until get/set handlers are registered.
        let device = device(slot, kind, CREATE_ON_ACTIVATE)?;
        let queue = DispatchQueue::new("app.latch.virtual-pad", None);
        let state = Arc::new(Mutex::new(reports::input(kind, &PadState::default())));
        let current = state.clone();
        let get = RcBlock::new(
            move |report_type: u32, id: u32, out: *mut u8, len: *mut isize| -> i32 {
                if report_type != 0 || id != 1 || out.is_null() || len.is_null() {
                    return 0xe00002c7u32 as i32; // kIOReturnUnsupported
                }
                let report = current.lock();
                // SAFETY: IOKit provides an output buffer of *len bytes.
                unsafe {
                    if *len < report.len() as isize {
                        return 0xe00002e8u32 as i32;
                    }
                    ptr::copy_nonoverlapping(report.as_ptr(), out, report.len());
                    *len = report.len() as isize;
                }
                0
            },
        );
        let stopped = Arc::new(AtomicBool::new(false));
        let stop = stopped.clone();
        let latest = Feedback::default();
        // Each pad's serial queue owns its output state. IOHID copies blocks.
        let latest = Arc::new(Mutex::new(latest));
        let set = RcBlock::new(
            move |report_type: u32, id: u32, bytes: *const u8, len: isize| -> i32 {
                if report_type != 1 || bytes.is_null() || !(0..=4096).contains(&len) {
                    return 0xe00002c7u32 as i32;
                }
                // SAFETY: the borrowed report is valid for this callback only.
                let bytes = unsafe { std::slice::from_raw_parts(bytes, len as usize) };
                let mut latest = latest.lock();
                if let Some(value) = reports::feedback(kind, id, bytes, *latest) {
                    *latest = value;
                    if !stop.load(Ordering::Acquire) {
                        feedback(slot, value);
                    }
                    0
                } else {
                    0xe00002c7u32 as i32
                }
            },
        );
        // Transfer the Create reference to cancellation, exactly once. Capturing
        // an integer avoids lending a Rust owner to an asynchronous callback.
        let raw = device.0 as usize;
        let (done, cancelled) = std::sync::mpsc::sync_channel(1);
        let cancel = RcBlock::new(move || {
            // SAFETY: this is the one Create reference transferred to cancellation.
            unsafe { ffi::CFRelease(raw as Ref) };
            let _ = done.send(());
        });
        unsafe {
            IOHIDUserDeviceRegisterGetReportBlock(device.0, &get);
            IOHIDUserDeviceRegisterSetReportBlock(device.0, &set);
            IOHIDUserDeviceSetCancelHandler(device.0, &cancel);
            IOHIDUserDeviceSetDispatchQueue(device.0, (&*queue as *const DispatchQueue).cast());
            IOHIDUserDeviceActivate(device.0);
        }
        Ok(Target {
            device: Some(device),
            queue: Some(queue),
            kind,
            state,
            stopped,
            cancelled,
        })
    }

    fn unplug_all(&mut self, mut pads: Vec<Target>) {
        cancel_all(
            &mut pads,
            CANCEL_TIMEOUT,
            Target::cancel,
            Target::finish_cancel,
        );
    }

    fn submit(&mut self, pad: &mut Target, state: &PadState) -> Result<()> {
        let report = reports::input(pad.kind, state);
        *pad.state.lock() = report.clone();
        // SAFETY: IOKit consumes the report synchronously while the pad is live.
        ffi::check(
            unsafe {
                IOHIDUserDeviceHandleReportWithTimeStamp(
                    pad.device.as_ref().unwrap().0,
                    mach_absolute_time(),
                    report.as_ptr(),
                    report.len() as isize,
                )
            },
            "submit virtual controller report",
        )
    }
}

#[link(name = "Security", kind = "framework")]
extern "C" {
    fn SecTaskCreateFromSelf(allocator: Ref) -> Ref;
    fn SecTaskCopyValueForEntitlement(task: Ref, key: Ref, error: *mut Ref) -> Ref;
}

extern "C" {
    fn mach_absolute_time() -> u64;
}

#[link(name = "IOKit", kind = "framework")]
extern "C" {
    fn IOHIDUserDeviceCreateWithProperties(allocator: Ref, properties: Ref, options: u32) -> Ref;
    fn IOHIDUserDeviceHandleReportWithTimeStamp(
        device: Ref,
        time: u64,
        report: *const u8,
        len: isize,
    ) -> i32;
    fn IOHIDUserDeviceSetDispatchQueue(device: Ref, queue: *const c_void);
    fn IOHIDUserDeviceRegisterSetReportBlock(
        device: Ref,
        block: &Block<dyn Fn(u32, u32, *const u8, isize) -> i32>,
    );
    fn IOHIDUserDeviceRegisterGetReportBlock(
        device: Ref,
        block: &Block<dyn Fn(u32, u32, *mut u8, *mut isize) -> i32>,
    );
    fn IOHIDUserDeviceSetCancelHandler(device: Ref, block: &Block<dyn Fn()>);
    fn IOHIDUserDeviceActivate(device: Ref);
    fn IOHIDUserDeviceCancel(device: Ref);
}
#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFGetTypeID(value: Ref) -> usize;
    fn CFBooleanGetTypeID() -> usize;
    fn CFBooleanGetValue(value: Ref) -> bool;
    fn CFStringCreateWithBytes(
        allocator: Ref,
        bytes: *const u8,
        len: isize,
        encoding: u32,
        external: bool,
    ) -> Ref;
    fn CFDataCreate(allocator: Ref, bytes: *const u8, len: isize) -> Ref;
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn probe_creates_one_inactive_object_and_releases_it() {
        struct Inactive<'a>(&'a std::cell::Cell<usize>);
        impl Drop for Inactive<'_> {
            fn drop(&mut self) {
                self.0.set(self.0.get() + 1);
            }
        }
        let created = std::cell::Cell::new(0);
        let released = std::cell::Cell::new(0);
        probe_inactive(|options| {
            assert_eq!(options, 1 << 0); // IOHIDUserDeviceOptionsCreateOnActivate
            created.set(created.get() + 1);
            Ok(Inactive(&released))
        })
        .unwrap();
        assert_eq!((created.get(), released.get()), (1, 1));
    }
    #[test]
    fn capability_is_probed_once_including_denial() {
        for available in [true, false] {
            let cached = OnceLock::new();
            let probes = std::cell::Cell::new(0);
            for _ in 0..5 {
                let result = capability(&cached, || {
                    probes.set(probes.get() + 1);
                    ensure!(available, UNAVAILABLE);
                    Ok(())
                });
                assert_eq!(result.is_ok(), available);
            }
            assert_eq!(probes.get(), 1);
        }
    }
    #[test]
    fn cancellation_wait_is_bounded_and_accepts_late_completion() {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        assert!(!wait_for_cancel(
            &rx,
            Instant::now() + Duration::from_millis(5)
        ));
        tx.send(()).unwrap();
        assert!(wait_for_cancel(
            &rx,
            Instant::now() + Duration::from_millis(5)
        ));
        drop(tx);
        assert!(!wait_for_cancel(
            &rx,
            Instant::now() + Duration::from_millis(5)
        ));
    }
    #[test]
    fn all_sixteen_stuck_controllers_share_one_teardown_deadline() {
        let channels: Vec<_> = (0..latch_proto::gamepad::MAX_PADS)
            .map(|_| std::sync::mpsc::sync_channel::<()>(1))
            .collect();
        let mut pads: Vec<_> = channels.iter().map(|(_, rx)| rx).collect();
        let starts = std::cell::Cell::new(0);
        let mut deadlines = Vec::new();
        let began = Instant::now();
        let cancel_started = std::cell::Cell::new(None);
        cancel_all(
            &mut pads,
            Duration::from_millis(5),
            |_| {
                cancel_started.set(Some(Instant::now()));
                starts.set(starts.get() + 1);
            },
            |rx, deadline| {
                assert_eq!(starts.get(), latch_proto::gamepad::MAX_PADS);
                deadlines.push(deadline);
                assert!(!wait_for_cancel(rx, deadline));
            },
        );
        assert_eq!(deadlines.len(), 16);
        assert!(deadlines.iter().all(|deadline| *deadline == deadlines[0]));
        assert!(
            deadlines[0].saturating_duration_since(cancel_started.get().unwrap())
                <= Duration::from_millis(5)
        );
        // A late callback still owns and releases its device, and sending to a
        // dropped receiver is harmless. Successful cancellations need no wait.
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        tx.send(()).unwrap();
        assert!(wait_for_cancel(&rx, began));
        drop(rx);
        assert!(tx.send(()).is_err());
    }
    #[test]
    fn native_creation_is_capability_gated_and_releases_every_pad() {
        match open(Arc::new(|_, _| {})) {
            Ok(mut pads) => {
                for kind in [PadKind::Xbox, PadKind::PlayStation, PadKind::Generic] {
                    pads.sink
                        .update(0, Some(kind), &PadState::default())
                        .unwrap();
                    pads.sink.update(0, None, &PadState::default()).unwrap();
                }
                pads.sink.unplug_all();
                eprintln!(
                    "IOHIDUserDevice capability available, reports submitted and devices released"
                );
            }
            Err(error) => {
                assert_eq!(error.to_string(), UNAVAILABLE);
                eprintln!("{error}");
            }
        }
    }
}
