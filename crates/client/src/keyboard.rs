//! Native physical keys, alongside egui's text/IME and clipboard events.
//! Uses the same egui-winit callback as the event-time dispatch work.

use egui::{Event, Modifiers};
use egui_winit::winit::{
    event::WindowEvent,
    keyboard::{KeyCode, PhysicalKey},
};
use std::cell::RefCell;
use std::collections::BTreeSet;

/// VKs name US physical positions. Values above 255 name negotiated HID
/// positions, as documented in `latch_proto::keyboard`.
pub fn vk(code: KeyCode) -> Option<u16> {
    use latch_proto::keyboard::*;
    use KeyCode::*;
    Some(match code {
        ShiftLeft => 0xa0,
        ShiftRight => 0xa1,
        ControlLeft => 0xa2,
        ControlRight => 0xa3,
        AltLeft => 0xa4,
        AltRight => 0xa5,
        SuperLeft => 0x5b,
        SuperRight => 0x5c,
        IntlBackslash => 0xe2,
        IntlRo => INTL_RO,
        IntlYen => INTL_YEN,
        NumpadEnter => KEYPAD_ENTER,
        NumpadEqual => KEYPAD_EQUAL,
        Numpad0 => 0x60,
        Numpad1 => 0x61,
        Numpad2 => 0x62,
        Numpad3 => 0x63,
        Numpad4 => 0x64,
        Numpad5 => 0x65,
        Numpad6 => 0x66,
        Numpad7 => 0x67,
        Numpad8 => 0x68,
        Numpad9 => 0x69,
        NumpadMultiply => 0x6a,
        NumpadAdd => 0x6b,
        NumpadComma => 0x6c,
        NumpadSubtract => 0x6d,
        NumpadDecimal => 0x6e,
        NumpadDivide => 0x6f,
        CapsLock => 0x14,
        NumLock => 0x90,
        ScrollLock => 0x91,
        ContextMenu => 0x5d,
        PrintScreen => 0x2c,
        Pause => 0x13,
        Lang1 => 0x15,
        Lang2 => 0x19,
        KanaMode => KANA,
        Convert => CONVERT,
        NonConvert => NON_CONVERT,
        AudioVolumeMute => 0xad,
        AudioVolumeDown => 0xae,
        AudioVolumeUp => 0xaf,
        MediaTrackNext => 0xb0,
        MediaTrackPrevious => 0xb1,
        MediaStop => 0xb2,
        MediaPlayPause => 0xb3,
        _ => {
            return egui_winit::key_from_key_code(code)
                .and_then(crate::input::vk)
                .map(|v| v as u16)
        }
    })
}

/// Preserve the existing default on Mac viewers. On W/L viewers the same
/// setting maps Ctrl to Command only for a Mac host. Super always stays Meta.
pub fn shortcut(vk: u16, viewer_mac: bool, host_mac: bool, map: bool) -> u16 {
    if !map || viewer_mac == host_mac {
        return vk;
    }
    match (viewer_mac, vk) {
        (true, 0x5b) => 0xa2,
        (true, 0x5c) => 0xa3,
        (false, 0xa2) => 0x5b,
        (false, 0xa3) => 0x5c,
        _ => vk,
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Key {
    pub vk: u16,
    pub down: bool,
}

/// The final flag marks literal dead-key text, independent of other keys in a frame.
pub type Frame = Vec<(Event, Option<Key>, bool)>;

#[derive(Default)]
struct State {
    pending: Frame,
    latest: Frame,
    frame: Option<Frame>,
    dead: bool,
    text_keys: BTreeSet<KeyCode>,
    alt_gr: bool,
    installed: bool,
    owned: bool,
    pressed: BTreeSet<u16>,
}

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State::default());
}

pub fn install(cc: &eframe::CreationContext<'_>) {
    let ctx = &cc.egui_ctx;
    STATE.with_borrow_mut(|s| s.installed = true);
    ctx.data_mut(|d| {
        d.insert_temp(
            egui_winit::EventHook::keyboard_id(),
            egui_winit::EventHook {
                on_event: |viewport, native, events, mods| {
                    if viewport == egui::ViewportId::ROOT {
                        STATE.with_borrow_mut(|s| {
                            let first = s.pending.len();
                            s.event(native, events, mods);
                            s.latest = s.pending[first..].to_vec();
                        });
                    }
                },
                on_frame: |viewport| {
                    if viewport == egui::ViewportId::ROOT {
                        STATE.with_borrow_mut(|s| s.frame = Some(std::mem::take(&mut s.pending)));
                    }
                },
            },
        )
    });
    #[cfg(windows)]
    windows::install();
    #[cfg(target_os = "linux")]
    linux::install(cc);
}

pub fn take() -> Option<Frame> {
    STATE.with_borrow_mut(|s| s.frame.take())
}

/// The native-key observer runs before the event-time dispatcher.
pub fn latest() -> Option<Frame> {
    STATE.with_borrow(|s| s.installed.then(|| s.latest.clone()))
}

