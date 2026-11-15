use super::*;

// Runtime-only operations in this shared fixture are used by relay_runtime.rs.
#[allow(dead_code)]
#[path = "relay_pilot.rs"]
mod relay;

use crate::video::EncodedVideo;
use std::net::TcpListener;
use std::sync::atomic::AtomicUsize;

fn config() -> VideoConfig {
    VideoConfig {
        color: Default::default(),
        width: 64,
        height: 64,
        fps: 60,
        bitrate_kbps: 1000,
        codec: Codec::H264Baseline,
        display: None,
    }
}

fn info() -> VideoInfo {
    VideoInfo {
        captured_display: None,
        color: Default::default(),
        encoder: "initial".into(),
        hardware: true,
        codec: Codec::H264Baseline,
        width: 64,
        height: 64,
        display: DisplayRect {
            width: 64,
            height: 64,
            ..Default::default()
        },
    }
}

fn session(addr: SocketAddr) -> Arc<Session> {
    Arc::new(Session {
        relay: RelayRoute::new(7, latch_relay::Role::Host, None),
        id: 7,
        token: [0; 16],
        viewer: "test".into(),
        announced: addr,
        addr: Mutex::new(Some(addr)),
        ended: Mutex::new(None),
        audio_notice: Mutex::new(None),
        stop: Arc::new(AtomicBool::new(false)),
        finished: Mutex::new(false),
        finished_cv: Condvar::new(),
        keyframe: AtomicBool::new(false),
        input: Mutex::new(InputState {
            receiver: InputReceiver::new(),
            sink: None,
            display: info().display,
        }),
        pads: Mutex::new(SessionPads {
            receiver: Default::default(),
            sink: None,
            slots: 0,
            failing: Default::default(),
        }),
        feedback: Default::default(),
        rtt: Mutex::new(RttEstimator::new()),
        rtt_low: Mutex::new(None),
        train_ping: AtomicU64::new(0),
        train_rtt: Mutex::new(None),
        pace_kbps: AtomicU32::new(0),
        fec_burst: AtomicU8::new(0),
        fec: AtomicU8::new(DEFAULT_FEC_PERCENT),
        heard_us: AtomicU64::new(0),
        frames: AtomicU64::new(0),
        bytes: AtomicU64::new(0),
        transport: TransportState::new(1000),
        keyframe_requests: Mutex::default(),
        recovery: Default::default(),
        audio_enabled: false,
        surround_kbps: 0,
        mic: Mutex::new(None),
        cursor_supported: false,
        probe_trains: false,
        draw_cursor: AtomicBool::new(true),
        control_writer: Mutex::new(None),
    })
}

fn sockets() -> (TcpStream, TcpStream) {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (host, _) = listener.accept().unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    (host, client)
}

#[test]
fn route_changes_finish_before_new_udp_pad_updates_are_acknowledged() {
    use latch_proto::gamepad::{PadKind, PadState, PadUpdate};
    let udp = UdpSocket::bind("127.0.0.1:0").unwrap();
    udp.set_read_timeout(Some(Duration::from_millis(50)))
        .unwrap();
    let address = udp.local_addr().unwrap();
    let session = session(address);
    session.pads.lock().slots = 1;
    let server = Server::bind(0, Options::default()).unwrap();
    let (started, entered) = mpsc::channel();
    let mut worker = None;
    assert!(session.update_relay(|_| {
        let server = server.clone();
        let session = session.clone();
        worker = Some(std::thread::spawn(move || {
            let mut bytes = Vec::new();
            wire::write_pad(
                &mut bytes,
                session.id,
                &[PadUpdate {
                    slot: 0,
                    seq: 1,
                    kind: Some(PadKind::Xbox),
                    state: PadState::default(),
                }],
            );
            started.send(()).unwrap();
            dispatch_datagram(&server, &session, &bytes, address, false);
        }));
        entered.recv_timeout(Duration::from_secs(1)).unwrap();
        // The new route may already be visible, but delivery must wait until
        // its transition has released the previous route's virtual devices.
        let error = udp.recv(&mut [0; 1500]).unwrap_err();
        assert!(matches!(
            error.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
        ));
        true
    }));
    udp.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    let mut bytes = [0; 1500];
    let length = udp.recv(&mut bytes).expect("new-route pad acknowledgment");
    let Datagram::PadAck { acks, .. } = wire::parse(&bytes[..length]).unwrap() else {
        panic!("expected a pad acknowledgment");
    };
    assert_eq!(acks.iter().next().unwrap().seq, 1);
    worker.unwrap().join().unwrap();
}

type Next = Box<dyn FnMut(bool, Duration, &mut VideoInfo) -> anyhow::Result<EncodedVideo> + Send>;

struct Video {
    info: VideoInfo,
    next: Next,
    held: Arc<AtomicBool>,
}

impl Drop for Video {
    fn drop(&mut self) {
        self.held.store(false, Ordering::Release);
    }
}

impl VideoPipeline for Video {
    fn info(&self) -> VideoInfo {
        self.info.clone()
    }
    fn next(&mut self, keyframe: bool, idle: Duration) -> anyhow::Result<EncodedVideo> {
        (self.next)(keyframe, idle, &mut self.info)
    }
}

#[test]
fn a_viewer_that_does_not_read_cannot_block_the_control_writer() {
    let server = Server::bind(0, Options::default()).unwrap();
    let session = session("127.0.0.1:9".parse().unwrap());
    let (mut host, _client) = sockets();
    // Windows grows a loopback send backlog on demand, so without a fixed
    // send buffer the prefill below never fills it and the Accept never
    // blocks. Setting SO_SNDBUF turns that autotuning off.
    #[cfg(windows)]
    {
        let _ = socket2::SockRef::from(&host).set_send_buffer_size(8192);
        let _ = socket2::SockRef::from(&_client).set_recv_buffer_size(8192);
    }
    host.set_nonblocking(true).unwrap();
    let until = Instant::now() + Duration::from_millis(200);
    while Instant::now() < until {
        match host.write(&[0; 65536]) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(1))
            }
            Err(e) => panic!("prefill failed: {e}"),
        }
    }
    host.set_nonblocking(false).unwrap();
    let accept = Accept {
        relay: false,
        color: Default::default(),
        cursor: false,
        gamepads: 0,
        gamepad_kinds: Vec::new(),
        gamepad_problem: String::new(),
        reference_invalidation: false,
        session: session.id,
        token: control::token_hex(&session.token),
        codec: Codec::H264Baseline,
        width: 64,
        height: 64,
        fps: 60,
        bitrate_kbps: 1000,
        encoder: "x".repeat(60_000),
        audio: None,
        audio_problem: String::new(),
        mic: None,
        mic_problem: String::new(),
    };
    let (tx, rx) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        let reason = server.control(&session, host, accept, None, || {
            panic!("blocked Accept must fail")
        });
        tx.send(reason).unwrap();
    });
    assert!(rx
        .recv_timeout(Duration::from_secs(10))
        .unwrap()
        .contains("before the stream started"));
    worker.join().unwrap();
}

