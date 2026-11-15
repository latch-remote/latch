use egui_winit::winit::keyboard::{Key as Logical, KeyCode, NamedKey, PhysicalKey};

fn captured_frame(viewer: &str, code: KeyCode, down: bool, events: &[Event], mods: Modifiers) -> crate::keyboard::Frame {
    // winit's scancode conversion runs on its native OS. Other producers use
    // the corresponding constructed physical key. Windows Super's native
    // hook is represented by its queued key, as OS hooks cannot run on macOS.
    use egui_winit::winit::platform::scancode::PhysicalKeyExtScancode;
    let scan = match code {
        KeyCode::ControlLeft => (0x1d, 0x3b, 29),
        KeyCode::ControlRight => (0xe01d, 0x3e, 97),
        KeyCode::SuperLeft => (0xe05b, 0x37, 125),
        KeyCode::SuperRight => (0xe05c, 0x36, 126),
        KeyCode::AltLeft => (0x38, 0x3a, 56),
        KeyCode::AltRight => (0xe038, 0x3d, 100),
        KeyCode::KeyC => (0x2e, 0x08, 46),
        KeyCode::KeyV => (0x2f, 0x09, 47),
        KeyCode::KeyZ => (0x2c, 0x06, 44),
        KeyCode::KeyE => (0x12, 0x0e, 18),
        _ => panic!("missing fixture scancode"),
    };
    let physical = if viewer == std::env::consts::OS {
        PhysicalKey::from_scancode(match viewer { "windows" => scan.0, "macos" => scan.1, _ => scan.2 })
    } else { PhysicalKey::Code(code) };
    assert_eq!(physical, PhysicalKey::Code(code));
    let logical = if code == KeyCode::AltRight && viewer != "macos" {
        Logical::Named(NamedKey::AltGraph)
    } else { Logical::Unidentified(egui_winit::winit::keyboard::NativeKey::Unidentified) };
    crate::keyboard::test_capture(physical, logical, down, events, mods)
}

#[test]
fn default_capture_mapping_datagrams_cover_every_pair_and_mouse_mode() {
    use latch_proto::{input::Event as Wire, wire::{self, Datagram}};
    let legacy: serde_json::Value = serde_json::from_str(include_str!("legacy_stream.json")).unwrap();
    for viewer in ["windows", "macos", "linux"] {
        for host in ["windows", "macos", "linux"] {
            // Default configured capture, released and engaged, in a window and fullscreen.
            for (captured, fullscreen) in [(false, false), (true, false), (false, true), (true, true)] {
                for positions in [false, true] {
                    for immediate in [false, true] {
                        crate::keyboard::test_install(positions);
                        let (mut h, tap) = recording_on(View { viewer_mac: viewer == "macos", captured, ..View::default() }, true, host);
                        h.input_mut().viewports.get_mut(&egui::ViewportId::ROOT).unwrap().fullscreen = Some(fullscreen);
                        h.run_steps(1);
                        tap.take();
                        assert!(h.state().cfg.capture_mouse && h.state().cfg.cmd_is_ctrl);
                        let mut all = Vec::new();
                        for (code, down, upstream, mods) in fixture_frames(viewer) {
                            let native = captured_frame(viewer, code, down, &upstream, mods);
                            if immediate {
                                for (event, physical, _) in &native {
                                    if let Some(key) = physical {
                                        h.state().view.early.deliver(early::Native::Physical { key: *key, mods }, Instant::now());
                                    } else {
                                        h.state().view.early.on_event(None, std::slice::from_ref(event), mods);
                                    }
                                }
                            }
                            crate::keyboard::test_frame(native);
                            frame(&mut h, vec![], mods);
                            let sent = tap.take();
                            if positions {
                                let letter = crate::keyboard::vk(code).unwrap();
                                if down && matches!(letter, 0x43 | 0x56 | 0x5a) {
                                    let mask = if host == "macos" && (viewer == "macos" || letter != 0x5a || !captured) { 8 } else { 2 };
                                    assert!(sent.contains(&Sent::Key(letter as i16, true, mask)), "{viewer}->{host}, captured={captured}, immediate={immediate}, {code:?}: {sent:?}");
                                }
                                if down && letter == 0x45 {
                                    assert!(sent.contains(&Sent::Key(0x45, true, latch_stream::input::MOD_ALT)));
                                }
                                if down && upstream.is_empty() {
                                    let expected = match letter {
                                        0x5b | 0x5c if viewer == "macos" && host != "macos" => letter + 0x47,
                                        0xa2 | 0xa3 if viewer == "macos" && host != "macos" && !captured => letter - 0x47,
                                        _ => letter,
                                    };
                                    assert!(sent.iter().any(|e| matches!(e, Sent::Key(vk, true, _) if *vk as u16 == expected)), "{viewer}->{host}, captured={captured}: {sent:?}");
                                }
                            }
                            all.extend(sent);
                        }
                        assert!(!h.state().view.held.any_down());
                        let bytes = fixture_datagram(all, positions);
                        let Ok(Datagram::Input { events, .. }) = wire::parse(&bytes) else { panic!("invalid input datagram") };
                        assert!(events.iter().all(|e| matches!(e, Wire::Key { .. })));
                        if !positions {
                            let expected: Vec<u8> = serde_json::from_value(legacy[format!("{viewer}-{host}-{captured}")].clone()).unwrap();
                            assert_eq!(bytes, expected, "bf5da0d {viewer}->{host}, captured={captured}, fullscreen={fullscreen}, immediate={immediate}");
                        }
                        crate::keyboard::test_install(false);
                    }
                }
            }
        }
    }
}

