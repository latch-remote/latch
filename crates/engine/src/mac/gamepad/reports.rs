//! HID report layouts, independent of device creation. Xbox Bluetooth-style
//! controls and wired DS4 controls use the existing normalized wire state.

use crate::gamepad::{map, Feedback, PadKind, PadState};
use latch_proto::gamepad::button;

const XBOX: &[u8] = &[
    0x05, 0x01, 0x09, 0x05, 0xa1, 0x01, 0x85, 0x01, // Four unsigned 16-bit sticks.
    0x09, 0x30, 0x09, 0x31, 0x09, 0x32, 0x09, 0x35, 0x15, 0x00, 0x27, 0xff, 0xff, 0x00, 0x00, 0x75,
    0x10, 0x95, 0x04, 0x81, 0x02, // Two 10-bit triggers in 16-bit fields.
    0x09, 0x33, 0x09, 0x34, 0x26, 0xff, 0x03, 0x95, 0x02, 0x81, 0x02,
    // Hat, 1..8, with 0 the null state.
    0x09, 0x39, 0x15, 0x01, 0x25, 0x08, 0x75, 0x04, 0x95, 0x01, 0x81, 0x42, 0x75, 0x04, 0x81, 0x03,
    0x05, 0x09, 0x19, 0x01, 0x29, 0x10, 0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x10, 0x81, 0x02,
    // Xbox output report 3, enable mask, four motors and timing.
    0x06, 0x00, 0xff, 0x85, 0x03, 0x09, 0x01, 0x15, 0x00, 0x26, 0xff, 0x00, 0x75, 0x08, 0x95, 0x08,
    0x91, 0x02, 0xc0,
];
const DS4: &[u8] = &[
    0x05, 0x01, 0x09, 0x05, 0xa1, 0x01, 0x85, 0x01, 0x09, 0x30, 0x09, 0x31, 0x09, 0x32, 0x09, 0x35,
    0x15, 0x00, 0x26, 0xff, 0x00, 0x75, 0x08, 0x95, 0x04, 0x81, 0x02, 0x09, 0x39, 0x25, 0x07, 0x35,
    0x00, 0x46, 0x3b, 0x01, 0x65, 0x14, 0x75, 0x04, 0x95, 0x01, 0x81, 0x42, 0x65, 0x00, 0x05, 0x09,
    0x19, 0x01, 0x29, 0x0e, 0x25, 0x01, 0x75, 0x01, 0x95, 0x0e, 0x81, 0x02, 0x75, 0x06, 0x95, 0x01,
    0x81, 0x03, 0x05, 0x01, 0x09, 0x33, 0x09, 0x34, 0x26, 0xff, 0x00, 0x75, 0x08, 0x95, 0x02, 0x81,
    0x02, 0x06, 0x00, 0xff, 0x09, 0x20, 0x95, 0x36, 0x81, 0x02, 0x85, 0x05, 0x09, 0x21, 0x95, 0x1f,
    0x91, 0x02, 0xc0,
];

pub fn descriptor(kind: PadKind) -> &'static [u8] {
    if kind == PadKind::PlayStation {
        DS4
    } else {
        XBOX
    }
}
pub fn identity(kind: PadKind) -> (i64, i64) {
    if kind == PadKind::PlayStation {
        (0x054c, 0x05c4)
    } else {
        (0x045e, 0x02fd)
    }
}

pub fn input(kind: PadKind, state: &PadState) -> Vec<u8> {
    if kind == PadKind::PlayStation {
        let mut out = vec![0; 64];
        out[0] = 1;
        out[1..10].copy_from_slice(&map::ds4(state)[..9]);
        out[30] = 0x1b; // wired, charged
        out[35] = 0x80; // no touches
        out[39] = 0x80;
        return out;
    }
    let mut out = vec![0; 16];
    out[0] = 1;
    for (i, value) in [
        i32::from(state.left_x),
        -i32::from(state.left_y) - 1,
        i32::from(state.right_x),
        -i32::from(state.right_y) - 1,
    ]
    .iter()
    .enumerate()
    {
        out[1 + i * 2..3 + i * 2].copy_from_slice(&((*value + 32768) as u16).to_le_bytes());
    }
    out[9..11].copy_from_slice(&(state.left_trigger >> 6).to_le_bytes());
    out[11..13].copy_from_slice(&(state.right_trigger >> 6).to_le_bytes());
    let hat = map::ds4(state)[4] & 15;
    out[13] = if hat == 8 { 0 } else { hat + 1 };
    let table = [
        (button::SOUTH, 0),
        (button::EAST, 1),
        (button::WEST, 3),
        (button::NORTH, 4),
        (button::LEFT_SHOULDER, 6),
        (button::RIGHT_SHOULDER, 7),
        (button::BACK | button::TOUCHPAD, 10),
        (button::START, 11),
        (button::GUIDE, 12),
        (button::LEFT_STICK, 13),
        (button::RIGHT_STICK, 14),
    ];
    let buttons = table.iter().fold(0u16, |bits, (mask, bit)| {
        bits | if state.buttons & mask != 0 {
            1 << bit
        } else {
            0
        }
    });
    out[14..16].copy_from_slice(&buttons.to_le_bytes());
    out
}

