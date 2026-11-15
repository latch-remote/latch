use super::*;
use crate::pair_format as format;
use format::{InputCase, InputRecording};
use latch_proto::input::Event as WireEvent;

fn wire(sent: Vec<Sent>) -> Vec<u8> {
    let mut bytes = Vec::new();
    for event in sent {
        let event = match event {
            Sent::Key(vk, down, modifiers) => WireEvent::Key {
                vk: vk as u16,
                down,
                modifiers,
            },
            Sent::Button(button, down) => WireEvent::MouseButton { button, down },
            Sent::Position(x, y, width, height) => WireEvent::MousePosition {
                x: x as u16,
                y: y as u16,
                width: width as u16,
                height: height as u16,
            },
            Sent::Motion(dx, dy) => WireEvent::MouseMove { dx, dy },
            Sent::Scroll(vertical, horizontal) => WireEvent::Scroll {
                vertical,
                horizontal,
            },
            Sent::Text(text) => WireEvent::Text(text),
            Sent::ReleaseAll => WireEvent::ReleaseAll,
        };
        event.write(&mut bytes);
    }
    bytes
}

fn case(
    target: &str,
    name: &str,
    events: Vec<Event>,
    end: Modifiers,
    expected: Vec<Sent>,
) -> InputCase {
    let (mut h, tap) = recording_on(
        View {
            captured: true,
            grabbed: true,
            ..Default::default()
        },
        true,
        target,
    );
    h.state().view.early.on_event(None, &events, end);
    frame(&mut h, events, end);
    InputCase {
        name: name.into(),
        bytes: wire(tap.take()),
        expected: wire(expected),
    }
}

