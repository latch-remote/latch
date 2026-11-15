//! Keyboard and mouse into a Wayland desktop.
//!
//! Through the RemoteDesktop portal where the session has it (GNOME, KDE):
//! keys are physical Linux key codes, which the compositor reads with its
//! own layout, text is keysyms, and absolute positions are in the
//! screencast stream's logical pixels. A compositor whose portal has no
//! RemoteDesktop (wlroots: sway, Hyprland) gets a virtual keyboard and
//! mice through `/dev/uinput` instead, as the gamepads do; that needs
//! write access to it (`deploy/native/README.md`, "Gamepads"), types
//! no Unicode text, and an absolute position spans the whole layout.

use super::portal::{self, Session};
use crate::input::{keymap, DisplayRect, Event, Held, InputSink};
use anyhow::{bail, ensure, Context};
use std::collections::BTreeMap;
use std::sync::{atomic::AtomicBool, Arc};
use std::time::{Duration, Instant};

const RELEASE_BUDGET: Duration = Duration::from_millis(250);

pub const BTN_LEFT: u16 = 0x110;
pub const BTN_RIGHT: u16 = 0x111;
pub const BTN_MIDDLE: u16 = 0x112;
pub const BTN_SIDE: u16 = 0x113;
pub const BTN_EXTRA: u16 = 0x114;

/// Latch's buttons (1 left, 2 middle, 3 right, 4 back, 5 forward) as
/// Linux button codes.
pub fn button_code(button: u8) -> anyhow::Result<u16> {
    Ok(match button {
        1 => BTN_LEFT,
        2 => BTN_MIDDLE,
        3 => BTN_RIGHT,
        4 => BTN_SIDE,
        5 => BTN_EXTRA,
        _ => bail!("unknown mouse button {button}"),
    })
}

/// Whole wheel notches in `delta` (120 a notch) with what is left over
/// kept in `remainder`.
pub fn notches(remainder: &mut i32, delta: i16) -> i32 {
    *remainder += i32::from(delta);
    let count = *remainder / 120;
    *remainder %= 120;
    count
}

/// Scroll steps as Linux counts them: down and right are positive. A
/// viewer's positive vertical delta scrolls up, as Windows' wheel does.
pub fn scroll_steps(vertical_notches: i32, horizontal_notches: i32) -> (i32, i32) {
    (-vertical_notches, horizontal_notches)
}

/// The X keysym that types `ch`: Latin-1 is itself, a few controls have
/// their function keys, anything else is a Unicode keysym.
pub fn keysym(ch: char) -> u32 {
    match ch {
        '\n' | '\r' => 0xff0d,
        '\t' => 0xff09,
        '\u{8}' => 0xff08,
        '\u{20}'..='\u{7e}' | '\u{a0}'..='\u{ff}' => u32::from(ch),
        _ => 0x0100_0000 | u32::from(ch),
    }
}

/// What a sink presses: keys by virtual key, with the code they went
/// down as, so a key comes up as what it went down as.
#[derive(Default)]
struct Pressed {
    held: Held,
    keys: BTreeMap<u16, u16>,
    scroll: [i32; 2],
}

/// One way into the compositor.
pub(super) trait Device: Send {
    fn set_stopping(&mut self, _stopping: Arc<AtomicBool>) {}
    fn key(&mut self, code: u16, down: bool) -> anyhow::Result<()>;
    fn button(&mut self, code: u16, down: bool) -> anyhow::Result<()>;
    fn absolute(&mut self, x: i32, y: i32, display: DisplayRect) -> anyhow::Result<()>;
    fn relative(&mut self, dx: i16, dy: i16) -> anyhow::Result<()>;
    fn scroll(&mut self, vertical: i32, horizontal: i32) -> anyhow::Result<()>;
    fn text(&mut self, text: &str) -> anyhow::Result<()>;
    fn release(&mut self, code: u16, button: bool, _limit: Duration) -> anyhow::Result<()> {
        if button {
            self.button(code, false)
        } else {
            self.key(code, false)
        }
    }
}

pub(super) struct WaylandInput<D: ?Sized + Device> {
    pressed: Pressed,
    device: Box<D>,
    session: Option<Arc<Session>>,
    release_on_drop: bool,
}

pub(super) type Sink = WaylandInput<dyn Device>;

