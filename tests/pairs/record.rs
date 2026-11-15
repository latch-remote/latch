//! Records the real server's wire output. Compiled only in the engine test binary.
use super::*;
use crate::pair_format as format;
use format::{Lane, Picture, Recording, Sound, Timed, FRAMES, VERSION};
use latch_core::http;
use latch_proto::video::{Reassembled, Reassembler};
use std::io::Write;
use std::net::TcpListener;
use std::path::Path;

fn assert_declared_ordering(codec: latch_proto::control::Codec, data: &[u8]) {
    use latch_codec::bitstream::{h264_sps, hevc_ordering};
    for nal in latch_codec::h264::nal::nal_units(data) {
        match codec {
            latch_proto::control::Codec::H264Baseline | latch_proto::control::Codec::H264High
                if nal[0] & 31 == 7 =>
            {
                let sps = h264_sps(nal).unwrap();
                assert_eq!(
                    sps.reorder_frames,
                    Some(0),
                    "host must declare no reordering"
                );
                assert_eq!(sps.buffering, Some(sps.reference_frames.max(1)));
            }
            latch_proto::control::Codec::Hevc if matches!((nal[0] >> 1) & 63, 32 | 33) => {
                for ordering in hevc_ordering(nal).unwrap() {
                    assert_eq!(ordering.reorder_pics, 0, "host must declare no reordering");
                    assert_eq!(ordering.latency_increase_plus1, 1);
                }
            }
            _ => {}
        }
    }
}

#[test]
fn hevc_recording_checks_declared_ordering() {
    let raw = include_bytes!("../../crates/stream/tests/fixtures/decode-probe.hevc");
    let fixed =
        latch_codec::bitstream::low_delay(raw, latch_codec::bitstream::Format::Hevc).unwrap();
    assert_declared_ordering(latch_proto::control::Codec::Hevc, &fixed);
}

fn config(width: u32, height: u32) -> VideoConfig {
    VideoConfig {
        color: Default::default(),
        width,
        height,
        fps: 25,
        bitrate_kbps: 4000,
        codec: Codec::H264Baseline,
        display: None,
    }
}

/// A different coloured/numbered picture for every access unit. Non-block-aligned
/// heights test SPS cropping. The identity stripe also detects stale pictures.
pub(crate) fn source(width: u32, height: u32, identity: usize) -> latch_codec::Yuv420 {
    let mut picture = latch_codec::Yuv420::new(width, height);
    for y in 0..height as usize {
        for x in 0..width as usize {
            picture.y[y * picture.y_stride + x] = if y < 16 {
                24 + identity as u8 * 8
            } else {
                (32 + (x * 3 + y * 2) % 176) as u8
            };
        }
    }
    for y in 0..height as usize / 2 {
        for x in 0..width as usize / 2 {
            picture.u[y * picture.c_stride + x] = (70 + x % 70) as u8;
            picture.v[y * picture.c_stride + x] = (160 - y % 70) as u8;
        }
    }
    picture
}

fn encoded(mode: &str) -> anyhow::Result<Vec<(video::EncodedVideo, VideoInfo)>> {
    let mut out = Vec::new();
    for (group, (width, height)) in [(160, 98), (128, 74)].into_iter().enumerate() {
        let config = config(width, height);
        let mut encoder: Box<dyn crate::software::PictureEncoder> = match mode {
            "software" => Box::new(crate::software::LatchEncoder::open(width, height, &config)?),
            #[cfg(target_os = "macos")]
            "native" => Box::new(crate::mac::synthetic::Synthetic::open(
                width, height, &config,
            )?),
            #[cfg(windows)]
            "native" => return crate::windows::pair_encode(),
            _ => anyhow::bail!("no native encoder on this runner OS"),
        };
        for index in 0..FRAMES / 2 {
            let mut data = Vec::new();
            let keyframe = encoder.encode(
                &source(width, height, group * FRAMES / 2 + index),
                index == 0 || index == 6,
                &mut data,
            )?;
            anyhow::ensure!(!data.is_empty(), "encoder gave no picture");
            if index == 0 || index == 6 {
                assert!(keyframe, "forced IDR was ignored");
            }
            if keyframe {
                for kind in [7, 8, 5] {
                    assert!(
                        data.windows(4)
                            .any(|n| n[..3] == [0, 0, 1] && n[3] & 31 == kind),
                        "IDR missing NAL {kind}"
                    );
                }
            }
            out.push((
                video::EncodedVideo {
                    data,
                    keyframe,
                    captured: Instant::now(),
                    picture: 0,
                    generation: 0,
                    recovery: None,
                },
                VideoInfo {
                    color: config.color,
                    encoder: encoder.name(),
                    hardware: encoder.hardware(),
                    codec: config.codec,
                    width,
                    height,
                    display: DisplayRect {
                        width,
                        height,
                        ..Default::default()
                    },
                    captured_display: None,
                },
            ));
        }
    }
    Ok(out)
}

