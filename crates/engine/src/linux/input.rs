//! XTest uses server keycodes, cached from the current keyboard mapping.
//! Text borrows an unused keycode and leaves time for clients to consume
//! each mapping before restoring it. Grabbing the server would prevent
//! their MappingNotify handlers from fetching the temporary keysym.

use super::X11;
use crate::input::{DisplayRect, Event, Held, InputSink};
use anyhow::{ensure, Context};
use std::collections::BTreeMap;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::ConnectionExt as _;
use x11rb::protocol::{xkb, xproto, xtest, Event as XEvent};

pub(super) struct X11Input {
    conn: X11,
    root: u32,
    screen: DisplayRect,
    selected: bool,
    mapping: xproto::GetKeyboardMappingReply,
    min_keycode: u8,
    key_names: xkb::GetNamesReply,
    held: Held,
    pressed: BTreeMap<u16, u8>,
    scroll: [i32; 2],
}
impl X11Input {
    pub(super) fn new() -> anyhow::Result<Self> {
        let (conn, screen) =
            super::connect(super::X11_SILENCE).context("connect to X11 for XTest")?;
        xtest::get_version(&conn, 2, 2)?.reply()?;
        ensure!(
            xkb::use_extension(&conn, 1, 0)?.reply()?.supported,
            "XKB unavailable"
        );
        let key_names = xkb::get_names(
            &conn,
            xkb::ID::USE_CORE_KBD.into(),
            xkb::NameDetail::KEY_NAMES,
        )?
        .reply()?;
        let setup = conn.setup();
        let root = setup.roots[screen].root;
        let screen = DisplayRect {
            width: setup.roots[screen].width_in_pixels.into(),
            height: setup.roots[screen].height_in_pixels.into(),
            ..Default::default()
        };
        let min_keycode = setup.min_keycode;
        let mapping = conn
            .get_keyboard_mapping(min_keycode, setup.max_keycode - min_keycode + 1)?
            .reply()?;
        conn.change_window_attributes(
            root,
            &xproto::ChangeWindowAttributesAux::new()
                .event_mask(xproto::EventMask::STRUCTURE_NOTIFY),
        )?
        .check()?;
        Ok(Self {
            conn,
            root,
            screen,
            selected: false,
            mapping,
            min_keycode,
            key_names,
            held: Held::default(),
            pressed: BTreeMap::new(),
            scroll: [0; 2],
        })
    }
    fn refresh(&mut self) -> anyhow::Result<()> {
        let mut changed = false;
        while let Some(event) = self.conn.poll_for_event()? {
            match event {
                XEvent::MappingNotify(_) => changed = true,
                XEvent::ConfigureNotify(event) if event.window == self.root => {
                    self.screen.width = event.width.into();
                    self.screen.height = event.height.into();
                }
                XEvent::Error(error) => anyhow::bail!("X11 input error: {error:?}"),
                _ => {}
            }
        }
        if changed {
            self.key_names = xkb::get_names(
                &self.conn,
                xkb::ID::USE_CORE_KBD.into(),
                xkb::NameDetail::KEY_NAMES,
            )?
            .reply()?;
            self.mapping = self
                .conn
                .get_keyboard_mapping(
                    self.min_keycode,
                    self.conn.setup().max_keycode - self.min_keycode + 1,
                )?
                .reply()?;
        }
        Ok(())
    }
    fn fake(&self, kind: u8, detail: u8, x: i16, y: i16) -> anyhow::Result<()> {
        xtest::fake_input(
            &self.conn,
            kind,
            detail,
            x11rb::CURRENT_TIME,
            self.root,
            x,
            y,
            0,
        )?
        .check()?;
        Ok(())
    }
    fn button(&self, button: u8, down: bool) -> anyhow::Result<()> {
        self.fake(
            if down {
                xproto::BUTTON_PRESS_EVENT
            } else {
                xproto::BUTTON_RELEASE_EVENT
            },
            button,
            0,
            0,
        )
    }
    fn text(&mut self, text: &str) -> anyhow::Result<()> {
        let per = self.mapping.keysyms_per_keycode as usize;
        ensure!(per > 0, "empty X keyboard mapping");
        let keys = self.conn.query_keymap()?.reply()?.keys;
        let spare = self
            .mapping
            .keysyms
            .chunks(per)
            .enumerate()
            .rev()
            .find(|(i, symbols)| {
                let code = usize::from(self.min_keycode) + i;
                symbols.iter().all(|&s| s == 0) && keys[code / 8] & (1 << (code % 8)) == 0
            })
            .map(|(i, symbols)| (self.min_keycode + i as u8, symbols.to_vec()))
            .context("no unused X keycode for Unicode text")?;
        let result: anyhow::Result<()> = (|| {
            for ch in text.chars() {
                let mut symbols = vec![0; per];
                // Bind both shift levels to avoid changing text under held Shift.
                symbols.fill(unicode_keysym(ch));
                self.conn
                    .change_keyboard_mapping(1, spare.0, per as u8, &symbols)?
                    .check()?;
                self.fake(xproto::KEY_PRESS_EVENT, spare.0, 0, 0)?;
                self.fake(xproto::KEY_RELEASE_EVENT, spare.0, 0, 0)?;
                // X11 has no acknowledgement that the focused application has
                // looked up a keycode. Match the pacing used by text injectors.
                std::thread::sleep(std::time::Duration::from_millis(12));
            }
            Ok(())
        })();
        let release = if result.is_err() {
            self.fake(xproto::KEY_RELEASE_EVENT, spare.0, 0, 0)
        } else {
            Ok(())
        };
        let restore: anyhow::Result<()> = (|| {
            self.conn
                .change_keyboard_mapping(1, spare.0, per as u8, &spare.1)?
                .check()?;
            Ok(())
        })();
        result?;
        release?;
        restore?;
        Ok(())
    }
}
fn unicode_keysym(ch: char) -> u32 {
    let code = u32::from(ch);
    match ch {
        '\n' | '\r' => 0xff0d,
        '\t' => 0xff09,
        _ if code <= 0xff => code,
        _ => 0x01000000 | code,
    }
}
fn button_code(button: u8) -> anyhow::Result<u8> {
    match button {
        1..=3 => Ok(button),
        4 => Ok(8),
        5 => Ok(9),
        _ => anyhow::bail!("unknown mouse button {button}"),
    }
}
fn notches(remainder: &mut i32, delta: i16) -> i32 {
    *remainder += i32::from(delta);
    let count = *remainder / 120;
    *remainder %= 120;
    count
}

