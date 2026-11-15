//! Authorized engine/viewer sessions over a real, explicitly trusted TLS relay.
//! A datagram forwarder blackholes both directions independently of TCP control.
#[path = "support/listeners.rs"]
mod listeners;

use latch_core::http;
use latch_engine::server::{Options, Server};
use latch_proto::control::{self, Codec, Message, RelayAction, RelayControl, StreamRequest};
use latch_relay::{Cancel, ClientConfig, Endpoint};
use latch_stream::{Event, Frame, FrameSlot, Session, Settings};
use std::{
    io::Write,
    net::{Ipv4Addr, TcpListener, UdpSocket},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc, Arc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

#[path = "support/relay_pilot.rs"]
mod relay;
use relay::Pilot;
#[path = "support/gateway.rs"]
mod gateway;

struct Network {
    delay: Duration,
    loss: f64,
    bandwidth: u32,
    queue: Duration,
    uplink: Option<Arc<parking_lot::Mutex<gateway::Uplink>>>,
    policy: bool,
    fps: u32,
}
impl Default for Network {
    fn default() -> Self {
        Self {
            delay: Duration::ZERO,
            loss: 0.0,
            bandwidth: 0,
            queue: Duration::from_millis(200),
            uplink: None,
            policy: true,
            fps: 30,
        }
    }
}

// Synthetic input is process-wide. Real-time session and TLS fixtures must
// also take turns so their host, viewer, relay and gateway workers have CPU
// on three-core CI runners, regardless of the test harness thread count.
static SESSION_TEST: std::sync::Mutex<()> = std::sync::Mutex::new(());

struct Running {
    server: Arc<Server>,
    session: Session,
    frames: Arc<FrameSlot>,
    events: mpsc::Receiver<Event>,
    blackhole: Arc<AtomicBool>,
    downstream: Arc<parking_lot::Mutex<latch_proto::sim::Link>>,
    max_datagram: Arc<AtomicU64>,
    probe_packets: Arc<parking_lot::Mutex<Vec<(Instant, usize)>>>,
    proxy_buffers: latch_proto::transport::SocketBuffers,
    proxy_send_retries: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    host: Option<JoinHandle<()>>,
    proxy: Option<JoinHandle<()>>,
}
impl Running {
    fn start(config: ClientConfig, blackhole: bool) -> Self {
        Self::with_pattern(config, blackhole, "still")
    }
    fn with_pattern(config: ClientConfig, blackhole: bool, pattern: &str) -> Self {
        Self::with_rate(config, blackhole, pattern, 2000, 128, 128)
    }
    fn with_rate(
        config: ClientConfig,
        blackhole: bool,
        pattern: &str,
        kbps: u32,
        width: u32,
        height: u32,
    ) -> Self {
        Self::with_network(
            config,
            blackhole,
            pattern,
            kbps,
            width,
            height,
            Network::default(),
        )
    }
    fn with_network(
        config: ClientConfig,
        blackhole: bool,
        pattern: &str,
        kbps: u32,
        width: u32,
        height: u32,
        network: Network,
    ) -> Self {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(
                "latch_engine::server=debug,latch_engine::congestion=debug,latch_stream::session=debug,latch_relay=info",
            )
            .with_test_writer()
            .try_init();
        let fps = network.fps;
        std::env::set_var("LATCH_TEST_PATTERN", pattern);
        // Filler NALs retain the requested network load. Small real pictures
        // leave CPU for the relay, FEC and decoder on three-core CI runners.
        let (width, height) = if pattern == "load" {
            (64, 64)
        } else {
            (width, height)
        };
        let (udp, listener) = listeners::proxy();
        let proxy_buffers =
            latch_proto::transport::configure_socket(&udp, 8 * 1024 * 1024, 8 * 1024 * 1024)
                .unwrap();
        let proxy_send_retries = Arc::new(AtomicU64::new(0));
        let port = listener.local_addr().unwrap().port();

        let server = Server::bind(
            0,
            Options {
                relay: Some(config.clone()),
                ..listeners::options()
            },
        )
        .unwrap();
        let host_port = server.port();
        let blackhole = Arc::new(AtomicBool::new(blackhole));
        let stop = Arc::new(AtomicBool::new(false));
        let viewer_port = Arc::new(AtomicU64::new(0));
        let mut link = latch_proto::sim::Link::new(
            latch_proto::sim::Impairment {
                delay: network.delay,
                loss: network.loss,
                ..latch_proto::sim::Impairment::NONE
            },
            11,
        );
        link.set_bandwidth(network.bandwidth, network.queue);
        let downstream = Arc::new(parking_lot::Mutex::new(link));
        let max_datagram = Arc::new(AtomicU64::new(1200));
        let probe_packets = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let proxy = {
            let proxy_send_retries = proxy_send_retries.clone();
            let probe_packets = probe_packets.clone();
            let downstream = downstream.clone();
            let max_datagram = max_datagram.clone();
            let blackhole = blackhole.clone();
            let stop = stop.clone();
            let viewer_port = viewer_port.clone();
            udp.set_nonblocking(true).unwrap();
            thread::spawn(move || {
                let mut bytes = [0; 1500];
                let mut upstream = latch_proto::sim::Link::new(
                    latch_proto::sim::Impairment {
                        delay: network.delay,
                        ..latch_proto::sim::Impairment::NONE
                    },
                    12,
                );
                let mut shared_flight = std::collections::VecDeque::new();
                let mut upstream_ready = std::collections::VecDeque::new();
                while !stop.load(Ordering::Acquire) {
                    for _ in 0..256 {
                        let Ok((len, from)) = udp.recv_from(&mut bytes) else {
                            break;
                        };
                        if blackhole.load(Ordering::Acquire) {
                            continue;
                        }
                        if from.port() == host_port {
                            if matches!(latch_proto::wire::parse(&bytes[..len]),
                                Ok(latch_proto::wire::Datagram::Probe { train, count: 16, .. })
                                    if train < 0x8000)
                            {
                                probe_packets.lock().push((Instant::now(), len));
                            }
                            if len <= max_datagram.load(Ordering::Acquire) as usize {
                                downstream.lock().send(Instant::now(), &bytes[..len]);
                            }
                        } else {
                            upstream.send(Instant::now(), &bytes[..len]);
                        }
                    }
                    upstream_ready.extend(upstream.receive(Instant::now()));
                    while let Some(bytes) = upstream_ready.front() {
                        if !forward_datagram(&udp, bytes, host_port, &proxy_send_retries) {
                            break;
                        }
                        upstream_ready.pop_front();
                    }
                    let target = viewer_port.load(Ordering::Acquire) as u16;
                    for bytes in downstream.lock().receive(Instant::now()) {
                        let due = network
                            .uplink
                            .as_ref()
                            .map_or(Some(Instant::now()), |limit| {
                                limit
                                    .lock()
                                    .schedule(bytes.len(), Duration::from_millis(100))
                            });
                        if let Some(due) = due {
                            shared_flight.push_back((due, bytes));
                        }
                    }
                    while shared_flight
                        .front()
                        .is_some_and(|(due, _)| *due <= Instant::now())
                    {
                        let (_, bytes) = shared_flight.front().unwrap();
                        if target != 0
                            && !blackhole.load(Ordering::Acquire)
                            && !forward_datagram(&udp, bytes, target, &proxy_send_retries)
                        {
                            break;
                        }
                        shared_flight.pop_front();
                    }
                    thread::sleep(Duration::from_micros(200));
                }
            })
        };
        let host_server = server.clone();
        let dispatch = tracing::dispatcher::get_default(Clone::clone);
        let host = thread::spawn(move || {
            let _tracing = tracing::dispatcher::set_default(&dispatch);
            let (mut tcp, peer) = listener.accept().unwrap();
            tcp.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let request = http::read_request(&mut tcp).unwrap().unwrap();
            let mut request: StreamRequest = request.json().unwrap();
            request.media_path_policy &= network.policy;
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
                width,
                height,
                fps,
                bitrate_kbps: kbps,
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
            downstream,
            max_datagram,
            probe_packets,
            proxy_buffers,
            proxy_send_retries,
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
                "deadline exceeded, stats {:?}, UDP sent/lost/duplicate {:?}",
                self.session.stats(),
                self.downstream.lock().counts()
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
    fn hold_key(&self) {
        self.session.input().key(66, true, 0);
        self.wait(Duration::from_secs(1), || {
            latch_engine::testmode::held_keys().contains(&66)
        });
    }
    fn release_key(&self) {
        assert!(
            latch_engine::testmode::held_keys().contains(&66),
            "handoff released a held key"
        );
        self.session.input().key(66, false, 0);
        self.wait(Duration::from_secs(1), || {
            !latch_engine::testmode::held_keys().contains(&66)
        });
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
        self.wait(Duration::from_secs(1), || {
            latch_engine::testmode::held_keys().is_empty()
        });
    }
}
impl Drop for Running {
    fn drop(&mut self) {
        if thread::panicking() {
            eprintln!("host diagnostics {:?}; path {:?}; network {:?}; proxy buffers {:?}; proxy send retries {}; simulated UDP {:?}",
                self.server.status(), self.session.media_path(), self.session.network_status(),
                self.proxy_buffers, self.proxy_send_retries.load(Ordering::Relaxed),
                self.downstream.lock().counts());
        }
        self.session.stop();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !self.session.finished() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        self.stop.store(true, Ordering::Release);
        let proxy = self.proxy.take().unwrap().join();
        if !std::thread::panicking() {
            proxy.unwrap();
        }
        if !std::thread::panicking() {
            assert!(self.session.finished(), "viewer cleanup deadline");
        }
        let host = self.host.take().unwrap().join();
        if !std::thread::panicking() {
            host.unwrap();
        }
        if !std::thread::panicking() {
            assert!(
                latch_engine::testmode::held_keys().is_empty(),
                "cleanup left held input"
            );
        }
    }
}
fn forward_datagram(udp: &UdpSocket, bytes: &[u8], port: u16, retries: &AtomicU64) -> bool {
    match udp.send_to(bytes, (Ipv4Addr::LOCALHOST, port)) {
        Ok(_) => true,
        Err(error) if latch_proto::transport::send_buffer_full(&error) => {
            retries.fetch_add(1, Ordering::Relaxed);
            false
        }
        Err(error) => panic!("fixture UDP forwarding failed: {error}"),
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
        let activated = running.session.media_path().since;
        running.wait(Duration::from_secs(9), || !running.session.relay_active());
        assert!(
            activated.elapsed() >= Duration::from_secs(5),
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
        // Include consuming the byte allowance, then the 100 ms renewal
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
fn actual_relay_audio_and_microphone_reach_pcm_sinks() {
    let _serial = SESSION_TEST
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let pilot = Pilot::start();
    relay_audio_and_microphone_pipeline(pilot.config.clone(), false);
    relay_audio_and_microphone_pipeline(pilot.config.clone(), true);
}

#[test]
fn negotiation_rejects_stale_malformed_and_unconfigured_offers() {
    let _serial = SESSION_TEST.lock().unwrap_or_else(|p| p.into_inner());
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
    let _serial = SESSION_TEST.lock().unwrap_or_else(|p| p.into_inner());
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
fn relay_audio_and_microphone_pipeline(config: ClientConfig, policy: bool) {
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
            ..listeners::options()
        },
    )
    .unwrap();
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let mut tcp = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (host_tcp, peer) = listener.accept().unwrap();
    let udp = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    udp.set_nonblocking(true).unwrap();
    let request: StreamRequest = serde_json::from_value(serde_json::json!({
        "protocol": 1, "width":128,"height":128,"fps":30,"bitrate_kbps":2000,
        "codecs":["h264-baseline"],"udp_port":udp.local_addr().unwrap().port(),
        "audio":true,"mic":true,"relay":true,"media_path_policy":policy
    }))
    .unwrap();
    let prepared = server.prepare(request, peer).unwrap();
    let host_port = server.port();
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
    route.enable_policy(accept.media_path_policy);
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
    if policy {
        let mut bytes = Vec::new();
        wire::write_hello(
            &mut bytes,
            accept.session,
            Some(&control::token_from_hex(&accept.token).unwrap()),
        );
        udp.send_to(&bytes, (Ipv4Addr::LOCALHOST, host_port))
            .unwrap();
        route.shutdown();
        let resumed = Instant::now();
        let mut frames_after = 0;
        let mut missing = 0;
        let mut bytes = [0; 1500];
        while frames_after < 10 {
            assert!(
                resumed.elapsed() < Duration::from_secs(1),
                "PCM did not resume on UDP"
            );
            while let Ok((length, _)) = udp.recv_from(&mut bytes) {
                if let Ok(Datagram::Audio { packet, .. }) = wire::parse(&bytes[..length]) {
                    assert!(length <= 1000);
                    audio.push(&packet, Instant::now());
                }
            }
            while let Some(frame) = audio.pop(Instant::now()) {
                match frame {
                    AudioOut::Frame { payload, .. } => {
                        let mut pcm = Vec::new();
                        assert_eq!(
                            latch_codec::audio::decode_channels(&payload, &mut pcm),
                            Ok(2)
                        );
                        assert_eq!(pcm.len(), format.frame_samples as usize * 2);
                        frames_after += 1;
                    }
                    AudioOut::Missing { .. } => missing += 1,
                }
            }
            thread::sleep(Duration::from_millis(1));
        }
        eprintln!(
            "relay-close PCM recovery {:?}, {missing} concealed packets",
            resumed.elapsed()
        );
    }
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

#[test]
fn blocked_start_has_a_two_second_first_picture_bound_and_healthy_udp_uses_no_grant() {
    let _serial = SESSION_TEST.lock().unwrap_or_else(|p| p.into_inner());
    let pilot = Pilot::start();
    let started = Instant::now();
    let running = Running::start(pilot.config.clone(), true);
    running.wait(Duration::from_secs(2), || {
        running.session.connected() && running.frames.seq() > 0
    });
    eprintln!("blocked UDP first picture {:?}", started.elapsed());
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(
        running.session.media_path().reason,
        latch_stream::path::PathReason::UdpBlocked
    );
    running.ordered_input();
    drop(running);
    let grants = pilot.metrics.grants_created.load(Ordering::Acquire);
    let healthy = Running::start(pilot.config.clone(), false);
    healthy.wait(Duration::from_secs(2), || healthy.session.connected());
    let before = healthy.frames.seq();
    healthy.wait(Duration::from_secs(2), || {
        healthy.frames.seq() > before + 15
    });
    assert_eq!(pilot.metrics.grants_created.load(Ordering::Acquire), grants);
    healthy.ordered_input();
    healthy.hold_key();
    let denied = Instant::now();
    healthy.blackhole.store(true, Ordering::Release);
    healthy.wait(Duration::from_secs(2), || {
        healthy.session.relay_active()
            && healthy
                .session
                .published_at()
                .is_some_and(|at| at >= healthy.session.media_path().since)
    });
    eprintln!(
        "mid-stream UDP denial to decoded relay picture {:?}",
        denied.elapsed()
    );
    healthy.release_key();
    healthy.ordered_input();
}

#[test]
fn recovered_udp_switches_back_at_twenty_and_sixty_megabits() {
    let _serial = SESSION_TEST.lock().unwrap_or_else(|p| p.into_inner());
    // Full-rate warmup counts both relay legs. Keep quota/grant renewal out of
    // this handoff measurement, with enough credit for both 90-second setups.
    let pilot = Pilot::with_limits(|config| {
        config.monthly_bytes = 4 * 1024 * 1024 * 1024;
        config.limits.session_bytes = 2 * 1024 * 1024 * 1024;
    });
    for kbps in [20_000, 60_000] {
        let running = Running::with_rate(pilot.config.clone(), true, "load", kbps, 64, 64);
        running.wait(Duration::from_secs(2), || {
            running.session.connected() && running.session.relay_active()
        });
        running.hold_key();
        // Relay starts at 8 Mbps and climbs. Fill the five-second delivery
        // window at the ceiling before the first UDP Hello starts its check.
        let stable = std::cell::Cell::new(Instant::now());
        let peak_delivery = std::cell::Cell::new(0.0_f32);
        running.wait(Duration::from_secs(90), || {
            let stats = running.session.stats();
            peak_delivery.set(peak_delivery.get().max(stats.received_mbps));
            if stats.wire_budget_kbps != kbps {
                stable.set(Instant::now());
            }
            stable.get().elapsed() >= Duration::from_secs(5)
        });
        assert!(
            peak_delivery.get() >= kbps as f32 / 1000.0 * 0.95,
            "relay delivered only {} Mbps",
            peak_delivery.get()
        );
        let probe_kbps = std::cell::Cell::new(0);
        running.blackhole.store(false, Ordering::Release);
        let start = Instant::now();
        let before = running.frames.seq();
        let last_picture = std::cell::Cell::new(running.session.published_at().unwrap());
        let largest_gap = std::cell::Cell::new(Duration::ZERO);
        let measure_gap = || {
            if let Some(rate) = running.session.media_path().udp_probe_kbps {
                probe_kbps.set(probe_kbps.get().max(rate));
            }
            if let Some(at) = running.session.published_at() {
                largest_gap.set(
                    largest_gap
                        .get()
                        .max(at.saturating_duration_since(last_picture.get())),
                );
                last_picture.set(at);
            }
        };
        running.wait(Duration::from_secs(9), || {
            measure_gap();
            !running.session.relay_active()
        });
        assert!(
            running.frames.seq() > before + 3,
            "media stopped during UDP measurement"
        );
        let handoff = running.frames.seq();
        running.wait(Duration::from_millis(250), || {
            measure_gap();
            running.frames.seq() > handoff
        });
        measure_gap();
        assert!(
            largest_gap.get() < Duration::from_millis(250),
            "handoff stalled video for {:?}",
            largest_gap.get()
        );
        let probe_kbps = probe_kbps.get();
        assert!(
            (kbps * 103 / 100..=kbps * 115 / 100).contains(&probe_kbps),
            "probe {probe_kbps} kbps for {kbps} kbps relay"
        );
        let packets = running.probe_packets.lock();
        let hold = packets[0].0 + Duration::from_millis(200);
        let last = packets.last().unwrap().0;
        assert!(last.duration_since(hold) >= Duration::from_millis(350));
        let hold_bytes = packets
            .iter()
            .filter(|(at, _)| *at >= hold)
            .map(|(_, len)| len + 80)
            .sum::<usize>();
        let sent_kbps = hold_bytes as f64 * 8.0 / last.duration_since(hold).as_secs_f64() / 1000.0;
        assert!(
            (f64::from(probe_kbps) * 0.90..=f64::from(probe_kbps) * 1.10).contains(&sent_kbps),
            "sent {sent_kbps:.0} kbps for {probe_kbps} kbps probe"
        );
        drop(packets);
        eprintln!(
            "UDP return at {kbps} kbps {:?}, largest decoded-picture gap {:?}, relay peak {:.2} Mbps, probe {probe_kbps} kbps, measured hold {sent_kbps:.0} kbps",
            start.elapsed(),
            largest_gap.get(),
            peak_delivery.get()
        );
        running.release_key();
        running.ordered_input();
    }
}

#[test]
fn size_dependent_udp_black_hole_keeps_media_on_udp() {
    let _serial = SESSION_TEST.lock().unwrap_or_else(|p| p.into_inner());
    let pilot = Pilot::start();
    let running = Running::with_pattern(pilot.config.clone(), false, "noise");
    running.max_datagram.store(850, Ordering::Release);
    running.wait(Duration::from_secs(2), || running.session.connected());
    running.wait(Duration::from_secs(1), || {
        matches!(
            running.session.media_path().reason,
            latch_stream::path::PathReason::DatagramSize { bytes: 800 }
        )
    });
    assert!(!running.session.relay_active());
    assert_eq!(pilot.metrics.grants_created.load(Ordering::Acquire), 0);
    let before = running.frames.seq();
    running.wait(Duration::from_secs(2), || running.frames.seq() > before + 3);
    running.ordered_input();
}

#[test]
fn bursty_seventeen_percent_udp_loss_switches_without_reconnecting() {
    let _serial = SESSION_TEST.lock().unwrap_or_else(|p| p.into_inner());
    let pilot = Pilot::start();
    let running = Running::with_rate(pilot.config.clone(), false, "load", 20_000, 128, 128);
    running.wait(Duration::from_secs(2), || running.session.connected());
    running.wait(Duration::from_secs(2), || {
        running.session.stats().mbps > 4.0
    });
    running.hold_key();
    *running.downstream.lock() = latch_proto::sim::Link::new(
        latch_proto::sim::Impairment {
            // 15% independent loss plus five-packet bursts gives about 17% total.
            loss: 0.15,
            burst_enter: 0.005,
            burst_exit: 0.2,
            ..latch_proto::sim::Impairment::NONE
        },
        61,
    );
    let start = Instant::now();
    running.wait(Duration::from_secs(2), || running.session.relay_active());
    eprintln!(
        "bursty loss relay selection {:?}, sent/lost/duplicate {:?}, {:?}",
        start.elapsed(),
        running.downstream.lock().counts(),
        running.session.stats()
    );
    assert!(running.session.connected());
    running.release_key();
    running.ordered_input();
}

#[test]
fn udp_policer_selects_relay_and_a_relay_close_recovers_in_under_a_second() {
    let _serial = SESSION_TEST.lock().unwrap_or_else(|p| p.into_inner());
    let mut pilot = Pilot::start();
    let running = Running::with_rate(pilot.config.clone(), false, "load", 20_000, 128, 128);
    running.wait(Duration::from_secs(3), || running.session.connected());
    running.wait(Duration::from_secs(2), || {
        running.session.stats().mbps > 4.0
    });
    running.hold_key();
    running
        .downstream
        .lock()
        .set_bandwidth(3000, Duration::from_millis(5));
    let start = Instant::now();
    running.wait(Duration::from_secs(2), || running.session.relay_active());
    eprintln!(
        "3 Mbps UDP policer relay selection {:?}, sent/lost/duplicate {:?}",
        start.elapsed(),
        running.downstream.lock().counts()
    );
    running.blackhole.store(true, Ordering::Release);
    pilot.stop();
    running.wait(Duration::from_secs(1), || !running.session.relay_active());
    pilot.restart();
    let start = Instant::now();
    let before = running.frames.seq();
    running.wait(Duration::from_millis(950), || {
        running.session.relay_active() && running.frames.seq() > before
    });
    eprintln!("relay close recovery {:?}", start.elapsed());
    running.release_key();
    running.ordered_input();
}

#[test]
fn flapping_udp_bounds_switches_and_preserves_a_held_key() {
    let _serial = SESSION_TEST.lock().unwrap_or_else(|p| p.into_inner());
    let pilot = Pilot::start();
    let running = Running::start(pilot.config.clone(), true);
    running.wait(Duration::from_secs(2), || {
        running.session.connected() && running.session.relay_active()
    });
    running.hold_key();
    for probe_wait in [9, 40, 65] {
        running.blackhole.store(false, Ordering::Release);
        running.wait(Duration::from_secs(probe_wait), || {
            !running.session.relay_active()
        });
        assert!(latch_engine::testmode::held_keys().contains(&66));
        running.blackhole.store(true, Ordering::Release);
        running.wait(Duration::from_secs(2), || running.session.relay_active());
        assert!(latch_engine::testmode::held_keys().contains(&66));
    }
    running.blackhole.store(false, Ordering::Release);
    let before = running.frames.seq();
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(3) {
        assert!(
            running.session.relay_active(),
            "flapping exceeded three returns per minute"
        );
        thread::sleep(Duration::from_millis(5));
    }
    assert!(running.frames.seq() > before + 3);
    assert_eq!(pilot.metrics.grants_created.load(Ordering::Acquire), 4);
    running.release_key();
    running.ordered_input();
}

#[test]
fn auto_on_a_twenty_five_megabit_uplink_adapts_on_udp_until_random_loss_persists() {
    let _serial = SESSION_TEST.lock().unwrap_or_else(|p| p.into_inner());
    let pilot = Pilot::start();
    let running = Running::with_network(
        pilot.config.clone(),
        false,
        "load",
        50_000,
        128,
        128,
        Network {
            bandwidth: 25_000,
            ..Default::default()
        },
    );
    running.wait(Duration::from_secs(3), || running.session.connected());
    let start = Instant::now();
    let mut budgets = Vec::new();
    while start.elapsed() < Duration::from_secs(5) {
        assert!(
            !running.session.relay_active(),
            "shared capacity triggered relay {:?}",
            running.session.stats()
        );
        assert_eq!(pilot.metrics.grants_created.load(Ordering::Acquire), 0);
        if start.elapsed() >= Duration::from_secs(3) {
            budgets.push(running.session.stats().wire_budget_kbps);
        }
        thread::sleep(Duration::from_millis(10));
    }
    let stats = running.session.stats();
    let mean_budget =
        budgets.iter().map(|rate| u64::from(*rate)).sum::<u64>() / budgets.len() as u64;
    eprintln!("25 Mbps Auto UDP adapted mean budget {mean_budget} kbps, video {:.2} Mbps, {:.1} fps, raw loss {:.1}%, host {:.1} ms, assembly {:.1} ms", stats.mbps, stats.fps, stats.raw_loss_pct, stats.host_ms, stats.assembly_ms);
    // Congestion-controlled capacity probes can temporarily exceed the FIFO.
    // Measure the adapted tail budget across its probe cycle.
    assert!(
        (15_000..=30_000).contains(&mean_budget),
        "mean budget {mean_budget}, {stats:?}"
    );
    let mut link = latch_proto::sim::Link::new(
        latch_proto::sim::Impairment {
            loss: 0.17,
            ..latch_proto::sim::Impairment::NONE
        },
        61,
    );
    link.set_bandwidth(25_000, Duration::from_millis(200));
    *running.downstream.lock() = link;
    let start = Instant::now();
    running.wait(Duration::from_secs(3), || running.session.relay_active());
    eprintln!(
        "25 Mbps plus 17% random loss selected relay in {:?}",
        start.elapsed()
    );
    let before = running.frames.seq();
    running.wait(Duration::from_secs(1), || {
        running.frames.seq() > before + 20
    });
}

#[test]
fn healthy_distant_udp_takes_no_grant_at_one_hundred_fifty_and_three_hundred_ms_one_way() {
    let _serial = SESSION_TEST.lock().unwrap_or_else(|p| p.into_inner());
    let pilot = Pilot::start();
    for millis in [150, 300] {
        let running = Running::with_network(
            pilot.config.clone(),
            false,
            "still",
            2000,
            128,
            128,
            Network {
                delay: Duration::from_millis(millis),
                ..Default::default()
            },
        );
        running.wait(Duration::from_secs(3), || running.session.connected());
        let before = running.frames.seq();
        running.wait(Duration::from_secs(3), || {
            running.frames.seq() > before + 15
        });
        assert_eq!(
            pilot.metrics.grants_created.load(Ordering::Acquire),
            0,
            "healthy {millis} ms path took a grant"
        );
        assert!(!running.session.relay_active());
    }
}

#[test]
fn silent_relay_black_hole_gets_a_fresh_relay_picture_within_one_second() {
    let _serial = SESSION_TEST.lock().unwrap_or_else(|p| p.into_inner());
    let pilot = Pilot::start();
    let gateway = gateway::Gateway::start(pilot.config.endpoint.port(), None);
    let mut config = pilot.config.clone();
    config.endpoint = Endpoint::new("localhost", gateway.port).unwrap();
    let running = Running::with_rate(config, true, "sweep", 2000, 64, 64);
    running.wait(Duration::from_secs(2), || {
        running.session.connected() && running.session.relay_active()
    });
    let before = running.frames.seq();
    running.wait(Duration::from_secs(1), || running.frames.seq() > before + 3);
    running.hold_key();
    let since = Instant::now();
    gateway.blackhole_existing_connections();
    running.wait(Duration::from_secs(1), || {
        running
            .session
            .published_at()
            .is_some_and(|at| at > since + Duration::from_millis(300))
    });
    eprintln!(
        "silently black-holed relay to fresh picture {:?}, grants {}",
        since.elapsed(),
        pilot.metrics.grants_created.load(Ordering::Acquire)
    );
    assert!(running.session.relay_active());
    assert_eq!(pilot.metrics.grants_created.load(Ordering::Acquire), 2);
    running.release_key();
}

#[test]
fn return_probes_share_a_constrained_uplink_without_stalling_relay_video() {
    let _serial = SESSION_TEST.lock().unwrap_or_else(|p| p.into_inner());
    let pilot = Pilot::start();
    let gateway = gateway::Gateway::start(pilot.config.endpoint.port(), Some(25_000));
    let mut config = pilot.config.clone();
    config.endpoint = Endpoint::new("localhost", gateway.port).unwrap();
    let running = Running::with_network(
        config,
        true,
        "load",
        50_000,
        128,
        128,
        Network {
            uplink: gateway.uplink.clone(),
            ..Default::default()
        },
    );
    running.wait(Duration::from_secs(2), || {
        running.session.connected() && running.session.relay_active()
    });
    // Measure a probe against an already healthy relay stream. Keep UDP
    // blocked until backpressure has drained, then its first Hello makes
    // the overdue return check start immediately.
    let stable = std::cell::Cell::new(Instant::now());
    running.wait(Duration::from_secs(10), || {
        let stats = running.session.stats();
        if stats.received_fps < 25.0 || stats.pacing_queue_ms > 40.0 {
            stable.set(Instant::now());
        }
        stable.get().elapsed() >= Duration::from_millis(600)
    });
    let start = Instant::now();
    running.blackhole.store(false, Ordering::Release);
    // Leave a little independent loss so the return check must stop early.
    *running.downstream.lock() = latch_proto::sim::Link::new(
        latch_proto::sim::Impairment {
            loss: 0.02,
            ..latch_proto::sim::Impairment::NONE
        },
        18,
    );
    let last = std::cell::Cell::new(running.session.published_at().unwrap());
    let gap = std::cell::Cell::new(Duration::ZERO);
    running.wait(Duration::from_secs(10), || {
        if let Some(at) = running.session.published_at() {
            gap.set(gap.get().max(at.saturating_duration_since(last.get())));
            last.set(at);
        }
        start.elapsed() >= Duration::from_secs(8)
    });
    assert!(
        gap.get() < Duration::from_millis(250),
        "probe disturbed relay video {:?}",
        gap.get()
    );
    assert!(running.session.relay_active());
    assert_eq!(pilot.metrics.grants_created.load(Ordering::Acquire), 1);
    eprintln!(
        "25 Mbps shared uplink return probe largest frame gap {:?}, {:?}",
        gap.get(),
        running.session.stats()
    );
}

#[test]
fn withdrawn_preparation_reuses_its_authorized_grant_on_a_new_attempt() {
    let _serial = SESSION_TEST.lock().unwrap_or_else(|p| p.into_inner());
    use latch_stream::session::RelayRoute;
    let pilot = Pilot::start();
    let route = RelayRoute::new(7, latch_relay::Role::Host, Some(pilot.config.clone()));
    route.enable_policy(true);
    let initial = route.path_state();
    for action in [RelayAction::Request, RelayAction::Pause] {
        route.handle(
            RelayControl {
                session: 7,
                attempt: 1,
                action,
            },
            |_| true,
        );
    }
    let deadline = Instant::now() + Duration::from_secs(3);
    while pilot.metrics.grants_created.load(Ordering::Acquire) == 0 {
        assert!(Instant::now() < deadline);
        route.maintain(false, |_| panic!("withdrawn request produced an offer"));
        thread::sleep(Duration::from_millis(5));
    }
    let parked_until = Instant::now() + Duration::from_millis(100);
    while Instant::now() < parked_until {
        route.maintain(false, |_| panic!("withdrawn request produced an offer"));
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(route.path_state(), initial);
    let mut offered = false;
    route.handle(
        RelayControl {
            session: 7,
            attempt: 2,
            action: RelayAction::Request,
        },
        |message| {
            offered = matches!(
                message,
                Message::Relay(RelayControl {
                    attempt: 2,
                    action: RelayAction::Offer(_),
                    ..
                })
            );
            true
        },
    );
    while !offered {
        assert!(Instant::now() < deadline);
        route.maintain(false, |message| {
            offered = matches!(
                message,
                Message::Relay(RelayControl {
                    attempt: 2,
                    action: RelayAction::Offer(_),
                    ..
                })
            );
            true
        });
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(pilot.metrics.grants_created.load(Ordering::Acquire), 1);
    assert!(!route.active());
}

#[test]
fn mtu_floor_recovers_udp_video_cadence_without_a_relay_grant() {
    let _serial = SESSION_TEST.lock().unwrap_or_else(|p| p.into_inner());
    let pilot = Pilot::start();
    let running = Running::with_rate(pilot.config.clone(), false, "load", 6000, 64, 64);
    running.max_datagram.store(592, Ordering::Release);
    running.wait(Duration::from_secs(2), || running.session.connected());
    running.wait(Duration::from_secs(1), || {
        matches!(
            running.session.media_path().reason,
            latch_stream::path::PathReason::DatagramSize { bytes: 576 }
        )
    });
    let warm = Instant::now();
    running.wait(Duration::from_secs(6), || {
        warm.elapsed() >= Duration::from_secs(5)
    });
    let before = running.frames.seq();
    let since = Instant::now();
    running.wait(Duration::from_secs(2), || {
        running.frames.seq() >= before + 40
    });
    eprintln!(
        "576-byte UDP MTU {} frames in {:?}, {:?}",
        running.frames.seq() - before,
        since.elapsed(),
        running.session.stats()
    );
    assert!(!running.session.relay_active());
    assert_eq!(pilot.metrics.grants_created.load(Ordering::Acquire), 0);
}

#[test]
fn delayed_relay_survives_its_first_large_keyframe_and_spends_one_grant() {
    use tracing_subscriber::prelude::*;
    struct FirstAck(Arc<AtomicU64>);
    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for FirstAck {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _: tracing_subscriber::layer::Context<'_, S>,
        ) {
            struct Sample(u64);
            impl tracing::field::Visit for Sample {
                fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
                    if field.name() == "first_relay_ack_us" {
                        self.0 = value;
                    }
                }
                fn record_debug(&mut self, _: &tracing::field::Field, _: &dyn std::fmt::Debug) {}
            }
            let mut sample = Sample(0);
            event.record(&mut sample);
            if sample.0 > 0 {
                let _ = self
                    .0
                    .compare_exchange(0, sample.0, Ordering::AcqRel, Ordering::Acquire);
            }
        }
    }
    let _serial = SESSION_TEST.lock().unwrap_or_else(|p| p.into_inner());
    let first_ack = Arc::new(AtomicU64::new(0));
    let _tracing = tracing::subscriber::set_default(tracing_subscriber::registry().with(
        FirstAck(first_ack.clone()).with_filter(tracing_subscriber::filter::filter_fn(
            |metadata| metadata.fields().field("first_relay_ack_us").is_some(),
        )),
    ));
    let pilot = Pilot::start();
    let gateway = gateway::Gateway::with_delay(
        pilot.config.endpoint.port(),
        Some(8000),
        Duration::from_millis(150),
    );
    let mut host_config = pilot.config.clone();
    host_config.endpoint = Endpoint::new("localhost", gateway.port).unwrap();
    let running = Running::with_network(
        host_config,
        false,
        "load",
        8000,
        64,
        64,
        Network {
            fps: 10,
            ..Default::default()
        },
    );
    running.wait(Duration::from_secs(3), || running.session.connected());
    running.hold_key();
    running.blackhole.store(true, Ordering::Release);
    running.wait(Duration::from_secs(6), || {
        running.session.relay_active() && first_ack.load(Ordering::Acquire) > 0
    });
    let start = Instant::now();
    let first_picture = running.frames.seq();
    let mut previous = first_picture;
    for _ in 0..8 {
        running.wait(Duration::from_secs(1), || {
            assert!(
                running.session.relay_active(),
                "healthy delayed relay left its route"
            );
            running.frames.seq() >= previous + 5
        });
        previous = running.frames.seq();
    }
    running.wait(Duration::from_secs(4), || {
        assert!(running.session.relay_active());
        start.elapsed() >= Duration::from_secs(4)
    });
    let ack = Duration::from_micros(first_ack.load(Ordering::Acquire));
    assert!(
        ack > Duration::from_millis(350),
        "fixture did not exercise the old watchdog {ack:?}"
    );
    assert_eq!(pilot.metrics.grants_created.load(Ordering::Acquire), 1);
    eprintln!(
        "150 ms one-way relay legs first video ACK {ack:?}, healthy for {:?}, pictures {}, wire budget {} kbps, grants 1",
        start.elapsed(),
        running.frames.seq() - first_picture,
        running.session.stats().wire_budget_kbps
    );
    running.release_key();
}
