include!("support/fixtures.rs");

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
    let error = create_grant(&config, &Cancel::new()).unwrap_err();
    assert_eq!(
        latch_relay::ConnectError::from_io(&error).failure,
        latch_relay::Failure::ClosedBeforeHandshake
    );
    assert!(error.to_string().contains("blocked or relay busy"));
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

fn proxy_config(gateway: &Gateway, auth: bool) -> ClientConfig {
    let mut config = test_config(443);
    let mut proxy = latch_relay::ProxyConfig::new("127.0.0.1", gateway.port).unwrap();
    if auth {
        proxy.credentials = Some(latch_relay::BasicCredentials::new("user", "pass").unwrap());
    }
    config.proxy = Some(proxy);
    config
}

#[test]
fn http_only_middlebox_falls_back_to_websocket_within_negotiation_deadline() {
    use latch_relay::{create_grant_with_info, Carrier};
    let pilot = Pilot::start();
    let gateway = Gateway::start(pilot.config.endpoint.port(), GatewayMode::HttpAlpnOnly);
    let config = test_config(gateway.port);
    let start = Instant::now();
    let (grant, info) = create_grant_with_info(&config, &Cancel::new()).unwrap();
    assert_eq!(info.carrier, Carrier::WebSocket);
    assert!(!info.via_proxy);
    assert_eq!(info.earlier_failures.len(), 1);
    let keys = SessionKeys::generate().unwrap();
    let host = Transport::connect(config.clone(), grant.host, keys.clone()).unwrap();
    let viewer = Transport::connect(config, grant.viewer, keys).unwrap();
    ready(&host);
    ready(&viewer);
    assert!(start.elapsed() < Duration::from_secs(5));
    assert_eq!(
        host.connection_info(latch_relay::Lane::Video)
            .unwrap()
            .carrier,
        Carrier::WebSocket
    );
    host.try_send_frame(&InFrame {
        frame: 7,
        data: vec![11; 500_000],
        keyframe: true,
        recovery: false,
        host_delay: 0,
        assembly: Duration::ZERO,
    })
    .unwrap();
    viewer.try_send_datagram(b"input").unwrap();
    host.try_send_control(b"control").unwrap();
    assert!(
        matches!(host.recv_timeout(Duration::from_secs(3)), Some(Event::Datagram(b)) if b == b"input")
    );
    let mut frame = false;
    let mut control = false;
    for _ in 0..2 {
        match viewer.recv_timeout(Duration::from_secs(3)).unwrap() {
            Event::Frame(f) => {
                assert_eq!(f.data, vec![11; 500_000]);
                frame = true;
            }
            Event::Control(b) => {
                assert_eq!(b, b"control");
                control = true;
            }
            event => panic!("{event:?}"),
        }
    }
    assert!(frame && control);
}
#[test]
fn connect_proxy_supports_both_carriers_with_and_without_basic_auth() {
    use latch_relay::{create_grant_with_info, Carrier};
    for auth in [false, true] {
        for http_only in [false, true] {
            let pilot = Pilot::start();
            let gateway = Gateway::start(
                pilot.config.endpoint.port(),
                GatewayMode::Connect {
                    auth,
                    status: 200,
                    intercept: false,
                    http_only,
                },
            );
            let config = proxy_config(&gateway, auth);
            let (grant, info) = create_grant_with_info(&config, &Cancel::new()).unwrap();
            assert!(info.via_proxy);
            assert_eq!(
                info.carrier,
                if http_only {
                    Carrier::WebSocket
                } else {
                    Carrier::Native
                }
            );
            let keys = SessionKeys::generate().unwrap();
            let host = Transport::connect(config.clone(), grant.host, keys.clone()).unwrap();
            let viewer = Transport::connect(config, grant.viewer, keys).unwrap();
            ready(&host);
            ready(&viewer);
            host.try_send_control(b"via proxy").unwrap();
            assert!(
                matches!(viewer.recv_timeout(Duration::from_secs(2)), Some(Event::Control(b)) if b == b"via proxy")
            );
        }
    }
    let credentials = latch_relay::BasicCredentials::new("secret-user", "secret-password").unwrap();
    let mut config = latch_relay::ProxyConfig::new("localhost", 3128).unwrap();
    config.credentials = Some(credentials);
    let debug = format!("{config:?}");
    assert!(!debug.contains("secret-user") && !debug.contains("secret-password"));
}
#[test]
fn proxy_rejections_and_interception_are_typed_and_never_trusted() {
    use latch_relay::{ConnectError, Failure};
    for status in [407, 403, 502, 503] {
        let gateway = Gateway::start(
            0,
            GatewayMode::Connect {
                auth: false,
                status,
                intercept: false,
                http_only: false,
            },
        );
        let error = create_grant(&proxy_config(&gateway, false), &Cancel::new()).unwrap_err();
        let typed = ConnectError::from_io(&error);
        assert_eq!(
            typed.failure,
            if status == 407 {
                Failure::ProxyAuthenticationRequired
            } else {
                Failure::ProxyRefused { status }
            }
        );
        assert_eq!(typed.attempts.len(), 1);
    }
    let gateway = Gateway::start(
        0,
        GatewayMode::Connect {
            auth: false,
            status: 200,
            intercept: true,
            http_only: false,
        },
    );
    let mut config = ClientConfig::with_roots(
        Endpoint::new("localhost", 443).unwrap(),
        rustls::RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
        },
    )
    .unwrap();
    config.proxy = proxy_config(&gateway, false).proxy;
    let typed = ConnectError::from_io(&create_grant(&config, &Cancel::new()).unwrap_err());
    assert!(
        matches!(typed.failure, Failure::TlsInterception { issuer } if issuer.contains("Latch Relay Test CA"))
    );
    assert_eq!(typed.attempts.len(), 1);
}
#[test]
fn plaintext_redirect_is_classified_as_probable_captive_portal() {
    let gateway = Gateway::start(0, GatewayMode::PlainHttp);
    let error = create_grant(&test_config(gateway.port), &Cancel::new()).unwrap_err();
    assert_eq!(
        latch_relay::ConnectError::from_io(&error).failure,
        latch_relay::Failure::CaptivePortal
    );
}
#[test]
fn ordinary_https_requests_receive_a_small_http_response_with_or_without_alpn() {
    let pilot = Pilot::start();
    for alpn in [vec![b"http/1.1".to_vec()], Vec::new()] {
        let mut roots = rustls::RootCertStore::empty();
        roots
            .add(CertificateDer::from_pem_slice(include_bytes!("certs/ca.pem")).unwrap())
            .unwrap();
        let mut config = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls_graviola::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
        config.alpn_protocols = alpn;
        let tcp = TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, pilot.config.endpoint.port()))
            .unwrap();
        tcp.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let mut tls = rustls::StreamOwned::new(
            rustls::ClientConnection::new(Arc::new(config), "localhost".try_into().unwrap())
                .unwrap(),
            tcp,
        );
        tls.write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .unwrap();
        tls.flush().unwrap();
        let mut response = vec![0; 256];
        let size = tls.read(&mut response).unwrap();
        response.truncate(size);
        assert!(response.starts_with(b"HTTP/1.1 200 OK\r\n"));
        assert!(response.ends_with(b"\r\n\r\nOK\n"));
    }
    assert_eq!(pilot.metrics.http_responses.load(Ordering::Relaxed), 2);
}
#[test]
fn websocket_admission_and_receive_bounds_match_native() {
    let pilot = Pilot::start();
    let gateway = Gateway::start(pilot.config.endpoint.port(), GatewayMode::HttpAlpnOnly);
    let config = test_config(gateway.port);
    let grant = create_grant(&config, &Cancel::new()).unwrap();
    let keys = SessionKeys::generate().unwrap();
    let host = Transport::connect(config.clone(), grant.host, keys.clone()).unwrap();
    let mut limited = config.clone();
    limited.limits.max_frame_bytes = 1000;
    limited.limits.video_queue_bytes = 1000;
    limited.limits.event_queue_bytes = 1400;
    let viewer = Transport::connect(limited, grant.viewer, keys).unwrap();
    ready(&host);
    ready(&viewer);
    for _ in 0..4 {
        host.try_send_datagram(&[7; 1200]).unwrap();
    }
    thread::sleep(Duration::from_millis(100));
    assert_eq!(closed(&viewer), io::ErrorKind::WouldBlock);
    create_grant(&config, &Cancel::new()).unwrap();
    create_grant(&config, &Cancel::new()).unwrap();
    let error = create_grant(&config, &Cancel::new()).unwrap_err();
    assert_eq!(
        latch_relay::ConnectError::from_io(&error).failure,
        latch_relay::Failure::RelayRejected
    );
}

