//! Authorized engine/viewer sessions over a real, explicitly trusted TLS relay.
//! A datagram forwarder blackholes both directions independently of TCP control.
use latch_core::http;
use latch_engine::server::{Options, Server};
use latch_proto::control::{self, Codec, Message, RelayAction, RelayControl, StreamRequest};
use latch_relay::{Cancel, ClientConfig, Endpoint};
use latch_stream::{Event, Frame, FrameSlot, Session, Settings};
use std::{
    io::Write,
    net::{Ipv4Addr, TcpListener},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc, Arc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

#[path = "support/ports.rs"]
mod ports;
#[path = "support/relay_pilot.rs"]
mod relay;
use relay::Pilot;

// The synthetic host's pattern and virtual pads are process-wide.
static SESSION_TEST: std::sync::Mutex<()> = std::sync::Mutex::new(());

struct Running {
    server: Arc<Server>,
    session: Session,
    frames: Arc<FrameSlot>,
    events: mpsc::Receiver<Event>,
    blackhole: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    host: Option<JoinHandle<()>>,
    proxy: Option<JoinHandle<()>>,
}
impl Running {
    fn start(config: ClientConfig, blackhole: bool) -> Self {
        Self::with_pattern(config, blackhole, "still")
    }
    fn with_pattern(config: ClientConfig, blackhole: bool, pattern: &str) -> Self {
        std::env::set_var("LATCH_TEST_PATTERN", pattern);
        let (udp, listener) = ports::udp_and_tcp_on_one_port();
        let port = udp.local_addr().unwrap().port();
        let server = Server::bind(
            0,
            Options {
                relay: Some(config.clone()),
                ..Options::default()
            },
        )
        .unwrap();
        let host_port = server.port();
        let blackhole = Arc::new(AtomicBool::new(blackhole));
        let stop = Arc::new(AtomicBool::new(false));
        let viewer_port = Arc::new(AtomicU64::new(0));
        let proxy = {
            let blackhole = blackhole.clone();
            let stop = stop.clone();
            let viewer_port = viewer_port.clone();
            udp.set_read_timeout(Some(Duration::from_millis(10)))
                .unwrap();
            thread::spawn(move || {
                let mut bytes = [0; 1500];
                while !stop.load(Ordering::Acquire) {
                    if let Ok((len, from)) = udp.recv_from(&mut bytes) {
                        if blackhole.load(Ordering::Acquire) {
                            continue;
                        }
                        let target = if from.port() == host_port {
                            viewer_port.load(Ordering::Acquire) as u16
                        } else {
                            host_port
                        };
                        if target != 0 {
                            let _ = udp.send_to(&bytes[..len], (Ipv4Addr::LOCALHOST, target));
                        }
                    }
                }
            })
        };
        let host_server = server.clone();
        let host = thread::spawn(move || {
            let (mut tcp, peer) = listener.accept().unwrap();
            tcp.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let request = http::read_request(&mut tcp).unwrap().unwrap();
            let mut request: StreamRequest = request.json().unwrap();
            viewer_port.store(request.udp_port.into(), Ordering::Release);
            request.udp_port = port;
            let prepared = host_server.prepare(request, peer).unwrap();
            write!(
                tcp,
                "HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: {}\r\n\r\n",
                control::UPGRADE
            )
            .unwrap();
            tcp.set_read_timeout(None).unwrap();
            host_server.run(prepared, tcp);
        });
        let frames = Arc::new(FrameSlot::default());
        let (tx, events) = mpsc::channel();
        let session = Session::start_with_relay(
            latch_stream::Server {
                address: Ipv4Addr::LOCALHOST.into(),
                port,
            },
            Settings {
                prefer_color: false,
                color: Default::default(),
                width: 128,
                height: 128,
                fps: 30,
                bitrate_kbps: 2000,
                hevc: false,
                av1: false,
                audio: false,
                name: "relay runtime test".into(),
                cursor: true,
                gamepads: true,
                mic: false,
                display: None,
            },
            frames.clone(),
            tx,
            || {},
            Some(config),
        );
        Self {
            server,
            session,
            frames,
            events,
            blackhole,
            stop,
            host: Some(host),
            proxy: Some(proxy),
        }
    }
    #[track_caller]
    fn wait(&self, limit: Duration, condition: impl Fn() -> bool) {
        let until = Instant::now() + limit;
        while !condition() {
            for event in self.events.try_iter() {
                assert!(
                    !matches!(event, Event::Failed { .. } | Event::Terminated { .. }),
                    "{event:?}"
                );
            }
            assert!(
                Instant::now() < until,
                "deadline exceeded, stats {:?}",
                self.session.stats()
            );
            thread::sleep(Duration::from_millis(5));
        }
    }
    fn picture(&self) -> Frame {
        let mut frame = None;
        self.wait(Duration::from_secs(3), || self.frames.seq() > 0);
        let deadline = Instant::now() + Duration::from_secs(3);
        while frame.is_none() && Instant::now() < deadline {
            frame = self.frames.take();
            if frame.is_none() {
                thread::sleep(Duration::from_millis(2));
            }
        }
        let mut frame = frame.expect("decoded picture");
        frame.ensure_cpu_planes();
        frame
    }
    fn ordered_input(&self) {
        let first = self.picture();
        let mut white = square(&first);
        self.frames.recycle(first);
        for _ in 0..5 {
            self.session.input().key(65, true, 0);
            let until = Instant::now() + Duration::from_secs(2);
            loop {
                assert!(
                    Instant::now() < until,
                    "input did not reach decoded picture"
                );
                let frame = self.picture();
                let next = square(&frame);
                self.frames.recycle(frame);
                if next != white {
                    white = next;
                    break;
                }
            }
            self.session.input().key(65, false, 0);
        }
        use latch_proto::gamepad::{PadKind, PadState};
        let (sent, received) = mpsc::channel();
        self.session.on_pad_feedback(move |slot, feedback| {
            let _ = sent.send((slot, feedback));
        });
        assert!(self.session.input().pad_slots() > 0);
        let right_trigger = (self.frames.seq() % 60000 + 100) as u16;
        self.session.input().pad(
            0,
            PadKind::Xbox,
            PadState {
                left_trigger: 12345,
                right_trigger,
                ..PadState::default()
            },
        );
        self.wait(Duration::from_secs(2), || {
            received
                .try_iter()
                .any(|(slot, f)| slot == 0 && f.large == right_trigger && f.small == 12345)
        });
        self.session.input().pad_gone(0);
    }
}
impl Drop for Running {
    fn drop(&mut self) {
        self.session.stop();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !self.session.finished() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        self.stop.store(true, Ordering::Release);
        self.proxy.take().unwrap().join().unwrap();
        assert!(self.session.finished(), "viewer cleanup deadline");
        self.host.take().unwrap().join().unwrap();
    }
}
fn square(frame: &Frame) -> bool {
    let mut sum = 0u64;
    for y in 16..48 {
        for x in 16..48 {
            sum += frame.y[y * frame.y_stride + x] as u64;
        }
    }
    sum / (32 * 32) > 128
}

#[test]
fn relay_preserves_explicit_stop_takeover_and_recoverable_end_codes() {
    let _serial = SESSION_TEST
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let pilot = Pilot::start();
    for code in [
        control::end::HOST_STOPPING,
        control::end::TAKEN_OVER,
        control::end::TRANSPORT_FAILED,
    ] {
        let running = Running::start(pilot.config.clone(), true);
        running.wait(Duration::from_secs(6), || {
            running.session.connected() && running.session.relay_active()
        });
        if code == control::end::TAKEN_OVER {
            let request: StreamRequest = serde_json::from_value(serde_json::json!({
                "protocol": 1, "width": 128, "height": 128, "fps": 30,
                "bitrate_kbps": 2000, "codecs": ["h264-baseline"], "udp_port": 1,
                "audio": false, "relay": true, "name": "second viewer"
            }))
            .unwrap();
            drop(
                running
                    .server
                    .prepare(request, (Ipv4Addr::LOCALHOST, 9).into())
                    .unwrap(),
            );
        } else {
            assert!(running.server.stop(code, "test termination"));
        }
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            match running
                .events
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("terminal event")
            {
                Event::Terminated { code: actual, .. } => {
                    assert_eq!(actual, code as i32);
                    break;
                }
                Event::Failed { stage, message } => panic!("{stage}: {message}"),
                _ => {}
            }
        }
    }
}