struct Playback {
    frames: std::collections::VecDeque<(video::EncodedVideo, VideoInfo)>,
    info: VideoInfo,
    next: Instant,
    cursor: bool,
}

impl VideoPipeline for Playback {
    fn info(&self) -> VideoInfo {
        self.info.clone()
    }
    fn next(&mut self, _keyframe: bool, _idle: Duration) -> anyhow::Result<video::EncodedVideo> {
        std::thread::sleep(self.next.saturating_duration_since(Instant::now()));
        self.next = Instant::now() + Duration::from_millis(40);
        let (mut frame, info) = self
            .frames
            .pop_front()
            .ok_or_else(|| anyhow::anyhow!("fixture exhausted"))?;
        self.info = info;
        frame.captured = Instant::now();
        Ok(frame)
    }
    fn set_cursor_drawn(&mut self, _drawn: bool) -> bool {
        true
    }
    fn take_cursor(&mut self) -> Option<video::CursorUpdate> {
        std::mem::take(&mut self.cursor).then(|| video::CursorUpdate {
            visible: true,
            shape: Some(control::CursorShape::new(7, 2, 2, 1, 0, &[255; 16])),
        })
    }
}

struct SyntheticAudio {
    next: Instant,
    sample: usize,
}
impl AudioCapture for SyntheticAudio {
    fn read(
        &mut self,
        _: &mut [i16; audio::FRAME * audio::CHANNELS],
        _: Duration,
    ) -> anyhow::Result<bool> {
        unreachable!("six-channel fixture uses read_frame")
    }
    fn channels(&self) -> usize {
        6
    }
    fn describe(&self) -> String {
        "deterministic six-channel ramp".into()
    }
    fn read_frame(&mut self, out: &mut Vec<i16>, _: Duration) -> anyhow::Result<bool> {
        std::thread::sleep(self.next.saturating_duration_since(Instant::now()));
        self.next = Instant::now() + Duration::from_millis(5);
        out.clear();
        for sample in self.sample..self.sample + audio::FRAME {
            for channel in 0..6 {
                out.push((channel as i16 + 1) * 1000 + (sample % 127) as i16);
            }
        }
        #[cfg(target_os = "macos")]
        let converted = crate::mac::pair_pcm(out);
        #[cfg(windows)]
        let converted = crate::windows::pair_pcm(out);
        #[cfg(target_os = "linux")]
        let converted = crate::linux::pair_pcm(out);
        assert_eq!(converted, *out, "OS capture format/channel order");
        *out = converted;
        self.sample += audio::FRAME;
        Ok(true)
    }
}

fn request(port: u16) -> StreamRequest {
    StreamRequest {
        startup_timeout_secs: 0,
        relay: false,
        prefer_color: false,
        color: Default::default(),
        protocol: latch_proto::PROTOCOL,
        width: 160,
        height: 98,
        fps: 25,
        bitrate_kbps: 4000,
        codecs: vec![Codec::H264Baseline],
        udp_port: port,
        audio: true,
        name: "pair recording".into(),
        cursor: true,
        gamepads: false,
        reference_invalidation: false,
        audio_channels: 6,
        mic: false,
        probe_trains: false,
        display: None,
    }
}