#[test]
fn an_opening_reservation_is_busy_before_a_viewer_is_installed() {
    let server = Server::bind(0, Options::default()).unwrap();
    assert!(!server.busy());
    let guard = server.opening.lock(Duration::ZERO).unwrap();
    assert!(server.busy());
    assert!(server.status().viewer.is_none());
    // A second opening gives up instead of waiting for ever.
    let start = Instant::now();
    assert!(server.opening.lock(Duration::from_millis(50)).is_none());
    assert!(start.elapsed() >= Duration::from_millis(50));
    drop(guard);
    assert!(!server.busy());
    assert!(server.opening.lock(Duration::ZERO).is_some());
}

#[test]
fn a_stuck_media_thread_is_left_behind_at_the_deadline() {
    let (release, stuck) = mpsc::channel::<()>();
    let thread = std::thread::spawn(move || {
        let _ = stuck.recv();
    });
    let start = Instant::now();
    assert!(!join_by(thread, start + Duration::from_millis(50)));
    assert!(start.elapsed() < Duration::from_secs(2));
    drop(release);
    let done = std::thread::spawn(|| {});
    assert!(join_by(done, Instant::now() + Duration::from_secs(5)));
}

#[test]
fn relay_control_backlog_ends_cleanly_at_its_bound() {
    let session = session("127.0.0.1:9".parse().unwrap());
    let mut inbox = VecDeque::new();
    for _ in 0..128 {
        assert!(session.queue_relay_control(&mut inbox, 1, Message::Keepalive));
    }
    assert!(!session.queue_relay_control(&mut inbox, 1, Message::Keepalive));
    assert_eq!(inbox.len(), 128);
    assert_eq!(
        session.ended.lock().as_ref().unwrap().code,
        end::TRANSPORT_FAILED
    );
    session.end(end::ENDED, "later cleanup preserves the first cause");
    assert_eq!(
        session.ended.lock().as_ref().unwrap().code,
        end::TRANSPORT_FAILED
    );
}