#[test]
fn upgrade_redirect_rejection_and_wrong_hostname_have_distinct_failures() {
    use latch_relay::{ConnectError, Failure};
    for status in [0, 200, 302, 403, 503] {
        let gateway = Gateway::start(0, GatewayMode::TlsHttp { status });
        let error = create_grant(&test_config(gateway.port), &Cancel::new()).unwrap_err();
        let typed = ConnectError::from_io(&error);
        assert_eq!(typed.failure, Failure::UpgradeRejected { status });
        assert_eq!(typed.attempts.len(), 2);
    }
    let gateway = Gateway::start(
        0,
        GatewayMode::Connect {
            auth: false,
            status: 200,
            intercept: true,
            http_only: false,
        },
    );
    let mut config = proxy_config(&gateway, false);
    config.endpoint = Endpoint::new("wrong.example", 443).unwrap();
    let error = create_grant(&config, &Cancel::new()).unwrap_err();
    assert_eq!(
        ConnectError::from_io(&error).failure,
        Failure::CaptivePortal
    );
}

#[test]
fn silent_native_handshake_is_blocked_and_fallback_preserves_mss() {
    let pilot = Pilot::start();
    let gateway = Gateway::start(pilot.config.endpoint.port(), GatewayMode::StallNativeSlow);
    let started = Instant::now();
    let config = test_config(gateway.port);
    let (grant, info) = latch_relay::create_grant_with_info(&config, &Cancel::new()).unwrap();
    // Includes the grant exchange and scheduler headroom beyond the 2 s open budget.
    assert!(started.elapsed() < Duration::from_secs(3));
    assert_eq!(info.carrier, latch_relay::Carrier::WebSocket);
    assert_eq!(
        info.earlier_failures[0].failure,
        latch_relay::Failure::ConnectionBlocked
    );
    assert!(!info.reduced_mss);
    assert_eq!(info.reduced_mss_reason, None);
    let keys = SessionKeys::generate().unwrap();
    let host = Transport::connect(config.clone(), grant.host, keys.clone()).unwrap();
    let viewer = Transport::connect(config.clone(), grant.viewer, keys).unwrap();
    ready(&host);
    ready(&viewer);
    for transport in [&host, &viewer] {
        for lane in [latch_relay::Lane::Priority, latch_relay::Lane::Video] {
            let info = transport.connection_info(lane).unwrap();
            assert_eq!(info.carrier, latch_relay::Carrier::WebSocket);
            assert!(info.earlier_failures.is_empty());
        }
    }
    // Stable cached path: 2 s cold open + 1.25 s warm open + 4 * 200 ms = 4.05 s.
    // Allow scheduling headroom while remaining 1.25 s below negotiation expiry.
    assert!(started.elapsed() < Duration::from_millis(4750));
    let (_, warm) = latch_relay::create_grant_with_info(&config, &Cancel::new()).unwrap();
    assert!(warm.earlier_failures.is_empty());
}