fn record(mode: &str, frames: Vec<(video::EncodedVideo, VideoInfo)>, root: &Path) {
    let hardware = frames[0].1.hardware;
    // Reserve TCP first. Windows can reserve TCP ranges independently of UDP.
    let (server, listener) = (0..50)
        .find_map(|_| {
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
            let port = listener.local_addr().unwrap().port();
            Server::bind(
                port,
                Options {
                    relay: None,
                    ..Default::default()
                },
            )
            .ok()
            .map(|server| (server, listener))
        })
        .expect("no loopback port free for both TCP and the engine UDP socket");
    let port = server.port();
    let running = server.clone();
    let host = std::thread::spawn(move || {
        let (mut tcp, peer) = listener.accept().unwrap();
        let req = http::read_request(&mut tcp).unwrap().unwrap();
        assert_eq!(req.path, control::PATH);
        let mut frames = Some(frames);
        let mut prepared = running
            .prepare_using(
                req.json().unwrap(),
                peer,
                || vec![Codec::H264Baseline],
                video::Limits::level,
                |_| {
                    let frames = frames.take().unwrap();
                    let info = frames[0].1.clone();
                    Ok(Box::new(Playback {
                        frames: frames.into(),
                        info,
                        next: Instant::now() + Duration::from_millis(150),
                        cursor: true,
                    }))
                },
            )
            .unwrap();
        prepared.audio = Some(Box::new(SyntheticAudio {
            next: Instant::now(),
            sample: 0,
        }));
        prepared.audio_problem.clear();
        write!(
            tcp,
            "HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: {}\r\n\r\n",
            control::UPGRADE
        )
        .unwrap();
        running.run(prepared, tcp);
    });
    let udp = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    udp.set_read_timeout(Some(Duration::from_millis(20)))
        .unwrap();
    let request = request(udp.local_addr().unwrap().port());
    let mut tcp = http::upgrade(
        (Ipv4Addr::LOCALHOST, port).into(),
        control::PATH,
        control::UPGRADE,
        &serde_json::to_vec(&request).unwrap(),
        Duration::from_secs(10),
    )
    .unwrap()
    .unwrap();
    tcp.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let Message::Accept(accept) = control::read_message(&mut tcp).unwrap() else {
        panic!("no Accept")
    };
    assert_eq!(accept.codec, Codec::H264Baseline);
    assert_eq!(accept.color, Default::default());
    assert!(accept.cursor);
    assert_eq!(accept.audio.as_ref().unwrap().channels, 6);
    let start = Instant::now();
    let (tx, rx) = mpsc::channel();
    let reader = std::thread::spawn(move || loop {
        let message = control::read_message(&mut tcp).unwrap();
        let end = matches!(message, Message::End(_));
        let mut bytes = Vec::new();
        control::write_message(&mut bytes, &message).unwrap();
        tx.send(Timed {
            us: start.elapsed().as_micros() as u64,
            lane: Lane::Control,
            bytes,
        })
        .unwrap();
        if end {
            break;
        }
    });
    let mut recording = Recording {
        version: VERSION,
        producer: format::platform(),
        mode: mode.into(),
        request,
        accept,
        events: Vec::new(),
        pictures: Vec::new(),
        audio: Vec::new(),
    };
    let mut reassembler = Reassembler::new(recording.accept.session);
    let mut decoder = latch_codec::h264::Decoder::new();
    let mut buf = [0; 2048];
    let mut hello = Vec::new();
    wire::write_hello(
        &mut hello,
        recording.accept.session,
        Some(&control::token_from_hex(&recording.accept.token).unwrap()),
    );
    udp.send_to(&hello, (Ipv4Addr::LOCALHOST, port)).unwrap();
    while recording.pictures.len() < FRAMES {
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "recording stalled"
        );
        if let Ok((len, _)) = udp.recv_from(&mut buf) {
            let bytes = &buf[..len];
            let datagram = wire::parse(bytes).unwrap();
            match datagram {
                Datagram::Video { shard, .. } => reassembler.push(&shard, Instant::now()),
                Datagram::Audio { packet, .. } if !packet.parity => {
                    let mut pcm = Vec::new();
                    let channels =
                        latch_codec::audio::decode_channels(packet.payload, &mut pcm).unwrap();
                    assert_eq!(channels, 6);
                    for sample in pcm.chunks_exact(channels) {
                        for (channel, &value) in sample.iter().enumerate() {
                            assert!(
                                (1000..1127).contains(&(value - channel as i16 * 1000)),
                                "audio content/channel order"
                            );
                        }
                    }
                    recording.audio.push(Sound {
                        seq: packet.seq,
                        channels,
                        hash: format::pcm_hash(&pcm),
                    });
                }
                Datagram::Ping { time, .. } => {
                    let mut reply = Vec::new();
                    wire::write_pong(&mut reply, recording.accept.session, time);
                    udp.send_to(&reply, (Ipv4Addr::LOCALHOST, port)).unwrap();
                }
                _ => {}
            }
            if matches!(datagram, Datagram::Video { .. } | Datagram::Audio { .. }) {
                recording.events.push(Timed {
                    us: start.elapsed().as_micros() as u64,
                    lane: Lane::Datagram,
                    bytes: bytes.to_vec(),
                });
            }
            while let Some(frame) = reassembler.poll(Instant::now()) {
                let Reassembled::Frame(frame) = frame else {
                    panic!("loss during recording")
                };
                if frame.keyframe {
                    if mode != "legacy-mac" {
                        assert_declared_ordering(recording.accept.codec, &frame.data);
                    }
                    assert!(
                        frame.data.windows(5).any(|nal| {
                            nal[..3] == [0, 0, 1] && nal[3] & 31 == 7 && nal[4] == 66
                        }),
                        "fallback SPS must describe 8-bit H.264 Baseline"
                    );
                }
                let picture = decoder
                    .decode(&frame.data)
                    .unwrap()
                    .expect("reference decoder gave no picture");
                let block = |center_y: usize| {
                    let width = picture.width as usize;
                    let sum: u32 = (center_y - 2..center_y + 2)
                        .flat_map(|y| {
                            (width / 2 - 4..width / 2 + 4)
                                .map(move |x| u32::from(picture.y[y * picture.y_stride + x]))
                        })
                        .sum();
                    f64::from(sum) / 32.
                };
                let stripe = block(picture.height as usize / 8);
                if mode == "capture" {
                    #[cfg(target_os = "linux")]
                    {
                        let gray = 32 + recording.pictures.len() * 8;
                        let expected = 16. + gray as f64 * 219. / 255.;
                        assert!(
                            (stripe - expected).abs() < 4.,
                            "X11 test-window identity, expected {expected}, got {stripe}"
                        );
                    }
                    #[cfg(not(target_os = "linux"))]
                    assert!(
                        stripe > block(picture.height as usize / 2) + 20.,
                        "native capture missed the deterministic two-colour window"
                    );
                }
                if mode != "capture" {
                    let identity = recording.pictures.len();
                    assert!(
                        (stripe - (24 + identity * 8) as f64).abs() < 4.,
                        "synthetic picture identity, frame {identity}, got {stripe}"
                    );
                    let (width, height) = if identity < FRAMES / 2 {
                        (160, 98)
                    } else {
                        (128, 74)
                    };
                    assert_eq!(
                        (picture.width, picture.height),
                        (width, height),
                        "visible SPS crop"
                    );
                    let expected = source(width, height, identity);
                    for (actual, stride, expected, reference_stride, width, height) in [
                        (
                            &picture.y,
                            picture.y_stride,
                            &expected.y,
                            expected.y_stride,
                            width as usize,
                            height as usize,
                        ),
                        (
                            &picture.u,
                            picture.c_stride,
                            &expected.u,
                            expected.c_stride,
                            width as usize / 2,
                            height as usize / 2,
                        ),
                        (
                            &picture.v,
                            picture.c_stride,
                            &expected.v,
                            expected.c_stride,
                            width as usize / 2,
                            height as usize / 2,
                        ),
                    ] {
                        let error: f64 = (0..height)
                            .flat_map(|y| {
                                (0..width).map(move |x| {
                                    (f64::from(actual[y * stride + x])
                                        - f64::from(expected[y * reference_stride + x]))
                                    .powi(2)
                                })
                            })
                            .sum();
                        let mse = error / (width * height) as f64;
                        assert!(
                            mse < 65.,
                            "encoded source/identity differs, frame {identity}, MSE {mse}"
                        );
                    }
                }
                let mut y = Vec::new();
                let mut uv = Vec::new();
                let (ys, cs) = latch_codec::color::yuv420_to_nv12(picture, &mut y, &mut uv);
                let mut pixels = Vec::new();
                for row in y.chunks(ys).take(picture.height as usize) {
                    pixels.extend_from_slice(&row[..picture.width as usize]);
                }
                for row in uv.chunks(cs).take(picture.height as usize / 2) {
                    pixels.extend_from_slice(&row[..picture.width as usize]);
                }
                recording.pictures.push(Picture {
                    frame: frame.frame,
                    keyframe: frame.keyframe,
                    width: picture.width,
                    height: picture.height,
                    hash: format::hash(&pixels),
                    pixels,
                });
            }
        }
    }
    assert!(server.stop(end::ENDED, "pair recording complete"));
    host.join().unwrap();
    reader.join().unwrap();
    recording.events.extend(rx.try_iter());
    recording.events.sort_by_key(|event| event.us);
    assert!(
        recording
            .events
            .iter()
            .any(|event| event.lane == Lane::Control
                && matches!(
                    control::read_message(&mut &event.bytes[..]).unwrap(),
                    Message::End(control::End { code: 0, .. })
                )),
        "host did not terminate cleanly"
    );
    // Audio after the final received frame is intentionally outside the tape.
    assert!(recording.audio.len() > 20);
    assert!(recording
        .events
        .iter()
        .any(|event| event.lane == Lane::Control
            && matches!(
                control::read_message(&mut &event.bytes[..]).unwrap(),
                Message::Cursor(_)
            )));
    let (audio_group, repair_us) = format::recovery_group(&recording);
    assert_eq!(
        (recording.pictures[0].width, recording.pictures[0].height),
        (160, 98)
    );
    assert_eq!(
        (recording.pictures[12].width, recording.pictures[12].height),
        (128, 74),
        "capture/encoder resize"
    );
    let path = root.join(format!("{mode}.session.json"));
    format::write(&path, &recording);
    format::result(
        "producer.jsonl",
        serde_json::json!({ "producer": format::platform(), "mode": mode, "status": "recorded", "hardware": hardware, "frames": recording.pictures.len(), "audio": recording.audio.len(), "recovery_group": audio_group, "repair_us": repair_us }),
    );
}

