use super::elevated::{self, Client};
use crate::input::{DisplayRect, Event, Held, InputSink};
use anyhow::{bail, Result};
use std::sync::mpsc::{self, TryRecvError};
use windows::Win32::Foundation::RECT;
use windows::Win32::UI::{Input::KeyboardAndMouse::*, WindowsAndMessaging::*};

/// SendInput from this process. The elevated helper injects with one too.
#[derive(Default)]
pub(super) struct Local {
    held: Held,
    previous_clip: Option<RECT>,
    restore_unconfined: bool,
}

/// The host's injector: through the elevated helper once it answers, so
/// windows that run as administrator follow the viewer too; by itself
/// until then, without setup's helper, or after the helper went away.
pub(super) struct Injector {
    local: Local,
    helper: Helper,
    /// This process's integrity level, and the foreground window last
    /// compared with it, to say once why a window ignores the viewer.
    level: u32,
    checked: Option<isize>,
    notice: Option<String>,
}
enum Helper {
    Connecting(mpsc::Receiver<Client>),
    Connected(Client),
    Gone,
}

fn keyboard(vk: u16, scan: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(vk),
                wScan: scan,
                dwFlags: flags,
                ..Default::default()
            },
        },
    }
}
fn mouse(dx: i32, dy: i32, data: u32, flags: MOUSE_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: data,
                dwFlags: flags,
                ..Default::default()
            },
        },
    }
}
use crate::input::keymap::{extended, us_scan};

// Target the middle of a pixel's 1/65536 interval, rather than its left edge.
fn normalize(n: i32, start: i32, size: i32) -> i32 {
    let size = i64::from(size.max(1));
    let pixel = (i64::from(n) - i64::from(start)).clamp(0, size - 1);
    (((2 * pixel + 1) * 65536 / (2 * size)).clamp(0, 65535)) as i32
}

