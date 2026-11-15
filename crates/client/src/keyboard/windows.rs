//! A capture-scoped hook on its own message-loop thread. The callback only
//! checks ownership, updates fixed-size state and tries bounded queue sends.

use super::{
    hook::{Capture, Event, Lease, QUEUE_SIZE},
    Key, STATE,
};
use std::{
    cell::RefCell,
    collections::VecDeque,
    sync::{
        mpsc::{self, Receiver, SyncSender},
        Arc,
    },
    thread::{self, JoinHandle},
    time::Duration,
};
use windows::Win32::{
    Foundation::{HINSTANCE, LPARAM, LRESULT, WPARAM},
    System::{LibraryLoader::GetModuleHandleW, Threading::GetCurrentProcessId},
    UI::{Input::KeyboardAndMouse::*, WindowsAndMessaging::*},
};

struct Worker {
    lease: Arc<Lease>,
    events: Receiver<Event>,
    thread: Option<JoinHandle<()>>,
    forwarded: VecDeque<Key>,
    pending: VecDeque<Event>,
}
impl Drop for Worker {
    fn drop(&mut self) {
        self.lease.fail();
        if let Some(thread) = self.thread.take() {
            thread.thread().unpark();
            let _ = thread.join();
        }
    }
}

struct Hook {
    handle: HHOOK,
    lease: Arc<Lease>,
    events: SyncSender<Event>,
    capture: Capture,
    repaint: egui::Context,
    dirty: bool,
}
impl Drop for Hook {
    fn drop(&mut self) {
        if !self.handle.0.is_null() {
            // SAFETY: the installing thread drops its hook, including on unwind.
            let _ = unsafe { UnhookWindowsHookEx(self.handle) };
        }
    }
}
thread_local! {
    static WORKER: RefCell<Option<Worker>> = const { RefCell::new(None) };
    static HOOK: RefCell<Option<Hook>> = const { RefCell::new(None) };
}

pub fn owns_keys(owned: bool, ctx: &egui::Context) {
    if !owned {
        WORKER.with_borrow_mut(|worker| *worker = None);
        return;
    }
    drain(None);
    WORKER.with_borrow_mut(|worker| {
        if worker
            .as_ref()
            .is_some_and(|w| !w.lease.active(w.lease.elapsed()))
        {
            *worker = None;
        }
        if let Some(worker) = worker {
            worker.lease.renew();
            return;
        }
        let lease = Arc::new(Lease::default());
        let (tx, rx) = mpsc::sync_channel(QUEUE_SIZE);
        let thread_lease = lease.clone();
        let ctx = ctx.clone();
        match thread::Builder::new()
            .name("keyboard-capture".into())
            .spawn(move || run(thread_lease, tx, ctx))
        {
            Ok(thread) => {
                *worker = Some(Worker {
                    lease,
                    events: rx,
                    thread: Some(thread),
                    forwarded: VecDeque::new(),
                    pending: VecDeque::new(),
                })
            }
            Err(error) => tracing::warn!(%error, "cannot start Windows keyboard capture"),
        }
    });
}

// Called before each native event and frame, on the UI thread. Publishing
// through the existing observer keeps Held, early receipts and datagrams shared.
pub fn drain(native: Option<&egui_winit::winit::event::WindowEvent>) {
    use egui_winit::winit::{event::WindowEvent, keyboard::PhysicalKey};
    let native_key = match native {
        Some(WindowEvent::KeyboardInput { event, .. }) => match event.physical_key {
            PhysicalKey::Code(code) => super::vk(code).map(|vk| Key {
                vk,
                down: event.state.is_pressed(),
            }),
            _ => None,
        },
        _ => None,
    };
    let (events, failed) = WORKER.with_borrow_mut(|worker| {
        let Some(worker) = worker else {
            return (Vec::new(), false);
        };
        let failed = !worker.lease.active(worker.lease.elapsed());
        if failed {
            return (Vec::new(), true);
        }
        worker.lease.renew();
        worker
            .pending
            .extend(worker.events.try_iter().take(QUEUE_SIZE));
        let events = super::hook::take_ready(&mut worker.pending, native_key);
        worker.forwarded.extend(
            events
                .iter()
                .filter(|e| matches!(e.key.vk, 0xa0..=0xa5))
                .map(|e| e.key),
        );
        (events, false)
    });
    if failed {
        crate::early::capture_lost();
    } else {
        for event in events {
            publish(event);
        }
    }
}

pub fn take_forwarded(key: Key) -> bool {
    WORKER.with_borrow_mut(|worker| {
        let Some(worker) = worker else { return false };
        if worker.forwarded.front() == Some(&key) {
            worker.forwarded.pop_front();
            true
        } else {
            false
        }
    })
}

fn foreground() -> (bool, HKL) {
    let mut process = 0;
    // SAFETY: reads foreground identity and its keyboard layout.
    unsafe {
        let thread = GetWindowThreadProcessId(GetForegroundWindow(), Some(&mut process));
        (process == GetCurrentProcessId(), GetKeyboardLayout(thread))
    }
}

fn has_altgr(layout: HKL) -> bool {
    let normal = [0; 256];
    let mut altgr = normal;
    altgr[0x11] = 0x80;
    altgr[0x12] = 0x80;
    // Match winit's definition, a printable Ctrl+Alt character differs from
    // the unmodified character. Flag 4 leaves the layout's dead-key state alone.
    (0..256).any(|vk| {
        let mut plain = [0; 8];
        let mut modified = [0; 8];
        // SAFETY: all keyboard-state and output buffers have the API's sizes.
        unsafe {
            let scan = MapVirtualKeyExW(vk, MAPVK_VK_TO_VSC, layout);
            let a = ToUnicodeEx(vk, scan, &normal, &mut plain, 4, layout);
            let b = ToUnicodeEx(vk, scan, &altgr, &mut modified, 4, layout);
            a > 0 && b > 0 && (a != b || plain != modified)
        }
    })
}

