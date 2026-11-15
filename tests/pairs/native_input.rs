// Included inside each platform injector's test module.
use crate::pair_format as format;
use format::InputRecording;
use serde_json::{json, Value};

fn reference(events: &[crate::input::Event]) -> Vec<Value> {
    use crate::input::Event;
    let mut out = Vec::new();
    let mut keys = std::collections::BTreeSet::new();
    let mut scroll = [0i32; 2];
    for event in events {
        match event {
            Event::Key { vk, down, .. } => {
                if *down {
                    keys.insert(*vk);
                } else {
                    keys.remove(vk);
                }
                let (windows, mac, linux) = match vk {
                    0x41 => (0x1e, 0, 38),
                    0x51 => (0x10, 12, 24),
                    0x59 => (0x15, 16, 29),
                    0x5a => (0x2c, 6, 52),
                    0x10 => (0x2a, 56, 50),
                    0xa1 => (0x36, 60, 62),
                    0x11 => (0x1d, 59, 37),
                    0x12 => (0x38, 58, 64),
                    0x5b => (0x5b, 55, 133),
                    _ => panic!("add an independent native key fixture for {vk:#x}"),
                };
                let code = if cfg!(windows) {
                    windows
                } else if cfg!(target_os = "macos") {
                    mac
                } else {
                    linux
                };
                let flags = if cfg!(target_os = "macos") {
                    (u64::from(keys.contains(&0x10) || keys.contains(&0xa1)) << 17)
                        | (u64::from(keys.contains(&0x11)) << 18)
                        | (u64::from(keys.contains(&0x12)) << 19)
                        | (u64::from(keys.contains(&0x5b)) << 20)
                } else {
                    0
                };
                out.push(json!(["key", code, down, flags]));
            }
            Event::Text(text) => {
                for unit in text.encode_utf16() {
                    out.push(json!(["text", unit]));
                }
            }
            Event::MouseMove { dx, dy } => out.push(json!(["relative", dx, dy])),
            Event::MousePosition { x, y, .. } => out.push(json!(["absolute", x, y])),
            Event::MouseButton { button, down } => out.push(json!([
                "button",
                if cfg!(target_os = "macos") {
                    match button {
                        1 => 0,
                        2 => 2,
                        3 => 1,
                        4 => 3,
                        5 => 4,
                        _ => panic!("unknown button"),
                    }
                } else if cfg!(target_os = "linux") && *button >= 4 {
                    button + 4
                } else {
                    *button
                },
                down
            ])),
            Event::Scroll {
                vertical,
                horizontal,
            } => {
                if cfg!(target_os = "linux") {
                    for (axis, (value, positive, negative)) in
                        [(*vertical, 4, 5), (*horizontal, 7, 6)]
                            .into_iter()
                            .enumerate()
                    {
                        scroll[axis] += i32::from(value);
                        let notches = scroll[axis] / 120;
                        scroll[axis] %= 120;
                        for _ in 0..notches.unsigned_abs() {
                            let button = if notches > 0 { positive } else { negative };
                            out.push(json!(["button", button, true]));
                            out.push(json!(["button", button, false]));
                        }
                    }
                } else if cfg!(target_os = "macos") {
                    out.push(json!([
                        "pixels",
                        i32::from(*vertical) / 3,
                        -i32::from(*horizontal) / 3
                    ]));
                } else {
                    if *vertical != 0 {
                        out.push(json!(["wheel-v", vertical]));
                    }
                    if *horizontal != 0 {
                        out.push(json!(["wheel-h", horizontal]));
                    }
                }
            }
            Event::ReleaseAll => panic!("fixture releases keys explicitly"),
        }
    }
    out
}

#[test]
#[ignore = "native artifact consumer, run through scripts/pairs.py"]
fn pair_native_input() {
    let root = std::path::PathBuf::from(
        std::env::var_os("LATCH_PAIR_ARTIFACTS").expect("set LATCH_PAIR_ARTIFACTS"),
    );
    let files = format::files(&root, ".input.json");
    let mut consumed = 0;
    let mut failures = 0;
    for path in files {
        let recording: InputRecording = format::read(&path);
        assert_eq!(recording.version, format::VERSION);
        if recording.target != std::env::consts::OS {
            continue;
        }
        consumed += 1;
        for case in recording.cases {
            let row = json!({"producer": recording.producer, "consumer": format::platform(), "case": case.name});
            if !format::cell("input.jsonl", row, || {
                let observed = native(&format::inputs(&case.bytes));
                assert_eq!(
                    observed,
                    reference(&format::inputs(&case.bytes)),
                    "native adapter regressed, {} {}",
                    recording.producer,
                    case.name
                );
                let expected = reference(&format::inputs(&case.expected));
                let gap = format::input_gap(&case.name, &recording.producer);
                if gap {
                    assert_ne!(observed, expected, "known gap unexpectedly passes");
                } else {
                    assert_eq!(
                        observed,
                        expected,
                        "{} -> {} {}",
                        recording.producer,
                        format::platform(),
                        case.name
                    );
                }
                format::result(
                    "input.jsonl",
                    json!({ "producer": recording.producer, "consumer": format::platform(), "case": case.name, "status": if gap { "known-gap" } else { "passed" }, "observed": observed, "expected": expected }),
                );
            }) {
                failures += 1;
            }
        }
        let row = json!({"producer": recording.producer, "consumer": format::platform(), "case": "gamepad-report"});
        if !format::cell("input.jsonl", row, || {
            let pad: serde_json::Value =
                format::read(&path.parent().unwrap().join("gamepad.pad.json"));
            assert_eq!(pad["producer"], recording.producer);
            let bytes: Vec<u8> = serde_json::from_value(pad["bytes"].clone()).unwrap();
            let latch_proto::wire::Datagram::Pad { updates, .. } =
                latch_proto::wire::parse(&bytes).unwrap()
            else {
                panic!("no pad packet")
            };
            let update = updates.iter().next().unwrap();
            let s = update.state;
            assert_eq!(
                crate::gamepad::map::xusb(&s),
                [4, 16, 64, 191, 1, 128, 255, 127, 0, 0, 0, 0]
            );
            assert_eq!(
                crate::gamepad::map::ds4(&s),
                [0, 0, 128, 128, 38, 12, 0, 64, 191, 0]
            );
            #[cfg(target_os = "linux")]
            {
                assert_eq!(
                    crate::gamepad::map::ev_axes(&s),
                    [
                        (0, -32767),
                        (1, -32768),
                        (3, 0),
                        (4, -1),
                        (2, 256),
                        (5, 767),
                        (16, -1),
                        (17, 0)
                    ]
                );
            }
            format::result(
                "input.jsonl",
                json!({ "producer": recording.producer, "consumer": format::platform(), "case": "gamepad-report", "status": "passed", "xusb": crate::gamepad::map::xusb(&s), "ds4": crate::gamepad::map::ds4(&s), "native_bus": if cfg!(target_os = "macos") { "unavailable, macOS cannot add virtual gamepads" } else { "conversion only, hosted runner has no configured virtual bus" } }),
            );
        }) {
            failures += 1;
        }
    }
    assert!(consumed > 0, "no input artifacts for this host");
    assert_eq!(failures, 0, "required input cells failed");
}