fn raw_http(
    pilot: &Pilot,
    alpn: Vec<Vec<u8>>,
) -> rustls::StreamOwned<rustls::ClientConnection, TcpStream> {
    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(CertificateDer::from_pem_slice(include_bytes!("certs/ca.pem")).unwrap())
        .unwrap();
    let mut config =
        rustls::ClientConfig::builder_with_provider(Arc::new(rustls_graviola::default_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
    config.alpn_protocols = alpn;
    let tcp =
        TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, pilot.config.endpoint.port())).unwrap();
    tcp.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    tcp.set_write_timeout(Some(Duration::from_secs(2))).unwrap();
    rustls::StreamOwned::new(
        rustls::ClientConnection::new(Arc::new(config), "localhost".try_into().unwrap()).unwrap(),
        tcp,
    )
}
fn raw_websocket(pilot: &Pilot) -> rustls::StreamOwned<rustls::ClientConnection, TcpStream> {
    let mut tls = raw_http(pilot, vec![b"http/1.1".to_vec()]);
    tls.write_all(b"GET /relay HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n").unwrap();
    tls.flush().unwrap();
    let mut response = Vec::new();
    while !response.ends_with(b"\r\n\r\n") {
        let mut byte = [0];
        tls.read_exact(&mut byte).unwrap();
        response.push(byte[0]);
        assert!(response.len() < 8192);
    }
    assert!(response.starts_with(b"HTTP/1.1 101 "));
    tls
}
fn client_frame(opcode: u8, final_frame: bool, payload: &[u8], masked: bool) -> Vec<u8> {
    assert!(payload.len() <= u16::MAX as usize);
    let mask_bit = if masked { 128 } else { 0 };
    let mut frame = vec![opcode | if final_frame { 128 } else { 0 }];
    if payload.len() < 126 {
        frame.push(mask_bit | payload.len() as u8);
    } else {
        frame.push(mask_bit | 126);
        frame.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    }
    let mask = [1, 2, 3, 4];
    if masked {
        frame.extend_from_slice(&mask);
    }
    frame.extend(
        payload
            .iter()
            .enumerate()
            .map(|(i, b)| if masked { b ^ mask[i % 4] } else { *b }),
    );
    frame
}
fn read_until_closed(
    tls: &mut rustls::StreamOwned<rustls::ClientConnection, TcpStream>,
) -> Vec<u8> {
    let start = Instant::now();
    let mut received = Vec::new();
    loop {
        assert!(start.elapsed() < Duration::from_secs(3));
        let mut bytes = [0; 1024];
        match tls.read(&mut bytes) {
            Ok(0) => return received,
            Ok(n) => {
                received.extend_from_slice(&bytes[..n]);
                assert!(received.len() <= 32768);
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::UnexpectedEof
                        | io::ErrorKind::ConnectionReset
                        | io::ErrorKind::ConnectionAborted
                        | io::ErrorKind::BrokenPipe
                ) =>
            {
                return received
            }
            Err(error) => panic!("daemon did not close within its bounds: {error}"),
        }
    }
}
#[test]
fn daemon_websocket_accepts_fragmentation_and_rejects_malformed_socket_frames() {
    let pilot = Pilot::start();
    let mut tls = raw_websocket(&pilot);
    let mut bytes = client_frame(2, false, &[0, 0], true);
    bytes.extend(client_frame(9, true, b"ping", true));
    bytes.extend(client_frame(0, true, &[0, 1, 1], true));
    tls.write_all(&bytes).unwrap();
    tls.flush().unwrap();
    let mut pong = [0; 6];
    tls.read_exact(&mut pong).unwrap();
    assert_eq!(&pong, b"\x8a\x04ping");
    let mut grant = [0; 189];
    tls.read_exact(&mut grant).unwrap();
    assert_eq!(&grant[..9], &[0x82, 126, 0, 185, 0, 0, 0, 181, 1]);
    assert!(read_until_closed(&mut tls).is_empty());
    for bytes in [
        client_frame(2, true, &[0, 0, 0, 1, 1], false),
        vec![0x82, 0xff, 0, 0, 0, 0, 0, 1, 0, 0, 1, 2, 3, 4],
        client_frame(9, true, &[0; 126], true),
        client_frame(8, true, &[0; 126], true),
        client_frame(9, false, b"ping", true),
        {
            let mut bytes = client_frame(2, false, &[0; 10_000], true);
            bytes.extend(client_frame(0, true, &[0; 10_000], true));
            bytes
        },
    ] {
        let mut tls = raw_websocket(&pilot);
        let _ = tls.write_all(&bytes).and_then(|_| tls.flush());
        assert!(read_until_closed(&mut tls).is_empty());
    }
    create_grant(&pilot.config, &Cancel::new()).unwrap();
    assert_eq!(pilot.metrics.grants_created.load(Ordering::Relaxed), 2);
}
#[test]
fn daemon_websocket_ping_and_close_floods_have_bounded_responses_and_lifetime() {
    let pilot = Pilot::with_limits(PilotLimits {
        handshake_timeout: Duration::from_millis(400),
        stall_timeout: Duration::from_millis(200),
        ..PilotLimits::default()
    });
    let mut tls = raw_websocket(&pilot);
    let start = Instant::now();
    let flood = client_frame(9, true, b"", true).repeat(2000);
    let _ = tls.write_all(&flood).and_then(|_| tls.flush());
    let responses = read_until_closed(&mut tls);
    let (pongs, remainder) = responses.as_chunks::<2>();
    assert!(remainder.is_empty());
    assert!(pongs.len() <= 2000 && pongs.iter().all(|bytes| *bytes == [0x8a, 0]));
    assert!(start.elapsed() < Duration::from_secs(2));
    let mut tls = raw_websocket(&pilot);
    let start = Instant::now();
    // Interleaved controls cannot extend a partial message's record deadline.
    let mut flood = client_frame(2, false, &[0, 0], true);
    for _ in 0..2000 {
        flood.extend(client_frame(9, true, b"", true));
    }
    let _ = tls.write_all(&flood).and_then(|_| tls.flush());
    let responses = read_until_closed(&mut tls);
    assert!(responses.len() <= 4000);
    let (pongs, remainder) = responses.as_chunks::<2>();
    assert!(remainder.is_empty());
    assert!(pongs.iter().all(|bytes| *bytes == [0x8a, 0]));
    assert!(start.elapsed() < Duration::from_secs(2));
    let mut tls = raw_websocket(&pilot);
    let close = client_frame(8, true, &1000u16.to_be_bytes(), true);
    let _ = tls.write_all(&close.repeat(1000)).and_then(|_| tls.flush());
    assert_eq!(read_until_closed(&mut tls), [0x88, 2, 3, 232]);
    create_grant(&pilot.config, &Cancel::new()).unwrap();
    assert!(pilot.metrics.network_bytes.load(Ordering::Relaxed) < 128 * 1024);
}
#[test]
fn daemon_rate_admission_closure_is_reported_as_blocked_or_relay_busy() {
    let pilot = Pilot::with_limits(PilotLimits {
        accepts_per_ip_per_minute: 1,
        ..PilotLimits::default()
    });
    create_grant(&pilot.config, &Cancel::new()).unwrap();
    let error = create_grant(&pilot.config, &Cancel::new()).unwrap_err();
    let typed = latch_relay::ConnectError::from_io(&error);
    assert_eq!(typed.failure, latch_relay::Failure::ClosedBeforeHandshake);
    assert!(typed
        .attempts
        .iter()
        .all(|attempt| attempt.failure == latch_relay::Failure::ClosedBeforeHandshake));
    assert!(error.to_string().contains("blocked or relay busy"));
    assert!(pilot.metrics.rejected_connections.load(Ordering::Relaxed) >= 2);
}

