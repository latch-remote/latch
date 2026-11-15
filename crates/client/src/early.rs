//! Keyboard and mouse sent from the OS event, ahead of the UI frame.
//!
//! A small egui-winit patch calls us immediately after translating each OS
//! event on macOS, Windows and Linux. Both dispatch paths share held state
//! and use the existing session sender and reliability layer.
//!
//! The frame publishes its focus/capture policy. Events that need a fresh
//! UI decision (host shortcuts, clipboard, IME, capture or focus changes)
//! close the gate until the frame handles them, preserving their order.
//! Sent events remain in egui's input, with one receipt per event to prevent
//! a second send. Receipts move to their frame when egui-winit takes input.

use crate::input::{self, SharedHeld, Sink};
use egui::{Event, Key, Modifiers, Pos2, Rect, Vec2};
use egui_winit::winit::event::WindowEvent;
use std::cell::RefCell;
use std::rc::{Rc, Weak};
use std::time::{Duration, Instant};

/// An input event as the OS delivered it, in the terms the frame uses.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Native {
    Physical {
        key: crate::keyboard::Key,
        mods: Modifiers,
    },
    /// A modifier key went down or up; these are the modifiers held now.
    Modifiers(Modifiers),
    /// `physical` is the key's place on a US keyboard, `logical` what the
    /// layout makes of it when the platform says.
    Key {
        physical: Option<Key>,
        logical: Option<Key>,
        down: bool,
        repeat: bool,
        mods: Modifiers,
    },
    /// `button` is one of the `BUTTON_*` codes.
    Button {
        button: u8,
        down: bool,
        mods: Modifiers,
        pos: Option<Pos2>,
    },
    /// Raw relative movement, in the units winit reports to egui.
    Motion {
        dx: f32,
        dy: f32,
    },
    Position(Pos2),
    /// Horizontal and vertical scroll in host units.
    Scroll(f32, f32),
    /// Something only the frame sends, such as a change of Caps Lock,
    /// which reaches the PC as a tap: what follows must wait behind it.
    Local,
}

/// What the hook sent for one event, for the frame to claim.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Ahead {
    Physical(u16, bool),
    Key(i16, bool),
    Button(u8, bool),
    /// The raw delta, as egui's `MouseMoved` carries it.
    Motion(f32, f32),
    Position(Pos2),
    Scroll(f32, f32),
}

/// What the frame allows the hook until its next end.
#[derive(Clone)]
pub struct Gate {
    /// Keys and modifiers go to the PC.
    pub keys: bool,
    /// Visible local controls may take focus on a navigation key.
    pub local_focus: bool,
    /// The mouse is captured: buttons and movement go to the PC.
    pub mouse: bool,
    pub cmd_is_ctrl: bool,
    pub host_mac: bool,
    pub map_shortcuts: bool,
    /// Stream pixels per unit of raw movement.
    pub motion_scale: f32,
    /// Geometry and hit region from the last completed frame.
    pub video: Rect,
    pub pointer_region: Rect,
    pub ctx: egui::Context,
    pub over_stream: bool,
    pub ppp: f32,
    pub free_clicks: bool,
    pub caps: Option<bool>,
    pub out: Rc<dyn Sink>,
}

/// How long notes have waited for their frame, for the debug log.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct Lead {
    /// Events the hook sent that a frame then claimed.
    pub claimed: u64,
    /// Times the hook handed an event to the frame and disarmed.
    pub declined: u64,
    pub total: Duration,
    pub max: Duration,
}

impl Lead {
    pub fn mean(&self) -> Duration {
        if self.claimed == 0 {
            Duration::ZERO
        } else {
            self.total / self.claimed as u32
        }
    }
}

/// How often the lead is logged, at debug level.
const LOG_EVERY: Duration = Duration::from_secs(10);

#[derive(Default)]
struct State {
    gate: Option<Gate>,
    /// Focus loss must release input even while the frame owns dispatch.
    out: Option<Rc<dyn Sink>>,
    active: bool,
    over_stream: bool,
    held: SharedHeld,
    /// Captured movement not yet a whole pixel.
    motion: (f32, f32),
    scroll: (f32, f32),
    /// Sent since the frame last took the window's input.
    sent: Vec<(Ahead, Instant)>,
    /// Sent before this frame took its input, waiting to be claimed.
    frame: Vec<(Ahead, Instant)>,
    lead: Lead,
    since: Option<Instant>,
}

