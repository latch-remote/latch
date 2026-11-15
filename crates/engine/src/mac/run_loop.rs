//! The background host needs a main run loop for AppKit cursor queries even
//! though its HTTP service blocks. UI hosts already pump it through AppKit.

use super::ffi::Ref;
use anyhow::Result;
use std::{sync::mpsc, time::Duration};

pub fn with_main_queue<T: Send + 'static>(
    work: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    anyhow::ensure!(
        objc2::MainThreadMarker::new().is_some(),
        "macOS main queue must run on the main thread"
    );
    let (tx, rx) = mpsc::sync_channel(1);
    let worker = std::thread::Builder::new()
        .name("host-service".into())
        .spawn(move || {
            // Transfer panics as well, so a failed worker cannot leave the run loop alive.
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(work));
            let _ = tx.send(result);
        })?;
    let result = loop {
        match rx.try_recv() {
            Ok(result) => break result,
            Err(mpsc::TryRecvError::Disconnected) => {
                anyhow::bail!("macOS service worker disconnected")
            }
            Err(mpsc::TryRecvError::Empty) => {}
        }
        // SAFETY: this function owns the process main thread. It only services
        // existing run-loop sources and never requests privacy authorization.
        let status = unsafe { CFRunLoopRunInMode(kCFRunLoopDefaultMode, 0.05, true) };
        if status == 1 {
            std::thread::sleep(Duration::from_millis(5));
        }
    };
    let _ = worker.join();
    match result {
        Ok(result) => result,
        Err(panic) => std::panic::resume_unwind(panic),
    }
}
#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    static kCFRunLoopDefaultMode: Ref;
    fn CFRunLoopRunInMode(mode: Ref, seconds: f64, return_after_source: bool) -> i32;
}