fn construct(event: &Event, display: DisplayRect) -> Result<Vec<INPUT>> {
    let _dpi = super::display::DpiAwareness::physical();
    let mut out = Vec::new();
    match event {
        Event::UnknownPosition { .. } => {}
        Event::Key { vk, down, .. } => {
            // Keys outside the US table (IME and other layout-specific
            // keys) keep the PC's own mapping.
            let scan = us_scan(*vk).unwrap_or_else(|| {
                // SAFETY: MapVirtualKeyW has no pointer parameters or side effects.
                unsafe { MapVirtualKeyW(u32::from(*vk), MAPVK_VK_TO_VSC) as u16 }
            });
            let mut flags = if *down {
                KEYBD_EVENT_FLAGS(0)
            } else {
                KEYEVENTF_KEYUP
            };
            if extended(*vk) {
                flags |= KEYEVENTF_EXTENDEDKEY;
            }
            // Microsoft KEYBDINPUT specifies that SCANCODE ignores wVk.
            // Korean/Japanese IME controls need their documented virtual keys.
            let ime_vk = match *vk {
                latch_proto::keyboard::KANA | 0x15 => Some(0x15),
                0x19 => Some(0x19),
                latch_proto::keyboard::CONVERT | 0x1c => Some(0x1c),
                latch_proto::keyboard::NON_CONVERT | 0x1d => Some(0x1d),
                _ => None,
            };
            if scan != 0 && ime_vk.is_none() {
                flags |= KEYEVENTF_SCANCODE;
            }
            out.push(keyboard(
                ime_vk.unwrap_or(if scan == 0 { *vk } else { 0 }),
                scan,
                flags,
            ));
        }
        Event::Text(text) => {
            for ch in text.chars() {
                let mut buffer = [0; 2];
                let units = ch.encode_utf16(&mut buffer);
                for flags in [KEYEVENTF_UNICODE, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP] {
                    for &unit in units.iter() {
                        out.push(keyboard(0, unit, flags));
                    }
                }
            }
        }
        // A plain relative move, as Sunshine sends it. With
        // MOUSEEVENTF_MOVE_NOCOALESCE the game's raw input reads usFlags
        // MOUSE_MOVE_NOCOALESCE instead of MOUSE_MOVE_RELATIVE (0), and
        // games that only accept the latter drop every move: Genshin
        // Impact's and Honkai: Star Rail's cameras stood still.
        Event::MouseMove { dx, dy } => {
            out.push(mouse(i32::from(*dx), i32::from(*dy), 0, MOUSEEVENTF_MOVE))
        }
        Event::MousePosition {
            x,
            y,
            width,
            height,
        } => {
            let (x, y) = display.map(*x, *y, *width, *height);
            // SAFETY: These queries only read the virtual desktop geometry.
            let (left, top, w, h) = unsafe {
                (
                    GetSystemMetrics(SM_XVIRTUALSCREEN),
                    GetSystemMetrics(SM_YVIRTUALSCREEN),
                    GetSystemMetrics(SM_CXVIRTUALSCREEN),
                    GetSystemMetrics(SM_CYVIRTUALSCREEN),
                )
            };
            out.push(mouse(
                normalize(x, left, w),
                normalize(y, top, h),
                0,
                MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
            ));
        }
        Event::MouseButton { button, down } => {
            let (up, dn, data) = match button {
                1 => (MOUSEEVENTF_LEFTUP, MOUSEEVENTF_LEFTDOWN, 0),
                2 => (MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MIDDLEDOWN, 0),
                3 => (MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_RIGHTDOWN, 0),
                4 => (MOUSEEVENTF_XUP, MOUSEEVENTF_XDOWN, 1),
                5 => (MOUSEEVENTF_XUP, MOUSEEVENTF_XDOWN, 2),
                _ => bail!("unknown mouse button {button}"),
            };
            out.push(mouse(0, 0, data, if *down { dn } else { up }));
        }
        Event::Scroll {
            vertical,
            horizontal,
        } => {
            if *vertical != 0 {
                out.push(mouse(0, 0, i32::from(*vertical) as u32, MOUSEEVENTF_WHEEL));
            }
            if *horizontal != 0 {
                out.push(mouse(
                    0,
                    0,
                    i32::from(*horizontal) as u32,
                    MOUSEEVENTF_HWHEEL,
                ));
            }
        }
        Event::ReleaseAll => {}
    }
    Ok(out)
}
fn post(inputs: &[INPUT]) -> Result<()> {
    if inputs.is_empty() {
        return Ok(());
    }
    // SAFETY: SendInput copies the initialized slice during this call.
    let sent = unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) };
    if sent as usize != inputs.len() {
        bail!(
            "SendInput inserted {sent}/{} events (desktop access or UIPI restriction): {}",
            inputs.len(),
            windows::core::Error::from_win32()
        );
    }
    Ok(())
}
impl Local {
    fn confine(&mut self, display: DisplayRect) -> Result<()> {
        let _dpi = super::display::DpiAwareness::physical();
        // Keep SendInput relative, including Windows acceleration and raw game
        // input. The OS clips the resulting cursor position in physical pixels.
        let rect = clip_rect(display)?;
        // SAFETY: Both rectangles are initialized and valid for these calls.
        unsafe {
            if self.previous_clip.is_none() {
                let mut previous = RECT::default();
                GetClipCursor(&mut previous)?;
                ClipCursor(Some(&rect))?;
                self.restore_unconfined = previous.left == GetSystemMetrics(SM_XVIRTUALSCREEN)
                    && previous.top == GetSystemMetrics(SM_YVIRTUALSCREEN)
                    && previous.right - previous.left == GetSystemMetrics(SM_CXVIRTUALSCREEN)
                    && previous.bottom - previous.top == GetSystemMetrics(SM_CYVIRTUALSCREEN);
                self.previous_clip = Some(previous);
            } else {
                ClipCursor(Some(&rect))?;
            }
        }
        Ok(())
    }

    pub(super) fn apply(&mut self, event: &Event, display: DisplayRect) -> Result<()> {
        if matches!(event, Event::ReleaseAll) {
            self.release_all();
            anyhow::ensure!(
                self.held.keys.is_empty() && self.held.buttons.is_empty(),
                "input releases still pending"
            );
            return Ok(());
        }
        if matches!(event, Event::MouseMove { .. } | Event::MousePosition { .. }) {
            self.confine(display)?;
        }
        let inputs = construct(event, display)?;
        post(&inputs)?;
        self.held.note(event);
        Ok(())
    }
    pub(super) fn release_all(&mut self) {
        for event in self.held.drain() {
            if let Err(e) = construct(&event, DisplayRect::default()).and_then(|v| post(&v)) {
                tracing::warn!("release input: {e:#}");
                match event {
                    Event::Key { vk, .. } => {
                        self.held.keys.insert(vk);
                    }
                    Event::MouseButton { button, .. } => {
                        self.held.buttons.insert(button);
                    }
                    _ => {}
                }
            }
        }
        self.restore_clip();
    }