enum Outcome {
    /// Sent, and what for the frame to claim, if egui reports it.
    Sent(Option<Ahead>),
    /// Nothing to send, and nothing the frame would send either.
    Ignored,
    /// The frame's to send: the hook stands down until the frame ends.
    Declined,
}

/// The hook's side of a stream view: shared with the view, on the
/// window's thread.
#[derive(Clone, Default)]
pub struct Early(Rc<RefCell<State>>);

thread_local! {
    /// The view the platform hook sends for.
    static CURRENT: RefCell<Weak<RefCell<State>>> = const { RefCell::new(Weak::new()) };
}

fn current() -> Option<Early> {
    CURRENT.with(|c| c.borrow().upgrade().map(Early))
}

/// Low-level Windows capture uses the same sender, focus gate and receipts.
#[cfg(windows)]
pub fn physical(key: crate::keyboard::Key, mods: Modifiers) {
    if let Some(early) = current() {
        early.deliver(Native::Physical { key, mods }, Instant::now());
    }
}

/// Register the cross-platform callback on the window's context.
pub fn install(ctx: &egui::Context) {
    ctx.data_mut(|d| {
        d.insert_temp(
            egui_winit::EventHook::id(),
            egui_winit::EventHook {
                on_event: |viewport, native, events, mods| {
                    if viewport == egui::ViewportId::ROOT {
                        if let Some(early) = current() {
                            early.on_event(native, events, mods);
                        }
                    }
                },
                on_frame: |viewport| {
                    if viewport == egui::ViewportId::ROOT {
                        if let Some(early) = current() {
                            early.frame_starts();
                        }
                    }
                },
            },
        );
    });
}

impl Early {
    /// The hook's side of a view whose keys and buttons are `held`.
    pub fn new(held: SharedHeld) -> Self {
        Self(Rc::new(RefCell::new(State {
            held,
            ..State::default()
        })))
    }

    /// Keep the focus-loss release path available while dispatch is disabled.
    pub fn listen(&self, out: Rc<dyn Sink>) {
        self.0.borrow_mut().out = Some(out);
        CURRENT.with(|c| *c.borrow_mut() = Rc::downgrade(&self.0));
    }

    /// Let the hook send under `gate` until the frame says otherwise.
    pub fn arm(&self, gate: Gate) {
        self.listen(gate.out.clone());
        let mut s = self.0.borrow_mut();
        s.over_stream = gate.over_stream;
        s.gate = Some(gate);
        s.active = true;
    }

    /// Stop the hook sending: everything goes through the frame.
    pub fn disarm(&self) {
        let mut s = self.0.borrow_mut();
        s.gate = None;
        s.out = None;
        s.active = false;
        s.motion = (0.0, 0.0);
        s.scroll = (0.0, 0.0);
    }

    pub fn armed(&self) -> bool {
        self.0.borrow().active
    }

    /// Pass subpixel remainders between the event loop and the frame.
    pub fn take_remainders(&self) -> Option<(Vec2, Vec2)> {
        let mut s = self.0.borrow_mut();
        s.gate.take()?;
        Some((s.motion.into(), s.scroll.into()))
    }

    pub fn set_remainders(&self, motion: (f32, f32), scroll: (f32, f32)) {
        let mut s = self.0.borrow_mut();
        s.motion = motion;
        s.scroll = scroll;
    }

    /// eframe has taken the window's input for a frame: what the hook sent
    /// so far is that frame's to claim. Notes the last frame left
    /// unclaimed were events egui never reported, and go.
    pub fn frame_starts(&self) {
        let mut s = self.0.borrow_mut();
        s.frame = std::mem::take(&mut s.sent);
        s.active = false;
    }

    /// The frame met `a` in egui's events: whether the hook sent it
    /// already, in which case the frame must not.
    pub fn claim(&self, a: Ahead) -> bool {
        let mut s = self.0.borrow_mut();
        let Some(i) = s.frame.iter().position(|(x, _)| *x == a) else {
            return false;
        };
        let (_, at) = s.frame.remove(i);
        let waited = at.elapsed();
        s.lead.claimed += 1;
        s.lead.total += waited;
        s.lead.max = s.lead.max.max(waited);
        s.log();
        true
    }

    /// The lead so far, since it was last logged.
    pub fn lead(&self) -> Lead {
        self.0.borrow().lead
    }

