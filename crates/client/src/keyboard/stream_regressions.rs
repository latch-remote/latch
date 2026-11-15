use egui_winit::winit::keyboard::{Key as Logical, KeyCode, NamedKey, PhysicalKey};

fn captured_frame(
    viewer: &str,
    code: KeyCode,
    down: bool,
    events: &[Event],
    mods: Modifiers,
) -> crate::keyboard::Frame {
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
        PhysicalKey::from_scancode(match viewer {
            "windows" => scan.0,
            "macos" => scan.1,
            _ => scan.2,
        })
    } else {
        PhysicalKey::Code(code)
    };
    assert_eq!(physical, PhysicalKey::Code(code));
    if crate::keyboard::available()
        && viewer == "windows"
        && matches!(code, KeyCode::SuperLeft | KeyCode::SuperRight)
    {
        // A swallowed Windows Super event enters from the hook queue.
        return vec![(
            key(Key::F35, down, mods),
            Some(crate::keyboard::Key {
                vk: crate::keyboard::vk(code).unwrap(),
                down,
            }),
            false,
        )];
    }
    let logical = if code == KeyCode::AltRight && viewer != "macos" {
        Logical::Named(NamedKey::AltGraph)
    } else {
        Logical::Unidentified(egui_winit::winit::keyboard::NativeKey::Unidentified)
    };
    crate::keyboard::test_capture(physical, logical, down, events, mods)
}

// Explicit host events, independent of the production mapping and Held logic.
fn expected_native_chord(viewer: &str, host: &str, captured: bool, letter: i16) -> Vec<Sent> {
    if viewer != "macos" && host == "macos" {
        if !captured {
            return vec![
                Sent::Key(0xa2, true, 2),
                Sent::Key(0xa2, false, 0),
                Sent::Key(0x5b, true, 8),
                Sent::Key(letter, true, 8),
                Sent::Key(0x5b, false, 0),
                Sent::Key(0xa2, true, 2),
                Sent::Key(letter, false, 2),
                Sent::Key(0xa2, false, 0),
            ];
        }
        if letter != 0x5a {
            return vec![
                Sent::Key(0xa2, true, 2),
                Sent::Key(0xa2, false, 0),
                Sent::Key(0x5b, true, 8),
                Sent::Key(letter, true, 8),
                Sent::Key(letter, false, 8),
                Sent::Key(0x5b, false, 0),
                Sent::Key(0xa2, true, 2),
                Sent::Key(0xa2, false, 0),
            ];
        }
    }
    let (modifier, mask) = if viewer == "macos" && host == "macos" {
        (0x5b, 8)
    } else {
        (0xa2, 2)
    };
    vec![
        Sent::Key(modifier, true, mask),
        Sent::Key(letter, true, mask),
        Sent::Key(letter, false, mask),
        Sent::Key(modifier, false, 0),
    ]
}

fn expected_native_fixture(viewer: &str, host: &str, captured: bool) -> Vec<Sent> {
    let mut expected = Vec::new();
    for letter in [0x43, 0x56, 0x5a] {
        expected.extend(expected_native_chord(viewer, host, captured, letter));
    }
    for vk in [0xa2, 0xa3, 0x5b, 0x5c, 0xa4, 0xa5] {
        let target = match vk {
            0x5b if viewer == "macos" && host != "macos" => 0xa2,
            0x5c if viewer == "macos" && host != "macos" => 0xa3,
            _ => vk,
        };
        let mask = match target {
            0xa2 | 0xa3 => 2,
            0xa4 | 0xa5 => 4,
            _ => 8,
        };
        expected.push(Sent::Key(target, true, mask));
        if vk == 0xa5 {
            expected.extend([Sent::Key(0x45, true, 4), Sent::Key(0x45, false, 4)]);
        }
        expected.push(Sent::Key(target, false, 0));
    }
    expected
}

