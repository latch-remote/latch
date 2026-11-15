//! Swallow Win keys and app-switch/menu chords while this process owns focused stream
//! input. Other keys continue through winit, including IME and clipboard.

use super::{Key, STATE};
use std::cell::Cell;
use windows::Win32::{
    Foundation::{HINSTANCE, LPARAM, LRESULT, WPARAM},
    System::{LibraryLoader::GetModuleHandleW, Threading::GetCurrentProcessId},
    UI::WindowsAndMessaging::*,
};

thread_local! {
    static OWNED: Cell<bool> = const { Cell::new(false) };
    static HOOK: Cell<HHOOK> = const { Cell::new(HHOOK(std::ptr::null_mut())) };
}

pub fn install() {
    // SAFETY: the callback lives for the process and runs on the installing
    // event-loop thread. No pointer to stack state is retained.
    let installed = unsafe {
        GetModuleHandleW(None).and_then(|module| {
            SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook), HINSTANCE(module.0), 0)
        })
    };
    match installed {
        Ok(hook) => HOOK.set(hook),
        Err(error) => tracing::warn!(%error, "cannot capture the Windows key"),
    }
}

pub fn owns_keys(owned: bool) {
    OWNED.set(owned);
}

fn captures(event: &KBDLLHOOKSTRUCT) -> bool {
    !event.flags.contains(LLKHF_INJECTED)
        && (matches!(event.vkCode, 0x5b | 0x5c)
            || (matches!(event.vkCode, 0x09 | 0x20 | 0x73) && event.flags.contains(LLKHF_ALTDOWN)))
}

unsafe extern "system" fn hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 && OWNED.get() {
        // SAFETY: WH_KEYBOARD_LL supplies a KBDLLHOOKSTRUCT for HC_ACTION.
        let event = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
        let mut process = 0;
        // SAFETY: both APIs only query process/window identity.
        unsafe {
            GetWindowThreadProcessId(GetForegroundWindow(), Some(&mut process));
        }
        let ours = process == unsafe { GetCurrentProcessId() };
        if ours && captures(event) {
            let down = !event.flags.contains(LLKHF_UP);
            STATE.with_borrow_mut(|s| {
                if down {
                    s.pressed.insert(event.vkCode as u16);
                } else {
                    s.pressed.remove(&(event.vkCode as u16));
                }
                s.pending.push((
                    egui::Event::Key {
                        key: egui::Key::F35,
                        physical_key: None,
                        pressed: down,
                        repeat: false,
                        modifiers: egui::Modifiers::NONE,
                    },
                    Some(Key {
                        vk: event.vkCode as u16,
                        down,
                    }),
                    false,
                ))
            });
            let mods = STATE.with_borrow(|s| egui::Modifiers {
                ctrl: s.pressed.contains(&0xa2) || s.pressed.contains(&0xa3),
                alt: event.flags.contains(LLKHF_ALTDOWN),
                ..egui::Modifiers::NONE
            });
            crate::early::physical(
                Key {
                    vk: event.vkCode as u16,
                    down,
                },
                mods,
            );
            return LRESULT(1);
        }
    }
    // SAFETY: pass unhandled input to the next hook unchanged.
    unsafe { CallNextHookEx(HHOOK::default(), code, wparam, lparam) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constructs_hook_records_without_installing_or_posting() {
        for (vk, flags, captured) in [
            (0x5b, KBDLLHOOKSTRUCT_FLAGS(0), true),
            (0x5c, LLKHF_UP, true),
            (0x09, LLKHF_ALTDOWN, true),
            (0x20, LLKHF_ALTDOWN, true),
            (0x73, LLKHF_ALTDOWN, true),
            (0x09, KBDLLHOOKSTRUCT_FLAGS(0), false),
            (0x5b, LLKHF_INJECTED, false),
            (0x41, KBDLLHOOKSTRUCT_FLAGS(0), false),
        ] {
            let event = KBDLLHOOKSTRUCT {
                vkCode: vk,
                flags,
                ..Default::default()
            };
            assert_eq!(captures(&event), captured);
        }
        owns_keys(true);
        assert!(OWNED.get());
        owns_keys(false);
        assert!(!OWNED.get());
    }
}
