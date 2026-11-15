//! Physical positions without a distinct Windows VK. Internal IDs above 255
//! use input kind 8 only after `key_positions` negotiation. Ordinary VKs,
//! including sided modifiers, retain their version-1 representation.

pub const KEYPAD_ENTER: u16 = 0x158;
pub const KEYPAD_EQUAL: u16 = 0x167;
pub const INTL_RO: u16 = 0x187;
pub const INTL_YEN: u16 = 0x189;
pub const CONVERT: u16 = 0x18a;
pub const NON_CONVERT: u16 = 0x18b;
pub const KANA: u16 = 0x188;

/// Committed IME, AltGr and dead-key results, shared by native tests.
pub const TEXT_CASES: &[&str] = &["한글", "漢字", "é", "€", "ß", "œ", "😀"];

/// The low byte is the USB HID keyboard-page usage.
pub fn position(vk: u16) -> Option<u16> {
    matches!(
        vk,
        KEYPAD_ENTER | KEYPAD_EQUAL | INTL_RO | INTL_YEN | CONVERT | NON_CONVERT | KANA
    )
    .then(|| vk - 0x100)
}

/// Image of client input::vk at bf5da0d (6.0.0), plus modifiers,
/// Caps Lock and the existing toolbar's Print Screen. New viewers must
/// retain egui/text translation for every other key with an old peer.
pub fn legacy(vk: u16) -> Option<u16> {
    matches!(vk, 0x08 | 0x09 | 0x0d | 0x10..=0x12 | 0x14 | 0x1b | 0x20..=0x28
        | 0x2c..=0x2e | 0x30..=0x39 | 0x41..=0x5a | 0x5b | 0x6b | 0x70..=0x87
        | 0xba..=0xc0 | 0xdb..=0xde)
    .then_some(vk)
}

/// The helper's v1 protocol already accepts all ordinary VKs. This is
/// deliberately separate from the 6.0.0 viewer's smaller capture set.
pub fn helper_legacy(vk: u16) -> Option<u16> {
    Some(match vk {
        0..=255 => vk,
        KEYPAD_ENTER => 0x0d,
        KEYPAD_EQUAL => 0xbb,
        INTL_RO => 0xe2,
        INTL_YEN => 0xdc,
        KANA => 0x15,
        CONVERT => 0x1c,
        NON_CONVERT => 0x1d,
        _ => return None,
    })
}

/// Apple ISO hardware reports these two native codes at reversed USB
/// positions. The involution also converts USB positions for injection.
pub fn apple_iso(code: u16, iso: bool) -> u16 {
    match (iso, code) {
        (true, 0x0a) => 0x32,
        (true, 0x32) => 0x0a,
        _ => code,
    }
}

#[cfg(target_os = "macos")]
pub fn apple_keyboard_is_iso() -> bool {
    #[link(name = "Carbon", kind = "framework")]
    unsafe extern "C" {
        fn LMGetKbdType() -> u8;
        fn KBGetLayoutType(kind: i16) -> u32;
    }
    // SAFETY: Carbon keyboard-type queries take no pointers or ownership.
    unsafe { KBGetLayoutType(i16::from(LMGetKbdType())) == 1 }
}

/// Shared fixtures consumed by each viewer and native host test. `mac` is
/// absent for positions the Carbon keyboard API cannot represent.
pub struct Case {
    pub vk: u16,
    pub usage: u32,
    pub linux: u16,
    pub scan: u16,
    pub extended: bool,
    pub mac: Option<u16>,
    pub xkb: &'static str,
    pub mask: u8,
}

include!("../fixtures/keyboard.rs");