    /// Called synchronously by egui-winit, before requesting a repaint.
    pub fn on_event(&self, native: Option<&WindowEvent>, events: &[Event], mods: Modifiers) {
        if matches!(
            native,
            Some(
                WindowEvent::Focused(false) | WindowEvent::CloseRequested | WindowEvent::Destroyed
            )
        ) {
            let mut s = self.0.borrow_mut();
            if let Some(out) = &s.out {
                s.held.release_all(&**out);
            }
            s.declined();
            return;
        }
        if matches!(
            native,
            Some(
                WindowEvent::Resized(_)
                    | WindowEvent::ScaleFactorChanged { .. }
                    | WindowEvent::CursorLeft { .. }
                    | WindowEvent::Ime(_)
            )
        ) {
            self.deliver(Native::Local, Instant::now());
            return;
        }
        // Caps Lock is a frame-generated tap on macOS. Keep later input
        // behind that tap even though egui has no Caps Lock event.
        let caps_changed = self
            .0
            .borrow()
            .gate
            .as_ref()
            .and_then(|g| g.caps.zip(crate::macos::caps_lock()))
            .is_some_and(|(before, now)| before != now);
        if caps_changed {
            self.deliver(Native::Local, Instant::now());
            return;
        }
        if matches!(native, Some(WindowEvent::ModifiersChanged(_))) && !crate::keyboard::available()
        {
            self.deliver(Native::Modifiers(mods), Instant::now());
        }
        let events = if matches!(native, Some(WindowEvent::KeyboardInput { .. })) {
            crate::keyboard::latest()
        } else {
            None
        }
        .unwrap_or_else(|| events.iter().cloned().map(|e| (e, None, false)).collect());
        let physical_text = events.iter().any(
            |(e, native, _)| matches!(e, Event::Key { key, pressed: true, .. } if native.as_ref().map_or_else(|| input::vk(*key).is_some(), |k| crate::keyboard::text_position(k.vk))),
        );
        for (event, key, literal) in events {
            if let Some(key) = key {
                let mods = match event {
                    Event::Key { modifiers, .. } => modifiers,
                    _ => mods,
                };
                self.deliver(Native::Physical { key, mods }, Instant::now());
                continue;
            }
            let ev = match event {
                Event::Key {
                    physical_key,
                    key,
                    pressed,
                    repeat,
                    modifiers,
                } => Native::Key {
                    physical: physical_key,
                    logical: Some(key),
                    down: pressed,
                    repeat,
                    mods: modifiers,
                },
                Event::PointerButton {
                    button,
                    pressed,
                    modifiers,
                    pos,
                } => {
                    let Some(button) = input::button(button) else {
                        continue;
                    };
                    Native::Button {
                        button,
                        down: pressed,
                        mods: modifiers,
                        pos: Some(pos),
                    }
                }
                Event::MouseMoved(d) => Native::Motion { dx: d.x, dy: d.y },
                Event::PointerMoved(p) => Native::Position(p),
                Event::MouseWheel { unit, delta, .. } => {
                    let (h, v) = crate::stream::wheel_to_host(unit, delta);
                    Native::Scroll(h, v)
                }
                // Text accompanying a physical key is suppressed by the
                // frame, too. Standalone text and clipboard events need it.
                Event::Text(_) if physical_text && !literal => continue,
                Event::Text(_) | Event::Ime(_) | Event::Copy | Event::Cut | Event::Paste(_) => {
                    Native::Local
                }
                _ => continue,
            };
            self.deliver(ev, Instant::now());
        }
    }

    /// Send `ev` now if the gate allows it. Returns whether it was sent.
    pub fn deliver(&self, ev: Native, now: Instant) -> bool {
        // The frame holds the state while it runs; an event that arrives
        // then (a nested run loop) is the frame's.
        let Ok(mut s) = self.0.try_borrow_mut() else {
            return false;
        };
        if !s.active {
            return false;
        }
        let Some(gate) = s.gate.clone() else {
            return false;
        };
        if s.held.busy() {
            s.declined();
            return false;
        }
        match s.route(&gate, ev) {
            Outcome::Sent(ahead) => {
                if let Some(a) = ahead {
                    s.sent.push((a, now));
                }
                true
            }
            Outcome::Ignored => false,
            Outcome::Declined => {
                s.declined();
                false
            }
        }
    }
}

impl State {
    fn declined(&mut self) {
        self.active = false;
        self.lead.declined += 1;
    }