#[test]
#[ignore = "viewer artifact producer, run through scripts/pairs.py"]
fn pair_input_record() {
    let root = std::path::PathBuf::from(
        std::env::var_os("LATCH_PAIR_RECORD").expect("set LATCH_PAIR_RECORD"),
    );
    for target in ["windows", "macos", "linux"] {
        let mut cases = Vec::new();
        for (name, logical, physical, vk) in [
            ("ansi-a", Key::A, Key::A, 0x41),
            ("azerty-a-at-q", Key::A, Key::Q, 0x51),
            ("qwertz-z-at-y", Key::Z, Key::Y, 0x59),
        ] {
            let events = [true, false]
                .into_iter()
                .map(|pressed| Event::Key {
                    key: logical,
                    physical_key: Some(physical),
                    pressed,
                    repeat: false,
                    modifiers: Modifiers::NONE,
                })
                .collect();
            cases.push(case(
                target,
                name,
                events,
                Modifiers::NONE,
                vec![Sent::Key(vk, true, 0), Sent::Key(vk, false, 0)],
            ));
        }
        cases.push(case(
            target,
            "shift-a",
            vec![
                key(Key::A, true, Modifiers::SHIFT),
                key(Key::A, false, Modifiers::SHIFT),
            ],
            Modifiers::NONE,
            vec![
                Sent::Key(0x10, true, 1),
                Sent::Key(0x41, true, 1),
                Sent::Key(0x41, false, 1),
                Sent::Key(0x10, false, 0),
            ],
        ));
        cases.push(case(
            target,
            "right-shift-identity",
            vec![],
            Modifiers::SHIFT,
            vec![Sent::Key(0xa1, true, 1)],
        ));
        for (name, modifiers, vk, mask) in [
            ("control-a", Modifiers::CTRL, 0x11, 2),
            ("alt-a", Modifiers::ALT, 0x12, 4),
        ] {
            cases.push(case(
                target,
                name,
                vec![key(Key::A, true, modifiers), key(Key::A, false, modifiers)],
                Modifiers::NONE,
                vec![
                    Sent::Key(vk, true, mask),
                    Sent::Key(0x41, true, mask),
                    Sent::Key(0x41, false, mask),
                    Sent::Key(vk, false, 0),
                ],
            ));
        }
        let meta = Modifiers {
            command: true,
            mac_cmd: cfg!(target_os = "macos"),
            ..Modifiers::NONE
        };
        let (modifier, mask) = if cfg!(target_os = "macos") && target != "macos" {
            (0x11, 2)
        } else {
            (0x5b, 8)
        };
        cases.push(case(
            target,
            "held-super-z",
            vec![key(Key::Z, true, meta), key(Key::Z, false, meta)],
            meta,
            vec![
                Sent::Key(modifier, true, mask),
                Sent::Key(0x5a, true, mask),
                Sent::Key(0x5a, false, mask),
            ],
        ));
        cases.push(case(
            target,
            "unicode-commit",
            vec![Event::Ime(egui::ImeEvent::Commit("éλ你😀".into()))],
            Modifiers::NONE,
            vec![Sent::Text("éλ你😀".into())],
        ));
        cases.push(case(
            target,
            "relative-pointer",
            vec![Event::MouseMoved(Vec2::new(3.5, -2.0))],
            Modifiers::NONE,
            vec![Sent::Motion(10, -6)],
        ));
        cases.push(case(
            target,
            "wheel-line",
            vec![Event::MouseWheel {
                unit: egui::MouseWheelUnit::Line,
                delta: Vec2::new(1., 1.),
                modifiers: Modifiers::NONE,
            }],
            Modifiers::NONE,
            vec![Sent::Scroll(120, -120)],
        ));
        cases.push(case(
            target,
            "wheel-two-half-notches",
            vec![
                Event::MouseWheel {
                    unit: egui::MouseWheelUnit::Point,
                    delta: Vec2::new(0., 20.),
                    modifiers: Modifiers::NONE,
                },
                Event::MouseWheel {
                    unit: egui::MouseWheelUnit::Point,
                    delta: Vec2::new(0., 20.),
                    modifiers: Modifiers::NONE,
                },
            ],
            Modifiers::NONE,
            vec![Sent::Scroll(60, 0), Sent::Scroll(60, 0)],
        ));
        let (mut h, tap) = recording_on(View::default(), true, target);
        h.state_mut().cfg.capture_mouse = false;
        let events = vec![
            Event::PointerMoved(ON_PICTURE),
            click(ON_PICTURE, true),
            click(ON_PICTURE, false),
        ];
        h.state()
            .view
            .early
            .on_event(None, &events, Modifiers::NONE);
        frame(&mut h, events, Modifiers::NONE);
        cases.push(InputCase {
            name: "absolute-pointer-button".into(),
            bytes: wire(tap.take()),
            expected: wire(vec![
                Sent::Position(20, 170, 640, 360),
                Sent::Position(20, 170, 640, 360),
                Sent::Button(1, true),
                Sent::Position(20, 170, 640, 360),
                Sent::Button(1, false),
            ]),
        });
        for item in &cases {
            let gap = item.name == "right-shift-identity"
                || (item.name == "held-super-z" && !cfg!(target_os = "macos"));
            if gap {
                assert_ne!(
                    item.bytes, item.expected,
                    "known gap unexpectedly passes, remove its entry after fixing"
                );
                let observed = if item.name == "right-shift-identity" {
                    wire(vec![Sent::Key(0x10, true, 1)])
                } else {
                    wire(vec![Sent::Key(0x5a, true, 0), Sent::Key(0x5a, false, 0)])
                };
                assert_eq!(item.bytes, observed, "known gap changed");
            } else {
                assert_eq!(
                    item.bytes,
                    item.expected,
                    "viewer translation regressed, {} -> {target}, {}",
                    format::platform(),
                    item.name
                );
            }
        }
        let clipboard = latch_core::api::Clipboard::fit(
            if cfg!(windows) {
                "first\r\néλ你😀\r\n"
            } else {
                "first\néλ你😀\n"
            },
            0,
        );
        format::write(
            &root.join(format!("{target}.input.json")),
            &InputRecording {
                version: format::VERSION,
                producer: format::platform(),
                target: target.into(),
                cases,
                clipboard: clipboard.text,
            },
        );
    }
}
