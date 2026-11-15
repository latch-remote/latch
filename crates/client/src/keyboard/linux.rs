//! X11 capture uses winit's connection, whose XInitThreads initialization
//! permits a watchdog to release its grab. A separate client cannot ungrab
//! another client's keyboard. App teardown joins before the display closes.

use super::hook::Lease;
use egui_winit::winit::keyboard::KeyCode;
use raw_window_handle::{HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle};
use std::{
    cell::{Cell, RefCell},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread::{self, JoinHandle},
    time::Duration,
};
use x11_dl::xlib;
use x11rb::{
    connection::Connection,
    protocol::{xproto, xtest},
    rust_connection::RustConnection,
};

struct Watchdog {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}
impl Drop for Watchdog {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            thread.thread().unpark();
            let _ = thread.join();
        }
    }
}
struct Window {
    // Drop the watchdog before the Xlib bindings or borrowed display.
    _watchdog: Watchdog,
    display: *mut xlib::Display,
    window: xlib::Window,
    calls: Arc<xlib::Xlib>,
    media_connection: RustConnection,
    lease: Arc<Lease>,
    grabbed: Arc<AtomicBool>,
    media: Cell<Option<u8>>,
}
impl Drop for Window {
    fn drop(&mut self) {
        // All UI calls and teardown run on the display's event-loop thread.
        unsafe {
            (self.calls.XUngrabKeyboard)(self.display, xlib::CurrentTime);
            (self.calls.XFlush)(self.display);
        }
    }
}
thread_local! {
    static WINDOW: RefCell<Option<Window>> = const { RefCell::new(None) };
    static REQUESTED: Cell<bool> = const { Cell::new(false) };
    static WARNED: Cell<bool> = const { Cell::new(false) };
}

pub fn install(cc: &eframe::CreationContext<'_>) {
    let (Ok(display), Ok(window)) = (cc.display_handle(), cc.window_handle()) else {
        return;
    };
    if let (RawDisplayHandle::Xlib(display), RawWindowHandle::Xlib(window)) =
        (display.as_raw(), window.as_raw())
    {
        let Some(display) = display.display else {
            return;
        };
        let Ok(calls) = xlib::Xlib::open() else {
            return;
        };
        // Use the workspace's pure-Rust XTest protocol implementation.
        let media_connection = (|| -> anyhow::Result<RustConnection> {
            let name = unsafe {
                std::ffi::CStr::from_ptr((calls.XDisplayString)(display.as_ptr().cast()))
            };
            let (conn, _) = x11rb::connect(Some(name.to_str()?))?;
            xtest::get_version(&conn, 2, 2)?.reply()?;
            Ok(conn)
        })();
        let media_connection = match media_connection {
            Ok(conn) => conn,
            Err(error) => {
                tracing::warn!(%error, "cannot capture X11 keys with local media replay");
                return;
            }
        };
        let calls = Arc::new(calls);
        let lease = Arc::new(Lease::default());
        let grabbed = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let display = display.as_ptr() as usize;
        let Ok(watchdog) =
            start_watchdog(display, calls.clone(), lease.clone(), grabbed.clone(), stop)
        else {
            return;
        };
        WINDOW.replace(Some(Window {
            _watchdog: watchdog,
            display: display as *mut xlib::Display,
            window: window.window,
            calls,
            media_connection,
            lease,
            grabbed,
            media: Cell::new(None),
        }));
    }
}