fn keycode(first: u8, names: &[xkb::KeyName], vk: u16) -> anyhow::Result<u8> {
    let name = crate::input::keymap::vk_to_xkb_name(vk).context("unmapped virtual key")?;
    names
        .iter()
        .position(|key| key.name == name)
        .and_then(|i| first.checked_add(u8::try_from(i).ok()?))
        .context("physical key absent from XKB keyboard mapping")
}
impl InputSink for X11Input {
    fn set_display_selected(&mut self, selected: bool) {
        self.selected = selected;
    }

    fn apply(&mut self, event: &Event, display: DisplayRect) -> anyhow::Result<()> {
        self.refresh()?;
        match event {
            Event::UnknownPosition { .. } => {}
            Event::Key { vk, down, .. } => {
                let code = if let Some(&code) = self.pressed.get(vk) {
                    code
                } else {
                    if !down {
                        return Ok(());
                    }
                    keycode(
                        self.key_names.first_key,
                        self.key_names
                            .value_list
                            .key_names
                            .as_ref()
                            .context("XKB key names unavailable")?,
                        *vk,
                    )?
                };
                self.fake(
                    if *down {
                        xproto::KEY_PRESS_EVENT
                    } else {
                        xproto::KEY_RELEASE_EVENT
                    },
                    code,
                    0,
                    0,
                )?;
                self.held.note(event);
                if *down {
                    self.pressed.insert(*vk, code);
                } else {
                    self.pressed.remove(vk);
                }
            }
            Event::MousePosition {
                x,
                y,
                width,
                height,
            } => {
                let (x, y) = display.map(*x, *y, *width, *height);
                self.fake(
                    xproto::MOTION_NOTIFY_EVENT,
                    0,
                    i16::try_from(x)?,
                    i16::try_from(y)?,
                )?;
            }
            Event::MouseMove { dx, dy } => {
                let position =
                    relative_position(display, self.screen, self.selected, *dx, *dy, || {
                        let pointer = self.conn.query_pointer(self.root)?.reply()?;
                        Ok((pointer.root_x, pointer.root_y))
                    })?;
                if let Some((x, y)) = position {
                    self.fake(xproto::MOTION_NOTIFY_EVENT, 0, x, y)?;
                } else {
                    // XTest MotionNotify detail=1 interprets x/y as relative deltas.
                    // Drop the void cookie: errors are handled by refresh, without a round trip.
                    drop(xtest::fake_input(
                        &self.conn,
                        xproto::MOTION_NOTIFY_EVENT,
                        1,
                        x11rb::CURRENT_TIME,
                        x11rb::NONE,
                        *dx,
                        *dy,
                        0,
                    )?);
                }
            }
            Event::MouseButton { button, down } => {
                let code = button_code(*button)?;
                if *down || self.held.buttons.contains(button) {
                    self.button(code, *down)?;
                    self.held.note(event);
                }
            }
            Event::Scroll {
                vertical,
                horizontal,
            } => {
                for (axis, delta) in [*vertical, *horizontal].into_iter().enumerate() {
                    let count = notches(&mut self.scroll[axis], delta);
                    let button = match (axis, count > 0) {
                        (0, true) => 4,
                        (0, false) => 5,
                        (_, true) => 7,
                        (_, false) => 6,
                    };
                    for _ in 0..count.unsigned_abs() {
                        self.button(button, true)?;
                        self.button(button, false)?;
                    }
                }
            }
            Event::Text(text) => self.text(text)?,
            Event::ReleaseAll => {
                self.release_all();
                ensure!(
                    self.held.keys.is_empty() && self.held.buttons.is_empty(),
                    "input releases still pending"
                );
            }
        }
        self.conn.flush()?;
        Ok(())
    }
    fn release_all(&mut self) {
        for (vk, code) in std::mem::take(&mut self.pressed) {
            if let Err(e) = self.fake(xproto::KEY_RELEASE_EVENT, code, 0, 0) {
                tracing::warn!("release key: {e:#}");
                self.pressed.insert(vk, code);
            } else {
                self.held.keys.remove(&vk);
            }
        }
        for button in std::mem::take(&mut self.held.buttons) {
            if let Ok(code) = button_code(button) {
                if let Err(e) = self.button(code, false) {
                    tracing::warn!("release button: {e:#}");
                    self.held.buttons.insert(button);
                }
            }
        }
        self.scroll = [0; 2];
        let _ = self.conn.flush();
    }
}