#[test]
fn automatic_blackhole_recovery_real_tls_input_and_udp_fallback() {
    let _serial = SESSION_TEST
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let _ = tracing_subscriber::fmt()
        .with_env_filter("latch_stream=debug,latch_engine=info,latch_relay=debug")
        .try_init();
    let mut pilot = Pilot::start();
    {
        let running = Running::start(pilot.config.clone(), true);
        running.wait(Duration::from_secs(6), || {
            running.session.connected() && running.session.relay_active()
        });
        assert_eq!(
            (running.picture().width, running.picture().height),
            (128, 128)
        );
        running.wait(Duration::from_secs(2), || {
            let stats = running.session.stats();
            stats.mbps > 0.0 && stats.received_mbps >= stats.mbps
        });
        thread::sleep(Duration::from_millis(200));
        let keyframes = running.session.recovery_log().keyframes;
        let before = running.frames.seq();
        running.wait(Duration::from_secs(2), || running.frames.seq() > before + 8);
        assert!(
            running.session.recovery_log().keyframes <= keyframes + 1,
            "idle relay is requesting repeated IDRs"
        );
        running.ordered_input();
        let held_pad = latch_proto::gamepad::PadState {
            buttons: 1,
            left_x: 16000,
            ..Default::default()
        };
        running
            .session
            .input()
            .pad(0, latch_proto::gamepad::PadKind::Xbox, held_pad);
        running.wait(Duration::from_secs(2), || {
            latch_engine::testmode::pads()[0].is_some_and(|(_, state)| state == held_pad)
        });
        // The relay stays healthy throughout the measured return to UDP.
        running.blackhole.store(false, Ordering::Release);
        let restored = Instant::now();
        running.wait(Duration::from_secs(65), || !running.session.relay_active());
        assert!(
            restored.elapsed() >= Duration::from_secs(40),
            "relay skipped minimum dwell"
        );
        running.wait(Duration::from_secs(2), || {
            latch_engine::testmode::pads()[0].is_some_and(|(_, state)| state == held_pad)
        });
        running.ordered_input();
        running.blackhole.store(true, Ordering::Release);
        running.wait(Duration::from_secs(6), || running.session.relay_active());
        running.blackhole.store(false, Ordering::Release);
        pilot.stop();
        running.wait(Duration::from_secs(3), || !running.session.relay_active());
        let before = running.frames.seq();
        running.wait(Duration::from_secs(3), || running.frames.seq() > before + 2);
        running.ordered_input();
        pilot.restart();
        running.blackhole.store(true, Ordering::Release);
        running.wait(Duration::from_secs(6), || running.session.relay_active());
        running.ordered_input();
    }
    let pilot = Pilot::start();
    let running = Running::start(pilot.config.clone(), false);
    running.wait(Duration::from_secs(3), || running.session.connected());
    assert!(
        !running.session.relay_active(),
        "healthy idle UDP must stay direct"
    );
    let before = running.frames.seq();
    running.wait(Duration::from_secs(2), || running.frames.seq() > before + 8);
    assert!(
        !running.session.relay_active(),
        "underfilled idle windows triggered relay"
    );
    running.ordered_input();
    running.blackhole.store(true, Ordering::Release);
    running.wait(Duration::from_secs(6), || running.session.relay_active());
    let before = running.frames.seq();
    running.wait(Duration::from_secs(3), || running.frames.seq() > before + 2);
    running.ordered_input();
    drop(running);
    drop(pilot);
}