fn start_watchdog(
    display: usize,
    calls: Arc<xlib::Xlib>,
    lease: Arc<Lease>,
    grabbed: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
) -> std::io::Result<Watchdog> {
    // Copy the immutable display name on the owner thread before starting recovery.
    let display_name =
        unsafe { std::ffi::CStr::from_ptr((calls.XDisplayString)(display as *mut xlib::Display)) }
            .to_string_lossy()
            .into_owned();
    let worker_calls = calls;
    let worker_lease = lease;
    let worker_grabbed = grabbed;
    let worker_stop = stop.clone();
    let worker = thread::Builder::new()
        .name("x11-keyboard-watchdog".into())
        .spawn(move || {
            while !worker_stop.load(Ordering::Acquire) {
                // Leave the display entirely alone while capture is idle.
                if worker_grabbed.load(Ordering::Acquire)
                    && !worker_lease.active(worker_lease.elapsed())
                {
                    let calls = worker_calls.clone();
                    let lease = worker_lease.clone();
                    let grabbed = worker_grabbed.clone();
                    let waiting_for_lock = Arc::new(AtomicBool::new(true));
                    let release_waiting = waiting_for_lock.clone();
                    let (done, completed) = std::sync::mpsc::sync_channel(1);
                    // Xlib has no try-lock API. Supervise the potentially stuck
                    // native call from this watchdog, with a bounded deadline.
                    let release = thread::spawn(move || {
                        let display = display as *mut xlib::Display;
                        // SAFETY: winit initialized Xlib threading. Window joins
                        // this watchdog before eframe destroys its display.
                        unsafe {
                            (calls.XLockDisplay)(display);
                            release_waiting.store(false, Ordering::Release);
                            if !lease.active(lease.elapsed())
                                && grabbed.swap(false, Ordering::AcqRel)
                            {
                                (calls.XUngrabKeyboard)(display, xlib::CurrentTime);
                                (calls.XFlush)(display);
                            }
                            (calls.XUnlockDisplay)(display);
                        }
                        let _ = done.send(());
                    });
                    let mut probe = None;
                    loop {
                        let wait = if probe.is_some() {
                            Duration::from_millis(100)
                        } else {
                            Duration::from_secs(2)
                        };
                        match completed.recv_timeout(wait) {
                            Ok(()) => break,
                            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                                tracing::error!("X11 keyboard grab recovery worker stopped unexpectedly");
                                break;
                            }
                            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                        }
                        if !waiting_for_lock.load(Ordering::Acquire) {
                            continue;
                        }
                        if probe.is_none() {
                            let name = display_name.clone();
                            let (answer, result) = std::sync::mpsc::sync_channel(1);
                            // This connection owns its resources and closes after a
                            // reply. Keep only one probe in flight while a slow or
                            // stopped server cannot even complete X11 setup.
                            thread::spawn(move || {
                                let response = (|| -> anyhow::Result<()> {
                                    let (conn, _) = x11rb::connect(Some(&name))?;
                                    xproto::get_input_focus(&conn)?.reply()?;
                                    Ok(())
                                })();
                                let _ = answer.send(response.is_ok());
                            });
                            probe = Some(result);
                        }
                        match probe.as_ref().unwrap().try_recv() {
                            Ok(responsive) => {
                                probe = None;
                                if responsive && waiting_for_lock.load(Ordering::Acquire) {
                                    tracing::error!("X11 server answers a separate connection while keyboard grab recovery remains stuck on the display lock, aborting to release the keyboard");
                                    std::process::abort();
                                }
                            }
                            Err(std::sync::mpsc::TryRecvError::Disconnected) => probe = None,
                            Err(std::sync::mpsc::TryRecvError::Empty) => {}
                        }
                    }
                    let _ = release.join();
                }
                thread::park_timeout(Duration::from_millis(10));
            }
        });
    let thread = worker?;
    Ok(Watchdog {
        stop,
        thread: Some(thread),
    })
}

pub fn shutdown() {
    WINDOW.with_borrow_mut(|window| *window = None);
}

pub fn owns_keys(owned: bool) {
    WINDOW.with_borrow(|window| {
        let Some(window) = window else { return };
        window.lease.renew();
        let grabbed = window.grabbed.load(Ordering::Acquire);
        if !grabbed && REQUESTED.replace(false) {
            crate::early::capture_lost();
        }
        if owned
            && window.media.get().is_some_and(|key| {
                let mut keys = [0; 32];
                unsafe {
                    (window.calls.XQueryKeymap)(window.display, keys.as_mut_ptr());
                }
                let held = keys[usize::from(key / 8)] & (1 << (key % 8)) != 0;
                if !held {
                    window.media.set(None);
                }
                held
            })
        {
            return;
        }
        if grabbed == owned {
            return;
        }
        // SAFETY: live Xlib display, initialized for threading by winit.
        unsafe {
            (window.calls.XLockDisplay)(window.display);
            if owned {
                let status = (window.calls.XGrabKeyboard)(
                    window.display,
                    window.window,
                    1,
                    xlib::GrabModeAsync,
                    xlib::GrabModeAsync,
                    xlib::CurrentTime,
                );
                window.grabbed.store(status == 0, Ordering::Release);
                if record_grab(status) {
                    tracing::warn!(status, "cannot capture X11 global shortcuts");
                }
            } else {
                window.grabbed.store(false, Ordering::Release);
                REQUESTED.set(false);
                WARNED.set(false);
                (window.calls.XUngrabKeyboard)(window.display, xlib::CurrentTime);
            }
            (window.calls.XFlush)(window.display);
            (window.calls.XUnlockDisplay)(window.display);
        }
    });
}