#[test]
fn cached_websocket_upgrade_redirect_continues_to_recovered_native_carrier() {
    let pilot = Pilot::start();
    let gateway = Gateway::start(pilot.config.endpoint.port(), GatewayMode::HttpAlpnOnly);
    let config = test_config(gateway.port);
    let (_, initial) = latch_relay::create_grant_with_info(&config, &Cancel::new()).unwrap();
    assert_eq!(initial.carrier, latch_relay::Carrier::WebSocket);
    gateway.recover_native.store(true, Ordering::Release);
    let (_, recovered) = latch_relay::create_grant_with_info(&config, &Cancel::new()).unwrap();
    assert_eq!(recovered.carrier, latch_relay::Carrier::Native);
    assert_eq!(recovered.earlier_failures.len(), 1);
    assert_eq!(
        recovered.earlier_failures[0].failure,
        latch_relay::Failure::UpgradeRejected { status: 302 }
    );
}

#[test]
fn stale_cached_native_recovers_with_full_ladder_at_280_ms_rtt() {
    let pilot = Pilot::start();
    let gateway = Gateway::start(
        pilot.config.endpoint.port(),
        GatewayMode::CachedNativeThenStall,
    );
    let config = test_config(gateway.port);
    let (_, initial) = latch_relay::create_grant_with_info(&config, &Cancel::new()).unwrap();
    assert_eq!(initial.carrier, latch_relay::Carrier::Native);
    gateway.recover_native.store(true, Ordering::Release);
    let started = Instant::now();
    let (_, recovered) = latch_relay::create_grant_with_info(&config, &Cancel::new()).unwrap();
    assert_eq!(recovered.carrier, latch_relay::Carrier::WebSocket);
    // The cached native probe fails, then fresh native and WebSocket both run.
    assert_eq!(recovered.earlier_failures.len(), 2);
    assert!(recovered
        .earlier_failures
        .iter()
        .all(|a| a.failure == latch_relay::Failure::ConnectionBlocked));
    assert!(!recovered.reduced_mss);
    assert!(started.elapsed() >= Duration::from_millis(2600));
    assert!(started.elapsed() < Duration::from_millis(3500));
}