#[test]
fn as_pressed_is_an_explicit_opt_out_in_every_mouse_mode() {
    for viewer in ["windows", "macos", "linux"] {
        for host in ["windows", "macos", "linux"] {
            for captured in [false, true] {
                crate::keyboard::test_install(true);
                let (mut h, tap) = recording_on(View { viewer_mac: viewer == "macos", captured, ..View::default() }, true, host);
                h.state_mut().cfg.cmd_is_ctrl = false;
                h.run_steps(1);
                for (code, vk) in [(KeyCode::SuperLeft, 0x5b), (KeyCode::ControlLeft, 0xa2), (KeyCode::AltRight, 0xa5)] {
                    for down in [true, false] {
                        let mods = fixture_modifiers(viewer, vk, down);
                        crate::keyboard::test_frame(captured_frame(viewer, code, down, &[], mods));
                        frame(&mut h, vec![], mods);
                        assert_eq!(tap.keys(), [(vk as i16, down)]);
                    }
                }
                let mods = fixture_modifiers(viewer, if viewer == "macos" { 0x5b } else { 0xa2 }, true);
                frame(&mut h, vec![Event::Copy], mods);
                let expected = if viewer == "macos" { VK_LWIN } else { VK_CONTROL };
                assert!(tap.keys().contains(&(expected, true)));
                crate::keyboard::test_install(false);
            }
        }
    }
}

#[test]
fn windowed_free_mouse_keeps_mains_quit_and_hide_menu_override() {
    for positions in [false, true] {
        crate::keyboard::test_install(positions);
        let (mut h, tap) = recording_on(View::default(), true, "windows");
        assert!(!h.state().view.captured);
        assert!(crate::macos::TEST_OWNS_KEYS.get());
        for letter in [Key::Q, Key::H] {
            frame(&mut h, vec![key(letter, true, Modifiers::MAC_CMD)], Modifiers::MAC_CMD);
            assert!(crate::macos::TEST_OWNS_KEYS.get());
            assert!(tap.keys().contains(&(input::vk(letter).unwrap(), true)));
        }
        frame(&mut h, vec![Event::WindowFocused(false)], Modifiers::NONE);
        assert!(!crate::macos::TEST_OWNS_KEYS.get());
        crate::keyboard::test_install(false);
    }
}

#[test]
fn free_mouse_setting_has_the_same_mapping_as_released_default_capture() {
    for viewer in ["windows", "macos", "linux"] {
        for host in ["windows", "macos", "linux"] {
            for positions in [false, true] {
                let mut recordings = Vec::new();
                for capture_mouse in [true, false] {
                    crate::keyboard::test_install(positions);
                    let (mut h, tap) = recording_on(View { viewer_mac: viewer == "macos", ..View::default() }, true, host);
                    h.state_mut().cfg.capture_mouse = capture_mouse;
                    let mut all = Vec::new();
                    for (code, down, upstream, mods) in fixture_frames(viewer) {
                        crate::keyboard::test_frame(captured_frame(viewer, code, down, &upstream, mods));
                        frame(&mut h, vec![], mods);
                        all.extend(tap.take());
                    }
                    recordings.push(fixture_datagram(all, positions));
                    crate::keyboard::test_install(false);
                }
                assert_eq!(recordings[0], recordings[1], "{viewer}->{host}, positions={positions}");
            }
        }
    }
}