#[test]
#[ignore = "artifact producer, run through scripts/pairs.py"]
fn pair_record() {
    let root = std::path::PathBuf::from(
        std::env::var_os("LATCH_PAIR_RECORD").expect("set LATCH_PAIR_RECORD"),
    );
    std::env::set_var("LATCH_TEST_PATTERN", "still");
    // prepare_using still opens the normal input adapter, in synthetic mode.
    std::env::set_var("LATCH_TEST_AUDIO", "clicks");
    record("software", encoded("software").unwrap(), &root);
    match native_probe() {
        Ok(()) => {
            let frames = encoded("native").expect("native encoder opened but encode/resize failed");
            #[cfg(target_os = "macos")]
            {
                // Actual pre-fix 6.0.0 VideoToolbox bytes from pairs run
                // 43697534260. Reuse their original access-unit boundaries,
                // including the resize, without running host normalization.
                let units =
                    latch_codec::h264::nal::access_units(include_bytes!("mac-vt-6.0.0.h264"));
                assert_eq!(units.len(), FRAMES);
                let mut legacy = frames.clone();
                for ((frame, _), unit) in legacy.iter_mut().zip(units) {
                    frame.data = unit.to_vec();
                    for nal in latch_codec::h264::nal::nal_units(unit)
                        .into_iter()
                        .filter(|n| n[0] & 31 == 7)
                    {
                        assert_eq!(
                            latch_codec::bitstream::h264_sps(nal)
                                .unwrap()
                                .reorder_frames,
                            None
                        );
                    }
                }
                record("legacy-mac", legacy, &root);
            }
            record("native", frames, &root);
        }
        Err(error) => format::result(
            "producer.jsonl",
            serde_json::json!({ "producer": format::platform(), "mode": "native", "status": "unavailable", "reason": format!("{error:#}") }),
        ),
    }
    #[cfg(target_os = "linux")]
    record("capture", capture_frames(), &root);
    #[cfg(not(target_os = "linux"))]
    {
        if std::env::var("LATCH_PAIR_CAPTURE_WINDOW").as_deref() != Ok("1") {
            format::result(
                "producer.jsonl",
                serde_json::json!({ "producer": format::platform(), "mode": "capture", "status": "unavailable", "reason": "deterministic test window did not open (local capture requires --capture-window)" }),
            );
            return;
        }
        std::env::remove_var("LATCH_TEST_PATTERN");
        let probe = video::open(&config(160, 98));
        match probe {
            Err(error) => format::result(
                "producer.jsonl",
                serde_json::json!({ "producer": format::platform(), "mode": "capture", "status": "unavailable", "reason": format!("{error:#}") }),
            ),
            Ok(mut pipeline) => {
                // pairs.py owns the deterministic window on hosted desktops.
                assert_eq!(std::env::var("LATCH_PAIR_CAPTURE_WINDOW").unwrap(), "1");
                let mut frames = Vec::new();
                for index in 0..FRAMES {
                    if index == FRAMES / 2 {
                        drop(pipeline);
                        pipeline =
                            video::open(&config(128, 74)).expect("native capture resize/reopen");
                    }
                    frames.push((
                        pipeline
                            .next(
                                index == 0 || index == 6 || index == 12 || index == 18,
                                Duration::from_millis(40),
                            )
                            .unwrap(),
                        pipeline.info(),
                    ));
                }
                record("capture", frames, &root);
            }
        }
    }
}

