use latch_proto::video::InFrame;
use latch_relay::{
    create_grant,
    daemon::{Daemon, DaemonConfig},
    Cancel, ClientConfig, Endpoint, Event, SessionKeys, Transport,
};
use rustls::pki_types::{pem::PemObject, CertificateDer, PrivateKeyDer};
use std::{
    collections::VecDeque,
    io::{self, Read, Write},
    net::{TcpListener, TcpStream},
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc, Arc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

static NEXT: AtomicU64 = AtomicU64::new(0);
struct Pilot {
    config: ClientConfig,
    cancel: Cancel,
    worker: Option<JoinHandle<io::Result<()>>>,
    quota: PathBuf,
}
fn test_config(port: u16) -> ClientConfig {
    let ca = CertificateDer::from_pem_slice(include_bytes!("certs/ca.pem")).unwrap();
    let mut roots = rustls::RootCertStore::empty();
    roots.add(ca).unwrap();
    ClientConfig::with_roots(Endpoint::new("localhost", port).unwrap(), roots).unwrap()
}
impl Pilot {
    fn start() -> Self {
        let cert = CertificateDer::from_pem_slice(include_bytes!("certs/server.pem")).unwrap();
        let key = PrivateKeyDer::from_pem_slice(include_bytes!("certs/server-key.pem")).unwrap();
        let quota = std::env::temp_dir().join(format!(
            "latch-relay-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let mut daemon_config = DaemonConfig::new(
            "127.0.0.1:0".parse().unwrap(),
            vec![cert],
            key,
            quota.clone(),
            32 * 1024 * 1024,
        )
        .unwrap();
        daemon_config.initialize_quota = true;
        let daemon = Daemon::bind(daemon_config).unwrap();
        let port = daemon.local_addr().unwrap().port();
        let config = test_config(port);
        let cancel = Cancel::new();
        let worker_cancel = cancel.clone();
        let worker = thread::spawn(move || daemon.run(&worker_cancel));
        Self {
            config,
            cancel,
            worker: Some(worker),
            quota,
        }
    }
    fn pair(&self) -> (Transport, Transport) {
        let grant = create_grant(&self.config, &Cancel::new()).unwrap();
        let keys = SessionKeys::generate().unwrap();
        let host = Transport::connect(self.config.clone(), grant.host, keys.clone()).unwrap();
        let viewer = Transport::connect(self.config.clone(), grant.viewer, keys).unwrap();
        ready(&host);
        ready(&viewer);
        (host, viewer)
    }
}
impl Drop for Pilot {
    fn drop(&mut self) {
        self.cancel.cancel();
        self.worker.take().unwrap().join().unwrap().unwrap();
        std::fs::remove_file(&self.quota).unwrap();
    }
}
fn ready(transport: &Transport) {
    let event = transport.recv_timeout(Duration::from_secs(5));
    assert!(
        matches!(event, Some(Event::Ready)),
        "expected Ready, got {event:?}"
    );
}
fn closed(transport: &Transport) -> io::ErrorKind {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match transport.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Some(Event::Closed { kind, .. }) => return kind,
            Some(_) => (),
            None => panic!("transport did not close"),
        }
    }
}
#[test]
fn real_tls_two_lanes_complete_frame_and_authenticated_control() {
    let pilot = Pilot::start();
    let (host, viewer) = pilot.pair();
    viewer
        .try_send_datagram(b"input/audio/mic/ping/recovery")
        .unwrap();
    host.try_send_control(b"serialized existing control")
        .unwrap();
    let frame = InFrame {
        frame: 42,
        data: (0..500_000).map(|i| (i % 251) as u8).collect(),
        keyframe: true,
        recovery: true,
        host_delay: 17,
        assembly: Duration::ZERO,
    };
    host.try_send_frame(&frame).unwrap();
    match host.recv_timeout(Duration::from_secs(3)).unwrap() {
        Event::Datagram(b) => assert_eq!(b, b"input/audio/mic/ping/recovery"),
        e => panic!("{e:?}"),
    }
    let mut saw_frame = false;
    let mut saw_control = false;
    for _ in 0..2 {
        match viewer.recv_timeout(Duration::from_secs(3)).unwrap() {
            Event::Control(b) => {
                assert_eq!(b, b"serialized existing control");
                saw_control = true;
            }
            Event::Frame(f) => {
                assert_eq!(f.frame, 42);
                assert_eq!(f.data, frame.data);
                assert!(f.keyframe && f.recovery);
                assert_eq!(f.host_delay, 17);
                saw_frame = true;
            }
            e => panic!("{e:?}"),
        }
    }
    assert!(saw_frame && saw_control);
    host.shutdown();
    assert_ne!(closed(&viewer), io::ErrorKind::WouldBlock);
}
#[test]
fn wrong_keys_never_reach_ready() {
    let pilot = Pilot::start();
    let grant = create_grant(&pilot.config, &Cancel::new()).unwrap();
    let host = Transport::connect(
        pilot.config.clone(),
        grant.host,
        SessionKeys::generate().unwrap(),
    )
    .unwrap();
    let viewer = Transport::connect(
        pilot.config.clone(),
        grant.viewer,
        SessionKeys::generate().unwrap(),
    )
    .unwrap();
    assert_ne!(closed(&host), io::ErrorKind::WouldBlock);
    assert_ne!(closed(&viewer), io::ErrorKind::WouldBlock);
}
#[test]
fn replayed_attachment_cannot_rejoin() {
    let pilot = Pilot::start();
    let grant = create_grant(&pilot.config, &Cancel::new()).unwrap();
    let keys = SessionKeys::generate().unwrap();
    let host = Transport::connect(pilot.config.clone(), grant.host.clone(), keys.clone()).unwrap();
    let viewer = Transport::connect(pilot.config.clone(), grant.viewer, keys.clone()).unwrap();
    ready(&host);
    ready(&viewer);
    let replay = Transport::connect(pilot.config.clone(), grant.host, keys).unwrap();
    assert_ne!(closed(&replay), io::ErrorKind::WouldBlock);
    host.try_send_datagram(b"still connected").unwrap();
    assert!(matches!(
        viewer.recv_timeout(Duration::from_secs(2)),
        Some(Event::Datagram(_))
    ));
}
#[test]
fn public_roots_reject_test_certificate() {
    let pilot = Pilot::start();
    let config = ClientConfig::production(pilot.config.endpoint.clone()).unwrap();
    assert_eq!(
        create_grant(&config, &Cancel::new()).unwrap_err().kind(),
        io::ErrorKind::PermissionDenied
    );
    let config = ClientConfig::with_roots(
        pilot.config.endpoint.clone(),
        rustls::RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
        },
    )
    .unwrap();
    assert_eq!(
        create_grant(&config, &Cancel::new()).unwrap_err().kind(),
        io::ErrorKind::InvalidData
    );
    assert!(Endpoint::new("127.0.0.1", 443).is_err());
    assert!(Endpoint::new("https://relay.example", 443).is_err());
    assert!(Endpoint::new("relay.example", 0).is_err());
}
#[test]
fn cancelling_silent_tls_handshake_and_drop_are_bounded() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let config = test_config(listener.local_addr().unwrap().port());
    let (stop, rx) = mpsc::channel();
    let (accepted, acceptance) = mpsc::channel();
    let peer = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(10);
        let socket = loop {
            match listener.accept() {
                Ok((socket, _)) => break socket,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    if rx.try_recv().is_ok() || Instant::now() >= deadline {
                        return;
                    }
                    thread::sleep(Duration::from_millis(2));
                }
                Err(e) => panic!("{e}"),
            }
        };
        let _ = accepted.send(());
        let _ = rx.recv_timeout(Duration::from_secs(5));
        drop(socket);
    });
    let cancel = Cancel::new();
    let worker_cancel = cancel.clone();
    let worker = thread::spawn(move || create_grant(&config, &worker_cancel));
    // Cancellation must occur during the silent TLS handshake. In particular,
    // localhost's initial IPv6 refusal must finish before timing cancellation.
    let connected = acceptance.recv_timeout(Duration::from_secs(10)).is_ok();
    let started = Instant::now();
    cancel.cancel();
    let result = worker.join().unwrap();
    let elapsed = started.elapsed();
    let _ = stop.send(());
    peer.join().unwrap();
    assert!(
        connected,
        "client never reached the silent TLS peer: {result:?}"
    );
    assert_eq!(result.unwrap_err().kind(), io::ErrorKind::Interrupted);
    assert!(elapsed < Duration::from_millis(300));

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let config = test_config(listener.local_addr().unwrap().port());
    let grant_pilot = Pilot::start();
    let grant = create_grant(&grant_pilot.config, &Cancel::new()).unwrap();
    let transport =
        Transport::connect(config, grant.host, SessionKeys::generate().unwrap()).unwrap();
    thread::sleep(Duration::from_millis(30));
    let started = Instant::now();
    drop(transport);
    assert!(started.elapsed() < Duration::from_millis(300));
}
#[test]
fn pre_cancelled_dns_and_connect_stop_without_waiting() {
    let config = test_config(443);
    let cancel = Cancel::new();
    cancel.cancel();
    let started = Instant::now();
    assert_eq!(
        create_grant(&config, &cancel).unwrap_err().kind(),
        io::ErrorKind::Interrupted
    );
    assert!(started.elapsed() < Duration::from_millis(100));
}