#[test]
fn default_capture_mapping_datagrams_cover_every_pair_and_mouse_mode() {
    use latch_proto::{
        input::Event as Wire,
        wire::{self, Datagram},
    };
    let legacy: serde_json::Value =
        serde_json::from_str(include_str!("legacy_stream.json")).unwrap();
    for viewer in ["windows", "macos", "linux"] {
        for host in ["windows", "macos", "linux"] {
            // Default configured capture, released and engaged, in a window and fullscreen.
            for (captured, fullscreen) in
                [(false, false), (true, false), (false, true), (true, true)]
            {
                for positions in [false, true] {
                    for immediate in [false, true] {
                        crate::keyboard::test_install(positions);
                        let (mut h, tap) = recording_on(
                            View {
                                viewer_mac: viewer == "macos",
                                captured,
                                ..View::default()
                            },
                            true,
                            host,
                        );
                        h.input_mut()
                            .viewports
                            .get_mut(&egui::ViewportId::ROOT)
                            .unwrap()
                            .fullscreen = Some(fullscreen);
                        h.run_steps(1);
                        tap.take();
                        assert!(h.state().cfg.capture_mouse && h.state().cfg.cmd_is_ctrl);
                        let mut all = Vec::new();
                        for (code, down, upstream, mods) in fixture_frames(viewer) {
                            let native = captured_frame(viewer, code, down, &upstream, mods);
                            if immediate {
                                for (event, physical, _) in &native {
                                    if let Some(key) = physical {
                                        h.state().view.early.deliver(
                                            early::Native::Physical { key: *key, mods },
                                            Instant::now(),
                                        );
                                    } else {
                                        h.state().view.early.on_event(
                                            None,
                                            std::slice::from_ref(event),
                                            mods,
                                        );
                                    }
                                }
                            }
                            crate::keyboard::test_frame(native);
                            frame(&mut h, vec![], mods);
                            let sent = tap.take();
                            all.extend(sent);
                        }
                        assert!(!h.state().view.held.any_down());
                        if positions {
                            assert_eq!(all, expected_native_fixture(viewer, host, captured),
                                "{viewer}->{host}, captured={captured}, fullscreen={fullscreen}, immediate={immediate}");
                        }
                        let bytes = fixture_datagram(all, positions);
                        let Ok(Datagram::Input { events, .. }) = wire::parse(&bytes) else {
                            panic!("invalid input datagram")
                        };
                        assert!(events.iter().all(|e| matches!(e, Wire::Key { .. })));
                        if !positions {
                            let expected: Vec<u8> = serde_json::from_value(
                                legacy[format!("{viewer}-{host}-{captured}")].clone(),
                            )
                            .unwrap();
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
                let (mut h, tap) = recording_on(
                    View {
                        viewer_mac: viewer == "macos",
                        captured,
                        ..View::default()
                    },
                    true,
                    host,
                );
                h.state_mut().cfg.cmd_is_ctrl = false;
                h.run_steps(1);
                for (code, vk) in [
                    (KeyCode::SuperLeft, 0x5b),
                    (KeyCode::ControlLeft, 0xa2),
                    (KeyCode::AltRight, 0xa5),
                ] {
                    for down in [true, false] {
                        let mods = fixture_modifiers(viewer, vk, down);
                        crate::keyboard::test_frame(captured_frame(viewer, code, down, &[], mods));
                        frame(&mut h, vec![], mods);
                        assert_eq!(tap.keys(), [(vk as i16, down)]);
                    }
                }
                let mods =
                    fixture_modifiers(viewer, if viewer == "macos" { 0x5b } else { 0xa2 }, true);
                frame(&mut h, vec![Event::Copy], mods);
                let expected = if viewer == "macos" {
                    VK_LWIN
                } else {
                    VK_CONTROL
                };
                assert_eq!(
                    tap.keys(),
                    [
                        (expected, true),
                        (input::VK_C, true),
                        (input::VK_C, false),
                        (expected, false),
                    ]
                );
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
            frame(
                &mut h,
                vec![key(letter, true, Modifiers::MAC_CMD)],
                Modifiers::MAC_CMD,
            );
            assert!(crate::macos::TEST_OWNS_KEYS.get());
            assert_eq!(
                tap.keys(),
                if letter == Key::Q && !positions {
                    vec![(VK_CONTROL, true), (input::vk(letter).unwrap(), true)]
                } else {
                    vec![(input::vk(letter).unwrap(), true)]
                }
            );
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
                    let (mut h, tap) = recording_on(
                        View {
                            viewer_mac: viewer == "macos",
                            ..View::default()
                        },
                        true,
                        host,
                    );
                    h.state_mut().cfg.capture_mouse = capture_mouse;
                    let mut all = Vec::new();
                    for (code, down, upstream, mods) in fixture_frames(viewer) {
                        crate::keyboard::test_frame(captured_frame(
                            viewer, code, down, &upstream, mods,
                        ));
                        frame(&mut h, vec![], mods);
                        all.extend(tap.take());
                    }
                    recordings.push(fixture_datagram(all, positions));
                    crate::keyboard::test_install(false);
                }
                assert_eq!(
                    recordings[0], recordings[1],
                    "{viewer}->{host}, positions={positions}"
                );
            }
        }
    }
}

#[test]
fn image_or_empty_clipboard_uses_mains_paste_modifier_in_every_pair_and_mouse_mode() {
    for viewer in ["windows", "macos", "linux"] {
        for host in ["windows", "macos", "linux"] {
            for captured in [false, true] {
                for positions in [false, true] {
                    crate::keyboard::test_install(positions);
                    let (mut h, tap) = recording_on(
                        View {
                            viewer_mac: viewer == "macos",
                            captured,
                            ..View::default()
                        },
                        true,
                        host,
                    );
                    assert!(h.state().cfg.capture_mouse && h.state().cfg.cmd_is_ctrl);
                    let (source, vk) = if viewer == "macos" {
                        (KeyCode::SuperLeft, 0x5b)
                    } else {
                        (KeyCode::ControlLeft, 0xa2)
                    };
                    let mods = fixture_modifiers(viewer, vk, true);
                    for (code, down, events) in [
                        (source, true, vec![]),
                        (KeyCode::KeyV, true, vec![]),
                        (KeyCode::KeyV, false, vec![key(Key::V, false, mods)]),
                    ] {
                        crate::keyboard::test_frame(captured_frame(
                            viewer, code, down, &events, mods,
                        ));
                        frame(&mut h, vec![], mods);
                    }
                    let events = tap.take();
                    // Main retains a held generic Ctrl while adding Command
                    // on its legacy PC-to-Mac clipboard path.
                    let mask = if host == "macos" {
                        if !positions && viewer != "macos" {
                            10
                        } else {
                            8
                        }
                    } else {
                        2
                    };
                    crate::keyboard::test_frame(captured_frame(
                        viewer,
                        source,
                        false,
                        &[],
                        Modifiers::NONE,
                    ));
                    frame(&mut h, vec![], Modifiers::NONE);
                    if positions {
                        let mut complete = events.clone();
                        complete.extend(tap.take());
                        assert_eq!(
                            complete,
                            expected_native_chord(viewer, host, captured, 0x56),
                            "{viewer}->{host}, captured={captured}"
                        );
                    }
                    assert!(!h.state().view.held.any_down());
                    if !positions {
                        assert!(
                        events.contains(&Sent::Key(input::VK_V, true, mask)),
                        "{viewer}->{host}, captured={captured}, positions={positions}: {events:?}"
                    );
                        assert_eq!(
                            events
                                .iter()
                                .filter(|e| matches!(e, Sent::Key(input::VK_V, true, _)))
                                .count(),
                            1
                        );
                    }
                    let bytes = fixture_datagram(events, positions);
                    assert!(latch_proto::wire::parse(&bytes).is_ok());
                    crate::keyboard::test_install(false);
                }
            }
        }
    }
}

#[test]
fn mac_control_option_and_capture_toggles_retarget_held_modifiers() {
    for host in ["windows", "linux"] {
        for option in [false, true] {
            for (code, ctrl, meta) in [
                (KeyCode::ControlLeft, 0xa2, 0x5b),
                (KeyCode::ControlRight, 0xa3, 0x5c),
            ] {
                for release_captured in [false, true] {
                    crate::keyboard::test_install(true);
                    let (mut h, tap) = recording_on(
                        View {
                            viewer_mac: true,
                            ..View::default()
                        },
                        true,
                        host,
                    );
                    h.state_mut().cfg.control_as_meta = option;
                    h.run_steps(1);
                    tap.take();
                    let mods = fixture_modifiers("macos", ctrl, true);
                    crate::keyboard::test_frame(captured_frame("macos", code, true, &[], mods));
                    frame(&mut h, vec![], mods);
                    let first = if option { meta } else { ctrl };
                    assert_eq!(
                        tap.take(),
                        [Sent::Key(first as i16, true, if option { 8 } else { 2 })]
                    );
                    h.state_mut().view.captured = true;
                    frame(&mut h, vec![], mods);
                    assert_eq!(
                        tap.take(),
                        if option {
                            vec![
                                Sent::Key(ctrl as i16, true, 10),
                                Sent::Key(meta as i16, false, 2),
                            ]
                        } else {
                            vec![]
                        }
                    );
                    if !release_captured {
                        h.state_mut().view.captured = false;
                        frame(&mut h, vec![], mods);
                        assert_eq!(
                            tap.take(),
                            if option {
                                vec![
                                    Sent::Key(ctrl as i16, false, 0),
                                    Sent::Key(meta as i16, true, 8),
                                ]
                            } else {
                                vec![]
                            }
                        );
                    }
                    crate::keyboard::test_frame(captured_frame(
                        "macos",
                        code,
                        false,
                        &[],
                        Modifiers::NONE,
                    ));
                    frame(&mut h, vec![], Modifiers::NONE);
                    let last = if option && !release_captured {
                        meta
                    } else {
                        ctrl
                    };
                    assert_eq!(tap.take(), [Sent::Key(last as i16, false, 0)]);
                    assert!(!h.state().view.held.any_down());
                    crate::keyboard::test_install(false);
                }
            }
        }
    }
}

#[test]
fn captured_stream_schedules_heartbeats_without_video_or_input() {
    use crate::input::Recorder;
    use crate::keyboard::hook::{Lease, HEARTBEAT_MS, LEASE_MS};
    for viewer in ["windows", "linux"] {
        crate::keyboard::test_install(true);
        let ctx = egui::Context::default();
        let live = super::tests::dummy_live(&ctx);
        let cfg = ClientConfig::default();
        let video = Rect::from_min_size(Pos2::ZERO, Vec2::new(640.0, 420.0));
        let tap = Rc::new(Recorder::default());
        tap.connected.set(true);
        let mut view = View {
            viewer_mac: false,
            captured: true,
            tap: Some(tap.clone()),
            ..View::default()
        };
        let lease = Lease::default();
        let mods = fixture_modifiers(viewer, 0xa2, true);
        crate::keyboard::test_frame(captured_frame(
            viewer,
            KeyCode::ControlLeft,
            true,
            &[],
            mods,
        ));
        let raw = |now| egui::RawInput {
            time: Some(now as f64 / 1000.0),
            screen_rect: Some(video),
            focused: true,
            modifiers: mods,
            ..Default::default()
        };
        let _ = ctx.run(raw(0), |ctx| view.input(ctx, &live, &cfg, video, None));
        view.held
            .button(&*tap, latch_stream::input::BUTTON_LEFT, true);
        tap.take();
        let mut last = 0;
        // Enter the production input path without UI animations that could
        // accidentally supply a repaint. No video frames or input arrive.
        for now in (0..=1_000).step_by(HEARTBEAT_MS as usize) {
            assert!(lease.active(now));
            let output = ctx.run(raw(now), |ctx| view.input(ctx, &live, &cfg, video, None));
            assert!(
                output.viewport_output[&egui::ViewportId::ROOT].repaint_delay
                    <= Duration::from_millis(HEARTBEAT_MS)
            );
            lease.renew_at(now);
            last = now;
            assert!(view.held.physical_down(0xa2));
            assert!(tap.take().is_empty());
        }
        assert!(lease.active(1_000));
        assert!(!lease.active(last + LEASE_MS));
        view.early.on_event(
            Some(&egui_winit::winit::event::WindowEvent::Focused(false)),
            &[],
            Modifiers::NONE,
        );
        assert_eq!(
            tap.take(),
            [
                Sent::Key(0xa2, false, 0),
                Sent::Button(latch_stream::input::BUTTON_LEFT, false),
                Sent::ReleaseAll
            ]
        );
        crate::keyboard::test_install(false);
    }
}

#[test]
fn mac_control_held_during_click_to_capture_retargets_before_the_click() {
    let on_picture = Pos2::new(20.0, 200.0);
    for host in ["windows", "linux"] {
        crate::keyboard::test_install(true);
        let (mut h, tap) = recording_on(
            View {
                viewer_mac: true,
                ..View::default()
            },
            true,
            host,
        );
        h.state_mut().cfg.control_as_meta = true;
        h.run_steps(1);
        let mods = fixture_modifiers("macos", 0xa2, true);
        crate::keyboard::test_frame(captured_frame(
            "macos",
            KeyCode::ControlLeft,
            true,
            &[],
            mods,
        ));
        frame(&mut h, vec![], mods);
        assert_eq!(tap.keys(), [(0x5b, true)]);
        frame(
            &mut h,
            vec![
                Event::PointerMoved(on_picture),
                Event::PointerButton {
                    pos: on_picture,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: mods,
                },
            ],
            mods,
        );
        assert!(h.state().view.captured);
        assert_eq!(tap.keys(), [(0xa2, true), (0x5b, false)]);
        crate::keyboard::test_frame(captured_frame(
            "macos",
            KeyCode::KeyZ,
            true,
            &[key(Key::Z, true, mods)],
            mods,
        ));
        frame(&mut h, vec![], mods);
        assert_eq!(tap.take(), [Sent::Key(0x5a, true, 2)]);
        crate::keyboard::test_frame(captured_frame(
            "macos",
            KeyCode::ControlLeft,
            false,
            &[],
            Modifiers::NONE,
        ));
        frame(&mut h, vec![], Modifiers::NONE);
        assert_eq!(tap.keys(), [(0xa2, false)]);
        frame(&mut h, vec![Event::WindowFocused(false)], Modifiers::NONE);
        assert!(!h.state().view.held.any_down());
        crate::keyboard::test_install(false);
    }
}