#[test]
fn finite_grants_get_fresh_authorization_without_stranding_media() {
    let _serial = SESSION_TEST
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    // Four successful, short grants cross the failure circuit-breaker threshold.
    let pilot =
        Pilot::with_limits(|config| config.limits.session_lifetime = Duration::from_secs(3));
    let running = Running::start(pilot.config.clone(), true);
    for grant in 1..=4 {
        running.wait(Duration::from_secs(7), || {
            pilot.metrics.grants_created.load(Ordering::Acquire) >= grant
                && running.session.relay_active()
                && running.session.connected()
        });
        let before = running.frames.seq();
        running.wait(Duration::from_secs(2), || running.frames.seq() > before + 2);
    }
    assert_eq!(pilot.metrics.grants_created.load(Ordering::Acquire), 4);
    running.ordered_input();
    drop(running);
    drop(pilot);
}

#[test]
fn finite_byte_grants_renew_after_actual_video_transfer() {
    let _serial = SESSION_TEST
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let pilot = Pilot::with_limits(|config| config.limits.session_bytes = 1024 * 1024);
    let running = Running::with_pattern(pilot.config.clone(), true, "noise");
    for grant in 1..=4 {
        // Include consuming the byte allowance, then the two-second renewal
        // cooldown and TLS setup. Hosted runners can take seven seconds just
        // to reach the byte threshold; this is not a throughput benchmark.
        running.wait(Duration::from_secs(20), || {
            pilot.metrics.grants_created.load(Ordering::Acquire) >= grant
                && running.session.relay_active()
                && running.session.connected()
        });
        let before = running.frames.seq();
        running.wait(Duration::from_secs(2), || running.frames.seq() > before + 2);
    }
    assert!(pilot.metrics.network_bytes.load(Ordering::Acquire) >= 2 * 1024 * 1024);
}

