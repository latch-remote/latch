//! Grab on winit's own X connection so its ordinary native key events keep
//! flowing while window-manager shortcuts are inhibited. Wayland exposes
//! no shortcut-inhibit API in this version of winit.

use raw_window_handle::{HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle};
use std::cell::{Cell, RefCell};
use x11_dl::xlib;

struct Window {
    display: *mut xlib::Display,
    window: xlib::Window,
    calls: xlib::Xlib,
}

thread_local! {
    // Borrowed from eframe on the event-loop thread. The X server releases
    // a grab automatically when this window is destroyed.
    static WINDOW: RefCell<Option<Window>> = const { RefCell::new(None) };
    static REQUESTED: Cell<bool> = const { Cell::new(false) };
}

pub fn install(cc: &eframe::CreationContext<'_>) {
    let (Ok(display), Ok(window)) = (cc.display_handle(), cc.window_handle()) else {
        return;
    };
    if let (RawDisplayHandle::Xlib(display), RawWindowHandle::Xlib(window)) =
        (display.as_raw(), window.as_raw())
    {
        if let Some(display) = display.display {
            let calls = match xlib::Xlib::open() {
                Ok(calls) => calls,
                Err(error) => {
                    tracing::warn!(%error, "cannot load X11 keyboard capture");
                    return;
                }
            };
            WINDOW.replace(Some(Window {
                display: display.as_ptr().cast(),
                window: window.window,
                calls,
            }));
        }
    }
}

pub fn owns_keys(owned: bool) {
    WINDOW.with_borrow(|window| {
        let Some(window) = window else { return };
        if REQUESTED.replace(owned) == owned {
            return;
        }
        // SAFETY: the Xlib display and window belong to the live eframe window.
        // Calls run only on its event-loop thread. GrabModeAsync leaves event
        // processing running, owner_events keeps delivery on winit's connection.
        unsafe {
            if owned {
                let status = (window.calls.XGrabKeyboard)(
                    window.display,
                    window.window,
                    1,
                    xlib::GrabModeAsync,
                    xlib::GrabModeAsync,
                    xlib::CurrentTime,
                );
                if status != 0 {
                    tracing::warn!(status, "cannot capture X11 global shortcuts");
                }
            } else {
                (window.calls.XUngrabKeyboard)(window.display, xlib::CurrentTime);
            }
            (window.calls.XFlush)(window.display);
        }
    });
}