/// IOHID set-report buffers include the report ID. Validate it independently
/// of the callback's ID, then retain fields absent from a partial DS4 update.
pub fn feedback(kind: PadKind, id: u32, bytes: &[u8], mut previous: Feedback) -> Option<Feedback> {
    let (&first, payload) = bytes.split_first()?;
    if u32::from(first) != id {
        return None;
    }
    if kind == PadKind::PlayStation {
        if id != 5 || payload.len() != 31 {
            return None;
        }
        if payload[0] & 1 != 0 {
            previous.small = u16::from(payload[3]) * 257;
            previous.large = u16::from(payload[4]) * 257;
        }
        if payload[0] & 2 != 0 {
            previous.led = Some([payload[5], payload[6], payload[7]]);
        }
        Some(previous)
    } else {
        if id != 3 || payload.len() != 8 {
            return None;
        }
        let level = |bit: u8, index: usize| {
            if payload[0] & (1 << bit) != 0 {
                (u32::from(payload[index].min(100)) * 65535 / 100) as u16
            } else {
                0
            }
        };
        Some(Feedback {
            left_trigger: level(0, 1),
            right_trigger: level(1, 2),
            large: level(2, 3),
            small: level(3, 4),
            ..Default::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn descriptors_match_input_and_output_report_lengths() {
        for (kind, input_len, output_id, output_len) in
            [(PadKind::Xbox, 16, 3, 9), (PadKind::PlayStation, 64, 5, 32)]
        {
            let (mut size, mut count, mut id) = (0, 0, 0);
            let mut bits = [[0usize; 2]; 6];
            let mut i = 0;
            let descriptor = descriptor(kind);
            while i < descriptor.len() {
                let tag = descriptor[i];
                let len = match tag & 3 {
                    3 => 4,
                    n => n as usize,
                };
                let value = descriptor[i + 1..i + 1 + len]
                    .iter()
                    .enumerate()
                    .fold(0usize, |v, (shift, b)| v | ((*b as usize) << (8 * shift)));
                match tag & 0xfc {
                    0x74 => size = value,
                    0x94 => count = value,
                    0x84 => id = value,
                    0x80 => bits[id][0] += size * count,
                    0x90 => bits[id][1] += size * count,
                    _ => {}
                }
                i += len + 1;
            }
            assert_eq!(bits[1][0] / 8 + 1, input_len);
            assert_eq!(bits[output_id][1] / 8 + 1, output_len);
            assert_eq!(input(kind, &PadState::default()).len(), input_len);
        }
    }
    #[test]
    fn reports_map_extremes_hat_and_buttons() {
        let state = PadState {
            left_x: i16::MIN,
            left_y: i16::MAX,
            right_trigger: u16::MAX,
            buttons: button::SOUTH | button::GUIDE | button::DPAD_UP,
            ..Default::default()
        };
        let xbox = input(PadKind::Xbox, &state);
        assert_eq!(&xbox[1..5], &[0, 0, 0, 0]);
        assert_eq!(&xbox[11..14], &[255, 3, 1]);
        assert_eq!(u16::from_le_bytes([xbox[14], xbox[15]]), 1 | 1 << 12);
        let ds4 = input(PadKind::PlayStation, &state);
        assert_eq!(&ds4[1..10], &map::ds4(&state)[..9]);
        assert_eq!(input(PadKind::Generic, &state), xbox);
    }
    #[test]
    fn feedback_preserves_partial_lights_and_validates_ids_and_lengths() {
        let mut report = [0; 32];
        report[0] = 5;
        report[1] = 3;
        report[4] = 128;
        report[5] = 255;
        report[6..9].copy_from_slice(&[1, 2, 3]);
        let value = feedback(PadKind::PlayStation, 5, &report, Feedback::default()).unwrap();
        assert_eq!(
            (value.small, value.large, value.led),
            (32896, 65535, Some([1, 2, 3]))
        );
        report[1] = 1;
        assert_eq!(
            feedback(PadKind::PlayStation, 5, &report, value)
                .unwrap()
                .led,
            value.led
        );
        assert!(feedback(PadKind::PlayStation, 5, &report[..8], value).is_none());
        assert!(feedback(PadKind::Xbox, 3, &report, value).is_none());
        let f = feedback(
            PadKind::Xbox,
            3,
            &[3, 15, 100, 50, 100, 0, 255, 0, 0],
            Feedback::default(),
        )
        .unwrap();
        assert_eq!(
            (f.left_trigger, f.right_trigger, f.large, f.small),
            (65535, 32767, 65535, 0)
        );
    }
}