/// Query after preceding XTest requests have run, then move absolutely so X
/// acceleration cannot carry us past an edge. Whole-screen sharing keeps the
/// existing relative XTest path, including acceleration and no round trip.
fn relative_position(
    display: DisplayRect,
    screen: DisplayRect,
    selected: bool,
    dx: i16,
    dy: i16,
    pointer: impl FnOnce() -> anyhow::Result<(i16, i16)>,
) -> anyhow::Result<Option<(i16, i16)>> {
    if display == screen && !selected {
        return Ok(None);
    }
    let (x, y) = pointer()?;
    let (x, y) = display.clamp(i32::from(x) + i32::from(dx), i32::from(y) + i32::from(dy));
    Ok(Some((i16::try_from(x)?, i16::try_from(y)?)))
}

impl Drop for X11Input {
    fn drop(&mut self) {
        self.release_all();
    }
}

#[cfg(test)]
mod pair_interop {
    use super::*;
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/pairs/native_input.rs"
    ));
    fn text_from_keysym(symbol: u32) -> char {
        #[link(name = "xkbcommon")]
        unsafe extern "C" {
            fn xkb_keysym_to_utf32(keysym: u32) -> u32;
        }
        // SAFETY: this pure conversion accepts any keysym value.
        let codepoint = unsafe { xkb_keysym_to_utf32(symbol) };
        assert_ne!(codepoint, 0, "scratch mapping expired before observation");
        char::from_u32(codepoint).expect("scratch mapping must contain a Unicode character")
    }

    #[test]
    fn text_observer_decodes_latin1_and_unicode_keysyms() {
        for (symbol, expected) in [
            (0xe9, 'é'),
            (0x010003bb, 'λ'),
            (0x01004f60, '你'),
            (0x0101f600, '😀'),
        ] {
            assert_eq!(text_from_keysym(symbol), expected);
        }
    }

    #[test]
    #[should_panic(expected = "scratch mapping expired before observation")]
    fn text_observer_rejects_restored_scratch_mapping() {
        text_from_keysym(0);
    }

    fn native(events: &[Event]) -> Vec<Value> {
        // Every event is posted only into the disposable Xvfb owned by pairs.py.
        assert_eq!(std::env::var("DISPLAY").unwrap(), ":9");
        assert_eq!(std::env::var("LATCH_PAIR_XVFB").unwrap(), "1");
        let (observer, screen) = x11rb::connect(None).unwrap();
        let root = observer.setup().roots[screen].root;
        let window = observer.generate_id().unwrap();
        observer
            .create_window(
                x11rb::COPY_DEPTH_FROM_PARENT,
                window,
                root,
                0,
                0,
                640,
                360,
                0,
                xproto::WindowClass::INPUT_OUTPUT,
                x11rb::COPY_FROM_PARENT,
                &xproto::CreateWindowAux::new().event_mask(
                    xproto::EventMask::KEY_PRESS
                        | xproto::EventMask::KEY_RELEASE
                        | xproto::EventMask::BUTTON_PRESS
                        | xproto::EventMask::BUTTON_RELEASE
                        | xproto::EventMask::POINTER_MOTION,
                ),
            )
            .unwrap()
            .check()
            .unwrap();
        observer.map_window(window).unwrap().check().unwrap();
        observer
            .set_input_focus(xproto::InputFocus::PARENT, window, x11rb::CURRENT_TIME)
            .unwrap()
            .check()
            .unwrap();
        observer
            .warp_pointer(x11rb::NONE, root, 0, 0, 0, 0, 320, 180)
            .unwrap()
            .check()
            .unwrap();
        observer.get_input_focus().unwrap().reply().unwrap();
        while observer.poll_for_event().unwrap().is_some() {}
        let mut sink = X11Input::new().unwrap();
        let display = DisplayRect {
            x: 0,
            y: 0,
            width: 640,
            height: 360,
        };
        let mut out = Vec::new();
        for event in events {
            let before = observer.query_pointer(root).unwrap().reply().unwrap();
            let expected_count = match event {
                Event::Text(t) => t.chars().count() * 2,
                Event::Scroll {
                    vertical,
                    horizontal,
                } => {
                    let vertical = (sink.scroll[0] + i32::from(*vertical)) / 120;
                    let horizontal = (sink.scroll[1] + i32::from(*horizontal)) / 120;
                    (vertical.unsigned_abs() + horizontal.unsigned_abs()) as usize * 2
                }
                _ => 1,
            };
            // Text remaps a scratch key for 12 ms. Read its keysym while the
            // production injector holds that mapping, using another connection.
            std::thread::scope(|scope| {
                let apply = scope.spawn(|| sink.apply(event, display).unwrap());
                let mut seen = 0;
                let mut text = match event {
                    Event::Text(text) => text.chars(),
                    _ => "".chars(),
                };
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
                while std::time::Instant::now() < deadline {
                    if let Some(native) = observer.poll_for_event().unwrap() {
                        match native {
                            XEvent::KeyPress(key) | XEvent::KeyRelease(key) => {
                                let down = key.response_type == xproto::KEY_PRESS_EVENT;
                                if matches!(event, Event::Text(_)) {
                                    if down {
                                        let mapping = observer
                                            .get_keyboard_mapping(key.detail, 1)
                                            .unwrap()
                                            .reply()
                                            .unwrap();
                                        let ch = text_from_keysym(mapping.keysyms[0]);
                                        assert_eq!(
                                            Some(ch),
                                            text.next(),
                                            "scratch mapping changed before observation"
                                        );
                                        for unit in ch.encode_utf16(&mut [0; 2]) {
                                            out.push(json!(["text", unit]));
                                        }
                                    }
                                } else {
                                    out.push(json!(["key", key.detail, down, 0]));
                                }
                                seen += 1;
                            }
                            XEvent::ButtonPress(button) | XEvent::ButtonRelease(button) => {
                                out.push(json!([
                                    "button",
                                    button.detail,
                                    button.response_type == xproto::BUTTON_PRESS_EVENT
                                ]));
                                seen += 1;
                            }
                            XEvent::MotionNotify(motion) => {
                                if matches!(event, Event::MouseMove { .. }) {
                                    out.push(json!([
                                        "relative",
                                        motion.root_x - before.root_x,
                                        motion.root_y - before.root_y
                                    ]));
                                } else {
                                    out.push(json!(["absolute", motion.root_x, motion.root_y]));
                                }
                                seen += 1;
                            }
                            XEvent::Error(error) => panic!("XTest observation {error:?}"),
                            _ => {}
                        }
                    } else if apply.is_finished() && seen >= expected_count {
                        break;
                    } else {
                        std::thread::sleep(std::time::Duration::from_micros(200));
                    }
                }
                apply.join().unwrap();
            });
        }
        drop(sink);
        observer.destroy_window(window).unwrap().check().unwrap();
        out
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_keys_resolve_against_real_xkb_layouts_without_posting() {
        use std::ffi::{c_char, c_int, c_void, CStr, CString};
        #[repr(C)]
        struct Names {
            rules: *const c_char,
            model: *const c_char,
            layout: *const c_char,
            variant: *const c_char,
            options: *const c_char,
        }
        #[link(name = "xkbcommon")]
        unsafe extern "C" {
            fn xkb_context_new(flags: u32) -> *mut c_void;
            fn xkb_context_unref(context: *mut c_void);
            fn xkb_keymap_new_from_names(
                context: *mut c_void,
                names: *const Names,
                flags: u32,
            ) -> *mut c_void;
            fn xkb_keymap_unref(keymap: *mut c_void);
            fn xkb_keymap_key_get_name(keymap: *mut c_void, key: u32) -> *const c_char;
            fn xkb_keymap_key_by_name(keymap: *mut c_void, name: *const c_char) -> u32;
            fn xkb_keysym_to_utf32(keysym: u32) -> u32;
            fn xkb_state_new(keymap: *mut c_void) -> *mut c_void;
            fn xkb_state_unref(state: *mut c_void);
            fn xkb_state_update_key(state: *mut c_void, key: u32, direction: u32) -> u32;
            fn xkb_state_mod_name_is_active(
                state: *mut c_void,
                name: *const c_char,
                components: u32,
            ) -> c_int;
        }
        for layout in [c"us", c"gb", c"de", c"fr", c"kr", c"jp"] {
            // SAFETY: all names are live C strings and the owned keymap and
            // context are unreferenced after lookups. No server is opened.
            unsafe {
                let context = xkb_context_new(0);
                assert!(!context.is_null());
                let keymap = xkb_keymap_new_from_names(
                    context,
                    &Names {
                        rules: c"evdev".as_ptr(),
                        model: c"pc105".as_ptr(),
                        layout: layout.as_ptr(),
                        variant: c"".as_ptr(),
                        options: c"".as_ptr(),
                    },
                    0,
                );
                assert!(!keymap.is_null(), "{layout:?}");
                let names: Vec<_> = (8..=255)
                    .map(|code| {
                        let name = xkb_keymap_key_get_name(keymap, code);
                        let mut bytes = [0; 4];
                        if !name.is_null() {
                            let name = CStr::from_ptr(name).to_bytes();
                            let len = name.len().min(4);
                            bytes[..len].copy_from_slice(&name[..len]);
                        }
                        xkb::KeyName { name: bytes }
                    })
                    .collect();

                for case in latch_proto::keyboard::CASES {
                    let name = CString::new(case.xkb).unwrap();
                    let expected = xkb_keymap_key_by_name(keymap, name.as_ptr());
                    assert_ne!(expected, u32::MAX, "host linux, {layout:?}, {}", case.xkb);
                    assert_eq!(expected, u32::from(case.linux));
                    assert_eq!(u32::from(keycode(8, &names, case.vk).unwrap()), expected);
                    let state = xkb_state_new(keymap);
                    assert!(!state.is_null());
                    xkb_state_update_key(state, expected, 1);
                    let active =
                        |name: &CStr| xkb_state_mod_name_is_active(state, name.as_ptr(), 8) > 0;
                    assert_eq!(active(c"Shift"), case.mask & 1 != 0);
                    assert_eq!(active(c"Control"), case.mask & 2 != 0);
                    assert_eq!(active(c"Mod1") || active(c"Mod5"), case.mask & 4 != 0);
                    assert_eq!(active(c"Mod4"), case.mask & 8 != 0);
                    xkb_state_update_key(state, expected, 0);
                    assert!(
                        !active(c"Shift")
                            && !active(c"Control")
                            && !active(c"Mod1")
                            && !active(c"Mod4")
                            && !active(c"Mod5")
                    );
                    xkb_state_unref(state);
                }

                xkb_keymap_unref(keymap);
                xkb_context_unref(context);
                for text in latch_proto::keyboard::TEXT_CASES {
                    for ch in text.chars() {
                        assert_eq!(xkb_keysym_to_utf32(unicode_keysym(ch)), u32::from(ch));
                    }
                }
            }
        }
    }
    #[test]
    fn scroll_remainders_and_buttons() {
        let mut r = 0;
        assert_eq!(notches(&mut r, 60), 0);
        assert_eq!(notches(&mut r, 90), 1);
        assert_eq!(r, 30);
        assert_eq!(notches(&mut r, -270), -2);
        assert_eq!(r, 0);
        assert_eq!(button_code(4).unwrap(), 8);
        assert_eq!(button_code(5).unwrap(), 9);
        assert!(button_code(6).is_err());
    }
}

