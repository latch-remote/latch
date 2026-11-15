//! Real Session startup/receive/reassembly/decode, with test-only observations.
use super::*;
use crate::pair_format as format;
use format::{Lane, Recording};
use std::net::Ipv4Addr;

type DecodedPicture = (u32, u32, u32, Vec<u8>);

pub(super) struct Probe {
    pub native: bool,
    pub channels: u32,
    pictures: Mutex<Vec<DecodedPicture>>,
    sound: Mutex<Vec<(u32, usize, String)>>,
    missing: Mutex<Vec<u32>>,
    pub decode_errors: Mutex<Vec<(u32, String)>>,
    pub fec: AtomicU64,
}

impl Probe {
    pub fn picture(&self, identity: u32, frame: &crate::Frame) {
        let pixels = frame.with_planes(|y, ys, uv, cs| {
            let mut pixels = Vec::new();
            for row in y.chunks(ys).take(frame.height as usize) {
                pixels.extend_from_slice(&row[..frame.width as usize]);
            }
            for row in uv.chunks(cs).take(frame.height as usize / 2) {
                pixels.extend_from_slice(&row[..frame.width as usize]);
            }
            pixels
        });
        self.pictures
            .lock()
            .push((identity, frame.width, frame.height, pixels));
    }
    pub fn audio(&self, out: &AudioOut) {
        match out {
            AudioOut::Frame { seq, payload, .. } => {
                let mut pcm = Vec::new();
                let channels = latch_codec::audio::decode_channels(payload, &mut pcm).unwrap();
                self.sound
                    .lock()
                    .push((*seq, channels, format::pcm_hash(&pcm)));
            }
            AudioOut::Missing { seq } => self.missing.lock().push(*seq),
        }
    }
}