#[test]
fn successful_cached_addresses_are_scoped_to_relay_and_proxy_endpoints() {
    let pilot = Pilot::start();
    let gateway = Gateway::start(pilot.config.endpoint.port(), GatewayMode::StallNative);
    let mut config = test_config(gateway.port);
    let (_, initial) = latch_relay::create_grant_with_info(&config, &Cancel::new()).unwrap();
    assert_eq!(initial.carrier, latch_relay::Carrier::WebSocket);
    config.endpoint = pilot.config.endpoint.clone();
    let (_, direct) = latch_relay::create_grant_with_info(&config, &Cancel::new()).unwrap();
    assert_eq!(direct.carrier, latch_relay::Carrier::Native);
    assert!(direct.earlier_failures.is_empty());
    let pilot = Pilot::start();
    let old_proxy = Gateway::start(
        pilot.config.endpoint.port(),
        GatewayMode::Connect {
            auth: false,
            status: 200,
            intercept: false,
            http_only: true,
        },
    );
    let new_proxy = Gateway::start(
        pilot.config.endpoint.port(),
        GatewayMode::Connect {
            auth: false,
            status: 200,
            intercept: false,
            http_only: false,
        },
    );
    let mut config = proxy_config(&old_proxy, false);
    let (_, initial) = latch_relay::create_grant_with_info(&config, &Cancel::new()).unwrap();
    assert_eq!(initial.carrier, latch_relay::Carrier::WebSocket);
    config.proxy = proxy_config(&new_proxy, false).proxy;
    let (_, changed) = latch_relay::create_grant_with_info(&config, &Cancel::new()).unwrap();
    assert_eq!(changed.carrier, latch_relay::Carrier::Native);
    assert!(changed.earlier_failures.is_empty());
}

