//! Runs alone in its own test binary: sibling tests on a small CI runner starve
//! the sender and the delay shim, which this throughput comparison measures.
#![allow(dead_code, unused_imports)]
include!("support/fixtures.rs");

#[test]
fn websocket_matches_native_video_through_a_delayed_path() {
    use latch_relay::Carrier;
    let mut rates = Vec::new();
    for carrier in [Carrier::Native, Carrier::WebSocket] {
        let pilot = Pilot::start();
        let gateway = Gateway::start(
            pilot.config.endpoint.port(),
            GatewayMode::Delay(Duration::from_millis(75)),
        );
        let mut config = test_config(gateway.port);
        config.carrier = Some(carrier);
        let grant = create_grant(&config, &Cancel::new()).unwrap();
        let keys = SessionKeys::generate().unwrap();
        let host = Transport::connect(config.clone(), grant.host, keys.clone()).unwrap();
        let viewer = Transport::connect(config, grant.viewer, keys).unwrap();
        ready(&host);
        ready(&viewer);
        let start = Instant::now();
        let warmup = Duration::from_secs(2);
        let duration = Duration::from_secs(6);
        let period = Duration::from_secs_f64(1.0 / 60.0);
        let mut next = start;
        let mut queued = Vec::new();
        let mut received = 0;
        let mut bytes = 0;
        let mut ages = Vec::new();
        let mut offered = 0;
        let mut refused = 0;
        let mut pending_event = None;
        while start.elapsed() < duration + Duration::from_millis(750) {
            while let Some(event) = pending_event
                .take()
                .or_else(|| viewer.recv_timeout(Duration::ZERO))
            {
                match event {
                    Event::Frame(frame) => {
                        let sent: Instant = queued[frame.frame as usize];
                        assert!(frame.data.iter().all(|&byte| byte == 19));
                        if sent.duration_since(start) >= warmup {
                            bytes += frame.data.len();
                            ages.push(sent.elapsed());
                        }
                        received += 1;
                    }
                    event => panic!("{carrier:?}: {event:?}"),
                }
            }
            if start.elapsed() >= duration {
                if received == queued.len() {
                    break;
                }
            } else if Instant::now() >= next {
                let size = if start.elapsed() < warmup {
                    10_416
                } else {
                    41_666
                };
                let frame = InFrame {
                    frame: queued.len() as u32,
                    data: vec![19; size],
                    keyframe: true,
                    recovery: false,
                    host_delay: 0,
                    assembly: Duration::ZERO,
                };
                let sent = Instant::now();
                offered += 1;
                match host.try_send_frame(&frame) {
                    Ok(()) => queued.push(sent),
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => refused += 1,
                    Err(error) => panic!("{carrier:?}: {error}"),
                }
                next += period;
            }
            let deadline = if start.elapsed() < duration {
                next.min(start + duration)
            } else {
                start + duration + Duration::from_millis(750)
            };
            let remaining = deadline.saturating_duration_since(Instant::now());
            #[cfg(not(windows))]
            {
                pending_event = viewer.recv_timeout(remaining);
            }
            #[cfg(windows)]
            {
                // The sender's next offer also needs a precise deadline on
                // Windows, where a condvar timeout can use the system tick.
                pending_event = viewer.recv_timeout(Duration::ZERO);
                if pending_event.is_none() {
                    thread::sleep(remaining.min(Duration::from_millis(2)));
                }
            }
        }
        let (chunks, p50_late, p95_late, max_late) = gateway.lateness();
        eprintln!(
            "{carrier:?}: product host waits={:?}, viewer waits={:?}",
            host.wait_stats(),
            viewer.wait_stats()
        );
        eprintln!("{carrier:?}: offered={offered}, accepted={}, refused_would_block={refused}, delivered={received}; gateway chunks={chunks}, lateness p50={p50_late:?}, p95={p95_late:?}, max={max_late:?}", queued.len());
        assert_eq!(received, queued.len(), "{carrier:?} failed to drain");
        assert!(host.recv_timeout(Duration::ZERO).is_none());
        ages.sort_unstable();
        assert!(!ages.is_empty());
        let p95 = ages[(ages.len() - 1) * 95 / 100];
        let max = *ages.last().unwrap();
        let mbps = bytes as f64 * 8.0 / (duration - warmup).as_secs_f64() / 1e6;
        eprintln!("{carrier:?}: {mbps:.3} Mbps, p95={p95:?}, max={max:?}");
        // The queue holds one frame, so a slow runner that accepts every other
        // frame delivers 120 of them in the window: 9.99984 Mbps, just under
        // half of the 20 Mbps offered. The floor sits below that rate, not on it.
        assert!(mbps >= 9.0, "{carrier:?}: {mbps} Mbps");
        assert!(p95 < Duration::from_millis(500), "{carrier:?}: {p95:?}");
        assert!(max < Duration::from_millis(750), "{carrier:?}: {max:?}");
        rates.push(mbps);
    }
    assert!(rates[1] >= rates[0] * 0.85, "native/WS Mbps: {rates:?}");
}