impl Sink {
    /// Through `session` when it has input, else through uinput.
    pub fn open(session: Option<Arc<Session>>) -> anyhow::Result<Self> {
        let device: Box<dyn Device> = match session.clone() {
            Some(session) if session.kind == super::token::Kind::RemoteDesktop => {
                Box::new(Portal::new(session)?)
            }
            _ => Box::new(Uinput::new(session.as_deref()).context(
                "this desktop's portal has no remote desktop, and the uinput fallback is unavailable",
            )?),
        };
        Ok(WaylandInput {
            pressed: Pressed::default(),
            device,
            session,
            release_on_drop: true,
        })
    }
}

impl<D: ?Sized + Device> InputSink for WaylandInput<D> {
    fn set_stopping(&mut self, stopping: Arc<AtomicBool>) {
        self.device.set_stopping(stopping);
    }

    fn apply(&mut self, event: &Event, display: DisplayRect) -> anyhow::Result<()> {
        self.release_on_drop = true;
        if let Some(session) = &self.session {
            if let Err(error) = session.check() {
                self.release_all();
                return Err(error);
            }
        }
        let p = &mut self.pressed;
        match event {
            Event::Key { vk, down, .. } => {
                let code = match p.keys.get(vk) {
                    Some(&code) => code,
                    None if !down => return Ok(()),
                    None => keymap::vk_to_evdev(*vk).context("unmapped virtual key")?,
                };
                // A press whose call failed or timed out may still have
                // reached the compositor, so track it first: its release,
                // or ReleaseAll, then lets it go.
                if *down {
                    p.keys.insert(*vk, code);
                    p.held.note(event);
                }
                self.device.key(code, *down)?;
                if !*down {
                    p.keys.remove(vk);
                    p.held.note(event);
                }
            }
            Event::MousePosition {
                x,
                y,
                width,
                height,
            } => {
                let (x, y) = display.map(*x, *y, *width, *height);
                self.device.absolute(x, y, display)?;
            }
            Event::MouseMove { dx, dy } => self.device.relative(*dx, *dy)?,
            Event::MouseButton { button, down } => {
                let code = button_code(*button)?;
                if *down || p.held.buttons.contains(button) {
                    // Tracked before the call, as keys are.
                    if *down {
                        p.held.note(event);
                    }
                    self.device.button(code, *down)?;
                    if !*down {
                        p.held.note(event);
                    }
                }
            }
            Event::Scroll {
                vertical,
                horizontal,
            } => {
                let v = notches(&mut p.scroll[0], *vertical);
                let h = notches(&mut p.scroll[1], *horizontal);
                if v != 0 || h != 0 {
                    let (v, h) = scroll_steps(v, h);
                    self.device.scroll(v, h)?;
                }
            }
            Event::Text(text) => self.device.text(text)?,
            Event::ReleaseAll => self.release_pressed()?,
            Event::UnknownPosition { .. } => {}
        }
        Ok(())
    }

    fn release_all(&mut self) {
        if let Err(e) = self.release_pressed() {
            tracing::warn!("release input: {e:#}");
        }
    }
}