#[test]
fn exhausted_receive_queue_closes_attempt_and_propagates_failure() {
    let pilot = Pilot::start();
    let grant = create_grant(&pilot.config, &Cancel::new()).unwrap();
    let keys = SessionKeys::generate().unwrap();
    let mut viewer_config = pilot.config.clone();
    viewer_config.limits.max_frame_bytes = 1000;
    viewer_config.limits.video_queue_bytes = 1000;
    viewer_config.limits.event_queue_bytes = 1400;
    let host = Transport::connect(pilot.config.clone(), grant.host, keys.clone()).unwrap();
    let viewer = Transport::connect(viewer_config, grant.viewer, keys).unwrap();
    ready(&host);
    ready(&viewer);
    for _ in 0..4 {
        host.try_send_datagram(&[7; 1200]).unwrap();
    }
    // Leave the bounded receiver full until its worker reports terminal failure.
    thread::sleep(Duration::from_millis(100));
    assert_eq!(closed(&viewer), io::ErrorKind::WouldBlock);
    closed(&host);
    assert_eq!(
        host.try_send_datagram(b"release input").unwrap_err().kind(),
        io::ErrorKind::BrokenPipe
    );
}

#[test]
fn repeated_frames_advance_the_bounded_authenticated_ack_window() {
    let pilot = Pilot::start();
    let (host, viewer) = pilot.pair();
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut sent = 0;
    let mut received = 0;
    while received < 24 {
        assert!(Instant::now() < deadline);
        if sent < 24 {
            let frame = InFrame {
                frame: sent,
                data: vec![sent as u8; 8192],
                keyframe: sent == 0,
                recovery: false,
                host_delay: 3,
                assembly: Duration::ZERO,
            };
            match host.try_send_frame(&frame) {
                Ok(()) => sent += 1,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => (),
                Err(e) => panic!("{e}"),
            }
        }
        if let Some(event) = viewer.recv_timeout(Duration::from_millis(2)) {
            match event {
                Event::Frame(f) => {
                    assert_eq!(f.frame, received);
                    assert_eq!(f.data, vec![received as u8; 8192]);
                    received += 1;
                }
                e => panic!("{e:?}"),
            }
        }
    }
}