#[test]
fn fresh_concurrent_lanes_share_the_first_successful_carrier_discovery() {
    let pilot = Pilot::start();
    let gateway = Gateway::start(pilot.config.endpoint.port(), GatewayMode::StallNativeSlow);
    let started = Instant::now();
    let grant = create_grant(&test_config(gateway.port), &Cancel::new()).unwrap();
    // A host receiving an attachment may have a fresh config with no grant lookup.
    let config = test_config(gateway.port);
    let keys = SessionKeys::generate().unwrap();
    let host = Transport::connect(config.clone(), grant.host, keys.clone()).unwrap();
    let viewer = Transport::connect(config, grant.viewer, keys).unwrap();
    ready(&host);
    ready(&viewer);
    let mut cold_attempts = 0;
    for transport in [&host, &viewer] {
        for lane in [latch_relay::Lane::Priority, latch_relay::Lane::Video] {
            let info = transport.connection_info(lane).unwrap();
            assert_eq!(info.carrier, latch_relay::Carrier::WebSocket);
            cold_attempts += info.earlier_failures.len();
        }
    }
    assert_eq!(cold_attempts, 1);
    // A separately configured host also shares fallback before the first upgrade completes.
    // 2 s grant + 2 s cold attach cohort + four 200 ms exchanges = 4.8 s.
    assert!(started.elapsed() < Duration::from_millis(5500));
}