impl<D: ?Sized + Device> WaylandInput<D> {
    fn release_pressed(&mut self) -> anyhow::Result<()> {
        // Explicit ReleaseAll retries retain unacknowledged releases. Drop
        // skips a pass already attempted during teardown, including failures.
        self.release_on_drop = false;
        let p = &mut self.pressed;
        let deadline = Instant::now() + RELEASE_BUDGET;
        let mut release = |code, button| {
            let left = deadline
                .checked_duration_since(Instant::now())
                .context("input release budget exhausted")?;
            self.device.release(code, button, left)
        };
        let mut failure = None;
        for (vk, code) in std::mem::take(&mut p.keys) {
            if let Err(e) = release(code, false) {
                p.keys.insert(vk, code);
                failure = Some(e);
            } else {
                p.held.keys.remove(&vk);
            }
        }
        for button in std::mem::take(&mut p.held.buttons) {
            if let Ok(code) = button_code(button) {
                if let Err(e) = release(code, true) {
                    p.held.buttons.insert(button);
                    failure = Some(e);
                }
            }
        }
        p.scroll = [0; 2];
        match failure {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}

impl<D: ?Sized + Device> Drop for WaylandInput<D> {
    fn drop(&mut self) {
        if self.release_on_drop {
            self.release_all();
        }
    }
}

/// The RemoteDesktop portal's `Notify*` methods.
struct Portal {
    session: Arc<Session>,
    stopping: Arc<AtomicBool>,
    /// The screencast stream absolute positions are relative to.
    stream: u32,
}

impl Portal {
    fn new(session: Arc<Session>) -> anyhow::Result<Self> {
        ensure!(
            session.devices != 0,
            "input sharing was not granted on this machine"
        );
        let stream = session
            .streams
            .first()
            .map(|s| s.node)
            .context("the remote desktop session has no screen")?;
        Ok(Self {
            session,
            stopping: Arc::new(AtomicBool::new(false)),
            stream,
        })
    }

    fn state(down: bool) -> u32 {
        u32::from(down)
    }
}

impl Device for Portal {
    fn set_stopping(&mut self, stopping: Arc<AtomicBool>) {
        self.stopping = stopping;
    }

    fn release(&mut self, code: u16, button: bool, limit: Duration) -> anyhow::Result<()> {
        let method = if button {
            "NotifyPointerButton"
        } else {
            "NotifyKeyboardKeycode"
        };
        self.session.notify_with_timeout(method, limit, |h| {
            (h, portal::no_options(), i32::from(code), Self::state(false))
        })
    }
    fn key(&mut self, code: u16, down: bool) -> anyhow::Result<()> {
        ensure!(
            self.session.devices & portal::KEYBOARD != 0,
            "the keyboard was not shared on this machine"
        );
        self.session
            .notify("NotifyKeyboardKeycode", &self.stopping, |h| {
                (h, portal::no_options(), i32::from(code), Self::state(down))
            })
    }
    fn button(&mut self, code: u16, down: bool) -> anyhow::Result<()> {
        ensure!(
            self.session.devices & portal::POINTER != 0,
            "the pointer was not shared on this machine"
        );
        self.session
            .notify("NotifyPointerButton", &self.stopping, |h| {
                (h, portal::no_options(), i32::from(code), Self::state(down))
            })
    }
    fn absolute(&mut self, x: i32, y: i32, _display: DisplayRect) -> anyhow::Result<()> {
        ensure!(
            self.session.devices & portal::POINTER != 0,
            "the pointer was not shared on this machine"
        );
        let stream = self.stream;
        self.session
            .notify("NotifyPointerMotionAbsolute", &self.stopping, |h| {
                (h, portal::no_options(), stream, f64::from(x), f64::from(y))
            })
    }
    fn relative(&mut self, dx: i16, dy: i16) -> anyhow::Result<()> {
        ensure!(
            self.session.devices & portal::POINTER != 0,
            "the pointer was not shared on this machine"
        );
        self.session
            .notify("NotifyPointerMotion", &self.stopping, |h| {
                (h, portal::no_options(), f64::from(dx), f64::from(dy))
            })
    }
    fn scroll(&mut self, vertical: i32, horizontal: i32) -> anyhow::Result<()> {
        ensure!(
            self.session.devices & portal::POINTER != 0,
            "the pointer was not shared on this machine"
        );
        for (axis, steps) in [(0u32, vertical), (1u32, horizontal)] {
            if steps != 0 {
                self.session
                    .notify("NotifyPointerAxisDiscrete", &self.stopping, |h| {
                        (h, portal::no_options(), axis, steps)
                    })?;
            }
        }
        Ok(())
    }
    fn text(&mut self, text: &str) -> anyhow::Result<()> {
        ensure!(
            self.session.devices & portal::KEYBOARD != 0,
            "the keyboard was not shared on this machine"
        );
        for ch in text.chars() {
            let sym = keysym(ch) as i32;
            for down in [true, false] {
                self.session
                    .notify("NotifyKeyboardKeysym", &self.stopping, |h| {
                        (h, portal::no_options(), sym, Self::state(down))
                    })?;
            }
        }
        Ok(())
    }
}

const EV_SYN: u16 = 0;
const EV_KEY: u16 = 1;
const EV_REL: u16 = 2;
const EV_ABS: u16 = 3;
const REL_X: u16 = 0;
const REL_Y: u16 = 1;
const REL_HWHEEL: u16 = 6;
const REL_WHEEL: u16 = 8;
const ABS_X: u16 = 0;
const ABS_Y: u16 = 1;
/// The absolute device's range: positions are scaled to it.
pub const ABS_MAX: i32 = 65535;

/// An absolute position on `display` in the absolute device's range.
pub fn absolute_axis(at: i32, start: i32, extent: u32) -> i32 {
    let span = i64::from(extent.max(2) - 1);
    (((i64::from(at) - i64::from(start)).clamp(0, span) * i64::from(ABS_MAX)) / span) as i32
}

/// A keyboard with a relative mouse, and an absolute pointer: libinput
/// takes a device with both kinds of axis for neither.
struct Uinput {
    keyboard: evdev::uinput::VirtualDevice,
    pointer: evdev::uinput::VirtualDevice,
    layout: super::layout::Layout,
}

impl Uinput {
    fn new(session: Option<&Session>) -> anyhow::Result<Self> {
        let position = session
            .and_then(|s| s.streams.first())
            .and_then(|s| s.position);
        let layout = super::layout::Layout::open(position)?;
        use evdev::{
            AbsInfo, AbsoluteAxisCode, AttributeSet, BusType, InputId, KeyCode, RelativeAxisCode,
            UinputAbsSetup,
        };
        super::super::uinput::open()?;
        let mut keys = AttributeSet::<KeyCode>::new();
        for vk in 0..=255 {
            if let Some(code) = keymap::vk_to_evdev(vk) {
                keys.insert(KeyCode(code));
            }
        }
        let mut buttons = AttributeSet::<KeyCode>::new();
        for code in [BTN_LEFT, BTN_RIGHT, BTN_MIDDLE, BTN_SIDE, BTN_EXTRA] {
            keys.insert(KeyCode(code));
            buttons.insert(KeyCode(code));
        }
        let mut rel = AttributeSet::<RelativeAxisCode>::new();
        for axis in [REL_X, REL_Y, REL_WHEEL, REL_HWHEEL] {
            rel.insert(RelativeAxisCode(axis));
        }
        let id = InputId::new(BusType::BUS_VIRTUAL, 0x4c61, 0x7463, 1);
        let keyboard = evdev::uinput::VirtualDevice::builder()?
            .name("Latch keyboard and mouse")
            .input_id(id.clone())
            .with_keys(&keys)?
            .with_relative_axes(&rel)?
            .build()
            .context("create the virtual keyboard")?;
        let mut pointer = evdev::uinput::VirtualDevice::builder()?
            .name("Latch absolute pointer")
            .input_id(id)
            .with_keys(&buttons)?;
        for axis in [ABS_X, ABS_Y] {
            pointer = pointer.with_absolute_axis(&UinputAbsSetup::new(
                AbsoluteAxisCode(axis),
                AbsInfo::new(0, 0, ABS_MAX, 0, 0, 0),
            ))?;
        }
        let pointer = pointer.build().context("create the virtual pointer")?;
        tracing::info!("Wayland input through uinput (this portal has no remote desktop)");
        Ok(Self {
            keyboard,
            pointer,
            layout,
        })
    }

    fn emit(
        device: &mut evdev::uinput::VirtualDevice,
        events: &[(u16, u16, i32)],
    ) -> anyhow::Result<()> {
        let mut list: Vec<_> = events
            .iter()
            .map(|&(t, c, v)| evdev::InputEvent::new(t, c, v))
            .collect();
        list.push(evdev::InputEvent::new(EV_SYN, 0, 0));
        device
            .emit(&list)
            .context("write to the virtual input device")
    }
}

impl Device for Uinput {
    fn key(&mut self, code: u16, down: bool) -> anyhow::Result<()> {
        Self::emit(&mut self.keyboard, &[(EV_KEY, code, i32::from(down))])
    }
    fn button(&mut self, code: u16, down: bool) -> anyhow::Result<()> {
        Self::emit(&mut self.keyboard, &[(EV_KEY, code, i32::from(down))])
    }
    fn absolute(&mut self, x: i32, y: i32, display: DisplayRect) -> anyhow::Result<()> {
        let (x, y) = self.layout.position(x, y, display)?;
        Self::emit(&mut self.pointer, &[(EV_ABS, ABS_X, x), (EV_ABS, ABS_Y, y)])
    }
    fn relative(&mut self, dx: i16, dy: i16) -> anyhow::Result<()> {
        Self::emit(
            &mut self.keyboard,
            &[
                (EV_REL, REL_X, i32::from(dx)),
                (EV_REL, REL_Y, i32::from(dy)),
            ],
        )
    }
    fn scroll(&mut self, vertical: i32, horizontal: i32) -> anyhow::Result<()> {
        // REL_WHEEL counts up as positive; the steps here count down.
        Self::emit(
            &mut self.keyboard,
            &[
                (EV_REL, REL_WHEEL, -vertical),
                (EV_REL, REL_HWHEEL, horizontal),
            ],
        )
    }
    fn text(&mut self, _text: &str) -> anyhow::Result<()> {
        bail!("typing text needs the RemoteDesktop portal, which this desktop does not have")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Recorder(Vec<String>, bool);

    impl Device for Recorder {
        fn key(&mut self, code: u16, down: bool) -> anyhow::Result<()> {
            ensure!(down || !self.1, "release failed");
            self.0.push(format!("key {code} {down}"));
            Ok(())
        }
        fn button(&mut self, code: u16, down: bool) -> anyhow::Result<()> {
            ensure!(down || !self.1, "release failed");
            self.0.push(format!("button {code:#x} {down}"));
            Ok(())
        }
        fn absolute(&mut self, x: i32, y: i32, _: DisplayRect) -> anyhow::Result<()> {
            self.0.push(format!("abs {x} {y}"));
            Ok(())
        }
        fn relative(&mut self, dx: i16, dy: i16) -> anyhow::Result<()> {
            self.0.push(format!("rel {dx} {dy}"));
            Ok(())
        }
        fn scroll(&mut self, v: i32, h: i32) -> anyhow::Result<()> {
            self.0.push(format!("scroll {v} {h}"));
            Ok(())
        }
        fn text(&mut self, text: &str) -> anyhow::Result<()> {
            self.0.push(format!("text {text}"));
            Ok(())
        }
    }

    /// A portal whose presses all fail (time out) although they may
    /// have taken effect; releases succeed.
    #[derive(Default)]
    struct LatePresses(Vec<String>);

    impl Device for LatePresses {
        fn key(&mut self, code: u16, down: bool) -> anyhow::Result<()> {
            ensure!(!down, "NotifyKeyboardKeycode timed out");
            self.0.push(format!("key {code} up"));
            Ok(())
        }
        fn button(&mut self, code: u16, down: bool) -> anyhow::Result<()> {
            ensure!(!down, "NotifyPointerButton timed out");
            self.0.push(format!("button {code:#x} up"));
            Ok(())
        }
        fn absolute(&mut self, _: i32, _: i32, _: DisplayRect) -> anyhow::Result<()> {
            Ok(())
        }
        fn relative(&mut self, _: i16, _: i16) -> anyhow::Result<()> {
            Ok(())
        }
        fn scroll(&mut self, _: i32, _: i32) -> anyhow::Result<()> {
            Ok(())
        }
        fn text(&mut self, _: &str) -> anyhow::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn native_positions_reach_evdev_and_unknown_positions_are_ignored() {
        let mut s = sink();
        s.apply(
            &Event::UnknownPosition {
                usage: 0xffff,
                down: true,
                modifiers: 0,
            },
            DISPLAY,
        )
        .unwrap();
        assert!(s.device.0.is_empty());
        for case in latch_proto::keyboard::CASES {
            for down in [true, false] {
                s.apply(
                    &Event::Key {
                        vk: case.vk,
                        down,
                        modifiers: case.mask,
                    },
                    DISPLAY,
                )
                .unwrap();
            }
            assert_eq!(
                std::mem::take(&mut s.device.0),
                [
                    format!("key {} true", case.linux - 8),
                    format!("key {} false", case.linux - 8)
                ]
            );
            assert!(s.pressed.keys.is_empty());
        }
    }

    #[test]
    fn a_press_whose_call_failed_is_still_released() {
        let mut s = WaylandInput::<LatePresses> {
            pressed: Pressed::default(),
            device: Box::default(),
            session: None,
            release_on_drop: true,
        };
        let key = |down| Event::Key {
            vk: 0x41,
            down,
            modifiers: 0,
        };
        let button = |down| Event::MouseButton { button: 1, down };
        assert!(s.apply(&key(true), DISPLAY).is_err());
        s.apply(&key(false), DISPLAY).unwrap();
        assert!(s.apply(&button(true), DISPLAY).is_err());
        s.apply(&Event::ReleaseAll, DISPLAY).unwrap();
        assert_eq!(s.device.0, ["key 30 up", "button 0x110 up"]);
        s.apply(&Event::ReleaseAll, DISPLAY).unwrap();
        assert_eq!(s.device.0.len(), 2, "each is released once");
    }

    fn sink() -> WaylandInput<Recorder> {
        WaylandInput {
            pressed: Pressed::default(),
            device: Box::default(),
            session: None,
            release_on_drop: true,
        }
    }

    const DISPLAY: DisplayRect = DisplayRect {
        x: 0,
        y: 0,
        width: 1920,
        height: 1080,
    };

    #[test]
    fn events_become_linux_codes_and_held_ones_are_released() {
        let mut s = sink();
        let key = |vk, down| Event::Key {
            vk,
            down,
            modifiers: 0,
        };
        s.apply(&key(0x41, true), DISPLAY).unwrap();
        s.apply(&key(0xA0, true), DISPLAY).unwrap();
        s.apply(&key(0x41, false), DISPLAY).unwrap();
        s.apply(&key(0x42, false), DISPLAY).unwrap(); // never pressed: dropped
        s.apply(
            &Event::MouseButton {
                button: 3,
                down: true,
            },
            DISPLAY,
        )
        .unwrap();
        s.apply(
            &Event::MouseButton {
                button: 1,
                down: false,
            },
            DISPLAY,
        )
        .unwrap(); // never pressed: dropped
        s.apply(
            &Event::MousePosition {
                x: 960,
                y: 540,
                width: 1920,
                height: 1080,
            },
            DISPLAY,
        )
        .unwrap();
        s.apply(&Event::MouseMove { dx: -3, dy: 4 }, DISPLAY)
            .unwrap();
        s.apply(&Event::Text("hé".into()), DISPLAY).unwrap();
        s.apply(&Event::ReleaseAll, DISPLAY).unwrap();
        assert_eq!(
            s.device.0,
            [
                "key 30 true",
                "key 42 true",
                "key 30 false",
                "button 0x111 true",
                "abs 960 540",
                "rel -3 4",
                "text hé",
                "key 42 false",
                "button 0x111 false",
            ]
        );
        assert!(s.pressed.keys.is_empty() && s.pressed.held.buttons.is_empty());
    }

    #[test]
    fn failed_releases_remain_held_until_an_explicit_retry_succeeds() {
        let mut s = sink();
        s.apply(
            &Event::Key {
                vk: 0x41,
                down: true,
                modifiers: 0,
            },
            DISPLAY,
        )
        .unwrap();
        s.apply(
            &Event::MouseButton {
                button: 1,
                down: true,
            },
            DISPLAY,
        )
        .unwrap();
        s.device.1 = true;
        assert!(s.apply(&Event::ReleaseAll, DISPLAY).is_err());
        assert!(s.pressed.held.keys.contains(&0x41));
        assert!(s.pressed.held.buttons.contains(&1));
        assert!(!s.release_on_drop);
        s.device.1 = false;
        s.apply(&Event::ReleaseAll, DISPLAY).unwrap();
        assert!(s.pressed.keys.is_empty());
        assert!(s.pressed.held.keys.is_empty());
        assert!(s.pressed.held.buttons.is_empty());
    }

    #[test]
    fn scroll_keeps_remainders_and_counts_down_as_positive() {
        let mut s = sink();
        let scroll = |vertical, horizontal| Event::Scroll {
            vertical,
            horizontal,
        };
        s.apply(&scroll(60, 0), DISPLAY).unwrap();
        assert!(s.device.0.is_empty(), "half a notch does nothing yet");
        s.apply(&scroll(60, -240), DISPLAY).unwrap();
        s.apply(&scroll(-360, 0), DISPLAY).unwrap();
        assert_eq!(s.device.0, ["scroll -1 -2", "scroll 3 0"]);
    }

    #[test]
    fn buttons_keysyms_and_the_absolute_range() {
        assert_eq!(button_code(1).unwrap(), BTN_LEFT);
        assert_eq!(button_code(2).unwrap(), BTN_MIDDLE);
        assert_eq!(button_code(3).unwrap(), BTN_RIGHT);
        assert_eq!(button_code(4).unwrap(), BTN_SIDE);
        assert_eq!(button_code(5).unwrap(), BTN_EXTRA);
        assert!(button_code(6).is_err());
        assert_eq!(keysym('a'), 0x61);
        assert_eq!(keysym('é'), 0xe9);
        assert_eq!(keysym('\n'), 0xff0d);
        assert_eq!(keysym('λ'), 0x0100_03bb);
        assert_eq!(keysym('😀'), 0x0101_f600);
        assert_eq!(absolute_axis(0, 0, 1920), 0);
        assert_eq!(absolute_axis(1919, 0, 1920), ABS_MAX);
        assert_eq!(absolute_axis(5000, 0, 1920), ABS_MAX);
        assert_eq!(absolute_axis(-5, 0, 1920), 0);
        assert_eq!(absolute_axis(100, 100, 1), 0, "a one-pixel display");
    }
}