#[test]
fn resource_rejection_retries_after_the_occupied_grant_expires() {
    let _serial = SESSION_TEST
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let pilot = Pilot::with_limits(|config| {
        config.limits.max_sessions = 1;
        config.limits.sessions_per_ip = 1;
        config.limits.grant_ttl = Duration::from_secs(2);
    });
    let _occupied = latch_relay::create_grant(&pilot.config, &Cancel::new()).unwrap();
    let running = Running::start(pilot.config.clone(), true);
    running.wait(Duration::from_secs(7), || {
        running.session.connected() && running.session.relay_active()
    });
    assert_eq!(pilot.metrics.grants_created.load(Ordering::Acquire), 2);
    assert!(pilot.metrics.accepted_connections.load(Ordering::Acquire) <= 9);
    running.ordered_input();
}

#[test]
fn monthly_cap_fails_closed_with_bounded_retry_and_direct_recovery() {
    let _serial = SESSION_TEST
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    // A depleted monthly ledger refuses TLS admission. Direct restoration still
    // recovers the existing session, and repeated failures remain rate bounded.
    let pilot = Pilot::with_limits(|config| config.monthly_bytes = 64 * 1024);
    let running = Running::start(pilot.config.clone(), true);
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(6) {
        assert!(!running.session.relay_active());
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(pilot.metrics.grants_created.load(Ordering::Acquire), 1);
    assert!(pilot.metrics.accepted_connections.load(Ordering::Acquire) <= 3);
    assert_eq!(
        pilot.metrics.quota_reserved_bytes.load(Ordering::Acquire),
        64 * 1024
    );
    assert!(!pilot.worker.as_ref().unwrap().is_finished());
    running.blackhole.store(false, Ordering::Release);
    running.wait(Duration::from_secs(2), || running.session.connected());
    running.ordered_input();
    drop(running);
    drop(pilot);
}

#[test]
#[cfg(not(target_os = "linux"))]
fn actual_relay_audio_and_microphone_reach_pcm_sinks() {
    let _serial = SESSION_TEST
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let pilot = Pilot::start();
    relay_audio_and_microphone_pipeline(pilot.config.clone());
}

#[test]
fn negotiation_rejects_stale_malformed_and_unconfigured_offers() {
    use latch_stream::session::RelayRoute;
    let pilot = Pilot::start();
    let route = RelayRoute::new(42, latch_relay::Role::Viewer, Some(pilot.config.clone()));
    let mut sent = Vec::new();
    assert!(route.maintain(true, |m| {
        sent.push(m.clone());
        true
    }));
    assert_eq!(route.number(), 1);
    let offer = control::RelayPayload::new("invalid json containing secret".into()).unwrap();
    assert!(!format!("{offer:?}").contains("secret"));
    for (session, attempt) in [(41, 1), (42, 0), (42, 2)] {
        assert!(route.handle(
            RelayControl {
                session,
                attempt,
                action: RelayAction::Offer(offer.clone())
            },
            |_| true
        ));
        assert!(route.pending());
    }
    assert!(route.handle(
        RelayControl {
            session: 42,
            attempt: 1,
            action: RelayAction::Commit
        },
        |_| panic!("premature commit")
    ));
    assert!(!route.active());
    assert!(route.handle(
        RelayControl {
            session: 42,
            attempt: 1,
            action: RelayAction::Offer(offer)
        },
        |m| {
            sent.push(m.clone());
            true
        }
    ));
    assert!(!route.pending());
    assert!(matches!(
        sent.last(),
        Some(Message::Relay(RelayControl {
            action: RelayAction::Abort,
            ..
        }))
    ));
    assert!(control::RelayPayload::new("x".repeat(8193)).is_err());
    let old: StreamRequest = serde_json::from_value(serde_json::json!({
        "protocol": 1, "width":128,"height":128,"fps":30,"bitrate_kbps":2000,
        "codecs":["h264-baseline"],"udp_port":1,"audio":false
    }))
    .unwrap();
    assert!(!old.relay);
    let old: control::Accept = serde_json::from_value(serde_json::json!({
        "session":42,"token":"00".repeat(16),"codec":"h264-baseline", "width":128,"height":128,
        "fps":30,"bitrate_kbps":2000,"encoder":"old host","audio":null
    }))
    .unwrap();
    assert_eq!(old.codec, Codec::H264Baseline);
    assert!(!old.relay);
}

#[test]
fn cancelled_prepare_joins_its_worker_and_rejects_endpoint_role_and_generation() {
    use latch_stream::session::RelayRoute;
    let pilot = Pilot::start();
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let mut stalled = pilot.config.clone();
    stalled.endpoint = Endpoint::new("localhost", listener.local_addr().unwrap().port()).unwrap();
    let route = RelayRoute::new(42, latch_relay::Role::Host, Some(stalled));
    route.handle(
        RelayControl {
            session: 42,
            attempt: 1,
            action: RelayAction::Request,
        },
        |_| true,
    );
    let deadline = Instant::now() + Duration::from_secs(2);
    let _stalled_tls = loop {
        if let Ok((tcp, _)) = listener.accept() {
            break tcp;
        }
        assert!(Instant::now() < deadline, "grant worker never connected");
        thread::sleep(Duration::from_millis(2));
    };
    let start = Instant::now();
    route.shutdown();
    assert!(
        start.elapsed() < Duration::from_millis(500),
        "prepare cancellation did not unblock TLS"
    );
    assert!(!route.pending());
    route.handle(
        RelayControl {
            session: 42,
            attempt: 1,
            action: RelayAction::Ready,
        },
        |_| true,
    );
    assert!(!route.active());

    let grant = latch_relay::create_grant(&pilot.config, &Cancel::new()).unwrap();
    let keys = latch_relay::SessionKeys::generate().unwrap();
    let base = serde_json::json!({ "session":42,"attempt":1,"endpoint":pilot.config.endpoint,"viewer":grant.viewer,"keys":keys });
    for (field, value) in [
        ("session", serde_json::json!(43)),
        ("attempt", serde_json::json!(2)),
        (
            "endpoint",
            serde_json::json!(Endpoint::new("unconfigured.example", 443).unwrap()),
        ),
        ("viewer", serde_json::json!(grant.host)),
    ] {
        let route = RelayRoute::new(42, latch_relay::Role::Viewer, Some(pilot.config.clone()));
        route.maintain(true, |_| true);
        let mut offer = base.clone();
        offer[field] = value;
        let offer = control::RelayPayload::new(serde_json::to_string(&offer).unwrap()).unwrap();
        let mut aborted = false;
        route.handle(
            RelayControl {
                session: 42,
                attempt: 1,
                action: RelayAction::Offer(offer),
            },
            |message| {
                aborted = matches!(
                    message,
                    Message::Relay(RelayControl {
                        action: RelayAction::Abort,
                        ..
                    })
                );
                true
            },
        );
        assert!(aborted, "invalid {field} was accepted");
        assert!(!route.active() && !route.pending());
    }
}

// Synthetic capture and a PCM-collecting virtual microphone exercise both
// directions without depending on a speaker, microphone or virtual-cable driver.
#[cfg(not(target_os = "linux"))]
fn relay_audio_and_microphone_pipeline(config: ClientConfig) {
    use latch_proto::audio::{AudioOut, AudioPacketizer, AudioReceiver};
    use latch_proto::wire::{self, Datagram};
    use latch_stream::session::RelayRoute;
    use std::net::{Shutdown, TcpStream};
    use std::sync::Mutex;

    // Every invocation must remain synthetic, including an isolated test run
    // in a Windows service session with no capturable desktop.
    std::env::set_var("LATCH_TEST_PATTERN", "still");
    std::env::set_var("LATCH_TEST_AUDIO", "clicks");
    let recorded = Arc::new(Mutex::new(Vec::<Vec<i16>>::new()));
    let captured = recorded.clone();
    let server = Server::bind(
        0,
        Options {
            relay: Some(config.clone()),
            mic: Some(Box::new(move |_| {
                let recorded = captured.clone();
                Ok(latch_engine::mic::MicOut {
                    describe: "synthetic PCM sink".into(),
                    push: Box::new(move |payload| {
                        let mut samples = Vec::new();
                        assert_eq!(
                            latch_codec::audio::decode_channels(payload, &mut samples),
                            Ok(1)
                        );
                        recorded.lock().unwrap().push(samples);
                    }),
                })
            })),
            ..Options::default()
        },
    )
    .unwrap();
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let mut tcp = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (host_tcp, peer) = listener.accept().unwrap();
    let request: StreamRequest = serde_json::from_value(serde_json::json!({
        "protocol": 1, "width":128,"height":128,"fps":30,"bitrate_kbps":2000,
        "codecs":["h264-baseline"],"udp_port":1,"audio":true,"mic":true,"relay":true
    }))
    .unwrap();
    let prepared = server.prepare(request, peer).unwrap();
    let host = thread::spawn(move || server.run(prepared, host_tcp));
    tcp.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    let Message::Accept(accept) = control::read_message(&mut tcp).unwrap() else {
        panic!("accept")
    };
    assert!(accept.mic.is_some(), "mock microphone was not negotiated");
    let format = accept.audio.unwrap();
    let mut audio = AudioReceiver::with_format(accept.session, format.frame_samples, format.rate);
    let (send, incoming) = mpsc::channel();
    let mut reader = tcp.try_clone().unwrap();
    reader.set_read_timeout(None).unwrap();
    let reader = thread::spawn(move || {
        while let Ok(message) = control::read_message(&mut reader) {
            if send.send(message).is_err() {
                break;
            }
        }
    });
    let route = RelayRoute::new(accept.session, latch_relay::Role::Viewer, Some(config));
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut microphone = AudioPacketizer::mic(accept.session);
    let expected: Vec<Vec<i16>> = (0..10)
        .map(|frame| {
            (0..240)
                .map(|sample| ((frame * 240 + sample) * 7) as i16)
                .collect()
        })
        .collect();
    let mut sent_mic = false;
    let mut audio_frames = 0;
    let mut heard_click = false;
    while recorded.lock().unwrap().len() < 10 || audio_frames < 10 || !heard_click {
        assert!(
            Instant::now() < deadline,
            "audio/microphone pipeline deadline"
        );
        for message in incoming.try_iter() {
            if let Message::Relay(message) = message {
                assert!(
                    route.handle(message, |message| control::write_message(&mut tcp, message)
                        .is_ok())
                );
            }
        }
        for _ in 0..64 {
            match route.poll() {
                Some((_, latch_relay::Event::Datagram(bytes))) => {
                    if let Ok(Datagram::Audio { packet, .. }) = wire::parse(&bytes) {
                        audio.push(&packet, Instant::now());
                    }
                }
                Some(_) => {}
                None => break,
            }
        }
        assert!(
            route.maintain(true, |message| control::write_message(&mut tcp, message)
                .is_ok())
        );
        if route.active() && !sent_mic {
            for (frame, samples) in expected.iter().enumerate() {
                let mut coded = Vec::new();
                latch_codec::audio::encode_channels(samples, 1, &mut coded);
                microphone.packetize(frame as u32 * 5000, &coded, |datagram| {
                    assert!(route.send_datagram(datagram).unwrap());
                });
            }
            sent_mic = true;
        }
        while let Some(frame) = audio.pop(Instant::now()) {
            let AudioOut::Frame { payload, .. } = frame else {
                panic!("lost relay audio")
            };
            let mut pcm = Vec::new();
            assert_eq!(
                latch_codec::audio::decode_channels(&payload, &mut pcm),
                Ok(2)
            );
            assert_eq!(pcm.len(), format.frame_samples as usize * 2);
            heard_click |= pcm.iter().any(|sample| *sample != 0);
            audio_frames += 1;
        }
        thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(
        *recorded.lock().unwrap(),
        expected,
        "microphone PCM changed or reordered"
    );
    control::write_message(
        &mut tcp,
        &Message::End(control::End {
            code: 0,
            message: String::new(),
        }),
    )
    .unwrap();
    route.shutdown();
    let _ = tcp.shutdown(Shutdown::Both);
    reader.join().unwrap();
    host.join().unwrap();
    std::env::remove_var("LATCH_TEST_AUDIO");
}