#[cfg(test)]
mod timeout_tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;
    use std::time::Duration;
    use x11rb::rust_connection::{DefaultStream, RustConnection};
    use x11rb::x11_utils::Serialize;

    #[test]
    fn silent_xtest_fails_instead_of_holding_the_session_input_lock() {
        let (client, mut server) = UnixStream::pair().unwrap();
        let worker = std::thread::spawn(move || {
            let mut request = [0; 12];
            server.read_exact(&mut request).unwrap();
            let mut setup = xproto::Setup {
                status: 1,
                protocol_major_version: 11,
                resource_id_mask: 0xffff,
                roots: vec![xproto::Screen::default()],
                ..Default::default()
            };
            setup.length = ((setup.serialize().len() - 8) / 4) as u16;
            server.write_all(&setup.serialize()).unwrap();
            let mut sequence = 0;
            loop {
                let mut header = [0; 4];
                if server.read_exact(&mut header).is_err() {
                    break;
                }
                let len = usize::from(u16::from_ne_bytes([header[2], header[3]])) * 4;
                server.read_exact(&mut vec![0; len - 4]).unwrap();
                sequence += 1;
                if header[0] == 98 {
                    let mut reply = xproto::QueryExtensionReply {
                        sequence,
                        present: true,
                        major_opcode: 140,
                        ..Default::default()
                    }
                    .serialize()
                    .to_vec();
                    reply.resize(32, 0);
                    server.write_all(&reply).unwrap();
                }
                // Leave XTest and its synchronization request unanswered.
            }
        });
        let (stream, _) = DefaultStream::from_unix_stream(client).unwrap();
        let conn = RustConnection::connect_to_stream(
            super::super::Bounded {
                deadline: std::sync::Mutex::new(None),
                stream,
                silence: Duration::from_millis(100),
            },
            0,
        )
        .unwrap();
        let input = X11Input {
            conn,
            root: 0,
            screen: Default::default(),
            selected: false,
            mapping: Default::default(),
            min_keycode: 8,
            key_names: Default::default(),
            held: Default::default(),
            pressed: Default::default(),
            scroll: [0; 2],
        };
        assert!(input.fake(xproto::KEY_PRESS_EVENT, 8, 0, 0).is_err());
        drop(input);
        worker.join().unwrap();
    }
}