fn native_probe() -> anyhow::Result<()> {
    #[cfg(target_os = "macos")]
    {
        crate::mac::synthetic::Synthetic::open(160, 98, &config(160, 98))?;
        Ok(())
    }
    #[cfg(windows)]
    {
        crate::windows::pair_probe_encoder()
    }
    #[cfg(target_os = "linux")]
    anyhow::bail!("Linux hosted runners have no native encoder device")
}

#[cfg(target_os = "linux")]
fn capture_frames() -> Vec<(video::EncodedVideo, VideoInfo)> {
    use crate::software::{Bgra, SoftwarePipeline};
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::{self, ConnectionExt};
    assert_eq!(std::env::var("DISPLAY").unwrap(), ":9");
    assert_eq!(std::env::var("LATCH_PAIR_XVFB").unwrap(), "1");
    let (conn, screen) = x11rb::connect(None).unwrap();
    let root = conn.setup().roots[screen].root;
    let window = conn.generate_id().unwrap();
    conn.create_window(
        x11rb::COPY_DEPTH_FROM_PARENT,
        window,
        root,
        0,
        0,
        640,
        360,
        0,
        xproto::WindowClass::INPUT_OUTPUT,
        x11rb::COPY_FROM_PARENT,
        &xproto::CreateWindowAux::new().background_pixel(0x204080),
    )
    .unwrap()
    .check()
    .unwrap();
    conn.map_window(window).unwrap().check().unwrap();
    let gc = conn.generate_id().unwrap();
    conn.create_gc(gc, window, &xproto::CreateGCAux::new())
        .unwrap()
        .check()
        .unwrap();
    let mut frames = Vec::new();
    for (group, (width, height)) in [(160, 98), (128, 74)].into_iter().enumerate() {
        let mut capture = crate::linux::X11Capture::new().unwrap();
        let mut bgra = Bgra::default();
        capture.capture_now(&mut bgra).unwrap();
        if group == 0 {
            assert_eq!(
                &bgra.data[200 * bgra.stride + 300 * 4..200 * bgra.stride + 300 * 4 + 3],
                &[0x80, 0x40, 0x20],
                "X11 capture must see the deterministic test window"
            );
        }
        let mut pipeline = SoftwarePipeline::new(capture, &config(width, height)).unwrap();
        for index in 0..12 {
            let gray = 32 + (group * 12 + index) as u32 * 8;
            conn.change_gc(gc, &xproto::ChangeGCAux::new().foreground(gray * 0x010101))
                .unwrap()
                .check()
                .unwrap();
            conn.poly_fill_rectangle(
                window,
                gc,
                &[xproto::Rectangle {
                    x: 0,
                    y: 0,
                    width: 640,
                    height: 64,
                }],
            )
            .unwrap()
            .check()
            .unwrap();
            frames.push((
                pipeline
                    .next(index == 0 || index == 6, Duration::from_millis(40))
                    .unwrap(),
                pipeline.info(),
            ));
        }
    }
    conn.free_gc(gc).unwrap().check().unwrap();
    conn.destroy_window(window).unwrap().check().unwrap();
    frames
}