/// Return a grabbed media press to local desktop bindings. Release the server's
/// pressed state before replaying its down, then let the real up arrive locally.
/// Wait for release before attempting another keyboard grab.
pub fn local_media(code: KeyCode, down: bool) -> bool {
    WINDOW.with_borrow(|window| {
        let Some(window) = window else { return false };
        let Some(symbol) = media_symbol(code) else {
            return false;
        };
        if !down || !window.grabbed.swap(false, Ordering::AcqRel) {
            return false;
        }
        unsafe {
            (window.calls.XLockDisplay)(window.display);
            let key = (window.calls.XKeysymToKeycode)(window.display, symbol);
            (window.calls.XUngrabKeyboard)(window.display, xlib::CurrentTime);
            if key != 0 {
                window.media.set(Some(key));
                // Complete ungrab on its owning connection before replaying on
                // the protocol connection, so local passive grabs can activate.
                (window.calls.XSync)(window.display, 0);
                if let Err(error) = replay_media(&window.media_connection, key) {
                    tracing::warn!(%error, "cannot replay local X11 media key");
                }
            }
            (window.calls.XFlush)(window.display);
            (window.calls.XUnlockDisplay)(window.display);
        }
        REQUESTED.set(false);
        true
    })
}

fn replay_media(conn: &RustConnection, key: u8) -> anyhow::Result<()> {
    for kind in [xproto::KEY_RELEASE_EVENT, xproto::KEY_PRESS_EVENT] {
        xtest::fake_input(conn, kind, key, x11rb::CURRENT_TIME, x11rb::NONE, 0, 0, 0)?.check()?;
    }
    conn.flush()?;
    Ok(())
}

fn media_symbol(code: KeyCode) -> Option<xlib::KeySym> {
    Some(match code {
        KeyCode::AudioVolumeMute => 0x1008ff12,
        KeyCode::AudioVolumeDown => 0x1008ff11,
        KeyCode::AudioVolumeUp => 0x1008ff13,
        KeyCode::MediaTrackNext => 0x1008ff17,
        KeyCode::MediaTrackPrevious => 0x1008ff16,
        KeyCode::MediaStop => 0x1008ff15,
        KeyCode::MediaPlayPause => 0x1008ff14,
        _ => return None,
    })
}