#[test]
fn priority_failure_keeps_its_end_code_when_the_sender_is_preempted() {
    let mut pilot = relay::Pilot::start();
    pilot.config.limits.priority_queue_bytes = 1280;
    for explicit_stop in [false, true] {
        let server = Server::bind(0, Options::default()).unwrap();
        let mut session = session("127.0.0.1:9".parse().unwrap());
        Arc::get_mut(&mut session).unwrap().relay =
            RelayRoute::new(7, latch_relay::Role::Host, Some(pilot.config.clone()));
        let viewer = RelayRoute::new(7, latch_relay::Role::Viewer, Some(pilot.config.clone()));
        let mut to_host = VecDeque::new();
        let mut to_viewer = VecDeque::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !session.relay.active() || !viewer.active() {
            assert!(Instant::now() < deadline, "relay setup deadline");
            session.relay.poll();
            viewer.poll();
            assert!(session.relay.maintain(false, |m| {
                to_viewer.push_back(m.clone());
                true
            }));
            assert!(viewer.maintain(true, |m| {
                to_host.push_back(m.clone());
                true
            }));
            while let Some(Message::Relay(message)) = to_host.pop_front() {
                assert!(session.relay.handle(message, |m| {
                    to_viewer.push_back(m.clone());
                    true
                }));
            }
            while let Some(Message::Relay(message)) = to_viewer.pop_front() {
                assert!(viewer.handle(message, |m| {
                    to_host.push_back(m.clone());
                    true
                }));
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        let (failed, failure) = mpsc::channel();
        let (resume, paused) = mpsc::channel();
        let sending = {
            let session = session.clone();
            std::thread::spawn(move || {
                let error = session
                    .relay
                    .send_control(&Message::Notice(Notice {
                        kind: "audio".into(),
                        message: "x".repeat(2000),
                    }))
                    .unwrap_err();
                assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
                failed.send(()).unwrap();
                // The transport has marked fatal, but the sending worker has
                // not yet recorded the session's end reason.
                let _ = paused.recv_timeout(Duration::from_secs(5));
                session.end(end::TRANSPORT_FAILED, "late sending worker");
            })
        };
        failure.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(session.ended.lock().is_none());
        if explicit_stop {
            session.end(end::HOST_STOPPING, "explicit Stop wins");
        }
        let (host, mut peer) = sockets();
        let accept: Accept = serde_json::from_value(serde_json::json!({
            "session": 7, "token": "00000000000000000000000000000000",
            "codec": "h264-baseline", "width": 64, "height": 64, "fps": 30,
            "bitrate_kbps": 2000, "encoder": "test", "audio": null, "relay": true
        }))
        .unwrap();
        let controlling = {
            let session = session.clone();
            std::thread::spawn(move || {
                server.control(&session, host, accept, None, || (None, None))
            })
        };
        assert!(matches!(
            control::read_message(&mut peer).unwrap(),
            Message::Accept(_)
        ));
        let ended = control::read_message(&mut peer).unwrap();
        let _ = resume.send(());
        sending.join().unwrap();
        let _ = peer.shutdown(Shutdown::Both);
        controlling.join().unwrap();
        viewer.shutdown();
        let expected = if explicit_stop {
            end::HOST_STOPPING
        } else {
            end::TRANSPORT_FAILED
        };
        assert!(
            matches!(&ended, Message::End(e) if e.code == expected),
            "{ended:?}"
        );
        assert_eq!(session.ended.lock().as_ref().unwrap().code, expected);
    }
}

#[test]
fn an_explicit_viewer_end_keeps_normal_completion() {
    let server = Server::bind(0, Options::default()).unwrap();
    let session = session("127.0.0.1:9".parse().unwrap());
    let (host, mut peer) = sockets();
    let accept: Accept = serde_json::from_value(serde_json::json!({
        "session": 7, "token": "00000000000000000000000000000000",
        "codec": "h264-baseline", "width": 64, "height": 64, "fps": 30,
        "bitrate_kbps": 2000, "encoder": "test", "audio": null
    }))
    .unwrap();
    let worker =
        std::thread::spawn(move || server.control(&session, host, accept, None, || (None, None)));
    assert!(matches!(
        control::read_message(&mut peer).unwrap(),
        Message::Accept(_)
    ));
    control::write_message(
        &mut peer,
        &Message::End(End {
            code: end::ENDED,
            message: "explicit viewer stop".into(),
        }),
    )
    .unwrap();
    let ended = control::read_message(&mut peer).unwrap();
    let _ = peer.shutdown(Shutdown::Both);
    worker.join().unwrap();
    assert!(matches!(ended, Message::End(e) if e.code == end::ENDED));
}

#[test]
fn ending_revokes_and_releases_input_immediately() {
    struct Sink(Arc<AtomicUsize>);
    impl InputSink for Sink {
        fn apply(&mut self, _: &input::Event, _: DisplayRect) -> anyhow::Result<()> {
            Ok(())
        }
        fn release_all(&mut self) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }
    let session = session("127.0.0.1:9".parse().unwrap());
    let releases = Arc::new(AtomicUsize::new(0));
    session.input.lock().sink = Some(Box::new(Sink(releases.clone())));
    session.end(end::ENDED, "done");
    assert!(session.input.lock().sink.is_none());
    assert_eq!(releases.load(Ordering::Relaxed), 1);
    session.end(end::ENDED, "again");
    assert_eq!(releases.load(Ordering::Relaxed), 1);
}

#[test]
fn viewer_stop_and_host_end_do_not_wait_for_in_flight_input() {
    struct BlockedInput(mpsc::Sender<()>, mpsc::Receiver<()>, Arc<AtomicUsize>);
    impl InputSink for BlockedInput {
        fn apply(&mut self, _: &input::Event, _: DisplayRect) -> anyhow::Result<()> {
            self.0.send(()).unwrap();
            self.1.recv_timeout(Duration::from_secs(3)).unwrap();
            Ok(())
        }
        fn release_all(&mut self) {
            self.2.fetch_add(1, Ordering::Relaxed);
        }
    }
    for viewer_stop in [true, false] {
        let server = Server::bind(0, Options::default()).unwrap();
        let session = session("127.0.0.1:9".parse().unwrap());
        let (entered, in_flight) = mpsc::channel();
        let (release, blocked) = mpsc::channel();
        let releases = Arc::new(AtomicUsize::new(0));
        session.input.lock().sink =
            Some(Box::new(BlockedInput(entered, blocked, releases.clone())));
        let (host, mut peer) = sockets();
        peer.set_read_timeout(Some(Duration::from_millis(500)))
            .unwrap();
        let accept: Accept = serde_json::from_value(serde_json::json!({
            "session": 7, "token": "00000000000000000000000000000000",
            "codec": "h264-baseline", "width": 64, "height": 64, "fps": 30,
            "bitrate_kbps": 1000, "encoder": "test", "audio": null
        }))
        .unwrap();
        let held = Arc::new(AtomicBool::new(true));
        let controlling = {
            let server = server.clone();
            let session = session.clone();
            let held = held.clone();
            std::thread::spawn(move || {
                server.control(&session, host, accept, None, || {
                    let session = session.clone();
                    let video = std::thread::spawn(move || {
                        while !session.stopping() {
                            std::thread::sleep(Duration::from_millis(1));
                        }
                        held.store(false, Ordering::Release);
                    });
                    (Some(video), None)
                })
            })
        };
        assert!(matches!(
            control::read_message(&mut peer).unwrap(),
            Message::Accept(_)
        ));
        let applying = {
            let server = server.clone();
            let session = session.clone();
            std::thread::spawn(move || {
                let mut bytes = Vec::new();
                wire::write_input(
                    &mut bytes,
                    session.id,
                    0,
                    &[input::Event::Text("abc".into())],
                );
                dispatch_datagram(&server, &session, &bytes, session.announced, false);
            })
        };
        in_flight.recv_timeout(Duration::from_secs(1)).unwrap();
        let ending = if viewer_stop {
            control::write_message(
                &mut peer,
                &Message::End(End {
                    code: end::ENDED,
                    message: "viewer Stop".into(),
                }),
            )
            .unwrap();
            None
        } else {
            let session = session.clone();
            Some(std::thread::spawn(move || {
                session.end(end::HOST_STOPPING, "host Stop")
            }))
        };
        let ended = control::read_message(&mut peer);
        let deadline = Instant::now() + Duration::from_millis(100);
        while held.load(Ordering::Acquire) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        let capture_stopped = !held.load(Ordering::Acquire);
        let end_returned = ending.as_ref().is_none_or(|worker| worker.is_finished());
        release.send(()).unwrap();
        applying.join().unwrap();
        if let Some(ending) = ending {
            ending.join().unwrap();
        }
        let _ = peer.shutdown(Shutdown::Both);
        controlling.join().unwrap();
        let expected = if viewer_stop {
            end::ENDED
        } else {
            end::HOST_STOPPING
        };
        assert!(matches!(ended, Ok(Message::End(e)) if e.code == expected));
        assert!(capture_stopped);
        assert!(end_returned, "host end waited for in-flight input");
        assert_eq!(releases.load(Ordering::Relaxed), 1);
    }
}

#[test]
fn end_reaches_the_viewer_and_stops_capture_while_input_cleanup_is_pending() {
    struct SlowSink(mpsc::Sender<()>, mpsc::Receiver<()>);
    impl InputSink for SlowSink {
        fn apply(&mut self, _: &input::Event, _: DisplayRect) -> anyhow::Result<()> {
            Ok(())
        }
        fn release_all(&mut self) {
            self.0.send(()).unwrap();
            let _ = self.1.recv_timeout(Duration::from_secs(5));
        }
    }
    let server = Server::bind(0, Options::default()).unwrap();
    let session = session("127.0.0.1:9".parse().unwrap());
    let (entered, cleanup) = mpsc::channel();
    let (release, blocked) = mpsc::channel();
    session.input.lock().sink = Some(Box::new(SlowSink(entered, blocked)));
    let (host, mut peer) = sockets();
    peer.set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
    let accept: Accept = serde_json::from_value(serde_json::json!({
        "session": 7, "token": "00000000000000000000000000000000",
        "codec": "h264-baseline", "width": 64, "height": 64, "fps": 30,
        "bitrate_kbps": 1000, "encoder": "test", "audio": null
    }))
    .unwrap();
    let held = Arc::new(AtomicBool::new(true));
    let worker = {
        let session = session.clone();
        let held = held.clone();
        std::thread::spawn(move || {
            server.control(&session, host, accept, None, || {
                let video_session = session.clone();
                let video = std::thread::spawn(move || {
                    let _capture = Video {
                        info: info(),
                        held,
                        next: Box::new(|_, _, _| unreachable!()),
                    };
                    while !video_session.stopping() {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                });
                (Some(video), None)
            })
        })
    };
    assert!(matches!(
        control::read_message(&mut peer).unwrap(),
        Message::Accept(_)
    ));
    let ending = {
        let session = session.clone();
        std::thread::spawn(move || session.end(end::HOST_STOPPING, "stop"))
    };
    cleanup.recv_timeout(Duration::from_secs(1)).unwrap();
    let ended = control::read_message(&mut peer);
    let deadline = Instant::now() + Duration::from_millis(500);
    while held.load(Ordering::Acquire) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(1));
    }
    let capture_stopped = !held.load(Ordering::Acquire);
    release.send(()).unwrap();
    ending.join().unwrap();
    let _ = peer.shutdown(Shutdown::Both);
    worker.join().unwrap();
    assert!(matches!(ended, Ok(Message::End(e)) if e.code == end::HOST_STOPPING));
    assert!(capture_stopped);
}

#[test]
fn video_reopen_failure_uses_remaining_budget() {
    let server = Server::bind(0, Options::default()).unwrap();
    let session = session("127.0.0.1:9".parse().unwrap());
    let mut attempts = 0;
    let pipeline = || {
        Box::new(Video {
            info: info(),
            held: Arc::new(AtomicBool::new(true)),
            next: Box::new(|_, _, _| anyhow::bail!("capture lost")),
        }) as Box<dyn VideoPipeline>
    };
    send_video(&server, &session, pipeline(), config(), |_| {
        attempts += 1;
        if attempts == 1 {
            anyhow::bail!("temporarily busy");
        }
        Ok(pipeline())
    });
    assert_eq!(attempts, VIDEO_RECOVERIES);
}

/// A codec whose encoder cannot go on (HEVC after a resize to a size its
/// only encoder does not take) ends the session at once, as an encoder
/// failure the viewer reconnects from, not after the reopen budget as a
/// capture failure.
#[test]
fn a_codec_that_cannot_go_on_ends_the_session_for_a_new_negotiation() {
    let server = Server::bind(0, Options::default()).unwrap();
    for during_reopen in [false, true] {
        let session = session("127.0.0.1:9".parse().unwrap());
        let mut attempts = 0;
        let pipeline = Box::new(Video {
            info: info(),
            held: Arc::new(AtomicBool::new(true)),
            next: Box::new(move |_, _, _| {
                if during_reopen {
                    anyhow::bail!("capture lost");
                }
                Err(video::Unencodable("1366 × 768 is not whole coding blocks".into()).into())
            }),
        });
        send_video(&server, &session, pipeline, config(), |_| {
            attempts += 1;
            Err(
                anyhow::Error::from(video::Unencodable("no HEVC at this size".into()))
                    .context("open the HEVC encoder"),
            )
        });
        assert_eq!(attempts, usize::from(during_reopen));
        let ended = session.ended.lock().clone().unwrap();
        assert_eq!(ended.code, end::ENCODER_FAILED);
        assert!(ended.message.contains("reconnecting"), "{}", ended.message);
    }
}

#[test]
fn repeated_video_errors_release_exclusive_capture_and_have_a_retry_budget() {
    let server = Server::bind(0, Options::default()).unwrap();
    let session = session("127.0.0.1:9".parse().unwrap());
    let held = Arc::new(AtomicBool::new(false));
    let attempts = AtomicUsize::new(0);
    let open = |_: &VideoConfig| -> anyhow::Result<Box<dyn VideoPipeline>> {
        assert!(
            !held.swap(true, Ordering::AcqRel),
            "old capture still owns output"
        );
        let mut sent = false;
        Ok(Box::new(Video {
            info: info(),
            next: Box::new(move |_, _, _| {
                if std::mem::replace(&mut sent, true) {
                    anyhow::bail!("capture lost");
                }
                Ok(EncodedVideo {
                    generation: 0,
                    data: vec![1; 100],
                    keyframe: true,
                    captured: Instant::now(),
                    picture: 0,
                    recovery: None,
                })
            }),
            held: held.clone(),
        }))
    };
    send_video(&server, &session, open(&config()).unwrap(), config(), |c| {
        attempts.fetch_add(1, Ordering::Relaxed);
        open(c)
    });
    assert_eq!(attempts.load(Ordering::Relaxed), VIDEO_RECOVERIES);
    // Fatal capture failure cancels queued media; reopening must still be bounded.
    assert!(session.frames.load(Ordering::Relaxed) <= (VIDEO_RECOVERIES + 1) as u64);
    assert!(!held.load(Ordering::Acquire));
    assert_eq!(
        session.ended.lock().as_ref().unwrap().code,
        end::CAPTURE_FAILED
    );
}

#[test]
fn idle_requests_are_prompt_and_each_encoded_frame_updates_mapping_and_is_sent() {
    struct Pointer(Arc<Mutex<Vec<(input::Event, DisplayRect)>>>);
    impl InputSink for Pointer {
        fn apply(&mut self, event: &input::Event, display: DisplayRect) -> anyhow::Result<()> {
            self.0.lock().push((event.clone(), display));
            Ok(())
        }
        fn release_all(&mut self) {}
    }
    let server = Server::bind(0, Options::default()).unwrap();
    let udp = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    udp.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
    let session = session(udp.local_addr().unwrap());
    let positions = Arc::new(Mutex::new(Vec::new()));
    session.input.lock().sink = Some(Box::new(Pointer(positions.clone())));
    let (entered_tx, entered_rx) = mpsc::channel();
    let (request_tx, request_rx) = mpsc::channel();
    let (idr_tx, idr_rx) = mpsc::channel();
    let mut calls = 0;
    let worker_session = session.clone();
    let pipeline = Video {
        info: info(),
        held: Arc::new(AtomicBool::new(true)),
        next: Box::new(move |keyframe, idle, info| {
            calls += 1;
            if calls == 1 {
                assert!(!keyframe);
                entered_tx.send(()).unwrap();
                request_rx.recv_timeout(Duration::from_secs(1)).unwrap();
                std::thread::sleep(idle);
                info.display = DisplayRect {
                    x: -128,
                    y: 10,
                    width: 128,
                    height: 96,
                };
                info.captured_display = Some("fallback-display".into());
                info.encoder = "fallback".into();
                info.hardware = false;
            } else if calls == 2 {
                assert!(keyframe);
                idr_tx.send(()).unwrap();
            } else {
                while !worker_session.stopping() {
                    std::thread::sleep(Duration::from_millis(1));
                }
                anyhow::bail!("stopped");
            }
            Ok(EncodedVideo {
                generation: 0,
                data: vec![calls; 100],
                keyframe,
                captured: Instant::now(),
                picture: 0,
                recovery: None,
            })
        }),
    };
    let worker = {
        let server = server.clone();
        let session = session.clone();
        std::thread::spawn(move || {
            send_video(&server, &session, Box::new(pipeline), config(), |_| {
                panic!("unexpected reopen")
            })
        })
    };
    entered_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    session.keyframe.store(true, Ordering::Release);
    let start = Instant::now();
    request_tx.send(()).unwrap();
    // What matters is that the very next encode is the keyframe: the mock
    // asserts that on its second call. Wall-clock bounds are left loose;
    // a loaded CI runner can take 4x longer to wake a sleeping thread.
    idr_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("the keyframe request was never served");
    eprintln!(
        "keyframe served {:?} after the request (idle repeat {:?})",
        start.elapsed(),
        IDLE_REPEAT
    );
    let mut seen = [false; 2];
    let mut buf = [0; 2048];
    while !seen.iter().all(|v| *v) {
        let n = udp.recv(&mut buf).unwrap();
        if let Datagram::Video { shard, .. } = wire::parse(&buf[..n]).unwrap() {
            seen[shard.frame as usize] = true;
        }
    }
    session.stop.store(true, Ordering::Release);
    worker.join().unwrap();
    let display = session.input.lock().display;
    assert_eq!(display.map(32, 32, 64, 64), (-64, 58));
    let positions = positions.lock();
    assert_eq!(positions.len(), 1);
    assert_eq!(
        positions[0],
        (
            input::Event::MousePosition {
                x: 32,
                y: 32,
                width: 64,
                height: 64
            },
            display
        )
    );
    assert_eq!(
        server.status().captured_display.as_deref(),
        Some("fallback-display")
    );
    assert_eq!(server.status().encoder, "fallback");
    assert!(!server.status().hardware);
    assert_eq!(session.frames.load(Ordering::Relaxed), 2);
}

struct Sound {
    session: Arc<Session>,
    fail: bool,
    frames: u32,
    held: Arc<AtomicBool>,
}

impl Drop for Sound {
    fn drop(&mut self) {
        self.held.store(false, Ordering::Release);
    }
}

impl AudioCapture for Sound {
    fn read(
        &mut self,
        out: &mut [i16; audio::FRAME * audio::CHANNELS],
        _: Duration,
    ) -> anyhow::Result<bool> {
        if self.fail {
            anyhow::bail!("audio device lost");
        }
        if self.frames == 4 {
            self.session.end(end::ENDED, "test complete");
            return Ok(false);
        }
        std::thread::sleep(Duration::from_millis(10));
        // Independent, full-range channel noise forces large coded frames.
        let mut state = 1 + self.frames;
        for sample in out {
            state = state.wrapping_mul(1664525).wrapping_add(1013904223);
            *sample = (state >> 16) as i16;
        }
        self.frames += 1;
        Ok(true)
    }
    fn describe(&self) -> String {
        "test noise".into()
    }
}

#[test]
fn audio_reports_failure_retries_with_backoff_clears_notice_and_fits_the_mtu() {
    let server = Server::bind(0, Options::default()).unwrap();
    let udp = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    udp.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
    let session = session(udp.local_addr().unwrap());
    let held = Arc::new(AtomicBool::new(true));
    let capture = Sound {
        session: session.clone(),
        fail: true,
        frames: 0,
        held: held.clone(),
    };
    let (host, mut client) = sockets();
    let accept = Accept {
        relay: false,
        color: Default::default(),
        cursor: false,
        gamepads: 0,
        gamepad_kinds: Vec::new(),
        gamepad_problem: String::new(),
        reference_invalidation: false,
        session: session.id,
        token: control::token_hex(&session.token),
        codec: Codec::H264Baseline,
        width: 64,
        height: 64,
        fps: 60,
        bitrate_kbps: 1000,
        encoder: "test".into(),
        audio: Some(AudioFormat {
            rate: audio::RATE,
            channels: 2,
            frame_samples: audio::FRAME as u32,
            codec: "lpcm-rice".into(),
        }),
        audio_problem: String::new(),
        mic: None,
        mic_problem: String::new(),
    };
    let worker = {
        let server = server.clone();
        let session = session.clone();
        std::thread::spawn(move || {
            server.control(&session, host, accept, None, || {
                let server = server.clone();
                let session = session.clone();
                let mut tries = 0;
                let start = Instant::now();
                let worker_session = session.clone();
                let audio = std::thread::spawn(move || {
                    send_audio(&server, &session, Box::new(capture), || {
                        assert!(
                            !held.load(Ordering::Acquire),
                            "failed audio capture still owns device"
                        );
                        tries += 1;
                        if tries == 1 {
                            anyhow::bail!("audio server still down");
                        }
                        assert!(
                            start.elapsed() >= Duration::from_millis(500),
                            "no retry backoff"
                        );
                        held.store(true, Ordering::Release);
                        Ok(Box::new(Sound {
                            session: worker_session.clone(),
                            fail: false,
                            frames: 0,
                            held: held.clone(),
                        }))
                    })
                });
                (None, Some(audio))
            })
        })
    };
    assert!(matches!(
        control::read_message(&mut client).unwrap(),
        Message::Accept(_)
    ));
    let mut notices = Vec::new();
    loop {
        match control::read_message(&mut client).unwrap() {
            Message::Notice(n) if n.kind == "audio" => notices.push(n.message),
            Message::End(_) => break,
            _ => {}
        }
    }
    worker.join().unwrap();
    assert_eq!(
        notices,
        ["audio device lost", "audio server still down", ""]
    );
    assert!(server.status().audio_problem.is_empty());
    let mut buf = [0; 4096];
    let mut packets = 0u32;
    let mut parity = 0;
    let mut decoded = Vec::new();
    while packets < 4 || parity < 1 {
        let n = udp.recv(&mut buf).unwrap();
        assert!(
            n <= latch_proto::MAX_DATAGRAM,
            "audio datagram is {n} bytes"
        );
        if let Datagram::Audio { packet, .. } = wire::parse(&buf[..n]).unwrap() {
            if packet.parity {
                parity += 1;
            } else {
                latch_codec::audio::decode(packet.payload, &mut decoded).unwrap();
                // One 5 ms frame, one packet.
                assert_eq!(decoded.len(), audio::FRAME * audio::CHANNELS);
                let mut state = 1 + packets;
                let expected: Vec<i16> = (0..audio::FRAME * audio::CHANNELS)
                    .map(|_| {
                        state = state.wrapping_mul(1664525).wrapping_add(1013904223);
                        (state >> 16) as i16
                    })
                    .collect();
                assert_eq!(decoded, expected);
                packets += 1;
            }
        }
    }
}

#[test]
fn audio_packets_split_the_frame_evenly_and_always_fit() {
    for (channels, samples) in [(1, 240), (2, 240), (6, 80), (8, 60)] {
        assert_eq!(audio_packet_samples(channels), samples, "{channels} ch");
        assert_eq!(audio::FRAME % samples, 0);
        assert!(
            latch_codec::audio::max_coded_len(channels, samples) <= latch_proto::audio::MAX_PAYLOAD
        );
    }
}

#[test]
fn surround_keeps_more_of_the_wire_for_sound_and_stereo_no_more_than_before() {
    assert_eq!(surround_kbps(1), 0);
    assert_eq!(surround_kbps(2), 0);
    assert_eq!(surround_kbps(6), 2400);
    assert_eq!(surround_kbps(8), 3600);
}

#[test]
fn requested_idrs_cannot_exceed_one_per_rtt_even_when_last_delivered_advances() {
    let start = Instant::now();
    for rtt_ms in [10, 100, 300] {
        let rtt = Duration::from_millis(rtt_ms);
        let mut last = None;
        let mut produced = Vec::new();
        for ms in 0..1000 {
            let now = start + Duration::from_millis(ms);
            if super::recovery_due(last, now, Some(rtt)) {
                last = Some(now);
                produced.push(now);
            }
        }
        assert!(produced
            .windows(2)
            .all(|pair| pair[1] - pair[0] >= rtt.max(KEYFRAME_GAP)));
        assert!(produced.len() >= 3, "recovery must not stall");
    }
}

#[test]
fn reopen_refuses_an_old_picture_held_in_the_send_queue() {
    use crate::software::{Bgra, CpuCapture, SoftwarePipeline};
    use latch_codec::h264::Decoder;

    struct Solid(u8);
    impl CpuCapture for Solid {
        fn size(&self) -> (u32, u32) {
            (16, 16)
        }
        fn display(&self) -> DisplayRect {
            info().display
        }
        fn name(&self) -> String {
            "solid".into()
        }
        fn grab(&mut self, frame: &mut Bgra, _: Duration) -> anyhow::Result<bool> {
            *frame = Bgra {
                width: 16,
                height: 16,
                stride: 64,
                data: [self.0, self.0, self.0, 255].repeat(256),
            };
            Ok(true)
        }
    }
    struct ReopenVideo {
        pipeline: SoftwarePipeline<Solid>,
        session: Arc<Session>,
        replacement: bool,
        calls: usize,
    }
    impl VideoPipeline for ReopenVideo {
        fn info(&self) -> VideoInfo {
            self.pipeline.info()
        }
        fn next(&mut self, keyframe: bool, idle: Duration) -> anyhow::Result<EncodedVideo> {
            self.calls += 1;
            anyhow::ensure!(self.calls == 1, "capture lost");
            let frame = self.pipeline.next(keyframe, idle)?;
            if self.replacement {
                self.session.stop.store(true, Ordering::Release);
            }
            Ok(frame)
        }
        fn recover_from(&mut self, picture: u64) -> bool {
            self.pipeline.recover_from(picture)
        }
    }
    let server = Server::bind(0, Options::default()).unwrap();
    let receiver = UdpSocket::bind("127.0.0.1:0").unwrap();
    let mut session = session(receiver.local_addr().unwrap());
    Arc::get_mut(&mut session).unwrap().recovery.enabled = true;
    let open = |replacement| ReopenVideo {
        pipeline: SoftwarePipeline::new(Solid(if replacement { 210 } else { 30 }), &config())
            .unwrap(),
        session: session.clone(),
        replacement,
        calls: 0,
    };
    let (tx, rx) = mpsc::sync_channel(2);
    // Hold both frames in the actual sender queue until encode_video has
    // reconstructed the entire capture/pipeline and cleared its recovery map.
    encode_video(
        &server,
        &session,
        Box::new(open(false)),
        config(),
        |_| Ok(Box::new(open(true))),
        tx,
    );
    let old = rx.recv().unwrap();
    let new = rx.recv().unwrap();
    assert_eq!(
        old.0.picture, new.0.picture,
        "pipeline-local numbers restart"
    );
    assert_ne!(old.0.data, new.0.data);
    let mut decoder = Decoder::new();
    let old_pixels = decoder.decode(&old.0.data).unwrap().unwrap().clone();
    let (tx, rx) = mpsc::sync_channel(2);
    tx.send(old).unwrap();
    tx.send(new).unwrap();
    drop(tx);
    session.stop.store(false, Ordering::Release);
    paced_video(&server, &session, rx);
    assert_eq!(session.recovery.sent.lock().len(), 2);
    let generation = session.recovery.sent.lock()[1].1;
    // The replacement IDR was sent but lost. The viewer only holds frame 0.
    session.keyframe.store(false, Ordering::Release);
    *session.recovery.request.lock() = Some(wire::RecoveryRequest {
        last_delivered: 0,
        first_invalid: 1,
        last_invalid: 1,
        id: 1,
    });
    let mut replacement = open(true);
    replacement.pipeline.next(true, Duration::ZERO).unwrap();
    session
        .recovery
        .take(&server, &session, &mut replacement, generation);
    assert_eq!(session.recovery.refused.load(Ordering::Relaxed), 1);
    assert!(session.keyframe.load(Ordering::Acquire));
    assert_eq!(session.recovery.asked.lock().as_ref(), None);
    let idr = replacement.pipeline.next(true, Duration::ZERO).unwrap();
    assert!(idr.keyframe);
    assert_eq!(idr.recovery, None);
    let pixels = decoder.decode(&idr.data).unwrap().unwrap();
    assert_ne!(pixels, &old_pixels);
    let mut fresh = Decoder::new();
    assert_eq!(pixels, fresh.decode(&idr.data).unwrap().unwrap());
}

thread_local! {
    pub(super) static SEND_CALLS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    pub(super) static SEND_SAMPLES: std::cell::RefCell<Vec<(f64, bool)>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Codec-free real sockets: isolates sender scheduling from encoder/decoder speed.
/// Re-run in release, one case per process (PB_CAP kbps, PB_RTT ms, PB_SECONDS).
#[test]
#[ignore]
fn paced_socket_measurement() {
    use latch_proto::{
        sim::{Impairment, Link},
        video::{Reassembled, Reassembler},
    };
    let cap: u32 = std::env::var("PB_CAP")
        .unwrap_or("20000".into())
        .parse()
        .unwrap();
    let rtt: u64 = std::env::var("PB_RTT")
        .unwrap_or("20".into())
        .parse()
        .unwrap();
    let seconds: u64 = std::env::var("PB_SECONDS")
        .unwrap_or("3".into())
        .parse()
        .unwrap();
    let requested = if cap == 0 {
        std::env::var("PB_REQUEST")
            .unwrap_or("100000".into())
            .parse()
            .unwrap()
    } else {
        cap * 2
    };
    let receiver = UdpSocket::bind("127.0.0.1:0").unwrap();
    receiver.set_nonblocking(true).unwrap();
    let viewer = UdpSocket::bind("127.0.0.1:0").unwrap();
    viewer.set_nonblocking(true).unwrap();
    let viewer_addr = viewer.local_addr().unwrap();
    let session = session(receiver.local_addr().unwrap());
    session.transport.budget.store(requested, Ordering::Relaxed);
    // Start clean, just like the production controller.
    session.pace_kbps.store(
        if std::env::var_os("PB_START_PACED").is_some() {
            cap * 5 / 2
        } else {
            0
        },
        Ordering::Relaxed,
    );
    let server = Server::bind(0, Options::default()).unwrap();
    let (tx, rx) = mpsc::sync_channel(2);
    let start = Instant::now();
    let stop_at = start + Duration::from_secs(seconds);
    let timeline = Arc::new(Mutex::new(Vec::new()));
    let mut produced = 0;
    let mut delays = Vec::new();
    let mut good = 0usize;
    let mut lost = 0;
    let mut delivered = 0;
    std::thread::scope(|scope| {
        let samples = scope.spawn(|| {
            let cpu = thread_cpu();
            paced_video(&server, &session, rx);
            (
                SEND_SAMPLES.with(|s| s.take()),
                thread_cpu() - cpu,
                SEND_CALLS.with(|s| s.get()),
            )
        });
        let timeline_ref = &timeline;
        let producer_session = &session;
        let producer = scope.spawn(move || {
            let mut last_key = None;
            let mut index = 0;
            while Instant::now() < stop_at {
                while std::env::var_os("PB_DROP").is_none()
                    && producer_session.pace_kbps.load(Ordering::Relaxed) != 0
                    && producer_session.transport.pending.load(Ordering::Acquire) > PACED_BACKLOG
                {
                    std::thread::sleep(Duration::from_micros(200));
                }
                let at = Instant::now();
                let target = crate::congestion::video_budget(
                    producer_session.transport.budget.load(Ordering::Relaxed),
                    10,
                    0,
                    60,
                    false,
                    false,
                );
                let size = target as usize * 1000 / 8 / 60;
                let key = index == 0
                    || (recovery_due(last_key, at, Some(Duration::from_millis(rtt)))
                        && producer_session.keyframe.swap(false, Ordering::Relaxed));
                if key {
                    last_key = Some(at);
                }
                timeline_ref.lock().push(at);
                let mut frame = EncodedVideo {
                    data: vec![17; size * if index == 0 { 8 } else { 1 }],
                    keyframe: key,
                    captured: at,
                    picture: index,
                    generation: 0,
                    recovery: None,
                };
                frame.data[..8].copy_from_slice(&index.to_le_bytes());
                producer_session
                    .transport
                    .pending
                    .fetch_add(1, Ordering::Release);
                if tx.send((frame, at)).is_err() {
                    break;
                }
                index += 1;
                std::thread::sleep(
                    (start + Duration::from_secs_f64(index as f64 / 60.0))
                        .saturating_duration_since(Instant::now()),
                );
            }
            index
        });
        let mut link = Link::new(
            Impairment {
                delay: Duration::from_millis(rtt / 2),
                ..Impairment::NONE
            },
            17,
        );
        link.set_bandwidth(cap, Duration::from_millis(50));
        let mut assembly = Reassembler::new(7);
        let mut controller = crate::congestion::Controller::new(requested, start);
        let mut mark = start;
        let mut before = (0, 0, 0);
        let mut sent_before = 0;
        let mut received = 0usize;
        let mut buf = [0; 2048];
        while Instant::now() < stop_at + Duration::from_secs(2) {
            while let Ok((n, _)) = receiver.recv_from(&mut buf) {
                link.send(Instant::now(), &buf[..n]);
            }
            let now = Instant::now();
            for d in link.receive(now) {
                received += d.len();
                viewer.send_to(&d, viewer_addr).unwrap();
            }
            while let Ok((n, _)) = viewer.recv_from(&mut buf) {
                if let Ok(wire::Datagram::Video { shard, .. }) = wire::parse(&buf[..n]) {
                    assembly.push(&shard, now);
                }
            }
            while let Some(out) = assembly.poll(now) {
                match out {
                    Reassembled::Frame(f) => {
                        good += f.data.len();
                        delivered += 1;
                        if let Some(at) = timeline
                            .lock()
                            .get(u64::from_le_bytes(f.data[..8].try_into().unwrap()) as usize)
                        {
                            delays.push(at.elapsed().as_secs_f64() * 1000.0);
                        }
                    }
                    Reassembled::Lost { .. } => {
                        lost += 1;
                        session.keyframe.store(true, Ordering::Relaxed);
                    }
                }
            }
            if now.duration_since(mark) >= Duration::from_millis(50) {
                let elapsed = now.duration_since(mark).as_secs_f64();
                let counts = link.counts();
                let sent = session.bytes.load(Ordering::Relaxed);
                controller
                    .observe_sent(((sent - sent_before) as f64 * 8.0 / 1000.0 / elapsed) as u32);
                let loss =
                    (counts.1 - before.1) as f32 * 100.0 / (counts.0 - before.0).max(1) as f32;
                let report = control::Report {
                    rtt_ms: rtt as f32 + link.queue_delay(now).as_secs_f32() * 1000.0,
                    loss_pct: loss,
                    unrecovered_loss_pct: Some(loss),
                    received_kbps: (received as f64 * 8.0 / 1000.0 / elapsed) as u32,
                    window_ms: (elapsed * 1000.0) as u32,
                    ..Default::default()
                };
                controller.update(
                    now,
                    &report,
                    report.rtt_ms,
                    session.transport.queue_us.swap(0, Ordering::Relaxed) as f32 / 1000.0,
                );
                session
                    .transport
                    .budget
                    .store(controller.budget(), Ordering::Relaxed);
                session
                    .pace_kbps
                    .store(controller.pace_kbps(), Ordering::Relaxed);
                mark = now;
                before = counts;
                sent_before = sent;
                received = 0;
            }
            std::thread::sleep(Duration::from_micros(100));
        }
        produced = producer.join().unwrap();
        let (mut samples, cpu, calls) = samples.join().unwrap();
        samples.sort_by(|a, b| a.0.total_cmp(&b.0));
        delays.sort_by(f64::total_cmp);
        let q = |p: f64| {
            samples
                .get(((samples.len().saturating_sub(1)) as f64 * p) as usize)
                .map_or(0.0, |s| s.0)
        };
        let d = |p: f64| {
            delays
                .get(((delays.len().saturating_sub(1)) as f64 * p) as usize)
                .copied()
                .unwrap_or(0.0)
        };
        eprintln!("PB cap={cap} rtt={rtt} request={requested} good_mbps={:.3} queue_ms={:.3}/{:.3}/{:.3} frame_ms={:.3}/{:.3} produced={produced} delivered={delivered} lost={lost} keys={} syscalls_per_s={:.1} skipped={} sender_cpu_ms={:.3}", good as f64 * 8.0 / seconds as f64 / 1e6, q(0.5), q(0.95), q(1.0), d(0.5), d(0.95), samples.iter().filter(|s| s.1).count(), calls as f64 / seconds as f64, assembly.stats().frames_skipped, cpu * 1000.0);
    });
}

#[cfg(unix)]
fn thread_cpu() -> f64 {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: clock_gettime writes exactly one initialized timespec.
    assert_eq!(
        unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut time) },
        0
    );
    time.tv_sec as f64 + time.tv_nsec as f64 / 1e9
}
#[cfg(not(unix))]
fn thread_cpu() -> f64 {
    0.0
}