    fn restore_clip(&mut self) {
        if let Some(previous) = self.previous_clip {
            let _dpi = super::display::DpiAwareness::physical();
            // An unrestricted cursor must stay unrestricted when session cleanup
            // restores a larger desktop after the injector has been dropped.
            let previous = if self.restore_unconfined {
                None
            } else {
                Some(std::ptr::from_ref(&previous))
            };
            // SAFETY: This rectangle came from GetClipCursor in this session.
            if let Err(e) = unsafe { ClipCursor(previous) } {
                tracing::warn!("restore cursor confinement: {e:#}");
            } else {
                self.previous_clip = None;
            }
        }
    }
}
impl Drop for Local {
    fn drop(&mut self) {
        self.release_all();
    }
}

impl Injector {
    pub(super) fn new() -> Self {
        Self {
            local: Local::default(),
            helper: Helper::Connecting(elevated::connect()),
            level: elevated::own_integrity(),
            checked: None,
            notice: None,
        }
    }
    fn client(&mut self) -> Option<&mut Client> {
        if let Helper::Connecting(started) = &self.helper {
            match started.try_recv() {
                Ok(client) => {
                    self.local.restore_clip();
                    tracing::info!("input goes through the elevated helper, so windows that run as administrator follow the viewer too");
                    self.helper = Helper::Connected(client);
                    self.notice = None;
                }
                Err(TryRecvError::Disconnected) => self.helper = Helper::Gone,
                Err(TryRecvError::Empty) => {}
            }
        }
        match &mut self.helper {
            Helper::Connected(client) => Some(client),
            _ => None,
        }
    }
    /// The helper's result, or `None` when there is no helper (any more).
    fn remote(&mut self, event: &Event, display: DisplayRect) -> Option<Result<()>> {
        match self.client()?.apply(event, display) {
            Ok(result) => Some(result),
            Err(e) => {
                tracing::warn!(
                    "the elevated input helper stopped ({e}); Latch injects input by itself again"
                );
                self.helper = Helper::Gone;
                self.checked = None;
                None
            }
        }
    }
    /// Windows drops this process's input to a window of a higher
    /// integrity level and reports it as sent: say so, once per window.
    fn warn_if_ignored(&mut self) {
        // Let the helper finish starting before asking the viewer to set it up.
        if matches!(self.helper, Helper::Connecting(_)) {
            return;
        }
        // SAFETY: Reads the foreground window's handle.
        let window = unsafe { GetForegroundWindow() };
        if self.checked == Some(window.0 as isize) {
            return;
        }
        self.checked = Some(window.0 as isize);
        self.notice = None;
        if let Some((name, level)) = elevated::window_owner(window) {
            if level > self.level {
                let message = format!("{name} runs as administrator, so Windows is blocking Latch's mouse and keyboard. On the Windows PC, enable \"Control windows that run as administrator\" in Latch's Sharing tab, then choose \"Set up as administrator\".");
                tracing::warn!("{message}");
                self.notice = Some(message);
            }
        }
    }
}

fn clip_rect(display: DisplayRect) -> Result<RECT> {
    anyhow::ensure!(
        display.width > 0 && display.height > 0,
        "empty input display"
    );
    Ok(RECT {
        left: display.x,
        top: display.y,
        right: i32::try_from(i64::from(display.x) + i64::from(display.width))?,
        bottom: i32::try_from(i64::from(display.y) + i64::from(display.height))?,
    })
}
impl InputSink for Injector {
    fn take_notice(&mut self) -> Option<String> {
        self.notice.take()
    }

