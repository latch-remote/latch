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
        Arc,
    },
};

pub const UNAVAILABLE: &str = "Mac host controllers unavailable. Apple requires entitlement com.apple.developer.hid.virtual.device and Apple-authorized signing. Ad-hoc Latch builds cannot create virtual controllers. See docs/MACOS.md.";

pub fn open(feedback: FeedbackFn) -> Result<Opened> {
    // Exercise both layouts and bound the advertised capacity by devices the
    // OS permits concurrently. Every temporary Create reference is released.
    drop(device(0, PadKind::PlayStation, 0)?);
    let slots = probe_slots(|slot| device(slot, PadKind::Xbox, 0))?;
    let mut opened = Pads::opened(Bus, feedback);
    opened.slots = slots;
    Ok(opened)
}

fn probe_slots<T>(mut create: impl FnMut(u8) -> Result<T>) -> Result<u8> {
    let mut devices = vec![create(0)?];
    for slot in 1..latch_proto::gamepad::MAX_PADS as u8 {
        match create(slot) {
            Ok(device) => devices.push(device),
            Err(_) => break,
        }
    }
    Ok(devices.len() as u8)
}

struct Bus;
struct Target {
    device: Option<Owned>,
    _queue: DispatchRetained<DispatchQueue>,
    kind: PadKind,
    state: Arc<Mutex<Vec<u8>>>,
    stopped: Arc<AtomicBool>,
    cancelled: std::sync::mpsc::Receiver<()>,
}

impl Drop for Target {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        if let Some(device) = self.device.take() {
            // SAFETY: cancellation prevents new callbacks. The cancel handler
            // owns the Create reference until all in-flight callbacks finish.
            unsafe { IOHIDUserDeviceCancel(device.0) };
            std::mem::forget(device);
            // Apple's cancellation completion follows every output callback
            // and releases the kernel device. A takeover must wait for it.
            let _ = self.cancelled.recv();
        }
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
    // Distinct identities keep temporary capacity probes separate from pads
    // created later for the session and avoid consumers reusing stale state.
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
        ("Transport", string("USB")),
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
    // No dispatch queue is attached to this probe, so CFRelease destroys it.
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
        const CREATE_ON_ACTIVATE: u32 = 1;
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
            _queue: queue,
            kind,
            state,
            stopped,
            cancelled,
        })
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
    fn capability_probe_counts_concurrent_slots_and_drops_all_devices() {
        use std::sync::atomic::AtomicUsize;
        struct Device(Arc<AtomicUsize>);
        impl Drop for Device {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::SeqCst);
            }
        }
        let live = Arc::new(AtomicUsize::new(0));
        assert_eq!(
            probe_slots(|slot| {
                assert_eq!(live.load(Ordering::SeqCst), slot as usize);
                anyhow::ensure!(slot < 3, "capacity exhausted");
                live.fetch_add(1, Ordering::SeqCst);
                Ok(Device(live.clone()))
            })
            .unwrap(),
            3
        );
        assert_eq!(live.load(Ordering::SeqCst), 0);
        assert!(probe_slots::<Device>(|_| anyhow::bail!(UNAVAILABLE)).is_err());
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