#[test]
fn idle_probes_share_sender_accounting_and_stop_under_pressure() {
    for pressure in [false, true] {
        let udp = UdpSocket::bind("127.0.0.1:0").unwrap();
        let session = session(udp.local_addr().unwrap());
        let server = Server::bind(0, Options::default()).unwrap();
        session
            .transport
            .probe_pressure
            .store(pressure, Ordering::Release);
        *session.transport.probe.lock() = Some((1, 6));
        let (tx, rx) = mpsc::sync_channel(2);
        drop(tx);
        paced_video(&server, &session, rx);
        assert_eq!(
            session.bytes.load(Ordering::Relaxed),
            if pressure {
                0
            } else {
                6 * latch_proto::MAX_DATAGRAM as u64 + 16
            }
        );
    }
}

#[test]
fn fallback_refits_geometry_and_prepares_the_final_mode() {
    let prepared_sizes = Arc::new(Mutex::new(Vec::new()));
    let seen = prepared_sizes.clone();
    let server = Server::bind(
        0,
        Options {
            prepare: Some(Box::new(move |r| {
                seen.lock().push((r.width, r.height, r.fps));
                Ok(None)
            })),
            ..Options::default()
        },
    )
    .unwrap();
    let req: StreamRequest = serde_json::from_value(serde_json::json!({
        "protocol": 1, "width": 16384, "height": 8704, "fps": 240,
        "bitrate_kbps": 100000, "codecs": ["h264-high", "h264-baseline"],
        "udp_port": 9, "audio": false
    }))
    .unwrap();
    let limits_for = |codec| video::Limits {
        max_side: if codec == Codec::H264Baseline {
            1920
        } else {
            8192
        },
        ..video::Limits::level(codec)
    };
    let limits = limits_for(Codec::H264Baseline);
    let mut expected = limits.fit(req.width, req.height, req.fps);
    if let Some(c) = crate::software::ceiling() {
        expected = c.fit(expected.0, expected.1, expected.2);
    }
    let mut calls = Vec::new();
    let result = server.prepare_using(
        req,
        "127.0.0.1:9".parse().unwrap(),
        || vec![Codec::H264High, Codec::H264Baseline],
        limits_for,
        |cfg| {
            calls.push(cfg.clone());
            Err(video::Unencodable("injected open failure".into()).into())
        },
    );
    assert!(result.is_err());
    assert_eq!(calls.len(), 2);
    let last = calls.last().unwrap();
    assert_eq!((last.width, last.height, last.fps), expected);
    assert_eq!(prepared_sizes.lock().last(), Some(&expected));
}