    fn apply(&mut self, event: &Event, display: DisplayRect) -> Result<()> {
        if matches!(event, Event::ReleaseAll) {
            // Each side lets go of what it pressed.
            let remote = self.remote(event, display).unwrap_or(Ok(()));
            let local = self.local.apply(event, display);
            return remote.and(local);
        }
        if matches!(event, Event::MouseMove { .. } | Event::MousePosition { .. }) {
            // Finish a helper handoff before acquiring the host's clip. The
            // host also confines input forwarded to an older installed helper.
            let _ = self.client();
            self.local.confine(display)?;
        }
        if let Some(result) = self.remote(event, display) {
            return result;
        }
        self.warn_if_ignored();
        self.local.apply(event, display)
    }
    fn release_all(&mut self) {
        if let Some(Err(e)) = self.remote(&Event::ReleaseAll, DisplayRect::default()) {
            tracing::warn!("release input: {e:#}");
        }
        self.local.release_all();
    }
}
impl Drop for Injector {
    fn drop(&mut self) {
        self.release_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn confinement_uses_the_full_physical_display_rectangle() -> Result<()> {
        for display in [
            DisplayRect {
                x: -3840,
                y: -2160,
                width: 3840,
                height: 2160,
            },
            DisplayRect {
                x: 1920,
                y: 500,
                width: 2560,
                height: 1440,
            },
        ] {
            let rect = clip_rect(display)?;
            assert_eq!((rect.left, rect.top), (display.x, display.y));
            assert_eq!(
                (rect.right, rect.bottom),
                (
                    display.x + display.width as i32,
                    display.y + display.height as i32
                )
            );
        }
        assert!(clip_rect(DisplayRect::default()).is_err());
        Ok(())
    }

    #[test]
    fn injected_motion_updates_confinement_and_release_restores_it() -> Result<()> {
        let _dpi = super::super::display::DpiAwareness::physical();
        let mut previous = RECT::default();
        // SAFETY: The writable output rectangle remains live for the query.
        if let Err(e) = unsafe { GetClipCursor(&mut previous) } {
            eprintln!("cursor desktop unavailable: {e}");
            return Ok(());
        }
        let width = (previous.right - previous.left) / 2;
        let height = previous.bottom - previous.top;
        if width < 1 || height < 1 {
            return Ok(());
        }
        let mut input = Local::default();
        for x in [previous.left, previous.left + width] {
            let display = DisplayRect {
                x,
                y: previous.top,
                width: width as u32,
                height: height as u32,
            };
            for event in [
                Event::MousePosition {
                    x: 0,
                    y: 0,
                    width: 100,
                    height: 100,
                },
                Event::MouseMove {
                    dx: i16::MAX,
                    dy: i16::MIN,
                },
            ] {
                input.apply(&event, display)?;
                let mut actual = RECT::default();
                // SAFETY: The writable output rectangle remains live for the query.
                unsafe {
                    GetClipCursor(&mut actual)?;
                }
                let expected = clip_rect(display)?;
                assert_eq!(
                    (actual.left, actual.top, actual.right, actual.bottom),
                    (expected.left, expected.top, expected.right, expected.bottom)
                );
            }
        }
        input.release_all();
        let mut actual = RECT::default();
        // SAFETY: The writable output rectangle remains live for the query.
        unsafe {
            GetClipCursor(&mut actual)?;
        }
        assert_eq!(
            (actual.left, actual.top, actual.right, actual.bottom),
            (previous.left, previous.top, previous.right, previous.bottom)
        );
        Ok(())
    }

    #[test]
    fn absolute_coordinates_reconstruct_every_pixel() {
        for size in [1, 1920, 3840, 7680] {
            for p in 0..size {
                let normalized = normalize(p - 1920, -1920, size);
                assert_eq!(
                    i64::from(normalized) * i64::from(size) / 65536,
                    i64::from(p)
                );
            }
        }
    }
    #[test]
    fn relative_motion_reaches_raw_input_as_plain_relative() -> Result<()> {
        let v = construct(&Event::MouseMove { dx: 1, dy: -2 }, DisplayRect::default())?;
        // SAFETY: construct produces mouse INPUT for MouseMove.
        unsafe {
            let mi = v[0].Anonymous.mi;
            // Any other flag shows in RAWMOUSE.usFlags, and games that want
            // MOUSE_MOVE_RELATIVE there ignore the move.
            assert_eq!(mi.dwFlags, MOUSEEVENTF_MOVE);
            assert_eq!((mi.dx, mi.dy), (1, -2));
        }
        Ok(())
    }
    #[test]
    fn keys_use_us_scancodes_whatever_the_pcs_layout() {
        for (vk, scan) in [
            (0x41, 0x1e), // A, not AZERTY's 0x10
            (0x51, 0x10), // Q
            (0x5a, 0x2c), // Z
            (0x57, 0x11), // W
            (0x31, 0x02), // 1
            (0x30, 0x0b), // 0
            (0x20, 0x39), // Space
            (0x1b, 0x01), // Escape
            (0x70, 0x3b), // F1
            (0x7b, 0x58), // F12
            (0xbd, 0x0c), // -
            (0xde, 0x28), // '
            (0x60, 0x52), // numpad 0
            (0x69, 0x49), // numpad 9
            (0x26, 0x48), // Up
        ] {
            assert_eq!(us_scan(vk), Some(scan), "vk {vk:#x}");
        }
        assert_eq!(us_scan(0x15), Some(0x72));
    }
    #[test]
    fn every_viewer_builds_the_shared_native_keyboard_cases() -> Result<()> {
        for case in latch_proto::keyboard::CASES {
            for down in [true, false] {
                let records = construct(
                    &Event::Key {
                        vk: case.vk,
                        down,
                        modifiers: case.mask,
                    },
                    DisplayRect::default(),
                )?;
                // SAFETY: construct returns keyboard INPUT records for keys.
                let key = unsafe { records[0].Anonymous.ki };
                assert_eq!(key.wScan, case.scan, "host windows, {:#x}", case.vk);
                let ime = matches!(
                    case.vk,
                    0x15 | 0x19
                        | latch_proto::keyboard::KANA
                        | latch_proto::keyboard::CONVERT
                        | latch_proto::keyboard::NON_CONVERT
                );
                assert_eq!(
                    key.wVk.0,
                    if ime {
                        latch_proto::keyboard::helper_legacy(case.vk).unwrap()
                    } else {
                        0
                    }
                );
                assert_eq!(key.dwFlags.contains(KEYEVENTF_SCANCODE), !ime);
                assert_eq!(key.dwFlags.contains(KEYEVENTF_EXTENDEDKEY), case.extended);
                assert_eq!(key.dwFlags.contains(KEYEVENTF_KEYUP), !down);
            }
        }

        for text in latch_proto::keyboard::TEXT_CASES {
            let records = construct(&Event::Text((*text).into()), DisplayRect::default())?;
            let units: Vec<_> = records
                .iter()
                .filter_map(|record| {
                    // SAFETY: these records all have INPUT_KEYBOARD discriminators.
                    let key = unsafe { record.Anonymous.ki };
                    assert!(key.dwFlags.contains(KEYEVENTF_UNICODE));
                    (!key.dwFlags.contains(KEYEVENTF_KEYUP)).then_some(key.wScan)
                })
                .collect();
            assert_eq!(String::from_utf16(&units).unwrap(), *text);
        }
        Ok(())
    }
    #[test]
    fn constructs_without_posting() -> Result<()> {
        let v = construct(&Event::Text("A😀".into()), DisplayRect::default())?;
        assert_eq!(v.len(), 6);
        // SAFETY: These were constructed with the INPUT_KEYBOARD discriminator.
        unsafe {
            assert_eq!(v[2].Anonymous.ki.wScan, 0xd83d);
            assert_eq!(v[3].Anonymous.ki.wScan, 0xde00);
            assert!(!v[2].Anonymous.ki.dwFlags.contains(KEYEVENTF_KEYUP));
            assert!(!v[3].Anonymous.ki.dwFlags.contains(KEYEVENTF_KEYUP));
            assert!(v[4].Anonymous.ki.dwFlags.contains(KEYEVENTF_KEYUP));
            assert!(v[5].Anonymous.ki.dwFlags.contains(KEYEVENTF_KEYUP));
        }
        for vk in [0x25, 0xa3, 0xa5, 0x5b, 0x90, 0x6f, 0x2c, 0x5d] {
            let v = construct(
                &Event::Key {
                    vk,
                    down: false,
                    modifiers: 0,
                },
                DisplayRect::default(),
            )?;
            // SAFETY: The event above constructs keyboard input.
            unsafe {
                assert!(v[0]
                    .Anonymous
                    .ki
                    .dwFlags
                    .contains(KEYEVENTF_EXTENDEDKEY | KEYEVENTF_KEYUP));
            }
        }
        let v = construct(
            &Event::MouseButton {
                button: 5,
                down: true,
            },
            DisplayRect::default(),
        )?;
        // SAFETY: The event above constructs mouse input.
        unsafe {
            assert_eq!(v[0].Anonymous.mi.mouseData, 2);
        }
        Ok(())
    }
}

#[cfg(test)]
mod pair_interop {
    use super::*;
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/pairs/native_input.rs"
    ));
    fn native(events: &[Event]) -> Vec<Value> {
        let mut out = Vec::new();
        let display = DisplayRect {
            x: 0,
            y: 0,
            width: 640,
            height: 360,
        };
        for event in events {
            for input in construct(event, display).unwrap() {
                // SAFETY: construct initializes the union matching its discriminator.
                unsafe {
                    if input.r#type == INPUT_KEYBOARD {
                        let key = input.Anonymous.ki;
                        if key.dwFlags.contains(KEYEVENTF_UNICODE) {
                            if !key.dwFlags.contains(KEYEVENTF_KEYUP) {
                                out.push(json!(["text", key.wScan]));
                            }
                        } else {
                            assert!(key.dwFlags.contains(KEYEVENTF_SCANCODE));
                            out.push(json!([
                                "key",
                                key.wScan,
                                !key.dwFlags.contains(KEYEVENTF_KEYUP),
                                0
                            ]));
                            assert_eq!(
                                key.dwFlags.contains(KEYEVENTF_EXTENDEDKEY),
                                matches!(event, Event::Key { vk: 0x5b, .. })
                            );
                        }
                    } else {
                        let mouse = input.Anonymous.mi;
                        if mouse.dwFlags.contains(MOUSEEVENTF_ABSOLUTE) {
                            assert!(mouse.dwFlags.contains(MOUSEEVENTF_VIRTUALDESK));
                            let x = i64::from(mouse.dx)
                                * i64::from(GetSystemMetrics(SM_CXVIRTUALSCREEN))
                                / 65536
                                + i64::from(GetSystemMetrics(SM_XVIRTUALSCREEN));
                            let y = i64::from(mouse.dy)
                                * i64::from(GetSystemMetrics(SM_CYVIRTUALSCREEN))
                                / 65536
                                + i64::from(GetSystemMetrics(SM_YVIRTUALSCREEN));
                            out.push(json!(["absolute", x, y]));
                        } else if mouse.dwFlags == MOUSEEVENTF_MOVE {
                            out.push(json!(["relative", mouse.dx, mouse.dy]));
                        } else if mouse.dwFlags == MOUSEEVENTF_WHEEL {
                            out.push(json!(["wheel-v", mouse.mouseData as i32]));
                        } else if mouse.dwFlags == MOUSEEVENTF_HWHEEL {
                            out.push(json!(["wheel-h", mouse.mouseData as i32]));
                        } else {
                            let (button, down) = match mouse.dwFlags {
                                MOUSEEVENTF_LEFTDOWN => (1, true),
                                MOUSEEVENTF_LEFTUP => (1, false),
                                MOUSEEVENTF_RIGHTDOWN => (3, true),
                                MOUSEEVENTF_RIGHTUP => (3, false),
                                MOUSEEVENTF_MIDDLEDOWN => (2, true),
                                MOUSEEVENTF_MIDDLEUP => (2, false),
                                MOUSEEVENTF_XDOWN | MOUSEEVENTF_XUP => {
                                    assert!(matches!(mouse.mouseData, 1 | 2));
                                    (mouse.mouseData + 3, mouse.dwFlags == MOUSEEVENTF_XDOWN)
                                }
                                flags => panic!("unexpected mouse flags {flags:?}"),
                            };
                            out.push(json!(["button", button, down]));
                        }
                    }
                }
            }
        }
        out
    }
}