pub fn mapped(vk: u16, host_mac: bool, map: bool) -> u16 {
    if alt_gr() && matches!(vk, 0xa2 | 0xa3) {
        vk
    } else {
        shortcut(vk, cfg!(target_os = "macos"), host_mac, map)
    }
}

#[cfg(test)]
pub(crate) fn test_frame(frame: Frame) {
    STATE.with_borrow_mut(|s| s.frame = Some(frame));
}

pub fn available() -> bool {
    STATE.with_borrow(|s| s.installed)
}

pub fn alt_gr() -> bool {
    STATE.with_borrow(|s| s.alt_gr)
}

pub fn owns_keys(owned: bool) {
    STATE.with_borrow_mut(|s| {
        if owned && !s.owned {
            let mut modifiers = Vec::new();
            for &vk in &s.pressed {
                if matches!(vk, 0xa0..=0xa5 | 0x5b | 0x5c) {
                    modifiers.push((
                        Event::Key {
                            key: egui::Key::F35,
                            physical_key: None,
                            pressed: true,
                            repeat: false,
                            modifiers: Modifiers::NONE,
                        },
                        Some(Key { vk, down: true }),
                        false,
                    ));
                }
            }
            if let Some(frame) = &mut s.frame {
                frame.splice(0..0, modifiers);
            }
        }
        s.owned = owned;
    });
    #[cfg(windows)]
    windows::owns_keys(owned);
    #[cfg(target_os = "linux")]
    linux::owns_keys(owned);
    #[cfg(target_os = "macos")]
    let _ = owned;
}

impl State {
    fn event(&mut self, native: Option<&WindowEvent>, events: &[Event], mods: Modifiers) {
        if matches!(native, Some(WindowEvent::Focused(false))) {
            #[cfg(windows)]
            windows::owns_keys(false);
            #[cfg(target_os = "linux")]
            linux::owns_keys(false);
            self.alt_gr = false;
            self.dead = false;
            self.text_keys.clear();
            self.pressed.clear();
            self.owned = false;
        }
        let Some(WindowEvent::KeyboardInput { event, .. }) = native else {
            self.pending
                .extend(events.iter().cloned().map(|e| (e, None, false)));
            return;
        };
        let PhysicalKey::Code(code) = event.physical_key else {
            self.pending
                .extend(events.iter().cloned().map(|e| (e, None, false)));
            return;
        };
        self.key(
            code,
            event.state.is_pressed(),
            event.repeat,
            matches!(event.logical_key, egui_winit::winit::keyboard::Key::Dead(_)),
            events,
            mods,
        );
    }

    fn key(
        &mut self,
        code: KeyCode,
        down: bool,
        repeat: bool,
        dead: bool,
        events: &[Event],
        mods: Modifiers,
    ) {
        if let Some(vk) = vk(code) {
            if down {
                self.pressed.insert(vk);
            } else {
                self.pressed.remove(&vk);
            }
        }
        if code == KeyCode::AltRight {
            self.alt_gr = down;
        }
        // Caps Lock on macOS is a state change, already converted to a tap
        // by the stream. Other platforms deliver a normal down/up pair.
        if cfg!(target_os = "macos") && code == KeyCode::CapsLock {
            return;
        }
        if down && (dead || self.dead) {
            self.text_keys.insert(code);
            self.dead = dead || !events.iter().any(|e| matches!(e, Event::Text(_)));
        }
        if self.text_keys.contains(&code) {
            self.pending.extend(
                events
                    .iter()
                    .filter(|e| !matches!(e, Event::Key { .. }))
                    .cloned()
                    .map(|e| {
                        let literal = matches!(e, Event::Text(_));
                        (e, None, literal)
                    }),
            );
            if !down {
                self.text_keys.remove(&code);
            }
            return;
        }
        // egui translates clipboard shortcuts into Copy/Cut/Paste events.
        // Their existing ordering with clipboard transport must be retained.
        if events
            .iter()
            .any(|e| matches!(e, Event::Copy | Event::Cut | Event::Paste(_)))
        {
            self.pending
                .extend(events.iter().cloned().map(|e| (e, None, false)));
            return;
        }
        let Some(vk) = vk(code) else {
            self.pending
                .extend(events.iter().cloned().map(|e| (e, None, false)));
            return;
        };
        let key = Key { vk, down };
        let mut attached = false;
        for e in events {
            if matches!(e, Event::Key { .. }) && !attached {
                self.pending.push((e.clone(), Some(key), false));
                attached = true;
            } else {
                self.pending.push((e.clone(), None, false));
            }
        }
        if !attached {
            // This synthetic egui event exists only in the remote-input
            // queue. The UI continues to receive upstream egui input.
            self.pending.push((
                Event::Key {
                    key: egui::Key::F35,
                    physical_key: None,
                    pressed: down,
                    repeat,
                    modifiers: mods,
                },
                Some(key),
                false,
            ));
        }
    }
}