/// Holds encrypted return traffic after setup, as a long ACK path would, while
/// allowing the sender's video to reach the real TLS daemon and viewer.
struct ReturnGate {
    port: u16,
    hold: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}
impl ReturnGate {
    fn start(destination: u16) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let hold = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let worker_hold = hold.clone();
        let worker_stop = stop.clone();
        let worker = thread::spawn(move || {
            let mut peers = Vec::new();
            while !worker_stop.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((client, _)) => {
                        let server =
                            TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, destination))
                                .unwrap();
                        let hold = worker_hold.clone();
                        let stop = worker_stop.clone();
                        peers.push(thread::spawn(move || {
                            client.set_nonblocking(true).unwrap();
                            server.set_nonblocking(true).unwrap();
                            client.set_nodelay(true).unwrap();
                            server.set_nodelay(true).unwrap();
                            let mut to_server = VecDeque::new();
                            let mut to_client = VecDeque::new();
                            while !stop.load(Ordering::Acquire) {
                                if forward(&client, &server, &mut to_server, false).is_err()
                                    || forward(
                                        &server,
                                        &client,
                                        &mut to_client,
                                        hold.load(Ordering::Acquire),
                                    )
                                    .is_err()
                                {
                                    break;
                                }
                                thread::sleep(Duration::from_millis(1));
                            }
                        }));
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(1));
                    }
                    Err(e) => panic!("{e}"),
                }
            }
            for peer in peers {
                peer.join().unwrap();
            }
        });
        Self {
            port,
            hold,
            stop,
            worker: Some(worker),
        }
    }
}
impl Drop for ReturnGate {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.worker.take().unwrap().join().unwrap();
    }
}
fn forward(
    mut source: &TcpStream,
    mut destination: &TcpStream,
    pending: &mut VecDeque<u8>,
    hold: bool,
) -> io::Result<()> {
    if pending.len() < 256 * 1024 {
        let mut bytes = [0; 16 * 1024];
        match source.read(&mut bytes) {
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => pending.extend(&bytes[..n]),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => (),
            Err(e) => return Err(e),
        }
    }
    if !hold && !pending.is_empty() {
        match destination.write(pending.as_slices().0) {
            Ok(n) => {
                pending.drain(..n);
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => (),
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

#[test]
fn delayed_authenticated_acks_adapt_without_relaxing_a_permanent_blackhole() {
    let pilot = Pilot::start();
    let gate = ReturnGate::start(pilot.config.endpoint.port());
    let grant = create_grant(&pilot.config, &Cancel::new()).unwrap();
    let keys = SessionKeys::generate().unwrap();
    let mut config = test_config(gate.port);
    // Leave room for hosted-runner/proxy scheduling around sub-floor ACKs.
    // The estimator unit test covers the production 750 ms floor; the final
    // held ACK below still exceeds this fixture's floor.
    config.limits.stall_timeout = Duration::from_secs(2);
    let host = Transport::connect(config, grant.host, keys.clone()).unwrap();
    let viewer = Transport::connect(pilot.config.clone(), grant.viewer, keys).unwrap();
    ready(&host);
    ready(&viewer);
    let send = |number| {
        host.try_send_frame(&InFrame {
            frame: number,
            data: vec![7; 1024],
            keyframe: true,
            recovery: false,
            host_delay: 0,
            assembly: Duration::ZERO,
        })
        .unwrap();
        let queued = Instant::now();
        assert!(matches!(
            viewer.recv_timeout(Duration::from_secs(2)),
            Some(Event::Frame(frame)) if frame.frame == number
        ));
        if number == 12 {
            // Model scheduling work after receipt. This is already part of
            // the ACK's age, not extra time to add to the requested delay.
            thread::sleep(Duration::from_millis(750));
        }
        queued
    };
    // Warm the estimator until its initial variance has decayed, then observe
    // sub-deadline jitter before testing an ACK beyond the configured floor.
    let delays = std::iter::repeat_n(20, 12).chain([1400, 1400, 1400, 2100]);
    for (number, delay) in delays.enumerate() {
        gate.hold.store(true, Ordering::Release);
        let queued = send(number as u32);
        thread::sleep(Duration::from_millis(delay).saturating_sub(queued.elapsed()));
        let event = host.recv_timeout(Duration::ZERO);
        assert!(
            event.is_none(),
            "return-path delay closed video: frame={number}, hold={delay}ms, event={event:?}"
        );
        gate.hold.store(false, Ordering::Release);
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            if let Some(elapsed) = host.take_frame_ack_elapsed() {
                eprintln!("frame={number} hold={delay}ms ack={elapsed:?}");
                assert!(elapsed >= Duration::from_millis(delay));
                break;
            }
            let event = host.recv_timeout(Duration::ZERO);
            assert!(
                event.is_none(),
                "after return-path release: frame={number}, hold={delay}ms, event={event:?}"
            );
            assert!(
                Instant::now() < deadline,
                "authenticated ACK never arrived: frame={number}, hold={delay}ms"
            );
            thread::sleep(Duration::from_millis(2));
        }
    }
    gate.hold.store(true, Ordering::Release);
    send(16);
    assert_eq!(closed(&host), io::ErrorKind::TimedOut);
}

#[test]
fn delayed_ack_path_does_not_limit_video_to_eight_frames_per_round_trip() {
    let pilot = Pilot::start();
    let gate = ReturnGate::start(pilot.config.endpoint.port());
    let grant = create_grant(&pilot.config, &Cancel::new()).unwrap();
    let keys = SessionKeys::generate().unwrap();
    let mut config = test_config(gate.port);
    config.limits.stall_timeout = Duration::from_secs(2);
    let host = Transport::connect(config, grant.host, keys.clone()).unwrap();
    let viewer = Transport::connect(pilot.config.clone(), grant.viewer, keys).unwrap();
    ready(&host);
    ready(&viewer);
    gate.hold.store(true, Ordering::Release);
    let mut sent = 0;
    let mut received = 0;
    let deadline = Instant::now() + Duration::from_secs(1);
    while received < 24 && Instant::now() < deadline {
        if sent < 24 {
            let frame = InFrame {
                frame: sent,
                data: vec![sent as u8; 1024],
                keyframe: sent == 0,
                recovery: false,
                host_delay: 0,
                assembly: Duration::ZERO,
            };
            match host.try_send_frame(&frame) {
                Ok(()) => sent += 1,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => (),
                Err(e) => panic!("{e}"),
            }
        }
        if let Some(event) = viewer.recv_timeout(Duration::from_millis(2)) {
            match event {
                Event::Frame(frame) => {
                    assert_eq!(frame.frame, received);
                    assert_eq!(frame.data, vec![received as u8; 1024]);
                    received += 1;
                }
                event => panic!("{event:?}"),
            }
        }
    }
    assert_eq!(host.take_frame_ack_elapsed(), None);
    gate.hold.store(false, Ordering::Release);
    assert_eq!(
        received, 24,
        "video stopped while only its ACK path was delayed"
    );
    let deadline = Instant::now() + Duration::from_secs(1);
    while host.take_frame_ack_elapsed().is_none() {
        assert!(
            Instant::now() < deadline,
            "authenticated ACKs did not recover"
        );
        thread::sleep(Duration::from_millis(2));
    }
    // A larger metadata window must still stop a peer that accepts bytes but
    // never acknowledges them. No connection is closed by the proxy here.
    gate.hold.store(true, Ordering::Release);
    host.try_send_frame(&InFrame {
        frame: 24,
        data: vec![24; 1024],
        keyframe: false,
        recovery: false,
        host_delay: 0,
        assembly: Duration::ZERO,
    })
    .unwrap();
    let started = Instant::now();
    assert_ne!(closed(&host), io::ErrorKind::WouldBlock);
    assert!(started.elapsed() < Duration::from_secs(3));
}

#[test]
fn pilot_admission_bounds_outstanding_grants_per_creator_ip() {
    let pilot = Pilot::start();
    create_grant(&pilot.config, &Cancel::new()).unwrap();
    create_grant(&pilot.config, &Cancel::new()).unwrap();
    assert!(create_grant(&pilot.config, &Cancel::new()).is_err());
}

#[test]
fn daemon_restart_preserves_spent_transfer_reservations() {
    let quota = std::env::temp_dir().join(format!(
        "latch-relay-restart-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let configure = |initialize| {
        let cert = CertificateDer::from_pem_slice(include_bytes!("certs/server.pem")).unwrap();
        let key = PrivateKeyDer::from_pem_slice(include_bytes!("certs/server-key.pem")).unwrap();
        let mut config = DaemonConfig::new(
            "127.0.0.1:0".parse().unwrap(),
            vec![cert],
            key,
            quota.clone(),
            64 * 1024,
        )
        .unwrap();
        config.initialize_quota = initialize;
        config
    };
    let daemon = Daemon::bind(configure(true)).unwrap();
    let config = test_config(daemon.local_addr().unwrap().port());
    let cancel = Cancel::new();
    let run_cancel = cancel.clone();
    let worker = thread::spawn(move || daemon.run(&run_cancel));
    create_grant(&config, &Cancel::new()).unwrap();
    cancel.cancel();
    worker.join().unwrap().unwrap();
    assert_eq!(std::fs::metadata(&quota).unwrap().len(), 32);
    let daemon = Daemon::bind(configure(false)).unwrap();
    let config = test_config(daemon.local_addr().unwrap().port());
    let cancel = Cancel::new();
    let run_cancel = cancel.clone();
    let metrics = daemon.metrics();
    let worker = thread::spawn(move || daemon.run(&run_cancel));
    assert!(create_grant(&config, &Cancel::new()).is_err());
    assert!(metrics.rejected_connections.load(Ordering::Relaxed) > 0);
    assert!(!worker.is_finished());
    cancel.cancel();
    worker.join().unwrap().unwrap();
    std::fs::remove_file(quota).unwrap();
}

#[test]
fn frame_ack_elapsed_is_authenticated_and_consumed_on_the_callers_cadence() {
    let pilot = Pilot::start();
    let (host, viewer) = pilot.pair();
    assert_eq!(host.take_frame_ack_elapsed(), None);
    assert_eq!(host.queued_frame_age(), Duration::ZERO);
    host.try_send_frame(&InFrame {
        frame: 123,
        data: vec![7; 48_000],
        keyframe: true,
        recovery: false,
        host_delay: 0,
        assembly: Duration::ZERO,
    })
    .unwrap();
    assert!(matches!(
        viewer.recv_timeout(Duration::from_secs(2)),
        Some(Event::Frame(_))
    ));
    let until = Instant::now() + Duration::from_secs(2);
    let elapsed = loop {
        if let Some(elapsed) = host.take_frame_ack_elapsed() {
            break elapsed;
        }
        assert!(Instant::now() < until, "authenticated ACK did not arrive");
        thread::sleep(Duration::from_millis(2));
    };
    assert!(elapsed > Duration::ZERO && elapsed < Duration::from_secs(2));
    assert_eq!(host.take_frame_ack_elapsed(), None);
    assert_eq!(host.queued_frame_age(), Duration::ZERO);
}
