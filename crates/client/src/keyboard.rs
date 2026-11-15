//! Native physical keys, alongside egui's text/IME and clipboard events.
//! Uses the same egui-winit callback as the event-time dispatch work.

use egui::{Event, Modifiers};
use egui_winit::winit::keyboard::NativeKeyCode;
#[cfg(any(target_os = "macos", test))]
use egui_winit::winit::platform::scancode::PhysicalKeyExtScancode;
use egui_winit::winit::{
    event::WindowEvent,
    keyboard::{KeyCode, NamedKey, PhysicalKey},
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

/// True sided Ctrl/Meta swap. The caller chooses its shortcut scope.
pub fn shortcut(vk: u16, viewer_mac: bool, host_mac: bool, map: bool) -> u16 {
    if !map || viewer_mac == host_mac {
        return vk;
    }
    match vk {
        0x5b => 0xa2,
        0x5c => 0xa3,
        0xa2 => 0x5b,
        0xa3 => 0x5c,
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
    key_positions: bool,
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

#[cfg(test)]
pub(crate) fn test_frame(frame: Frame) {
    STATE.with_borrow_mut(|s| s.frame = Some(frame));
}

#[cfg(test)]
pub(crate) fn test_install(installed: bool) {
    STATE.with_borrow_mut(|s| {
        s.installed = installed;
        s.key_positions = installed;
    });
}

pub fn available() -> bool {
    STATE.with_borrow(|s| s.installed && s.key_positions)
}

pub fn key_positions(accepted: bool) {
    STATE.with_borrow_mut(|s| s.key_positions = accepted);
}

/// System shortcut capture requires deliberate fullscreen/mouse capture.
pub fn capture_system_keys(keys: bool, fullscreen: bool, mouse: bool) -> bool {
    keys && (fullscreen || mouse)
}

fn event_key(
    physical: PhysicalKey,
    logical: &egui_winit::winit::keyboard::Key,
    iso: bool,
    positions: bool,
) -> Option<KeyCode> {
    if !positions || *logical == egui_winit::winit::keyboard::Key::Named(NamedKey::Process) {
        return None;
    }
    native_code(physical, iso)
}

fn native_code(physical: PhysicalKey, iso: bool) -> Option<KeyCode> {
    let scan = match physical {
        PhysicalKey::Unidentified(NativeKeyCode::MacOS(code)) => Some(code),
        #[cfg(target_os = "macos")]
        PhysicalKey::Code(_) => physical.to_scancode().map(|v| v as u16),
        _ => None,
    };
    if let Some(scan) = scan {
        return Some(match latch_proto::keyboard::apple_iso(scan, iso) {
            0x0a => KeyCode::IntlBackslash,
            0x32 => KeyCode::Backquote,
            0x5e => KeyCode::IntlRo,
            0x5f => KeyCode::NumpadComma,
            0x66 => KeyCode::Lang2,
            0x68 => KeyCode::Lang1,
            // Mac Clear must not toggle a PC host's Num Lock.
            0x47 => return None,
            _ => match physical {
                PhysicalKey::Code(code) => code,
                _ => return None,
            },
        });
    }
    match physical {
        PhysicalKey::Code(code) => Some(code),
        _ => None,
    }
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
        #[cfg(target_os = "macos")]
        let iso = latch_proto::keyboard::apple_keyboard_is_iso();
        #[cfg(not(target_os = "macos"))]
        let iso = false;
        let Some(code) = event_key(
            event.physical_key,
            &event.logical_key,
            iso,
            self.key_positions,
        ) else {
            self.pending
                .extend(events.iter().cloned().map(|e| (e, None, false)));
            return;
        };
        self.logical_modifier(code, &event.logical_key, event.state.is_pressed());
        self.key(
            code,
            event.state.is_pressed(),
            event.repeat,
            matches!(event.logical_key, egui_winit::winit::keyboard::Key::Dead(_)),
            events,
            mods,
        );
    }

    fn logical_modifier(
        &mut self,
        code: KeyCode,
        logical: &egui_winit::winit::keyboard::Key,
        down: bool,
    ) {
        if code == KeyCode::AltRight {
            self.alt_gr =
                down && *logical == egui_winit::winit::keyboard::Key::Named(NamedKey::AltGraph);
        }
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
        if !self.key_positions {
            self.pending
                .extend(events.iter().cloned().map(|e| (e, None, false)));
            return;
        }
        #[cfg(windows)]
        if vk(code).is_some_and(|vk| windows::take_forwarded(Key { vk, down })) {
            return;
        }
        if let Some(vk) = vk(code) {
            if down {
                self.pressed.insert(vk);
            } else {
                self.pressed.remove(&vk);
            }
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
        if matches!(
            code,
            KeyCode::AudioVolumeMute
                | KeyCode::AudioVolumeDown
                | KeyCode::AudioVolumeUp
                | KeyCode::MediaTrackNext
                | KeyCode::MediaTrackPrevious
                | KeyCode::MediaStop
                | KeyCode::MediaPlayPause
        ) {
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
    fn process_keys_stay_with_ime_and_altgr_requires_layout_identity() {
        use egui_winit::winit::keyboard::Key as Logical;
        let physical = PhysicalKey::Code(KeyCode::KeyA);
        assert!(event_key(physical, &Logical::Named(NamedKey::Process), false, true).is_none());
        assert!(event_key(physical, &Logical::Character("a".into()), false, false).is_none());
        assert_eq!(
            event_key(physical, &Logical::Character("a".into()), false, true),
            Some(KeyCode::KeyA)
        );
        let mut state = State::default();
        for logical in [
            Logical::Named(NamedKey::Alt),
            Logical::Named(NamedKey::AltGraph),
        ] {
            state.logical_modifier(KeyCode::AltRight, &logical, true);
            assert_eq!(state.alt_gr, logical == Logical::Named(NamedKey::AltGraph));
            state.logical_modifier(KeyCode::AltRight, &logical, false);
            assert!(!state.alt_gr);
        }
    }

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
            let mut state = State {
                key_positions: true,
                ..State::default()
            };
            state.key(KeyCode::KeyQ, true, false, false, &[event], Modifiers::NONE);
            assert_eq!(state.pending[0].1.as_ref().unwrap().vk, 0x51);
        }
        let mut state = State {
            key_positions: true,
            ..State::default()
        };
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
        assert!(!state.alt_gr, "Right Alt alone is not evidence of AltGr");
        state.event(Some(&WindowEvent::Focused(false)), &[], Modifiers::NONE);
        assert!(!state.alt_gr);
        assert!(state.pressed.is_empty());
    }

    const CODES: &[KeyCode] = &[
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
    mod legacy {
        include!("keyboard/legacy_fixture.rs");
    }

    #[test]
    fn native_viewer_translation_crosses_each_host_map() {
        use latch_engine::input::keymap;
        assert_eq!(CODES.len(), latch_proto::keyboard::CASES.len());
        for viewer in ["windows", "macos", "linux"] {
            for (code, case) in CODES.iter().zip(latch_proto::keyboard::CASES) {
                let physical = if viewer == "macos" {
                    let Some(mac) = case.mac else { continue };
                    if matches!(mac, 0x0a | 0x32 | 0x5e | 0x5f | 0x66 | 0x68 | 0x47) {
                        PhysicalKey::Unidentified(NativeKeyCode::MacOS(
                            latch_proto::keyboard::apple_iso(mac, true),
                        ))
                    } else if cfg!(target_os = "macos") {
                        PhysicalKey::from_scancode(u32::from(mac))
                    } else {
                        PhysicalKey::Code(*code)
                    }
                } else if (viewer == "windows" && cfg!(windows))
                    || (viewer == "linux" && cfg!(target_os = "linux"))
                {
                    let scan = if viewer == "windows" {
                        u32::from(case.scan) | if case.extended { 0xe000 } else { 0 }
                    } else {
                        u32::from(case.linux - 8)
                    };
                    PhysicalKey::from_scancode(scan)
                } else {
                    PhysicalKey::Code(*code)
                };
                // Mac media are system-defined events and Clear stays local.
                if viewer == "macos"
                    && (matches!(case.vk, 0xad..=0xb3 | 0x90)
                        || matches!(case.vk, 0x188 | 0x18a | 0x18b))
                {
                    continue;
                }
                let native_viewer = viewer == "macos"
                    || (viewer == "windows" && cfg!(windows))
                    || (viewer == "linux" && cfg!(target_os = "linux"));
                let translated = if native_viewer {
                    native_code(physical, viewer == "macos").and_then(vk)
                } else {
                    vk(*code)
                }
                .unwrap_or_else(|| panic!("unmapped {viewer} {code:?}"));
                assert_eq!(translated, case.vk, "{viewer}, {code:?}");
                // Each host runs its production injection lookup, including
                // different sided flags and native numeric code spaces.
                assert_eq!(keymap::us_scan(translated), Some(case.scan));
                assert_eq!(keymap::extended(translated), case.extended);
                assert_eq!(keymap::vk_to_mac(translated), case.mac);
                let name = keymap::vk_to_xkb_name(translated).unwrap();
                assert_eq!(
                    std::str::from_utf8(&name).unwrap().trim_end_matches('\0'),
                    case.xkb
                );
            }
        }
    }

    #[test]
    fn legacy_full_position_replay_matches_600_events_and_text() {
        for code in CODES {
            let mut upstream = Vec::new();
            for down in [true, false] {
                if let Some(key) = egui_winit::key_from_key_code(*code) {
                    upstream.push(Event::Key {
                        key,
                        physical_key: Some(key),
                        pressed: down,
                        repeat: false,
                        modifiers: Modifiers::NONE,
                    });
                }
                if down
                    && matches!(
                        code,
                        KeyCode::KeyA
                            | KeyCode::KeyQ
                            | KeyCode::KeyY
                            | KeyCode::IntlBackslash
                            | KeyCode::IntlRo
                            | KeyCode::IntlYen
                            | KeyCode::Numpad0
                            | KeyCode::Numpad1
                            | KeyCode::Numpad2
                            | KeyCode::Numpad3
                            | KeyCode::Numpad4
                            | KeyCode::Numpad5
                            | KeyCode::Numpad6
                            | KeyCode::Numpad7
                            | KeyCode::Numpad8
                            | KeyCode::Numpad9
                            | KeyCode::NumpadMultiply
                            | KeyCode::NumpadAdd
                            | KeyCode::NumpadComma
                            | KeyCode::NumpadSubtract
                            | KeyCode::NumpadDecimal
                            | KeyCode::NumpadDivide
                            | KeyCode::NumpadEqual
                    )
                {
                    upstream.push(Event::Text("<>".into()));
                }
            }
            let mut state = State::default();
            state.key(*code, true, false, false, &upstream, Modifiers::NONE);
            assert!(state.pending.iter().all(|(_, key, _)| key.is_none()));
            assert_eq!(
                state
                    .pending
                    .iter()
                    .map(|(e, _, _)| e.clone())
                    .collect::<Vec<_>>(),
                upstream
            );
            let render = |events: Vec<Event>, historical: bool| {
                let physical = events.iter().any(|e| matches!(e, Event::Key { key, pressed: true, .. } if if historical { legacy::vk(*key).is_some() } else { crate::input::vk(*key).is_some() }));
                events
                    .into_iter()
                    .filter_map(|e| match e {
                        Event::Key { key, pressed, .. } => (if historical {
                            legacy::vk(key)
                        } else {
                            crate::input::vk(key)
                        })
                        .map(|vk| latch_proto::input::Event::Key {
                            vk: vk as u16,
                            down: pressed,
                            modifiers: 0,
                        }),
                        Event::Text(t) if !physical => Some(latch_proto::input::Event::Text(t)),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
            };
            let actual = render(
                state.pending.into_iter().map(|(e, _, _)| e).collect(),
                false,
            );
            let expected = render(upstream, true);
            assert_eq!(actual, expected, "{code:?}");
            let mut sender = latch_proto::input::InputSender::new(1);
            for event in actual {
                sender.push(event);
            }
            let mut bytes = Vec::new();
            assert_eq!(
                sender.poll(std::time::Instant::now(), &mut bytes),
                !expected.is_empty()
            );
            if !expected.is_empty() {
                let latch_proto::wire::Datagram::Input { events, .. } =
                    latch_proto::wire::parse(&bytes).unwrap()
                else {
                    panic!()
                };
                assert_eq!(events.iter().collect::<Vec<_>>(), expected, "{code:?}");
            }
        }
        let legacy_codes: BTreeSet<_> = egui::Key::ALL
            .iter()
            .filter_map(|&key| legacy::vk(key))
            .collect();
        for new_key in [
            0xe2, 0x15, 0x19, 0x1c, 0x1d, 0xad, 0xae, 0xaf, 0xb0, 0xb1, 0xb2, 0xb3,
        ] {
            assert!(!legacy_codes.contains(&new_key));
        }
    }

    #[test]
    fn apple_missing_positions_and_iso_swap_are_native() {
        for (scan, iso, expected) in [
            (0x0a, false, KeyCode::IntlBackslash),
            (0x32, true, KeyCode::IntlBackslash),
            (0x0a, true, KeyCode::Backquote),
            (0x5e, false, KeyCode::IntlRo),
            (0x5f, false, KeyCode::NumpadComma),
            (0x66, false, KeyCode::Lang2),
            (0x68, false, KeyCode::Lang1),
        ] {
            assert_eq!(
                native_code(PhysicalKey::Unidentified(NativeKeyCode::MacOS(scan)), iso),
                Some(expected)
            );
        }
        assert!(
            native_code(PhysicalKey::Unidentified(NativeKeyCode::MacOS(0x47)), false).is_none()
        );
    }

    #[test]
    fn system_capture_requires_focused_capture_and_media_remain_local() {
        for keys in [false, true] {
            for full in [false, true] {
                for mouse in [false, true] {
                    assert_eq!(
                        capture_system_keys(keys, full, mouse),
                        keys && (full || mouse)
                    );
                }
            }
        }
        for code in [
            KeyCode::AudioVolumeMute,
            KeyCode::AudioVolumeDown,
            KeyCode::AudioVolumeUp,
            KeyCode::MediaTrackPrevious,
            KeyCode::MediaPlayPause,
            KeyCode::MediaTrackNext,
            KeyCode::MediaStop,
        ] {
            let mut state = State {
                key_positions: true,
                ..State::default()
            };
            state.key(code, true, false, false, &[], Modifiers::NONE);
            assert!(state.pending.is_empty());
        }
    }
}