pub fn text_position(vk: u16) -> bool {
    matches!(vk, 0x20 | 0x30..=0x39 | 0x41..=0x5a | 0x60..=0x6f | 0xba..=0xc0 | 0xdb..=0xe2
        | latch_proto::keyboard::KEYPAD_EQUAL | latch_proto::keyboard::INTL_RO | latch_proto::keyboard::INTL_YEN)
}

#[cfg(target_os = "linux")]
mod linux;
#[cfg(windows)]
mod windows;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layouts_keep_positions_and_dead_keys_deliver_one_composed_text() {
        for logical in [egui::Key::Q, egui::Key::A] {
            let event = Event::Key {
                key: logical,
                physical_key: Some(egui::Key::Q),
                pressed: true,
                repeat: false,
                modifiers: Modifiers::NONE,
            };
            let mut state = State::default();
            state.key(KeyCode::KeyQ, true, false, false, &[event], Modifiers::NONE);
            assert_eq!(state.pending[0].1.as_ref().unwrap().vk, 0x51);
        }
        let mut state = State::default();
        state.key(KeyCode::Quote, true, false, true, &[], Modifiers::NONE);
        state.key(KeyCode::Quote, false, false, true, &[], Modifiers::NONE);
        let key = Event::Key {
            key: egui::Key::E,
            physical_key: Some(egui::Key::E),
            pressed: true,
            repeat: false,
            modifiers: Modifiers::NONE,
        };
        state.key(
            KeyCode::KeyE,
            true,
            false,
            false,
            &[key, Event::Text("é".into())],
            Modifiers::NONE,
        );
        state.key(KeyCode::KeyE, false, false, false, &[], Modifiers::NONE);
        assert_eq!(state.pending.len(), 1);
        assert_eq!(state.pending[0].0, Event::Text("é".into()));
        assert!(state.pending[0].1.is_none());
        assert!(state.pending[0].2);
        state.key(KeyCode::AltRight, true, false, false, &[], Modifiers::ALT);
        assert!(state.alt_gr);
        state.event(Some(&WindowEvent::Focused(false)), &[], Modifiers::NONE);
        assert!(!state.alt_gr);
        assert!(state.pressed.is_empty());
    }

    #[test]
    fn native_positions_and_shortcut_policy_cover_every_os_pair() {
        let codes = [
            KeyCode::KeyA,
            KeyCode::KeyQ,
            KeyCode::KeyY,
            KeyCode::IntlBackslash,
            KeyCode::ShiftLeft,
            KeyCode::ShiftRight,
            KeyCode::ControlLeft,
            KeyCode::ControlRight,
            KeyCode::AltLeft,
            KeyCode::AltRight,
            KeyCode::SuperLeft,
            KeyCode::SuperRight,
            KeyCode::Numpad0,
            KeyCode::NumpadEnter,
            KeyCode::NumpadEqual,
            KeyCode::IntlRo,
            KeyCode::IntlYen,
            KeyCode::Lang1,
            KeyCode::Lang2,
            KeyCode::F13,
            KeyCode::F24,
            KeyCode::CapsLock,
            KeyCode::NumLock,
            KeyCode::AudioVolumeMute,
            KeyCode::F14,
            KeyCode::F15,
            KeyCode::F16,
            KeyCode::F17,
            KeyCode::F18,
            KeyCode::F19,
            KeyCode::F20,
            KeyCode::F21,
            KeyCode::F22,
            KeyCode::F23,
            KeyCode::Numpad1,
            KeyCode::Numpad2,
            KeyCode::Numpad3,
            KeyCode::Numpad4,
            KeyCode::Numpad5,
            KeyCode::Numpad6,
            KeyCode::Numpad7,
            KeyCode::Numpad8,
            KeyCode::Numpad9,
            KeyCode::NumpadMultiply,
            KeyCode::NumpadAdd,
            KeyCode::NumpadComma,
            KeyCode::NumpadSubtract,
            KeyCode::NumpadDecimal,
            KeyCode::NumpadDivide,
            KeyCode::AudioVolumeDown,
            KeyCode::AudioVolumeUp,
            KeyCode::MediaTrackNext,
            KeyCode::MediaTrackPrevious,
            KeyCode::MediaStop,
            KeyCode::MediaPlayPause,
            KeyCode::KanaMode,
            KeyCode::Convert,
            KeyCode::NonConvert,
        ];
        assert_eq!(codes.len(), latch_proto::keyboard::CASES.len());
        for viewer in ["windows", "macos", "linux"] {
            for host in ["windows", "macos", "linux"] {
                for (code, case) in codes.iter().zip(latch_proto::keyboard::CASES) {
                    assert_eq!(vk(*code), Some(case.vk), "{viewer} -> {host}, {code:?}");
                    assert_eq!(
                        shortcut(case.vk, viewer == "macos", host == "macos", false),
                        case.vk
                    );
                }
                let expected = if viewer == "macos" && host != "macos" {
                    0xa2
                } else {
                    0x5b
                };
                assert_eq!(
                    shortcut(0x5b, viewer == "macos", host == "macos", true),
                    expected
                );
            }
        }
    }
}