    fn route(&mut self, gate: &Gate, ev: Native) -> Outcome {
        let out = &*gate.out;
        match ev {
            Native::Physical { key, mods } => {
                if !gate.keys
                    || host_key(mods)
                    || (!key.down && !self.held.physical_down(key.vk))
                    || (key.down && gate.local_focus && matches!(key.vk, 0x09 | 0x1b | 0x25..=0x28))
                {
                    return Outcome::Declined;
                }
                self.held.shortcut_key(
                    out,
                    key,
                    cfg!(target_os = "macos"),
                    gate.host_mac,
                    gate.map_shortcuts && !gate.mouse,
                );
                Outcome::Sent(Some(Ahead::Physical(key.vk, key.down)))
            }
            // Free mode uses absolute pointer positions, so raw device
            // movement has no remote effect.
            Native::Motion { .. } if !gate.mouse => Outcome::Ignored,
            Native::Motion { dx, dy } => {
                self.motion.0 += dx * gate.motion_scale;
                self.motion.1 += dy * gate.motion_scale;
                if self.motion.0.abs() >= 1.0 || self.motion.1.abs() >= 1.0 {
                    let x = self.motion.0.trunc();
                    let y = self.motion.1.trunc();
                    self.motion.0 -= x;
                    self.motion.1 -= y;
                    out.motion(x as i16, y as i16);
                }
                Outcome::Sent(Some(Ahead::Motion(dx, dy)))
            }
            Native::Position(_) if gate.mouse => Outcome::Ignored,
            Native::Position(p) => {
                if !gate.pointer_region.contains(p)
                    || gate
                        .ctx
                        .layer_id_at(p)
                        .is_some_and(|l| l.order > egui::Order::Background)
                {
                    return Outcome::Declined;
                }
                self.over_stream = true;
                let (x, y, w, h) = crate::stream::mouse_position_in_video(p, gate.video, gate.ppp);
                out.position(x, y, w, h);
                Outcome::Sent(Some(Ahead::Position(p)))
            }
            Native::Scroll(_, _) if !gate.mouse && !self.over_stream => Outcome::Declined,
            Native::Scroll(h, v) => {
                let ahead = Ahead::Scroll(h, v);
                self.scroll.0 += h;
                self.scroll.1 += v;
                let h = self.scroll.0.trunc();
                let v = self.scroll.1.trunc();
                self.scroll.0 -= h;
                self.scroll.1 -= v;
                if h != 0.0 || v != 0.0 {
                    out.scroll(
                        v.clamp(-32000.0, 32000.0) as i16,
                        h.clamp(-32000.0, 32000.0) as i16,
                    );
                }
                Outcome::Sent(Some(ahead))
            }
            Native::Local => Outcome::Declined,
            Native::Modifiers(m) => {
                if !gate.keys || host_key(m) {
                    return Outcome::Declined;
                }
                self.held.modifiers(out, m, gate.cmd_is_ctrl);
                Outcome::Sent(None)
            }
            Native::Key {
                physical,
                logical,
                down,
                repeat,
                mods,
            } => {
                if !gate.keys
                    || host_key(mods)
                    || stays_local(mods, physical, logical)
                    || (down
                        && gate.local_focus
                        && [physical, logical].into_iter().flatten().any(|key| {
                            matches!(
                                key,
                                Key::Tab
                                    | Key::ArrowUp
                                    | Key::ArrowDown
                                    | Key::ArrowLeft
                                    | Key::ArrowRight
                                    | Key::Escape
                            )
                        }))
                    || (!down && repeat)
                {
                    return Outcome::Declined;
                }
                // A key egui places only by its label is the frame's.
                let Some(vk) = physical.and_then(input::vk) else {
                    return Outcome::Declined;
                };
                // A key let go that the PC never saw go down: the frame
                // knows why (a ⌘V egui swallowed, say).
                if !down && !self.held.is_down(vk) {
                    return Outcome::Declined;
                }
                // Autorepeat goes as more presses, as the frame sends it.
                self.held.modifiers(out, mods, gate.cmd_is_ctrl);
                self.held.key(out, vk, down);
                Outcome::Sent(Some(Ahead::Key(vk, down)))
            }
            Native::Button {
                button,
                down,
                mods,
                pos,
            } => {
                if (!gate.mouse && !gate.free_clicks) || host_key(mods) {
                    return Outcome::Declined;
                }
                if !gate.mouse {
                    let Some(p) = pos else {
                        return Outcome::Declined;
                    };
                    if !gate.pointer_region.contains(p)
                        || gate
                            .ctx
                            .layer_id_at(p)
                            .is_some_and(|l| l.order > egui::Order::Background)
                    {
                        return Outcome::Declined;
                    }
                    let (x, y, w, h) =
                        crate::stream::mouse_position_in_video(p, gate.video, gate.ppp);
                    out.position(x, y, w, h);
                }
                if gate.keys {
                    self.held.modifiers(out, mods, gate.cmd_is_ctrl);
                }
                self.held.button(out, button, down);
                Outcome::Sent(Some(Ahead::Button(button, down)))
            }
        }
    }