#[test]
fn portal_shutdown_ends_video_and_releases_input_without_reopening_capture() {
    struct Sink(Arc<AtomicUsize>);
    impl InputSink for Sink {
        fn apply(&mut self, _: &input::Event, _: DisplayRect) -> anyhow::Result<()> {
            Ok(())
        }
        fn release_all(&mut self) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }
    let server = Server::bind(0, Options::default()).unwrap();
    let session = session("127.0.0.1:9".parse().unwrap());
    let releases = Arc::new(AtomicUsize::new(0));
    session.input.lock().sink = Some(Box::new(Sink(releases.clone())));
    let pipeline = Box::new(Video {
        info: info(),
        held: Arc::new(AtomicBool::new(true)),
        next: Box::new(|_, _, _| {
            Err(anyhow::Error::from(video::CaptureEnded(
                "sharing was stopped on this machine".into(),
            ))
            .context("grab"))
        }),
    });
    send_video(&server, &session, pipeline, config(), |_| {
        panic!("a revoked portal session must not reopen")
    });
    let ended = session.ended.lock().clone().unwrap();
    assert_eq!(ended.code, end::CAPTURE_FAILED);
    assert!(ended.message.contains("sharing was stopped"));
    assert_eq!(releases.load(Ordering::Relaxed), 1);
    assert!(session.input.lock().sink.is_none());
}

