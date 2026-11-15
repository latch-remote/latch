//! The host side of a Latch stream. [`server`] takes a session from the
//! control service and runs it: [`video`] (capture and encode),
//! [`audio`] (desktop sound), [`input`] (keyboard and mouse into the
//! desktop), [`gamepad`] (virtual gamepads) and [`display`] (the desktop's
//! size), each with one
//! implementation per platform in `mac/`, `windows/` and `linux/`.
//! [`software`] is the CPU path (any BGRA source through `latch-codec`),
//! [`pattern`] a synthetic desktop for tests and measurements.

pub mod audio;
mod bitstream;
mod congestion;
#[cfg(any(target_os = "macos", windows, test))]
mod cursor;
pub mod display;
pub mod gamepad;
pub mod input;
pub mod mic;
mod pacing;
pub mod pattern;
pub mod priority;
pub mod server;
pub mod software;
pub mod testmode;
pub mod video;
#[cfg(any(windows, test))]
mod windows_cursor;

#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "macos")]
pub mod mac;
#[cfg(windows)]
pub mod windows;

#[cfg(all(test, target_os = "macos"))]
#[path = "linux/x11.rs"]
mod x11_bootstrap_tests;

#[cfg(test)]
#[path = "../../../tests/pairs/format.rs"]
pub(crate) mod pair_format;

/// The platform-free parts of the Wayland backend (backend choice, the
/// PipeWire format negotiation, restore tokens, damage), tested on every
/// build machine rather than only on Linux.
#[cfg(all(test, not(target_os = "linux")))]
#[path = "linux/wayland"]
#[allow(dead_code)]
mod wayland_tests {
    mod backend;
    mod frame;
    mod pod;
    mod token;
}
