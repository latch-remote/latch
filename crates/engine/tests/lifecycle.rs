//! Session boundaries through real TCP connections, without a desktop.
#[path = "support/listeners.rs"]
mod listeners;

use latch_core::http;
use latch_engine::server::{Options, Server};
use latch_proto::control::{self, Accept, Codec, End, Message, StreamRequest};
use latch_stream::{Event, FrameSlot, Session, Settings};
use std::io::Write;
use std::net::{IpAddr, Ipv4Addr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

fn request(name: &str) -> StreamRequest {
    StreamRequest {
        key_positions: false,
        startup_timeout_secs: 0,
        relay: false,
        prefer_color: false,
        color: Default::default(),
        protocol: latch_proto::PROTOCOL,
        width: 64,
        height: 64,
        fps: 60,
        bitrate_kbps: 1000,
        codecs: vec![Codec::H264Baseline],
        udp_port: 9,
        audio: false,
        name: name.into(),
        cursor: false,
        gamepads: false,
        reference_invalidation: false,
        audio_channels: 0,
        mic: false,
        probe_trains: false,
        display: None,
    }
}

#[test]
fn preparation_stays_reserved_until_run_or_drop_and_takeover_waits_for_slow_undo() {
    std::env::set_var("LATCH_TEST_PATTERN", "still");
    let active = Arc::new(AtomicBool::new(false));
    let server = Server::bind(
        0,
        Options {
            prepare: Some(Box::new({
                let active = active.clone();
                move |req| {
                    assert!(
                        !active.swap(true, Ordering::AcqRel),
                        "prep overlapped the previous undo"
                    );
                    let active = active.clone();
                    let slow = req.name == "slow";
                    Ok(Some(Box::new(move || {
                        if slow {
                            std::thread::sleep(Duration::from_secs(6));
                        }
                        active.store(false, Ordering::Release);
                    })))
                }
            })),
            ..listeners::options()
        },
    )
    .unwrap();
    let peer = "127.0.0.1:12345".parse().unwrap();
    let first = server.prepare(request("unused"), peer).unwrap();
    let (tx, rx) = mpsc::channel();
    let worker = {
        let server = server.clone();
        std::thread::spawn(move || tx.send(server.prepare(request("second"), peer)).unwrap())
    };
    assert!(matches!(
        rx.recv_timeout(Duration::from_millis(100)),
        Err(mpsc::RecvTimeoutError::Timeout)
    ));
    drop(first);
    drop(rx.recv_timeout(Duration::from_secs(3)).unwrap().unwrap());
    worker.join().unwrap();
    assert!(!active.load(Ordering::Acquire));

    let first = server.prepare(request("slow"), peer).unwrap();
    let (tx, rx) = mpsc::channel();
    let next = {
        let server = server.clone();
        std::thread::spawn(move || tx.send(server.prepare(request("takeover"), peer)).unwrap())
    };
    assert!(matches!(
        rx.recv_timeout(Duration::from_millis(100)),
        Err(mpsc::RecvTimeoutError::Timeout)
    ));
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let mut old_viewer = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    old_viewer
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let (tcp, _) = listener.accept().unwrap();
    let running = {
        let server = server.clone();
        std::thread::spawn(move || server.run(first, tcp))
    };
    assert!(matches!(
        control::read_message(&mut old_viewer).unwrap(),
        Message::Accept(_)
    ));
    loop {
        if let Message::End(e) = control::read_message(&mut old_viewer).unwrap() {
            assert_eq!(e.code, control::end::TAKEN_OVER);
            break;
        }
    }
    // The old viewer ends promptly, but its six-second undo must complete
    // before the next prep. The former five-second wait returned HTTP 503.
    let start = Instant::now();
    let prepared = rx.recv_timeout(Duration::from_secs(10)).unwrap().unwrap();
    assert!(start.elapsed() > Duration::from_secs(5));
    drop(prepared);
    next.join().unwrap();
    running.join().unwrap();
    assert!(!active.load(Ordering::Acquire));
}

#[test]
fn post_accept_end_close_and_missing_media_are_reported_as_failures() {
    for mode in ["end", "close", "no media"] {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        // Serve one request only; the worker is joined below.
        let host = std::thread::spawn(move || {
            let (mut tcp, _) = listener.accept().unwrap();
            assert!(http::read_request(&mut tcp).unwrap().is_some());
            write!(
                tcp,
                "HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: {}\r\n\r\n",
                control::UPGRADE
            )
            .unwrap();
            control::write_message(
                &mut tcp,
                &Message::Accept(Accept {
                    key_positions: false,
                    relay: false,
                    color: Default::default(),
                    session: 123,
                    token: control::token_hex(&[7; 16]),
                    codec: Codec::H264Baseline,
                    width: 64,
                    height: 64,
                    fps: 60,
                    bitrate_kbps: 1000,
                    encoder: "fake host without UDP".into(),
                    audio: None,
                    audio_problem: String::new(),
                    cursor: false,
                    gamepads: 0,
                    gamepad_kinds: Vec::new(),
                    gamepad_problem: String::new(),
                    reference_invalidation: false,
                    mic: None,
                    mic_problem: String::new(),
                }),
            )
            .unwrap();
            match mode {
                "end" => control::write_message(
                    &mut tcp,
                    &Message::End(End {
                        code: control::end::CAPTURE_FAILED,
                        message: "capture failed before media".into(),
                    }),
                )
                .unwrap(),
                "no media" => {
                    tcp.set_read_timeout(Some(Duration::from_secs(12))).unwrap();
                    while control::read_message(&mut tcp).is_ok() {
                        if control::write_message(&mut tcp, &Message::Keepalive).is_err() {
                            break;
                        }
                    }
                }
                _ => {}
            }
        });
        let (tx, rx) = mpsc::channel();
        let session = Session::start(
            latch_stream::Server {
                address: IpAddr::V4(Ipv4Addr::LOCALHOST),
                port,
            },
            Settings {
                prefer_color: false,
                color: Default::default(),
                width: 64,
                height: 64,
                fps: 60,
                bitrate_kbps: 1000,
                hevc: false,
                av1: false,
                audio: false,
                name: "failure test".into(),
                cursor: false,
                gamepads: false,
                mic: false,
                display: None,
            },
            Arc::new(FrameSlot::default()),
            tx,
            || {},
        );
        let deadline = Instant::now() + Duration::from_secs(12);
        loop {
            match rx
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("no failure event after Accept")
            {
                Event::Failed { stage, message } => {
                    match mode {
                        "end" => assert!(message.contains("capture failed before media")),
                        "close" => assert!(message.contains("closed the connection")),
                        _ => {
                            assert_eq!(stage, "connecting");
                            assert!(message.contains("no picture arrived"));
                            assert!(message.contains("may be blocked"));
                        }
                    }
                    break;
                }
                Event::Connected | Event::Terminated { .. } => {
                    panic!("incorrect outcome for {mode}")
                }
                _ => {}
            }
        }
        host.join().unwrap();
        session.stop();
    }
}

/// Keep control traffic healthy while selectively breaking the media or input
/// path. The host finishes only when the viewer closes its control socket.
fn impaired_host(mode: &'static str) -> (u16, std::thread::JoinHandle<()>) {
    let (udp, listener) = listeners::proxy();
    let port = listener.local_addr().unwrap().port();

    let worker = std::thread::spawn(move || {
        let (mut tcp, peer) = listener.accept().unwrap();
        tcp.set_read_timeout(Some(Duration::from_secs(15))).unwrap();
        let request: StreamRequest = http::read_request(&mut tcp)
            .unwrap()
            .unwrap()
            .json()
            .unwrap();
        write!(
            tcp,
            "HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: {}\r\n\r\n",
            control::UPGRADE
        )
        .unwrap();
        control::write_message(
            &mut tcp,
            &Message::Accept(Accept {
                key_positions: false,
                relay: false,
                color: Default::default(),
                session: 123,
                token: control::token_hex(&[7; 16]),
                codec: Codec::H264Baseline,
                width: 64,
                height: 64,
                fps: 60,
                bitrate_kbps: 1000,
                encoder: "impaired test host".into(),
                audio: None,
                audio_problem: String::new(),
                cursor: false,
                gamepads: 0,
                gamepad_kinds: Vec::new(),
                gamepad_problem: String::new(),
                reference_invalidation: false,
                mic: None,
                mic_problem: String::new(),
            }),
        )
        .unwrap();
        let viewer = std::net::SocketAddr::new(peer.ip(), request.udp_port);
        let mut encoder = latch_codec::h264::Encoder::new(latch_codec::h264::EncoderConfig {
            width: 64,
            height: 64,
            fps: 60,
            bitrate_kbps: 1000,
            threads: 1,
        })
        .unwrap();
        let mut picture = Vec::new();
        encoder
            .encode(&latch_codec::Yuv420::new(64, 64), true, &mut picture)
            .unwrap();
        let mut packetizer = latch_proto::video::Packetizer::new(123);
        let mut send_frame = |data: &[u8], partial: bool| {
            let mut sent = false;
            packetizer
                .packetize(
                    &latch_proto::video::OutFrame {
                        data,
                        keyframe: true,
                        host_delay: 0,
                    },
                    |dg| {
                        if !partial || !sent {
                            udp.send_to(dg, viewer).unwrap();
                            sent = true;
                        }
                    },
                )
                .unwrap();
        };
        if mode == "partial" {
            send_frame(&vec![0x55; 5000], true);
        } else if mode == "startup decode starvation" {
            send_frame(&[0x55; 100], false);
        } else {
            send_frame(&picture, false);
        }
        let mut next_frame = Instant::now() + Duration::from_millis(100);
        while control::read_message(&mut tcp).is_ok() {
            if control::write_message(&mut tcp, &Message::Keepalive).is_err() {
                break;
            }
            if Instant::now() >= next_frame {
                next_frame = Instant::now() + Duration::from_millis(100);
                match mode {
                    "ack blackhole" | "backlog" => send_frame(&picture, false),
                    "decode starvation" | "startup decode starvation" => {
                        send_frame(&[0x55; 100], false)
                    }
                    _ => {}
                }
                if mode == "ack blackhole" {
                    // Duplicate acknowledgements are traffic, but no progress.
                    let mut ack = Vec::new();
                    latch_proto::wire::write_input_ack(&mut ack, 123, 0);
                    udp.send_to(&ack, viewer).unwrap();
                }
            }
        }
    });
    (port, worker)
}

fn impaired_session(port: u16) -> (Session, Arc<FrameSlot>, mpsc::Receiver<Event>) {
    let frames = Arc::new(FrameSlot::default());
    let (tx, rx) = mpsc::channel();
    let session = Session::start(
        latch_stream::Server {
            address: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port,
        },
        Settings {
            prefer_color: false,
            color: Default::default(),
            width: 64,
            height: 64,
            fps: 60,
            bitrate_kbps: 1000,
            hevc: false,
            av1: false,
            audio: false,
            name: "impaired viewer".into(),
            cursor: false,
            gamepads: false,
            mic: false,
            display: None,
        },
        frames.clone(),
        tx,
        || {},
    );
    (session, frames, rx)
}

fn next_boundary(rx: &mpsc::Receiver<Event>) -> Event {
    let deadline = Instant::now() + Duration::from_secs(12);
    loop {
        let event = rx
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .expect("session boundary deadline");
        if matches!(
            event,
            Event::Connected | Event::Failed { .. } | Event::Terminated { .. }
        ) {
            return event;
        }
    }
}

#[test]
fn a_partial_frame_and_tcp_keepalives_cannot_disable_startup_timeout() {
    let (port, host) = impaired_host("partial");
    let (session, frames, rx) = impaired_session(port);
    let start = Instant::now();
    assert!(
        matches!(next_boundary(&rx), Event::Failed { stage, message }
        if stage == "connecting" && message.contains("no picture arrived"))
    );
    assert!(start.elapsed() < Duration::from_secs(11));
    assert_eq!(frames.seq(), 0);
    assert!(!session.connected());
    host.join().unwrap();
    session.stop();
}

#[test]
fn complete_frames_without_a_picture_report_decode_starvation() {
    let (port, host) = impaired_host("startup decode starvation");
    let (session, frames, rx) = impaired_session(port);
    assert!(
        matches!(next_boundary(&rx), Event::Failed { stage, message }
        if stage == "decoding" && message.contains("complete video frames"))
    );
    assert_eq!(frames.seq(), 0);
    assert!(!session.connected());
    host.join().unwrap();
    session.stop();
}

#[test]
fn established_media_stalls_recover_despite_tcp_keepalives() {
    for mode in ["capture idle", "decode starvation"] {
        let (port, host) = impaired_host(mode);
        let (session, frames, rx) = impaired_session(port);
        assert_eq!(next_boundary(&rx), Event::Connected);
        assert!(frames.seq() > 0, "connected requires a published picture");
        let start = Instant::now();
        let Event::Terminated { code, message } = next_boundary(&rx) else {
            panic!("media stall must end recoverably");
        };
        assert_eq!(code, -1);
        assert!(
            message.contains(if mode == "capture idle" {
                "including idle frames"
            } else {
                "decoding stopped"
            }),
            "{message}"
        );
        assert!(start.elapsed() < Duration::from_secs(11));
        host.join().unwrap();
        session.stop();
    }
}

#[test]
fn input_ack_blackholes_and_backlogs_close_the_session() {
    for mode in ["ack blackhole", "backlog"] {
        let (port, host) = impaired_host(mode);
        let (session, _, rx) = impaired_session(port);
        assert_eq!(next_boundary(&rx), Event::Connected);
        let input = session.input();
        let start = Instant::now();
        let count = if mode == "backlog" { 5000 } else { 1 };
        for _ in 0..count {
            input.key(65, true, 0);
        }
        input.key(65, false, 0);
        let Event::Terminated { code, message } = next_boundary(&rx) else {
            panic!("input failure must end recoverably");
        };
        assert_eq!(code, -1);
        assert!(message.contains("acknowledging input"));
        assert!(start.elapsed() < Duration::from_secs(if mode == "backlog" { 3 } else { 8 }));
        host.join().unwrap();
        session.stop();
    }
}