#[test]
fn compatible_display_offer_probes_before_takeover_and_opens_after_cleanup() {
    let server = Server::bind(0, Options::default()).unwrap();
    let old = session("127.0.0.1:9".parse().unwrap());
    *server.current.lock() = Some(old.clone());
    let cleanup = {
        let old = old.clone();
        std::thread::spawn(move || {
            while !old.stopping() {
                std::thread::sleep(Duration::from_millis(1));
            }
            old.mark_finished();
        })
    };
    let req: StreamRequest = serde_json::from_value(serde_json::json!({
        "protocol": 1, "width": 64, "height": 64, "fps": 30,
        "bitrate_kbps": 1000, "codecs": ["av1", "h264-baseline"],
        "udp_port": 9, "audio": false, "display": "secondary-monitor"
    }))
    .unwrap();
    let mut opened = Vec::new();
    let result = server.prepare_using(
        req,
        "127.0.0.1:9".parse().unwrap(),
        || {
            assert!(!old.stopping(), "probe must preserve the current viewer");
            assert!(!*old.finished.lock());
            // The selected adapter supports H.264, even if the old one used AV1.
            vec![Codec::H264Baseline]
        },
        video::Limits::level,
        |config| {
            assert!(old.stopping());
            assert!(
                *old.finished.lock(),
                "capture must follow topology restoration"
            );
            opened.push(config.clone());
            Err(video::Unencodable("injected open failure".into()).into())
        },
    );
    cleanup.join().unwrap();
    assert!(result.is_err());
    assert_eq!(opened.len(), 1);
    assert_eq!(opened[0].codec, Codec::H264Baseline);
    assert_eq!(opened[0].display.as_deref(), Some("secondary-monitor"));
}