#[cfg(test)]
mod display_motion_tests {
    use super::*;

    #[test]
    fn selected_monitor_clamps_each_edge_and_whole_screen_keeps_relative_input() {
        let screen = DisplayRect {
            width: 4480,
            height: 1440,
            ..Default::default()
        };
        let monitor = DisplayRect {
            x: 1920,
            y: 100,
            width: 2560,
            height: 1340,
        };
        for (dx, dy, expected) in [
            (i16::MIN, i16::MIN, (1920, 100)),
            (i16::MAX, i16::MAX, (4479, 1439)),
            (10, -5, (2010, 195)),
        ] {
            assert_eq!(
                relative_position(monitor, screen, true, dx, dy, || Ok((2000, 200))).unwrap(),
                Some(expected)
            );
        }
        assert_eq!(
            relative_position(screen, screen, false, 123, -456, || panic!(
                "whole screen must stay asynchronous"
            ))
            .unwrap(),
            None
        );
        // Switching back to a left-hand monitor also clamps a stale position.
        let left = DisplayRect {
            x: 0,
            width: 1920,
            ..screen
        };
        assert_eq!(
            relative_position(left, screen, true, 1, 1, || Ok((3000, 200))).unwrap(),
            Some((1919, 201))
        );
        // A selected monitor filling this root still uses absolute XTest on
        // this root, even when the pointer has crossed to another X screen.
        assert_eq!(
            relative_position(screen, screen, true, i16::MAX, i16::MIN, || Ok((100, 200))).unwrap(),
            Some((4479, 0))
        );
    }
}
