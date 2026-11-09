//! Congestion feedback through an encoded noise-pattern desktop, UDP relay and viewer.
//! A sustained downstream queue-delay step checks delivery and RTT recovery.
//! The controller tests below use a fake clock to check underfilled bitrate
//! adaptation and saturated FIFO convergence without runner scheduling jitter.

// Exercise the production controller without widening its public API.
#[allow(dead_code)]
#[path = "../src/congestion.rs"]
mod congestion;
#[path = "support/relay.rs"]
mod relay;

use latch_core::http::{self, Response};
use latch_engine::server::{Options, Server};
use latch_proto::control::{self, StreamRequest};
use latch_proto::sim::{Impairment, Link};
use latch_stream::{Event, FrameSlot, Session, Settings};
use std::net::{IpAddr, Ipv4Addr, TcpListener, UdpSocket};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

fn host() -> u16 {
    for _ in 0..50 {
        // The OS's next free UDP port, then TCP on the same number. Taking
        // TCP's next port first failed 50 times running on Windows CI: its
        // TCP ports come in sequence and can sit inside a block reserved
        // (by Hyper-V) against UDP.
        let port = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
            .and_then(|u| u.local_addr())
            .unwrap()
            .port();
        let Ok(listener) = TcpListener::bind((Ipv4Addr::LOCALHOST, port)) else {
            continue;
        };
        let Ok(server) = Server::bind(port, Options::default()) else {
            continue;
        };
        std::thread::spawn(move || {
            http::serve_with_peer_check(
                listener,
                |_| true,
                move |peer, req| {
                    let request: StreamRequest = req.json().unwrap();
                    match server.prepare(request, peer) {
                        Ok(prepared) => {
                            let server = server.clone();
                            Response::switching(control::UPGRADE, move |tcp| {
                                server.run(prepared, tcp)
                            })
                        }
                        Err(r) => {
                            Response::json(r.status, &serde_json::json!({ "error": r.message }))
                        }
                    }
                },
            )
        });
        return port;
    }
    panic!("no free port");
}

#[test]
fn noise_stream_keeps_delivering_through_sustained_queue_delay() {
    use std::sync::atomic::{AtomicU32, Ordering};
    std::env::set_var("LATCH_TEST_PATTERN", "noise");
    let queue_delay_ms = Arc::new(AtomicU32::new(5));
    let port = relay::relay(host(), Impairment::NONE, queue_delay_ms.clone());
    let frames = Arc::new(FrameSlot::default());
    let (tx, rx) = mpsc::channel();
    let session = Session::start(
        latch_stream::Server {
            address: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port,
        },
        // The pattern has a small noise patch, not full-frame entropy. Use a
        // modest frame rate to deliberately underfill the requested bitrate;
        // keep exercising delivery on a slow CI runner.
        Settings {
            prefer_color: false,
            color: Default::default(),
            cursor: false,
            gamepads: false,
            mic: false,
            width: 640,
            height: 360,
            fps: 12,
            bitrate_kbps: 50_000,
            hevc: false,
            av1: false,
            audio: false,
            name: "bottleneck test".into(),
        },
        frames.clone(),
        tx,
        || {},
    );
    let start = Instant::now();
    loop {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Event::Connected) => break,
            Ok(Event::Failed { stage, message }) => panic!("{stage}: {message}"),
            _ => assert!(start.elapsed() < Duration::from_secs(15), "never connected"),
        }
    }
    // A 5 ms propagation delay keeps the unloaded RTT above integer-ms
    // rounding; introduce an additional standing 40 ms queue after learning it.
    // Count fresh decoded pictures, not repeated reads of one stats window.
    let deadline = Instant::now() + Duration::from_secs(30);
    let baseline = loop {
        std::thread::sleep(Duration::from_millis(300));
        let s = session.stats();
        if frames.seq() >= 10 && s.rtt_ms > 0 && s.applied_bitrate_kbps > 0 && s.application_limited
        {
            break s;
        }
        assert!(Instant::now() < deadline, "no baseline: {s:?}");
    };
    eprintln!("baseline: {baseline:?}");
    // Wall-clock ping delays include runner scheduling as well as our relay.
    // They can legitimately lower the initial budget before we add a queue.
    // `an_underfilled_stream_recovers_once_the_queue_clears` exercises
    // the bitrate cuts and full recovery with deterministic RTT samples.
    assert!(
        baseline.application_limited,
        "must exercise an underfilled encoder"
    );
    // The first stats window can still contain a scheduling spike (42 ms
    // unloaded on a busy Mac). Comparing the injected queue to that noisy
    // snapshot can make a stable 46 ms queued RTT fail baseline + 20 forever.
    // Use the midpoint of our known 5/45 ms relay delays for both phases;
    // still require delivery and a return below that same threshold.
    let queue_threshold_ms = 25;
    queue_delay_ms.store(45, Ordering::Relaxed);
    let mut clean = 0;
    let mut clean_from = frames.seq();
    let mut settled = false;
    let deadline = Instant::now() + Duration::from_secs(45);
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(300));
        let s = session.stats();
        eprintln!(
            "queued: applied={} received={:.2} fps={:.1} loss={:.1} rtt={}",
            s.applied_bitrate_kbps, s.received_mbps, s.fps, s.loss_pct, s.rtt_ms
        );
        // At the floor rate pictures come unevenly, a few a second: one
        // 300 ms poll may see none, so the picture must keep coming over the
        // whole settled run, not in every poll. 12 fps over a ~250 ms stats
        // window is 1–3 frames: one frame reads as 3.8 fps.
        if s.applied_bitrate_kbps > 0
            && s.rtt_ms >= queue_threshold_ms
            && s.fps >= 3.0
            && s.loss_pct < 3.0
        {
            if clean == 0 {
                clean_from = frames.seq();
            }
            clean += 1;
            if clean >= 5 && frames.seq() >= clean_from + 3 {
                settled = true;
                break;
            }
        } else {
            clean = 0;
        }
    }
    assert!(settled, "never settled: {:?}", session.stats());
    queue_delay_ms.store(5, Ordering::Relaxed);
    let deadline = Instant::now() + Duration::from_secs(40);
    let recovery_frame = frames.seq();
    let mut recovered = false;
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(300));
        let s = session.stats();
        if s.rtt_ms < queue_threshold_ms
            && frames.seq() > recovery_frame + 10
            && s.fps >= 3.0
            && s.loss_pct < 3.0
        {
            eprintln!("recovered: {s:?}");
            recovered = true;
            break;
        }
    }
    session.stop();
    assert!(
        recovered,
        "delivery/RTT did not recover: {:?}",
        session.stats()
    );
}