#[test]
fn an_unsupported_display_codec_preserves_the_current_capture() {
    let server = Server::bind(0, Options::default()).unwrap();
    let old = session("127.0.0.1:9".parse().unwrap());
    *server.current.lock() = Some(old.clone());
    let req: StreamRequest = serde_json::from_value(serde_json::json!({
        "protocol": 1, "width": 64, "height": 64, "fps": 30,
        "bitrate_kbps": 1000, "codecs": ["av1"],
        "udp_port": 9, "audio": false, "display": "h264-only-secondary"
    }))
    .unwrap();
    let result = server.prepare_using(
        req,
        "127.0.0.1:9".parse().unwrap(),
        || {
            assert!(!old.stopping());
            vec![Codec::H264Baseline]
        },
        video::Limits::level,
        |_| panic!("an unsupported codec must be refused before capture opens"),
    );
    assert!(matches!(result, Err(Refusal { status: 406, .. })));
    assert!(!old.stopping());
    assert!(old.ended.lock().is_none(), "no TAKEN_OVER should be sent");
    assert!(!*old.finished.lock());
    assert!(server
        .current
        .lock()
        .as_ref()
        .is_some_and(|s| Arc::ptr_eq(s, &old)));
    assert!(
        !*server.opening.busy.lock(),
        "refusal releases preparation lock"
    );
}

