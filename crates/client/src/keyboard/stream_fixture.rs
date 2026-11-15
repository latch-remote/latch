// Shared stimulus for a recording made by running bf5da0d's real View::forward
// and InputSender. The frozen bytes live in legacy_stream.json.
fn fixture_modifiers(viewer: &str, vk: u16, down: bool) -> Modifiers {
    let mut mods = Modifiers::NONE;
    if down {
        match vk {
            0xa0 | 0xa1 => mods.shift = true,
            0xa2 | 0xa3 => {
                mods.ctrl = true;
                mods.command = viewer != "macos";
            }
            0xa4 | 0xa5 => mods.alt = true,
            0x5b | 0x5c if viewer == "macos" => {
                mods.mac_cmd = true;
                mods.command = true;
            }
            _ => {}
        }
    }
    mods
}

fn fixture_datagram(events: Vec<Sent>, positions: bool) -> Vec<u8> {
    let mut sender = latch_proto::input::InputSender::new(109).with_key_positions(positions);
    for event in events {
        match event {
            Sent::Key(vk, down, modifiers) => sender.push(latch_proto::input::Event::Key {
                vk: vk as u16,
                down,
                modifiers,
            }),
            Sent::Text(text) => sender.push(latch_proto::input::Event::Text(text)),
            other => panic!("unexpected keyboard fixture event {other:?}"),
        }
    }
    let mut bytes = Vec::new();
    sender.poll(Instant::now(), &mut bytes);
    bytes
}

// Each element is a native position, its egui translation and the frame's
// final modifiers. Separate frames exercise held keys and release ordering.
fn fixture_frames(
    viewer: &str,
) -> Vec<(
    egui_winit::winit::keyboard::KeyCode,
    bool,
    Vec<Event>,
    Modifiers,
)> {
    use egui_winit::winit::keyboard::KeyCode;
    let mut frames = Vec::new();
    let command = if viewer == "macos" {
        (KeyCode::SuperLeft, 0x5b)
    } else {
        (KeyCode::ControlLeft, 0xa2)
    };
    for (code, letter, clipboard) in [
        (KeyCode::KeyC, Key::C, Some(Event::Copy)),
        (KeyCode::KeyV, Key::V, Some(Event::Paste(String::new()))),
        (KeyCode::KeyZ, Key::Z, None),
    ] {
        let mods = fixture_modifiers(viewer, command.1, true);
        frames.push((command.0, true, vec![], mods));
        frames.push((
            code,
            true,
            vec![clipboard.unwrap_or_else(|| key(letter, true, mods))],
            mods,
        ));
        frames.push((code, false, vec![key(letter, false, mods)], mods));
        frames.push((command.0, false, vec![], Modifiers::NONE));
    }
    for (code, vk) in [
        (KeyCode::ControlLeft, 0xa2),
        (KeyCode::ControlRight, 0xa3),
        (KeyCode::SuperLeft, 0x5b),
        (KeyCode::SuperRight, 0x5c),
        (KeyCode::AltLeft, 0xa4),
        (KeyCode::AltRight, 0xa5),
    ] {
        let mods = fixture_modifiers(viewer, vk, true);
        frames.push((code, true, vec![], mods));
        if vk == 0xa5 {
            frames.push((KeyCode::KeyE, true, vec![key(Key::E, true, mods)], mods));
            frames.push((KeyCode::KeyE, false, vec![key(Key::E, false, mods)], mods));
        }
        frames.push((code, false, vec![], Modifiers::NONE));
    }
    frames
}