    fn log(&mut self) {
        let now = Instant::now();
        let since = *self.since.get_or_insert(now);
        if now.duration_since(since) < LOG_EVERY {
            return;
        }
        let l = std::mem::take(&mut self.lead);
        self.since = Some(now);
        tracing::debug!(
            "input from the OS event: {} sent ahead of their frame by {:.2} ms on average, {:.2} ms at most; {} left to the frame",
            l.claimed,
            l.mean().as_secs_f64() * 1000.0,
            l.max.as_secs_f64() * 1000.0,
            l.declined,
        );
    }
}

/// Ctrl+Alt, Latch's host key.
fn host_key(m: Modifiers) -> bool {
    m.ctrl && m.alt && !crate::keyboard::alt_gr()
}

/// A key press the frame has to see first: egui turns it into a clipboard
/// event ([`egui_winit`'s rules], by label or by place).
///
/// [`egui_winit`'s rules]: https://docs.rs/egui-winit/0.31.1/src/egui_winit/lib.rs.html
fn stays_local(m: Modifiers, physical: Option<Key>, logical: Option<Key>) -> bool {
    [physical, logical]
        .into_iter()
        .flatten()
        .any(|k| clipboard_key(m, k))
}

fn clipboard_key(m: Modifiers, k: Key) -> bool {
    matches!(k, Key::Cut | Key::Copy | Key::Paste)
        || (m.command && matches!(k, Key::X | Key::C | Key::V))
        || (cfg!(windows)
            && ((m.shift && matches!(k, Key::Delete | Key::Insert))
                || (m.ctrl && k == Key::Insert)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::{Recorder, Sent, VK_CONTROL, VK_MENU, VK_SHIFT};
    use latch_stream::input::{BUTTON_LEFT, MOD_CTRL, MOD_SHIFT};

    const SHIFT: Modifiers = Modifiers::SHIFT;

    #[test]
    fn native_positions_use_the_existing_event_time_gate_and_receipts() {
        for case in latch_proto::keyboard::CASES {
            let (early, held, out) = armed(true, false);
            for down in [true, false] {
                assert!(deliver(
                    &early,
                    Native::Physical {
                        key: crate::keyboard::Key { vk: case.vk, down },
                        mods: Modifiers::NONE
                    }
                ));
            }
            assert!(!held.any_down());
            assert_eq!(
                out.take(),
                [
                    Sent::Key(case.vk as i16, true, case.mask),
                    Sent::Key(case.vk as i16, false, 0)
                ]
            );
            early.frame_starts();
            assert!(early.claim(Ahead::Physical(case.vk, true)));
            assert!(early.claim(Ahead::Physical(case.vk, false)));
            assert!(!early.claim(Ahead::Physical(case.vk, true)));
        }
    }

    fn armed(keys: bool, mouse: bool) -> (Early, SharedHeld, Rc<Recorder>) {
        let held = SharedHeld::default();
        let early = Early::new(held.clone());
        let out = Rc::new(Recorder::default());
        out.connected.set(true);
        early.arm(Gate {
            keys,
            local_focus: false,
            mouse,
            cmd_is_ctrl: true,
            host_mac: false,
            map_shortcuts: false,
            motion_scale: 1.0,
            video: Rect::from_min_size(Pos2::ZERO, Vec2::splat(100.0)),
            pointer_region: Rect::from_min_size(Pos2::ZERO, Vec2::splat(100.0)),
            ctx: egui::Context::default(),
            over_stream: true,
            ppp: 1.0,
            free_clicks: false,
            caps: None,
            out: out.clone(),
        });
        (early, held, out)
    }

    fn key(k: Key, down: bool, mods: Modifiers) -> Native {
        Native::Key {
            physical: Some(k),
            logical: Some(k),
            down,
            repeat: false,
            mods,
        }
    }

    fn deliver(e: &Early, ev: Native) -> bool {
        e.deliver(ev, Instant::now())
    }

    #[test]
    fn keys_go_at_once_and_in_order_with_their_modifiers() {
        let (e, _, out) = armed(true, false);
        assert!(deliver(&e, Native::Modifiers(SHIFT)));
        assert!(deliver(&e, key(Key::A, true, SHIFT)));
        assert!(deliver(&e, key(Key::A, false, SHIFT)));
        assert!(deliver(&e, Native::Modifiers(Modifiers::NONE)));
        assert_eq!(
            out.take(),
            [
                Sent::Key(VK_SHIFT, true, MOD_SHIFT),
                Sent::Key(0x41, true, MOD_SHIFT),
                Sent::Key(0x41, false, MOD_SHIFT),
                Sent::Key(VK_SHIFT, false, 0),
            ]
        );
    }

    #[test]
    fn the_frame_claims_each_sent_event_once() {
        let (e, _, _) = armed(true, true);
        deliver(&e, key(Key::A, true, Modifiers::NONE));
        deliver(&e, key(Key::A, false, Modifiers::NONE));
        deliver(&e, key(Key::A, true, Modifiers::NONE));
        deliver(&e, Native::Motion { dx: 2.5, dy: -1.0 });
        // Not the frame's until eframe takes the input for it.
        assert!(!e.claim(Ahead::Key(0x41, true)));
        e.frame_starts();
        assert!(e.claim(Ahead::Key(0x41, true)));
        assert!(e.claim(Ahead::Key(0x41, false)));
        assert!(e.claim(Ahead::Key(0x41, true)), "pressed twice, sent twice");
        assert!(!e.claim(Ahead::Key(0x41, true)), "but not three times");
        assert!(e.claim(Ahead::Motion(2.5, -1.0)));
        assert!(!e.claim(Ahead::Motion(2.5, -1.0)));
        assert_eq!(e.lead().claimed, 4);
        // A frame later, what was not claimed has gone.
        deliver(&e, key(Key::B, true, Modifiers::NONE));
        e.frame_starts();
        e.frame_starts();
        assert!(!e.claim(Ahead::Key(0x42, true)));
    }

    #[test]
    fn the_host_key_stays_local_and_disarms() {
        let (e, held, out) = armed(true, true);
        let ctrl = Modifiers {
            ctrl: true,
            ..Modifiers::NONE
        };
        let ctrl_alt = Modifiers {
            ctrl: true,
            alt: true,
            ..Modifiers::NONE
        };
        assert!(deliver(&e, Native::Modifiers(ctrl)));
        assert!(!deliver(&e, Native::Modifiers(ctrl_alt)));
        assert!(!e.armed(), "the frame has the host key");
        // Nothing else goes until the frame arms the hook again: S with
        // the host key toggles stats, and must not reach the PC.
        assert!(!deliver(&e, key(Key::S, true, ctrl_alt)));
        assert!(!deliver(&e, Native::Motion { dx: 5.0, dy: 0.0 }));
        assert_eq!(out.take(), [Sent::Key(VK_CONTROL, true, MOD_CTRL)]);
        assert!(held.is_down(VK_CONTROL) && !held.is_down(VK_MENU));
        assert_eq!(e.lead().declined, 1);
    }

    #[test]
    fn clipboard_shortcuts_go_through_the_frame() {
        let cmd = Modifiers {
            command: true,
            mac_cmd: cfg!(target_os = "macos"),
            ctrl: !cfg!(target_os = "macos"),
            ..Modifiers::NONE
        };
        for k in [Key::C, Key::X, Key::V] {
            let (e, _, out) = armed(true, false);
            assert!(!deliver(&e, key(k, true, cmd)), "{k:?}");
            assert!(!e.armed());
            assert!(out.take().is_empty());
        }
        // By label too: on Dvorak, the key at I is C.
        let (e, _, _) = armed(true, false);
        let dvorak_c = Native::Key {
            physical: Some(Key::I),
            logical: Some(Key::C),
            down: true,
            repeat: false,
            mods: cmd,
        };
        assert!(!deliver(&e, dvorak_c));
        // A key let go that the PC never saw pressed is the frame's too.
        let (e, _, out) = armed(true, false);
        assert!(!deliver(&e, key(Key::V, false, Modifiers::NONE)));
        assert!(out.take().is_empty());
    }

    #[test]
    fn what_comes_after_a_declined_event_waits_for_the_frame() {
        let (e, _, out) = armed(true, false);
        assert!(deliver(&e, key(Key::A, true, Modifiers::NONE)));
        // A click in free mode is the frame's; the key after it must not
        // overtake it.
        let click = Native::Button {
            button: BUTTON_LEFT,
            down: true,
            mods: Modifiers::NONE,
            pos: None,
        };
        assert!(!deliver(&e, click));
        assert!(!deliver(&e, key(Key::A, false, Modifiers::NONE)));
        assert_eq!(out.take(), [Sent::Key(0x41, true, 0)]);
    }

    #[test]
    fn a_letter_after_caps_lock_waits_for_the_frames_tap() {
        let (e, _, out) = armed(true, false);
        assert!(!deliver(&e, Native::Local));
        assert!(!deliver(&e, key(Key::A, true, Modifiers::NONE)));
        assert!(out.take().is_empty());
    }

    #[test]
    fn free_movement_neither_goes_nor_disarms() {
        let (e, _, out) = armed(true, false);
        assert!(!deliver(&e, Native::Motion { dx: 4.0, dy: 4.0 }));
        assert!(e.armed());
        assert!(deliver(&e, key(Key::A, true, Modifiers::NONE)));
        assert_eq!(out.take(), [Sent::Key(0x41, true, 0)]);
    }

    #[test]
    fn captured_movement_goes_in_whole_pixels_before_a_click() {
        let held = SharedHeld::default();
        let e = Early::new(held.clone());
        let out = Rc::new(Recorder::default());
        out.connected.set(true);
        e.arm(Gate {
            keys: false,
            local_focus: false,
            mouse: true,
            cmd_is_ctrl: true,
            host_mac: false,
            map_shortcuts: false,
            motion_scale: 2.0,
            video: Rect::NOTHING,
            pointer_region: Rect::NOTHING,
            ctx: egui::Context::default(),
            over_stream: true,
            ppp: 1.0,
            free_clicks: false,
            caps: None,
            out: out.clone(),
        });
        assert!(deliver(&e, Native::Motion { dx: 0.25, dy: 0.0 }));
        assert!(deliver(&e, Native::Motion { dx: 0.5, dy: -1.0 }));
        let click = |down| Native::Button {
            button: BUTTON_LEFT,
            down,
            mods: SHIFT,
            pos: None,
        };
        assert!(deliver(&e, click(true)));
        assert!(deliver(&e, click(false)));
        assert_eq!(
            out.take(),
            [
                Sent::Motion(1, -2),
                Sent::Button(BUTTON_LEFT, true),
                Sent::Button(BUTTON_LEFT, false),
            ],
            "no modifiers while the keys are not the PC's"
        );
        assert!(!held.any_down());
    }

    #[test]
    fn nothing_goes_unarmed_or_while_the_frame_runs() {
        let held = SharedHeld::default();
        let e = Early::new(held.clone());
        assert!(!deliver(&e, key(Key::A, true, Modifiers::NONE)));
        let (e, held, out) = armed(true, true);
        let busy = held.borrow_for_test();
        assert!(!deliver(&e, key(Key::A, true, Modifiers::NONE)));
        drop(busy);
        assert!(out.take().is_empty());
        e.disarm();
        assert!(!current().is_some_and(|e| deliver(&e, key(Key::A, true, Modifiers::NONE))));
    }
    #[test]
    fn translated_events_stop_at_clipboard_and_keep_later_keys_in_order() {
        let (e, _, out) = armed(true, true);
        let a = Event::Key {
            key: Key::A,
            physical_key: Some(Key::A),
            pressed: true,
            repeat: false,
            modifiers: Modifiers::NONE,
        };
        e.on_event(None, &[a.clone(), Event::Text("a".into())], Modifiers::NONE);
        assert_eq!(out.take(), [Sent::Key(0x41, true, 0)]);
        assert!(e.armed(), "text from a physical key adds no barrier");
        e.on_event(None, &[Event::Paste("hello".into()), a], Modifiers::NONE);
        assert!(out.take().is_empty());
        assert!(!e.armed(), "a paste and everything after it wait together");
    }

    #[test]
    fn focus_loss_releases_early_input_before_any_frame() {
        let (e, held, out) = armed(true, true);
        deliver(&e, key(Key::A, true, SHIFT));
        deliver(
            &e,
            Native::Button {
                button: BUTTON_LEFT,
                down: true,
                mods: SHIFT,
                pos: None,
            },
        );
        out.take();
        e.on_event(Some(&WindowEvent::Focused(false)), &[], Modifiers::NONE);
        assert_eq!(
            out.take(),
            [
                Sent::Key(VK_SHIFT, false, 0),
                Sent::Key(0x41, false, 0),
                Sent::Button(BUTTON_LEFT, false),
                Sent::ReleaseAll,
            ]
        );
        assert!(!held.any_down());
        assert!(!e.armed());
        assert!(!deliver(&e, key(Key::A, true, Modifiers::NONE)));
    }

    #[test]
    fn focus_loss_releases_frame_input_without_ever_arming() {
        let held = SharedHeld::default();
        let e = Early::new(held.clone());
        let out = Rc::new(Recorder::default());
        out.connected.set(true);
        e.listen(out.clone());
        held.key(&*out, 0x57, true);
        out.take();
        assert!(!e.armed());
        e.on_event(Some(&WindowEvent::Focused(false)), &[], Modifiers::NONE);
        assert_eq!(out.take(), [Sent::Key(0x57, false, 0), Sent::ReleaseAll]);
        assert!(!held.any_down());
    }

    #[test]
    fn navigation_keys_wait_only_when_local_controls_can_take_focus() {
        for k in [
            Key::Tab,
            Key::ArrowUp,
            Key::ArrowDown,
            Key::ArrowLeft,
            Key::ArrowRight,
            Key::Escape,
        ] {
            for local_focus in [true, false] {
                let (e, _, out) = armed(true, false);
                e.0.borrow_mut().gate.as_mut().unwrap().local_focus = local_focus;
                assert_eq!(deliver(&e, key(k, true, Modifiers::NONE)), !local_focus);
                if local_focus {
                    assert!(out.take().is_empty());
                    assert!(!e.armed());
                } else {
                    assert_eq!(out.take(), [Sent::Key(input::vk(k).unwrap(), true, 0)]);
                }
            }
        }
    }

    #[test]
    fn geometry_changes_close_the_gate_until_the_frame_republishes_it() {
        let (e, _, out) = armed(true, true);
        e.on_event(
            Some(&WindowEvent::Resized(
                egui_winit::winit::dpi::PhysicalSize::new(800, 600),
            )),
            &[],
            Modifiers::NONE,
        );
        assert!(!deliver(&e, Native::Motion { dx: 10.0, dy: 2.0 }));
        assert!(out.take().is_empty());
    }

    #[test]
    fn egui_winit_calls_the_installed_hook_for_raw_motion_without_a_frame() {
        use egui_winit::winit::raw_window_handle::{DisplayHandle, HandleError, HasDisplayHandle};
        struct NoDisplay;
        impl HasDisplayHandle for NoDisplay {
            fn display_handle(&self) -> Result<DisplayHandle<'_>, HandleError> {
                Err(HandleError::Unavailable)
            }
        }
        let (e, _, out) = armed(true, true);
        let ctx = egui::Context::default();
        install(&ctx);
        let mut state = egui_winit::State::new(
            ctx.clone(),
            egui::ViewportId::ROOT,
            &NoDisplay,
            Some(1.0),
            None,
            None,
        );
        state.on_mouse_motion((3.0, -2.0));
        assert_eq!(out.take(), [Sent::Motion(3, -2)]);
        assert_eq!(
            state.egui_input().events,
            [Event::MouseMoved(Vec2::new(3.0, -2.0))]
        );
        let hook = ctx
            .data(|d| d.get_temp::<egui_winit::EventHook>(egui_winit::EventHook::id()))
            .unwrap();
        (hook.on_frame)(egui::ViewportId::ROOT);
        assert!(e.claim(Ahead::Motion(3.0, -2.0)));
        assert!(!e.claim(Ahead::Motion(3.0, -2.0)));
        assert!(!e.armed(), "dispatch waits while the UI changes its policy");
        let mut other = egui_winit::State::new(
            ctx,
            egui::ViewportId::from_hash_of("other"),
            &NoDisplay,
            Some(1.0),
            None,
            None,
        );
        other.on_mouse_motion((20.0, 0.0));
        assert!(out.take().is_empty());
    }
}