fn run(lease: Arc<Lease>, events: SyncSender<Event>, ctx: egui::Context) {
    let (_, mut layout) = foreground();
    let mut state = Hook {
        handle: HHOOK::default(),
        lease: lease.clone(),
        events,
        capture: Capture::new(has_altgr(layout)),
        repaint: ctx,
        dirty: false,
    };
    // SAFETY: callback and module remain valid until the thread unhooks.
    let installed = unsafe {
        GetModuleHandleW(None).and_then(|module| {
            SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook), HINSTANCE(module.0), 0)
        })
    };
    match installed {
        Ok(handle) => state.handle = handle,
        Err(error) => {
            tracing::warn!(%error, "cannot capture Windows system shortcuts");
            lease.fail();
            return;
        }
    }
    HOOK.with_borrow_mut(|slot| *slot = Some(state));
    // PeekMessage dispatches the hook's sent messages on this dedicated thread.
    // The short park also services the lease when there are no key messages.
    while lease.active(lease.elapsed()) {
        let (owned, current_layout) = foreground();
        if !owned {
            lease.fail();
            break;
        }
        if layout != current_layout {
            layout = current_layout;
            let altgr = has_altgr(layout);
            HOOK.with_borrow_mut(|slot| {
                if let Some(h) = slot {
                    h.capture.altgr = altgr;
                }
            });
        }
        let mut msg = MSG::default();
        // SAFETY: this thread owns this message queue.
        while unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() } {
            if msg.message == WM_QUIT {
                lease.fail();
                break;
            }
            unsafe {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        HOOK.with_borrow_mut(|slot| {
            if let Some(h) = slot {
                if std::mem::take(&mut h.dirty) {
                    h.repaint.request_repaint();
                }
            }
        });
        thread::park_timeout(Duration::from_millis(2));
    }
    HOOK.with_borrow_mut(|slot| *slot = None);
}

fn deliver(tx: &SyncSender<Event>, lease: &Lease, event: Event) -> bool {
    if tx.try_send(event).is_ok() {
        true
    } else {
        lease.fail();
        false
    }
}

unsafe extern "system" fn hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // Never unwind through Windows. Hook's thread-local RAII unhooks on worker panic.
    let swallowed = std::panic::catch_unwind(|| {
        if code != HC_ACTION as i32 {
            return false;
        }
        // SAFETY: HC_ACTION supplies this record for WH_KEYBOARD_LL.
        let raw = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
        if raw.flags.contains(LLKHF_INJECTED) {
            return false;
        }
        HOOK.with_borrow_mut(|slot| {
            let Some(h) = slot else { return false };
            if !h.lease.active(h.lease.elapsed()) || !foreground().0 {
                h.lease.fail();
                return false;
            }
            let event = Event {
                key: Key {
                    vk: raw.vkCode as u16,
                    down: !raw.flags.contains(LLKHF_UP),
                },
                alt: raw.flags.contains(LLKHF_ALTDOWN),
                time: raw.time,
                altgr: h.capture.altgr,
            };
            h.capture.consume(event, |e| {
                h.dirty = true;
                deliver(&h.events, &h.lease, e)
            })
        })
    });
    match swallowed {
        Ok(true) => return LRESULT(1),
        Err(_) => HOOK.with_borrow(|slot| {
            if let Some(h) = slot {
                h.lease.fail();
            }
        }),
        Ok(false) => {}
    }
    // SAFETY: all unowned events, failures and orphan ups pass to Windows.
    unsafe { CallNextHookEx(HHOOK::default(), code, wparam, lparam) }
}

fn publish(event: Event) {
    let key = event.key;
    let mods = STATE.with_borrow_mut(|s| {
        if key.down {
            s.pressed.insert(key.vk);
        } else {
            s.pressed.remove(&key.vk);
        }
        s.pending.push((
            egui::Event::Key {
                key: egui::Key::F35,
                physical_key: None,
                pressed: key.down,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            },
            Some(key),
            false,
        ));
        egui::Modifiers {
            shift: s.pressed.contains(&0xa0) || s.pressed.contains(&0xa1),
            ctrl: s.pressed.contains(&0xa2) || s.pressed.contains(&0xa3),
            alt: event.alt,
            ..egui::Modifiers::NONE
        }
    });
    crate::early::physical(key, mods);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[test]
    fn capture_owner_joins_its_worker_on_drop_and_unwind() {
        for panic in [false, true] {
            let stopped = Arc::new(AtomicBool::new(false));
            let result = std::panic::catch_unwind({
                let stopped = stopped.clone();
                move || {
                    let lease = Arc::new(Lease::default());
                    let thread_lease = lease.clone();
                    let (_, events) = mpsc::sync_channel(QUEUE_SIZE);
                    let thread = thread::spawn(move || {
                        while !thread_lease.failed.load(Ordering::Acquire) {
                            thread::park_timeout(Duration::from_millis(2));
                        }
                        stopped.store(true, Ordering::Release);
                    });
                    let _owner = Worker {
                        lease,
                        events,
                        thread: Some(thread),
                        forwarded: VecDeque::new(),
                        pending: VecDeque::new(),
                    };
                    assert!(!panic, "exercise unwinding teardown");
                }
            });
            assert_eq!(result.is_err(), panic);
            assert!(stopped.load(Ordering::Acquire));
        }
    }
}