fn check(recording: Recording, native: bool, schedule: &str) {
    let impaired = schedule == "loss-reorder";
    let late_audio = schedule == "late-audio";
    let (audio_group, repair_us) = format::recovery_group(&recording);
    let probe = Arc::new(Probe {
        native,
        channels: 6,
        pictures: Mutex::new(Vec::new()),
        sound: Mutex::new(Vec::new()),
        missing: Mutex::new(Vec::new()),
        decode_errors: Mutex::new(Vec::new()),
        fec: AtomicU64::new(0),
    });
    let recording = Arc::new(recording);
    let (udp, listener) = format::ports::udp_and_tcp_on_one_port();
    udp.set_nonblocking(true).unwrap();
    let port = udp.local_addr().unwrap().port();
    let tape = recording.clone();
    let host_probe = probe.clone();
    let host = std::thread::spawn(move || {
        let (mut tcp, peer) = listener.accept().unwrap();
        tcp.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let request = latch_core::http::read_request(&mut tcp).unwrap().unwrap();
        assert_eq!(request.path, control::PATH);
        let request: control::StreamRequest = request.json().unwrap();
        assert_eq!(request.protocol, tape.request.protocol);
        assert!(request.codecs.contains(&tape.accept.codec));
        assert!(request.audio && request.cursor);
        assert_eq!(request.audio_channels, tape.request.audio_channels);
        write!(
            tcp,
            "HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: {}\r\n\r\n",
            control::UPGRADE
        )
        .unwrap();
        control::write_message(&mut tcp, &Message::Accept(tape.accept.clone())).unwrap();
        let viewer = SocketAddr::new(peer.ip(), request.udp_port);
        let control_drain = {
            let mut read = tcp.try_clone().unwrap();
            std::thread::spawn(move || while control::read_message(&mut read).is_ok() {})
        };
        let token = control::token_from_hex(&tape.accept.token).unwrap();
        let hello_seen = std::cell::Cell::new(false);
        let requests = std::cell::Cell::new(0);
        let mut dropped_video = 0;
        let mut dropped_audio = 0;
        let mut fec_erased = 0;
        let mut requests_at_loss = None;
        let mut receive = [0; 2048];
        let mut reply = Vec::new();
        let mut poll = || {
            while let Ok((length, _)) = udp.recv_from(&mut receive) {
                match wire::parse(&receive[..length]).unwrap() {
                    Datagram::Hello {
                        token: Some(value), ..
                    } => {
                        assert_eq!(value, &token);
                        hello_seen.set(true);
                    }
                    Datagram::Ping { time, .. } => {
                        wire::write_pong(&mut reply, tape.accept.session, time);
                        udp.send_to(&reply, viewer).unwrap();
                    }
                    Datagram::KeyframeRequest { .. } => requests.set(requests.get() + 1),
                    _ => {}
                }
            }
        };
        // Wait for the real viewer Hello before feeding the recording.
        let deadline = Instant::now() + Duration::from_secs(3);
        while !hello_seen.get() {
            assert!(Instant::now() < deadline, "viewer Hello deadline");
            poll();
            std::thread::sleep(Duration::from_millis(1));
        }
        let start = Instant::now();
        let mut reordered = Vec::new();
        let mut pause = Duration::ZERO;
        let mut audio_sent = false;
        for event in &tape.events {
            // Leave time for receiver/decode startup on two-core hosted machines.
            let due = start + Duration::from_micros(event.us) + Duration::from_millis(100) + pause;
            while Instant::now() < due {
                poll();
                std::thread::sleep(Duration::from_millis(1));
            }
            if event.lane == Lane::Control {
                let message = control::read_message(&mut &event.bytes[..]).unwrap();
                if matches!(message, Message::End(_)) {
                    continue;
                }
                tcp.write_all(&event.bytes).unwrap();
                continue;
            }
            let datagram = wire::parse(&event.bytes).unwrap();
            // Hold the recovery IDR until the real viewer asks. Freeze tape time
            // while waiting so subsequent frames and audio retain their spacing.
            if impaired
                && matches!(datagram, Datagram::Video { shard, .. } if shard.frame == 6)
                && requests.get() <= requests_at_loss.expect("frame 4 must precede recovery IDR")
            {
                let waiting = Instant::now();
                let deadline = waiting + Duration::from_secs(2);
                while requests.get() <= requests_at_loss.unwrap() {
                    assert!(
                        Instant::now() < deadline,
                        "viewer did not request a recovery keyframe within 2 s"
                    );
                    poll();
                    std::thread::sleep(Duration::from_millis(1));
                }
                pause += waiting.elapsed();
            }
            if impaired || late_audio {
                if let Datagram::Audio { packet, .. } = datagram {
                    if packet.seq >= audio_group
                        && packet.seq < audio_group + latch_proto::audio::GROUP
                    {
                        if !audio_sent {
                            audio_sent = true;
                            // Dispatch this tape-derived group together. This removes
                            // OS sleep granularity from the inside-hold recovery check.
                            let mut parity = None;
                            for data in &tape.events {
                                if data.lane != Lane::Datagram {
                                    continue;
                                }
                                if let Datagram::Audio { packet: p, .. } =
                                    wire::parse(&data.bytes).unwrap()
                                {
                                    if p.parity && p.seq == audio_group {
                                        parity = Some(&data.bytes);
                                    }
                                    if !p.parity
                                        && p.seq > audio_group
                                        && p.seq < audio_group + latch_proto::audio::GROUP
                                    {
                                        udp.send_to(&data.bytes, viewer).unwrap();
                                    }
                                }
                            }
                            dropped_audio += 1;
                            if late_audio {
                                let waiting = Instant::now();
                                let deadline = waiting + Duration::from_secs(2);
                                while !host_probe.missing.lock().contains(&audio_group)
                                    || waiting.elapsed() < latch_proto::audio::AUDIO_HOLD * 2
                                {
                                    assert!(
                                        Instant::now() < deadline,
                                        "viewer did not conceal late audio within 2 s"
                                    );
                                    poll();
                                    std::thread::sleep(Duration::from_millis(1));
                                }
                                pause += waiting.elapsed();
                            }
                            udp.send_to(parity.expect("selected group parity"), viewer)
                                .unwrap();
                        }
                        continue;
                    }
                }
            }
            if impaired {
                match datagram {
                    Datagram::Video { shard, .. } if shard.frame == 4 => {
                        requests_at_loss.get_or_insert(requests.get());
                        dropped_video += 1;
                        continue;
                    }
                    Datagram::Video { shard, .. } if shard.frame == 2 && shard.shard == 0 => {
                        fec_erased += 1;
                        continue;
                    }
                    Datagram::Video { shard, .. } if shard.frame == 7 => {
                        reordered.push(event.bytes.clone());
                        continue;
                    }
                    _ => {}
                }
            }
            for bytes in reordered.drain(..).rev() {
                udp.send_to(&bytes, viewer).unwrap();
            }
            udp.send_to(&event.bytes, viewer).unwrap();
        }
        for bytes in reordered.drain(..).rev() {
            udp.send_to(&bytes, viewer).unwrap();
        }
        let deadline = Instant::now() + Duration::from_millis(350);
        while Instant::now() < deadline {
            poll();
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(
            hello_seen.get(),
            "viewer never authenticated the UDP session"
        );
        if impaired || late_audio {
            assert!(
                audio_sent && dropped_audio == 1,
                "selected audio loss must run"
            );
        }
        if impaired {
            assert!(dropped_video > 0 && fec_erased > 0);
            assert!(
                requests.get() > 0,
                "loss did not exercise keyframe requests"
            );
        }
        control::write_message(
            &mut tcp,
            &Message::End(control::End {
                code: control::end::ENDED,
                message: "pair replay complete".into(),
            }),
        )
        .unwrap();
        let _ = tcp.shutdown(Shutdown::Write);
        control_drain.join().unwrap();
        requests.get()
    });
    let (tx, rx) = mpsc::channel();
    let mut inner = Inner::new(tx, Arc::new(FrameSlot::default()), Box::new(|| {}));
    inner.pair_probe = Some(probe.clone());
    let inner = Arc::new(inner);
    let session = Session {
        inner: inner.clone(),
    };
    let worker = std::thread::spawn(move || {
        run(
            &inner,
            &Server {
                address: Ipv4Addr::LOCALHOST.into(),
                port,
            },
            &Settings {
                prefer_color: false,
                color: Default::default(),
                width: 160,
                height: 98,
                fps: 25,
                bitrate_kbps: 4000,
                hevc: false,
                av1: false,
                audio: true,
                name: "pair replay".into(),
                cursor: true,
                gamepads: false,
                mic: false,
            },
        );
        inner.finish();
    });
    loop {
        match rx
            .recv_timeout(Duration::from_secs(15))
            .expect("replay did not terminate")
        {
            Event::Terminated { code, message } => {
                assert_eq!(code, 0, "{message}");
                break;
            }
            Event::Failed { stage, message } => panic!("{stage}: {message}"),
            _ => {}
        }
    }
    worker.join().unwrap();
    let requests = host.join().unwrap();
    assert!(session.finished());
    let cursor = session.cursor().expect("no cursor message received");
    assert!(cursor.visible);
    let shape = cursor.shape.unwrap();
    assert_eq!(
        (
            shape.serial,
            shape.width,
            shape.height,
            shape.hot_x,
            shape.hot_y
        ),
        (7, 2, 2, 1, 0)
    );
    assert_eq!(shape.decode_rgba().unwrap(), [255; 16]);
    let intended: Vec<_> = recording
        .pictures
        .iter()
        .filter(|picture| !impaired || ![4, 5].contains(&picture.frame))
        .collect();
    let pictures = probe.pictures.lock();
    let observed_frames: Vec<_> = pictures.iter().map(|p| p.0).collect();
    let expected_frames: Vec<_> = intended.iter().map(|p| p.frame).collect();
    let gap = format::MAC_VT_MF_GAP
        && cfg!(windows)
        && native
        && recording.producer.starts_with("macos-")
        && matches!(recording.mode.as_str(), "native" | "capture");
    if gap {
        assert_ne!(
            observed_frames, expected_frames,
            "Mac VideoToolbox -> Media Foundation gap unexpectedly passes"
        );
        assert_eq!(
            observed_frames,
            [6, 12, 18],
            "Mac VideoToolbox -> Media Foundation gap changed"
        );
    }
    let expected: Vec<_> = intended
        .into_iter()
        .filter(|p| !gap || [6, 12, 18].contains(&p.frame))
        .collect();
    assert_eq!(
        pictures.len(),
        expected.len(),
        "decoded identities {:?}",
        pictures.iter().map(|f| f.0).collect::<Vec<_>>()
    );
    let mut min_psnr = 99f64;
    for ((identity, width, height, pixels), expected) in pictures.iter().zip(expected) {
        assert_eq!(
            (*identity, *width, *height),
            (expected.frame, expected.width, expected.height),
            "frame identity/order/cropping"
        );
        assert_eq!(pixels.len(), expected.pixels.len());
        assert_eq!(format::hash(&expected.pixels), expected.hash);
        if !native {
            assert_eq!(
                format::hash(pixels),
                expected.hash,
                "software picture hash, frame {identity}"
            );
        }
        for (plane, reference) in pixels
            .chunks((*width * *height) as usize)
            .zip(expected.pixels.chunks((*width * *height) as usize))
        {
            let mse = plane
                .iter()
                .zip(reference)
                .map(|(&a, &b)| (f64::from(a) - f64::from(b)).powi(2))
                .sum::<f64>()
                / plane.len() as f64;
            let psnr = if mse == 0. {
                99.
            } else {
                10. * (255. * 255. / mse).log10()
            };
            assert!(psnr >= 40., "frame {identity} visible-plane PSNR {psnr}");
            min_psnr = min_psnr.min(psnr);
        }
    }
    let expected_missing = if late_audio {
        vec![audio_group]
    } else {
        vec![]
    };
    assert_eq!(
        *probe.missing.lock(),
        expected_missing,
        "audio concealment identities"
    );
    let sound = probe.sound.lock();
    let expected_audio: Vec<_> = recording
        .audio
        .iter()
        .filter(|a| !late_audio || a.seq != audio_group)
        .collect();
    assert_eq!(sound.len(), expected_audio.len(), "audio frame count");
    for ((seq, channels, hash), expected) in sound.iter().zip(expected_audio) {
        assert_eq!(
            (*seq, *channels, hash),
            (expected.seq, expected.channels, &expected.hash),
            "audio identity/content/WAVE channel order"
        );
    }
    if impaired {
        assert!(
            probe.fec.load(Ordering::Relaxed) > 0,
            "FEC recovery counter"
        );
    }
    format::result(
        "media.jsonl",
        serde_json::json!({ "producer": recording.producer, "consumer": format::platform(), "mode": recording.mode, "decoder": if native { "native" } else { "software" }, "backend": session.stats().decoder, "schedule": schedule, "audio_group": audio_group, "recorded_repair_us": repair_us, "concealed": *probe.missing.lock(), "status": if gap { "known-gap" } else { "passed" }, "gap": if gap { Some("mac-vt-mf-native") } else { None }, "observed_frames": observed_frames, "expected_frames": expected_frames, "decode_errors": *probe.decode_errors.lock(), "frames": pictures.len(), "audio": sound.len(), "min_psnr": min_psnr, "keyframe_requests": requests }),
    );
}

#[test]
#[ignore = "artifact consumer, run through scripts/pairs.py"]
fn pair_replay() {
    let root = std::path::PathBuf::from(
        std::env::var_os("LATCH_PAIR_ARTIFACTS").expect("set LATCH_PAIR_ARTIFACTS"),
    );
    let files = format::files(&root, ".session.json");
    assert!(!files.is_empty(), "no producer recordings");
    // Each cell runs in a child so a failed assertion also closes its sockets
    // and worker threads before the next independent session starts.
    if let Some(file) = std::env::var_os("LATCH_PAIR_CELL") {
        let recording: Recording = format::read(std::path::Path::new(&file));
        assert_eq!(recording.version, format::VERSION);
        assert_eq!(recording.pictures.len(), format::FRAMES);
        check(
            recording,
            std::env::var("LATCH_PAIR_DECODER").unwrap() == "native",
            &std::env::var("LATCH_PAIR_SCHEDULE").unwrap(),
        );
        return;
    }
    // VideoToolbox creates its OS session lazily on the first access unit.
    // Probe actual decoding before declaring the optional backend available.
    let native = match video::pair_decoder(true, 256, 256).and_then(|mut decoder| {
        let mut encoder = latch_codec::h264::Encoder::new(latch_codec::h264::EncoderConfig {
            width: 256,
            height: 256,
            fps: 25,
            bitrate_kbps: 4000,
            threads: 1,
        })?;
        let mut bytes = Vec::new();
        encoder.encode(&latch_codec::Yuv420::new(256, 256), true, &mut bytes)?;
        anyhow::ensure!(
            decoder.decode(&bytes, true, None)?.is_some(),
            "OS decoder probe gave no picture"
        );
        Ok(())
    }) {
        Ok(()) => true,
        Err(error) => {
            format::result(
                "media.jsonl",
                serde_json::json!({ "consumer": format::platform(), "decoder": "native", "status": "unavailable", "reason": format!("{error:#}") }),
            );
            false
        }
    };
    let mut failures = 0;
    for file in files {
        for decoder in ["software", "native"]
            .into_iter()
            .filter(|d| *d != "native" || native)
        {
            for schedule in ["clean", "loss-reorder", "late-audio"] {
                let recording: Recording = format::read(&file);
                let output = std::process::Command::new(std::env::current_exe().unwrap())
                    .args([
                        "pair_replay",
                        "--ignored",
                        "--nocapture",
                        "--test-threads=1",
                    ])
                    .env("LATCH_PAIR_CELL", &file)
                    .env("LATCH_PAIR_DECODER", decoder)
                    .env("LATCH_PAIR_SCHEDULE", schedule)
                    .output()
                    .expect("start replay cell");
                println!("{}", String::from_utf8_lossy(&output.stdout));
                if !output.status.success() {
                    failures += 1;
                    format::result(
                        "media.jsonl",
                        serde_json::json!({
                            "producer": recording.producer, "consumer": format::platform(),
                            "mode": recording.mode, "decoder": decoder, "schedule": schedule,
                            "status": "failed", "error": String::from_utf8_lossy(&output.stderr),
                            "output": String::from_utf8_lossy(&output.stdout)
                        }),
                    );
                }
            }
        }
    }
    assert_eq!(failures, 0, "required media cells failed");
}

#[test]
#[ignore = "two-process live session, run through scripts/pairs.py"]
fn pair_live() {
    let port: u16 = std::env::var("LATCH_PAIR_LIVE_PORT")
        .expect("set LATCH_PAIR_LIVE_PORT")
        .parse()
        .unwrap();
    let probe = Arc::new(Probe {
        native: false,
        channels: 0,
        pictures: Mutex::new(Vec::new()),
        sound: Mutex::new(Vec::new()),
        missing: Mutex::new(Vec::new()),
        decode_errors: Mutex::new(Vec::new()),
        fec: AtomicU64::new(0),
    });
    let (tx, rx) = mpsc::channel();
    let mut inner = Inner::new(tx, Arc::new(FrameSlot::default()), Box::new(|| {}));
    inner.pair_probe = Some(probe.clone());
    let inner = Arc::new(inner);
    let session = Session {
        inner: inner.clone(),
    };
    let worker = std::thread::spawn(move || {
        run(
            &inner,
            &Server {
                address: Ipv4Addr::LOCALHOST.into(),
                port,
            },
            &Settings {
                prefer_color: false,
                color: Default::default(),
                width: 640,
                height: 360,
                fps: 25,
                bitrate_kbps: 4000,
                hevc: false,
                av1: false,
                audio: true,
                name: "pair live process".into(),
                cursor: true,
                gamepads: false,
                mic: false,
            },
        );
        inner.finish();
    });
    loop {
        match rx
            .recv_timeout(Duration::from_secs(10))
            .expect("live startup timeout")
        {
            Event::Connected => break,
            Event::Failed { stage, message } => panic!("{stage}: {message}"),
            _ => {}
        }
    }
    let white = |pictures: &[DecodedPicture]| {
        let (_, width, _, pixels) = pictures.last().unwrap();
        (16..48)
            .flat_map(|y| (16..48).map(move |x| u64::from(pixels[y * *width as usize + x])))
            .sum::<u64>()
            > 128 * 32 * 32
    };
    let before = white(&probe.pictures.lock());
    session.input().key(0x41, true, 0);
    let deadline = Instant::now() + Duration::from_secs(5);
    while white(&probe.pictures.lock()) == before || probe.sound.lock().len() < 10 {
        assert!(
            Instant::now() < deadline,
            "live key/audio round trip failed"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    session.input().key(0x41, false, 0);
    session.stop();
    worker.join().unwrap();
    assert!(session.finished());
    assert!(probe.missing.lock().is_empty());
    assert!(probe
        .sound
        .lock()
        .iter()
        .all(|(_, channels, _)| *channels == 2));
    format::result(
        "live.jsonl",
        serde_json::json!({ "producer": format::platform(), "mode": "two-process-live", "status": "passed", "frames": probe.pictures.lock().len(), "audio": probe.sound.lock().len(), "input_round_trip": true }),
    );
}

#[test]
#[ignore = "viewer gamepad artifact producer, run through scripts/pairs.py"]
fn pair_gamepad_record() {
    let state = crate::gamepad::pair_state();
    assert_eq!(
        state,
        PadState {
            buttons: latch_proto::gamepad::button::SOUTH | latch_proto::gamepad::button::DPAD_LEFT,
            left_x: -32767,
            left_y: 32767,
            left_trigger: 16384,
            right_trigger: 49151,
            ..Default::default()
        }
    );
    let receive = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    receive
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let send = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let (tx, _rx) = mpsc::channel();
    let inner = Arc::new(Inner::new(
        tx,
        Arc::new(FrameSlot::default()),
        Box::new(|| {}),
    ));
    inner.connected.store(true, Ordering::Release);
    inner.pad_slots.store(1, Ordering::Release);
    *inner.link.lock() = Some(Link {
        udp: send,
        host: receive.local_addr().unwrap(),
        relay: Arc::new(RelayRoute::new(1, latch_relay::Role::Viewer, None)),
        sender: InputSender::new(1),
        input_progress: None,
        input_failed: false,
        pads: PadSender::new(1),
        buf: Vec::new(),
    });
    let session = Session { inner };
    session.input().pad(0, PadKind::Xbox, state);
    let mut bytes = [0; 1200];
    let length = receive.recv(&mut bytes).unwrap();
    let Datagram::Pad { updates, .. } = wire::parse(&bytes[..length]).unwrap() else {
        panic!("no gamepad packet")
    };
    assert_eq!(updates.iter().next().unwrap().state, state);
    let root = std::path::PathBuf::from(
        std::env::var_os("LATCH_PAIR_RECORD").expect("set LATCH_PAIR_RECORD"),
    );
    format::write(
        &root.join("gamepad.pad.json"),
        &serde_json::json!({ "version": format::VERSION, "producer": format::platform(), "bytes": bytes[..length] }),
    );
    session.stop();
}