#[test]
fn an_offer_containing_only_future_codecs_preserves_the_current_capture() {
    let server = Server::bind(0, Options::default()).unwrap();
    let old = session("127.0.0.1:9".parse().unwrap());
    *server.current.lock() = Some(old.clone());
    let req: StreamRequest = serde_json::from_value(serde_json::json!({
        "protocol": 1, "width": 64, "height": 64, "fps": 30,
        "bitrate_kbps": 1000, "codecs": ["future-codec"],
        "udp_port": 9, "audio": false
    }))
    .unwrap();
    let result = server.prepare_using(
        req,
        "127.0.0.1:9".parse().unwrap(),
        || panic!("an empty known-codec offer must be refused before probing"),
        video::Limits::level,
        |_| panic!("an empty known-codec offer must be refused before capture opens"),
    );
    assert!(matches!(result, Err(Refusal { status: 406, .. })));
    assert!(!old.stopping());
}

#[test]
fn startup_refusals_preserve_transient_statuses_and_terminal_permission_denial() {
    for (status, retry) in [
        (500, true),
        (503, true),
        (408, true),
        (429, true),
        (403, false),
        (406, false),
    ] {
        assert_eq!(
            Refusal::new(status, "refused").into_end().code == end::TRANSPORT_FAILED,
            retry
        );
    }
    let server = Server::bind(0, Options::default()).unwrap();
    for denied in [false, true] {
        let req: StreamRequest = serde_json::from_value(serde_json::json!({
            "protocol": latch_proto::PROTOCOL, "width": 64, "height": 64, "fps": 30,
            "bitrate_kbps": 1000, "codecs": ["h264-baseline"], "udp_port": 9, "audio": false
        }))
        .unwrap();
        let result = server.prepare_using(
            req,
            "127.0.0.1:9".parse().unwrap(),
            || vec![Codec::H264Baseline],
            video::Limits::level,
            |_| {
                if denied {
                    Err(
                        anyhow::Error::from(video::CaptureEnded("approval declined".into()))
                            .context("portal"),
                    )
                } else {
                    anyhow::bail!("temporary native capture failure")
                }
            },
        );
        let refusal = result.err().expect("injected startup failure");
        assert_eq!(refusal.status, if denied { 403 } else { 500 });
    }
}

#[test]
fn stop_waits_for_a_preparation_that_is_being_dropped() {
    let server = Server::bind(0, Options::default()).unwrap();
    let reservation = server.opening.lock(Duration::ZERO).unwrap();
    let (tx, result) = mpsc::channel();
    let stopping = {
        let server = server.clone();
        std::thread::spawn(move || tx.send(server.stop(end::HOST_STOPPING, "stop")).unwrap())
    };
    assert!(matches!(
        result.recv_timeout(Duration::from_millis(50)),
        Err(mpsc::RecvTimeoutError::Timeout)
    ));
    drop(reservation);
    assert!(result.recv_timeout(Duration::from_secs(1)).unwrap());
    stopping.join().unwrap();
}