#[test]
fn saturated_ten_megabit_fifo_converges_and_recovers_with_fake_clock() {
    let start = Instant::now();
    let mut controller = congestion::Controller::new(50_000, start);
    let mut link = Link::new(Impairment::NONE, 11);
    let mut pressured = 0;
    let mut clean = 0;
    let mut settled = Vec::new();
    for tick in 0..256 {
        let limited = (8..128).contains(&tick);
        let now = start + Duration::from_millis(tick * 250);
        link.set_bandwidth(
            if limited { 10_000 } else { 50_000 },
            Duration::from_millis(40),
        );
        // Generate the full wire budget, including Link's 80 byte overhead,
        // without an encoder or scheduler limiting the offered load.
        let packet = [0u8; 1200];
        let spacing =
            Duration::from_secs_f64(1280.0 * 8.0 / (f64::from(controller.budget()) * 1000.0));
        let before = link.counts();
        let mut at = now;
        while at < now + Duration::from_millis(250) {
            link.send(at, &packet);
            link.receive(at);
            at += spacing;
        }
        let after = link.counts();
        let loss = (after.1 - before.1) as f32 * 100.0 / (after.0 - before.0) as f32;
        let report = control::Report {
            rtt_ms: 10.0,
            loss_pct: loss,
            unrecovered_loss_pct: Some(loss),
            ..Default::default()
        };
        controller.update(now + Duration::from_millis(250), &report, 10.0, 0.0);
        if limited && loss > 3.0 {
            pressured += 1;
        }
        if limited && loss == 0.0 && controller.budget() <= 10_000 {
            clean += 1;
        }
        // Probes deliberately cross capacity: judge convergence over the
        // constrained interval rather than one probe's phase.
        if limited && tick >= 24 {
            settled.push(controller.budget());
        }
    }
    let mean = settled.iter().sum::<u32>() / settled.len() as u32;
    assert!((8_000..=10_500).contains(&mean), "did not converge: {mean}");
    assert!(congestion::video_budget(10_000, 10, 0, 120, false, false) < 10_000);
    assert!(pressured >= 4, "must exercise sustained overload");
    assert!(
        clean >= 8,
        "must deliver clean windows under capacity: {clean}"
    );
    assert_eq!(
        controller.budget(),
        50_000,
        "must recover to requested rate"
    );
}

/// The actual software noise-pattern codec and viewer, separate from the
/// codec-free sender isolation test. Stats are window means, not frame percentiles.
#[test]
#[ignore]
fn noise_socket_measurement() {
    std::env::set_var("LATCH_TEST_PATTERN", "noise");
    let cap: u32 = std::env::var("PB_CAP")
        .unwrap_or("20000".into())
        .parse()
        .unwrap();
    let rtt: u64 = std::env::var("PB_RTT")
        .unwrap_or("20".into())
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
    let port = relay::relay_with_bandwidth(
        host(),
        Impairment {
            delay: Duration::from_millis(rtt / 2),
            ..Impairment::NONE
        },
        Arc::default(),
        cap,
    );
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
            cursor: false,
            gamepads: false,
            mic: false,
            width: 640,
            height: 360,
            fps: 60,
            bitrate_kbps: requested,
            hevc: false,
            av1: false,
            audio: false,
            name: "paced measurement".into(),
        },
        frames.clone(),
        tx,
        || {},
    );
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Event::Connected) => break,
            Ok(Event::Failed { stage, message }) => panic!("{stage}: {message}"),
            _ => assert!(Instant::now() < deadline),
        }
    }
    let start = Instant::now();
    let mut mark = start;
    let mut sequence = frames.seq();
    let mut gaps = Vec::new();
    while start.elapsed() < Duration::from_secs(5) {
        if frames.seq() != sequence {
            gaps.push(mark.elapsed().as_secs_f64() * 1000.0);
            mark = Instant::now();
            sequence = frames.seq();
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    gaps.sort_by(f64::total_cmp);
    let stats = session.stats();
    let keys = session.recovery_log().keyframes;
    session.stop();
    eprintln!("NOISE cap={cap} rtt={rtt} request={requested} decoded={} gap_p50/p95_ms={:.3}/{:.3} keys={keys} stats={stats:?}", frames.seq(), gaps[gaps.len()/2], gaps[gaps.len()*95/100]);
}