#[test]
fn discovered_proxy_only_environment_uses_connect_before_direct() {
    let pilot = Pilot::start();
    let gateway = Gateway::start(
        pilot.config.endpoint.port(),
        GatewayMode::Connect {
            auth: true,
            status: 200,
            intercept: false,
            http_only: false,
        },
    );
    let mut config = proxy_config(&gateway, true);
    let proxy = config.proxy.clone();
    config.set_proxy_discovery(move || (proxy.clone(), false));
    assert!(config.proxy.is_some());
    let (_, info) = latch_relay::create_grant_with_info(&config, &Cancel::new()).unwrap();
    assert!(info.via_proxy);
    assert_eq!(info.carrier, latch_relay::Carrier::Native);
    assert!(info.earlier_failures.is_empty());
}

#[test]
fn forced_carrier_overrides_cached_path_for_grant_and_lanes() {
    use latch_relay::{create_grant_with_info, Carrier, Lane};
    let mut pilot = Pilot::start();
    let (_, info) = create_grant_with_info(&pilot.config, &Cancel::new()).unwrap();
    assert_eq!(info.carrier, Carrier::Native);
    for carrier in [Carrier::WebSocket, Carrier::Native] {
        pilot.config.carrier = Some(carrier);
        let (host, viewer) = pilot.pair();
        for transport in [&host, &viewer] {
            for lane in [Lane::Priority, Lane::Video] {
                assert_eq!(transport.connection_info(lane).unwrap().carrier, carrier);
            }
        }
        host.try_send_control(b"forced carrier").unwrap();
        assert!(
            matches!(viewer.recv_timeout(Duration::from_secs(2)), Some(Event::Control(b)) if b == b"forced carrier")
        );
        host.shutdown();
        viewer.shutdown();
    }
}

#[test]
fn forced_native_does_not_fall_back_through_http_only_gateway() {
    use latch_relay::{create_grant_with_info, Carrier};
    let pilot = Pilot::start();
    let gateway = Gateway::start(
        pilot.config.endpoint.port(),
        GatewayMode::Connect {
            auth: false,
            status: 200,
            intercept: false,
            http_only: true,
        },
    );
    let mut config = proxy_config(&gateway, false);
    config.carrier = Some(Carrier::Native);
    let error = create_grant_with_info(&config, &Cancel::new()).unwrap_err();
    let error = latch_relay::ConnectError::from_io(&error);
    assert!(!error.attempts.is_empty());
    assert!(error
        .attempts
        .iter()
        .all(|attempt| attempt.carrier == Carrier::Native));
    config.carrier = Some(Carrier::WebSocket);
    let (_, info) = create_grant_with_info(&config, &Cancel::new()).unwrap();
    assert_eq!(info.carrier, Carrier::WebSocket);
    assert!(info.earlier_failures.is_empty());
}

#[test]
fn fresh_connections_report_the_applied_mss_step_down() {
    let pilot = Pilot::start();
    for expected in [1200, 512] {
        pilot.config.reduce_mss_after_stall();
        let (_, info) = latch_relay::create_grant_with_info(&pilot.config, &Cancel::new()).unwrap();
        assert_eq!(info.reduced_mss, cfg!(unix));
        assert_eq!(info.reduced_mss_value, cfg!(unix).then_some(expected));
        assert_eq!(
            info.reduced_mss_reason,
            cfg!(unix).then_some(latch_relay::MssReason::ExplicitStall)
        );
    }
}