fn record_grab(status: i32) -> bool {
    REQUESTED.set(status == 0);
    status != 0 && !WARNED.replace(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[ignore = "requires an isolated Xvfb server, changes its focus and grabs"]
    fn watchdog_and_media_replay_restore_local_x11_delivery() {
        // A subprocess can wedge Xlib and verify the last-resort abort without
        // terminating the test runner. It uses this isolated server only.
        if let Some(mode) = std::env::var_os("LATCH_TEST_STUCK_X11") {
            let slow_server = mode == "slow-server";
            let _ = tracing_subscriber::fmt().with_test_writer().try_init();
            let calls = Arc::new(xlib::Xlib::open().unwrap());
            unsafe {
                (calls.XInitThreads)();
                let display = (calls.XOpenDisplay)(std::ptr::null());
                assert!(!display.is_null());
                let root = (calls.XDefaultRootWindow)(display);
                assert_eq!(
                    (calls.XGrabKeyboard)(
                        display,
                        root,
                        0,
                        xlib::GrabModeAsync,
                        xlib::GrabModeAsync,
                        xlib::CurrentTime
                    ),
                    xlib::GrabSuccess
                );
                (calls.XSync)(display, 0);
                let daemon = if slow_server {
                    (calls.XOpenDisplay)(std::ptr::null())
                } else {
                    std::ptr::null_mut()
                };
                if slow_server {
                    assert!(!daemon.is_null());
                    (calls.XGrabServer)(daemon);
                    (calls.XSync)(daemon, 0);
                }
                (calls.XLockDisplay)(display);
                let watchdog = start_watchdog(
                    display as usize,
                    calls.clone(),
                    Arc::new(Lease::default()),
                    Arc::new(AtomicBool::new(true)),
                    Arc::new(AtomicBool::new(false)),
                )
                .unwrap();
                let (_tx, rx) = std::sync::mpsc::channel::<()>();
                if slow_server {
                    let _ = rx.recv_timeout(Duration::from_secs(3));
                    // A server-side stall has outlasted the abort deadline.
                    // Release the lock and server, then wait for real recovery.
                    (calls.XUnlockDisplay)(display);
                    (calls.XUngrabServer)(daemon);
                    (calls.XSync)(daemon, 0);
                    let until = std::time::Instant::now() + Duration::from_secs(1);
                    let mut recovered = false;
                    while !watchdog.thread.as_ref().unwrap().is_finished()
                        && std::time::Instant::now() < until
                    {
                        // Drop signals stop once the native release has completed.
                        if (calls.XGrabKeyboard)(
                            daemon,
                            root,
                            0,
                            xlib::GrabModeAsync,
                            xlib::GrabModeAsync,
                            xlib::CurrentTime,
                        ) == xlib::GrabSuccess
                        {
                            (calls.XUngrabKeyboard)(daemon, xlib::CurrentTime);
                            recovered = true;
                            break;
                        }
                        thread::sleep(Duration::from_millis(10));
                    }
                    assert!(recovered, "capture releases after the slow server recovers");
                    drop(watchdog);
                    (calls.XCloseDisplay)(daemon);
                    (calls.XCloseDisplay)(display);
                    std::process::exit(0);
                }
                let _ = rx.recv_timeout(Duration::from_secs(4));
                eprintln!("a stuck display lock did not abort grab recovery");
                std::process::exit(1);
            }
        }
        use std::os::unix::process::ExitStatusExt;
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "keyboard::linux::tests::watchdog_and_media_replay_restore_local_x11_delivery",
                "--ignored",
                "--nocapture",
            ])
            .env("LATCH_TEST_STUCK_X11", "1")
            .output()
            .unwrap();
        assert_eq!(child.status.signal(), Some(6), "{:?}", child);
        assert!(
            String::from_utf8_lossy(&child.stdout).contains("aborting to release the keyboard"),
            "{:?}",
            child
        );
        let slow = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "keyboard::linux::tests::watchdog_and_media_replay_restore_local_x11_delivery",
                "--ignored",
                "--nocapture",
            ])
            .env("LATCH_TEST_STUCK_X11", "slow-server")
            .output()
            .unwrap();
        assert!(
            slow.status.success(),
            "a slow server must recover without aborting: {slow:?}"
        );
        let calls = Arc::new(xlib::Xlib::open().unwrap());
        let (media_connection, _) = x11rb::connect(None).unwrap();
        // SAFETY: this isolated test initializes threading before opening either display.
        unsafe {
            (calls.XInitThreads)();
            let display = (calls.XOpenDisplay)(std::ptr::null());
            let daemon = (calls.XOpenDisplay)(std::ptr::null());
            assert!(
                !display.is_null() && !daemon.is_null(),
                "start an isolated Xvfb first"
            );
            let root = (calls.XDefaultRootWindow)(display);
            let window = (calls.XCreateSimpleWindow)(display, root, 0, 0, 100, 100, 0, 0, 0);
            (calls.XSelectInput)(display, window, xlib::KeyPressMask | xlib::KeyReleaseMask);
            (calls.XMapWindow)(display, window);
            (calls.XSetInputFocus)(display, window, xlib::RevertToParent, xlib::CurrentTime);
            (calls.XSync)(display, 0);
            let lease = Arc::new(Lease::default());
            let grabbed = Arc::new(AtomicBool::new(false));
            let watchdog = start_watchdog(
                display as usize,
                calls.clone(),
                lease.clone(),
                grabbed.clone(),
                Arc::new(AtomicBool::new(false)),
            )
            .unwrap();
            WINDOW.replace(Some(Window {
                _watchdog: watchdog,
                display,
                window,
                calls: calls.clone(),
                media_connection,
                lease,
                grabbed: grabbed.clone(),
                media: Cell::new(None),
            }));
            let heartbeat_started = std::time::Instant::now();
            for _ in 0..12 {
                owns_keys(true);
                thread::sleep(Duration::from_millis(50));
                assert!(
                    grabbed.load(Ordering::Acquire),
                    "real owns_keys heartbeats keep capture alive without video"
                );
            }
            assert!(
                heartbeat_started.elapsed() > Duration::from_millis(super::super::hook::LEASE_MS)
            );
            for code in [
                KeyCode::AudioVolumeMute,
                KeyCode::AudioVolumeDown,
                KeyCode::AudioVolumeUp,
                KeyCode::MediaTrackNext,
                KeyCode::MediaTrackPrevious,
                KeyCode::MediaStop,
                KeyCode::MediaPlayPause,
            ] {
                let key = (calls.XKeysymToKeycode)(display, media_symbol(code).unwrap());
                assert_ne!(key, 0);
                // A separate client's root passive grab stands in for a media daemon.
                (calls.XGrabKey)(
                    daemon,
                    i32::from(key),
                    xlib::AnyModifier,
                    root,
                    0,
                    xlib::GrabModeAsync,
                    xlib::GrabModeAsync,
                );
                (calls.XSync)(daemon, 0);
                owns_keys(true);
                assert!(grabbed.load(Ordering::Acquire));
                WINDOW.with_borrow(|w| {
                    xtest::fake_input(
                        &w.as_ref().unwrap().media_connection,
                        xproto::KEY_PRESS_EVENT,
                        key,
                        0,
                        0,
                        0,
                        0,
                        0,
                    )
                    .unwrap()
                    .check()
                    .unwrap();
                });
                (calls.XSync)(display, 0);
                assert!(local_media(code, true));
                assert!(!grabbed.load(Ordering::Acquire));
                assert!(!local_media(code, true), "replayed presses stay local");
                let until = std::time::Instant::now() + Duration::from_secs(1);
                let mut local_events = Vec::new();
                while std::time::Instant::now() < until && local_events.is_empty() {
                    while (calls.XPending)(daemon) > 0 {
                        let mut event: xlib::XEvent = std::mem::zeroed();
                        (calls.XNextEvent)(daemon, &mut event);
                        local_events.push((event.get_type(), event.key.keycode));
                    }
                    thread::sleep(Duration::from_millis(2));
                }
                assert_eq!(
                    local_events,
                    [(xlib::KeyPress, u32::from(key))],
                    "{code:?} must reach the local daemon exactly once"
                );
                owns_keys(true);
                assert!(
                    !grabbed.load(Ordering::Acquire),
                    "wait for the physical release"
                );
                WINDOW.with_borrow(|w| {
                    xtest::fake_input(
                        &w.as_ref().unwrap().media_connection,
                        xproto::KEY_RELEASE_EVENT,
                        key,
                        0,
                        0,
                        0,
                        0,
                        0,
                    )
                    .unwrap()
                    .check()
                    .unwrap();
                });
                (calls.XSync)(display, 0);
                (calls.XSync)(daemon, 0);
                while (calls.XPending)(daemon) > 0 {
                    let mut event: xlib::XEvent = std::mem::zeroed();
                    (calls.XNextEvent)(daemon, &mut event);
                    local_events.push((event.get_type(), event.key.keycode));
                }
                assert_eq!(
                    local_events,
                    [
                        (xlib::KeyPress, u32::from(key)),
                        (xlib::KeyRelease, u32::from(key))
                    ],
                    "{code:?} local replay must have one down and one up"
                );
                (calls.XUngrabKey)(daemon, i32::from(key), xlib::AnyModifier, root);
                (calls.XSync)(daemon, 0);
            }
            owns_keys(true);
            assert!(grabbed.load(Ordering::Acquire));
            let until = std::time::Instant::now() + Duration::from_secs(1);
            while grabbed.load(Ordering::Acquire) && std::time::Instant::now() < until {
                thread::sleep(Duration::from_millis(10));
            }
            assert!(
                !grabbed.load(Ordering::Acquire),
                "250 ms event-loop lease expires"
            );
            (calls.XSync)(display, 0);
            assert_eq!(
                (calls.XGrabKeyboard)(
                    daemon,
                    root,
                    0,
                    xlib::GrabModeAsync,
                    xlib::GrabModeAsync,
                    xlib::CurrentTime
                ),
                xlib::GrabSuccess
            );
            (calls.XUngrabKeyboard)(daemon, xlib::CurrentTime);
            shutdown();
            (calls.XDestroyWindow)(display, window);
            (calls.XCloseDisplay)(daemon);
            (calls.XCloseDisplay)(display);
        }
    }

    #[test]
    fn a_failed_grab_remains_retryable_and_logs_once() {
        REQUESTED.set(false);
        WARNED.set(false);
        assert!(record_grab(1));
        assert!(!REQUESTED.get());
        assert!(!record_grab(1));
        assert!(!REQUESTED.get());
        assert!(!record_grab(0));
        assert!(REQUESTED.get());
        REQUESTED.set(false);
        WARNED.set(false);
    }
    #[test]
    fn local_media_covers_the_original_local_controls() {
        for code in [
            KeyCode::AudioVolumeMute,
            KeyCode::AudioVolumeDown,
            KeyCode::AudioVolumeUp,
            KeyCode::MediaTrackNext,
            KeyCode::MediaTrackPrevious,
            KeyCode::MediaStop,
            KeyCode::MediaPlayPause,
        ] {
            assert!(media_symbol(code).is_some());
            assert!(!local_media(code, true), "no native grab in unit tests");
        }
    }
}
