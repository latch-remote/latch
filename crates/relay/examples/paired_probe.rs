//! Bounded public transport probe. Both roles run here and connect outbound.
//! Usage: paired_probe HOST SECONDS MBPS FPS [WARMUP_SECONDS] [auto|native|websocket] [PROXY_HOST:PORT]
//! Payloads are synthetic whole frames. Capabilities and keys stay in memory.
use latch_proto::video::InFrame;
use latch_relay::{
    create_grant_with_info, Cancel, Carrier, ClientConfig, Endpoint, Event, Lane, ProxyConfig,
    SessionKeys, Transport,
};
use rustls::pki_types::{pem::PemObject, CertificateDer};
use serde_json::json;
use std::{
    env, io, thread,
    time::{Duration, Instant},
};

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

fn ready(transport: &Transport) -> io::Result<()> {
    match transport.recv_timeout(Duration::from_secs(10)) {
        Some(Event::Ready) => Ok(()),
        _ => Err(io::Error::other("pair did not become ready")),
    }
}

fn pair(
    config: &ClientConfig,
    bytes_per_second: u64,
) -> io::Result<(Transport, Transport, Duration)> {
    let (grant, info) = create_grant_with_info(config, &Cancel::new())?;
    // Renew well inside both finite limits, using a new grant and fresh keys.
    let seconds = (u64::from(grant.host.session_seconds) * 3 / 4)
        .min(grant.host.session_bytes / (3 * bytes_per_second));
    if seconds < 10 {
        return Err(invalid(
            "session allowance is too small for this probe rate",
        ));
    }
    let keys = SessionKeys::generate()?;
    let host = Transport::connect(config.clone(), grant.host, keys.clone())?;
    let viewer = Transport::connect(config.clone(), grant.viewer, keys)?;
    ready(&host)?;
    ready(&viewer)?;
    println!(
        "{}",
        json!({"phase":"connected", "grant_carrier":format!("{:?}", info.carrier), "via_proxy":info.via_proxy})
    );
    for transport in [&host, &viewer] {
        for lane in [Lane::Priority, Lane::Video] {
            let info = transport
                .connection_info(lane)
                .ok_or_else(|| io::Error::other("missing lane diagnostics"))?;
            if config
                .carrier
                .is_some_and(|carrier| carrier != info.carrier)
            {
                return Err(io::Error::other("forced carrier did not match lane"));
            }
            println!(
                "{}",
                json!({"phase":"lane", "lane":format!("{lane:?}"), "carrier":format!("{:?}", info.carrier), "via_proxy":info.via_proxy})
            );
        }
    }
    Ok((host, viewer, Duration::from_secs(seconds)))
}

fn percentile(samples: &mut [u64], percent: usize) -> u64 {
    samples.sort_unstable();
    samples
        .get(samples.len().saturating_sub(1) * percent / 100)
        .copied()
        .unwrap_or(0)
}

#[derive(Default)]
struct Stats {
    full_rate_start: Option<(Instant, u64)>,
    sent: u64,
    received: u64,
    bytes: u64,
    dropped: u64,
    priority_echoes: u64,
    control_echoes: u64,
    ages_us: Vec<u64>,
    acks_us: Vec<u64>,
    priority_us: Vec<u64>,
    max_queue_us: u64,
}

fn report(
    stats: &mut Stats,
    start: Instant,
    previous: &mut (Instant, u64),
    rotations: u64,
    final_report: bool,
) {
    let now = Instant::now();
    let interval = now.duration_since(previous.0).as_secs_f64();
    let rate = (stats.bytes - previous.1) as f64 * 8.0 / interval / 1e6;
    println!(
        "{}",
        json!({
            "elapsed_s":start.elapsed().as_secs_f64(), "final":final_report,
            "sent_frames":stats.sent, "received_frames":stats.received,
            "received_bytes":stats.bytes, "source_dropped_frames":stats.dropped,
        "accepted_not_received":stats.sent.saturating_sub(stats.received),
            "received_mbps":rate, "session_rotations":rotations,
            "payload_mbps":stats.bytes as f64 * 8.0 / start.elapsed().as_secs_f64() / 1e6,
            "post_warmup_payload_mbps":stats.full_rate_start.map(|(time, bytes)|
                (stats.bytes - bytes) as f64 * 8.0 / time.elapsed().as_secs_f64() / 1e6),
            "frame_age_p50_ms":percentile(&mut stats.ages_us,50) as f64 / 1000.0,
            "frame_age_p95_ms":percentile(&mut stats.ages_us,95) as f64 / 1000.0,
            "frame_age_p99_ms":percentile(&mut stats.ages_us,99) as f64 / 1000.0,
            "frame_age_max_ms":stats.ages_us.last().copied().unwrap_or(0) as f64 / 1000.0,
            "ack_samples":stats.acks_us.len(),
            "ack_p95_ms":percentile(&mut stats.acks_us,95) as f64 / 1000.0,
            "ack_max_ms":stats.acks_us.last().copied().unwrap_or(0) as f64 / 1000.0,
            "max_unsent_queue_ms":stats.max_queue_us as f64 / 1000.0,
            "priority_echoes":stats.priority_echoes,"control_echoes":stats.control_echoes,
            "priority_rtt_p95_ms":percentile(&mut stats.priority_us,95) as f64 / 1000.0,
        })
    );
    // Keep cumulative distributions so the final drain cannot replace a run's
    // latency measurements with just its last few frames.
    *previous = (now, stats.bytes);
}

fn parse_carrier(value: &str) -> io::Result<Option<Carrier>> {
    match value {
        "auto" => Ok(None),
        "native" => Ok(Some(Carrier::Native)),
        "websocket" => Ok(Some(Carrier::WebSocket)),
        _ => Err(invalid("carrier must be auto, native or websocket")),
    }
}

fn run() -> io::Result<()> {
    let args: Vec<String> = env::args().skip(1).collect();
    if !(4..=7).contains(&args.len()) {
        return Err(invalid(
            "usage: paired_probe HOST SECONDS MBPS FPS [WARMUP_SECONDS] [auto|native|websocket] [PROXY_HOST:PORT]",
        ));
    }
    let seconds: u64 = args[1].parse().map_err(|_| invalid("invalid seconds"))?;
    let mbps: u64 = args[2].parse().map_err(|_| invalid("invalid Mbps"))?;
    let fps: u32 = args[3].parse().map_err(|_| invalid("invalid FPS"))?;
    let warmup: u64 = args
        .get(4)
        .map_or(Ok(0), |value| value.parse())
        .map_err(|_| invalid("invalid warmup seconds"))?;
    if !(1..=3600).contains(&seconds) || !(1..=200).contains(&mbps) || !(1..=120).contains(&fps) {
        return Err(invalid("bounds: 1..3600 seconds, 1..200 Mbps, 1..120 FPS"));
    }
    if warmup > 60 || warmup >= seconds {
        return Err(invalid(
            "warmup must be 0..60 seconds and shorter than the run",
        ));
    }
    let bytes_per_second = mbps * 1_000_000 / 8;
    // 25% headroom beyond relay ingress plus egress, for a <=50 GB test budget.
    if bytes_per_second * seconds * 5 / 2 > 50_000_000_000 {
        return Err(invalid(
            "probe exceeds the 50 GB conservative transfer ceiling",
        ));
    }
    let size = (bytes_per_second / u64::from(fps)) as usize;
    let warm_size = (size / 4).max(12);
    let mut config = ClientConfig::production(Endpoint::new(&args[0], 443)?)?;
    // Disposable netlab CA, with hostname and public-address checks retained.
    if let Some(path) = env::var_os("PAIRED_PROBE_CA") {
        let pem = std::fs::read(path)?;
        let mut roots = rustls::RootCertStore::empty();
        for cert in CertificateDer::pem_slice_iter(&pem) {
            roots
                .add(cert.map_err(|_| invalid("invalid probe CA PEM"))?)
                .map_err(|_| invalid("invalid probe CA certificate"))?;
        }
        config = config.with_tls_roots(roots)?;
    }
    config.carrier = parse_carrier(args.get(5).map_or("auto", String::as_str))?;
    if let Some(proxy) = args.get(6) {
        let (host, port) = proxy
            .rsplit_once(':')
            .ok_or_else(|| invalid("proxy must be HOST:PORT"))?;
        config.proxy = Some(ProxyConfig::new(
            host,
            port.parse().map_err(|_| invalid("invalid proxy port"))?,
        )?);
    }
    if !(12..=config.limits.max_frame_bytes).contains(&size) {
        return Err(invalid("frame size exceeds the supported range"));
    }
    let mut data: Vec<u8> = (0..size)
        .map(|i| (i.wrapping_mul(73) % 251) as u8)
        .collect();
    let expected = data.clone();
    let (mut host, mut viewer, mut renew_after) = pair(&config, bytes_per_second)?;
    let start = Instant::now();
    let period = Duration::from_secs_f64(1.0 / f64::from(fps));
    let mut next_frame = start;
    let mut next_ping = start;
    let mut session_started = start;
    let mut previous = (start, 0);
    let mut stats = Stats::default();
    let mut sequence = 0u32;
    let mut rotations = 0;
    let mut full_rate_announced = false;
    let mut last_received = None;
    println!(
        "{}",
        json!({"phase":"ready","host":args[0],"seconds":seconds,"target_mbps":mbps,
        "fps":fps,"frame_bytes":size,"stall_ms":config.limits.stall_timeout.as_millis(),
        "roles":"two roles in one process, both outbound to public endpoint",
        "warmup_seconds_per_pair":warmup,"warmup_rate_fraction":0.25})
    );
    while start.elapsed() < Duration::from_secs(seconds + 2) {
        while let Some(event) = viewer.recv_timeout(Duration::ZERO) {
            match event {
                Event::Frame(frame) => {
                    let len = frame.data.len();
                    if (len != size && len != warm_size)
                        || frame.data[12..] != expected[12..len]
                        || u32::from_be_bytes(frame.data[8..12].try_into().unwrap()) != frame.frame
                        || last_received.is_some_and(|last| frame.frame <= last)
                    {
                        return Err(io::Error::other("whole-frame integrity or ordering failed"));
                    }
                    let stamped = u64::from_be_bytes(frame.data[..8].try_into().unwrap());
                    stats
                        .ages_us
                        .push((start.elapsed().as_micros() as u64).saturating_sub(stamped));
                    stats.received += 1;
                    stats.bytes += frame.data.len() as u64;
                    last_received = Some(frame.frame);
                }
                Event::Datagram(bytes) => viewer.try_send_datagram(&bytes)?,
                Event::Control(bytes) => viewer.try_send_control(&bytes)?,
                Event::Closed { kind, message } => {
                    report(&mut stats, start, &mut previous, rotations, true);
                    return Err(io::Error::new(
                        kind,
                        format!("viewer transport closed: {message}"),
                    ));
                }
                Event::Ready => return Err(io::Error::other("duplicate viewer ready")),
            }
        }
        while let Some(event) = host.recv_timeout(Duration::ZERO) {
            match event {
                Event::Datagram(bytes) if bytes.len() == 8 => {
                    let stamped = u64::from_be_bytes(bytes[..].try_into().unwrap());
                    stats
                        .priority_us
                        .push((start.elapsed().as_micros() as u64).saturating_sub(stamped));
                    stats.priority_echoes += 1;
                }
                Event::Control(bytes) if bytes == b"paired-probe-control" => {
                    stats.control_echoes += 1
                }
                Event::Closed { kind, message } => {
                    report(&mut stats, start, &mut previous, rotations, true);
                    return Err(io::Error::new(
                        kind,
                        format!("host transport closed: {message}"),
                    ));
                }
                _ => return Err(io::Error::other("unexpected host event")),
            }
        }
        if let Some(elapsed) = host.take_frame_ack_elapsed() {
            stats.acks_us.push(elapsed.as_micros() as u64);
        }
        stats.max_queue_us = stats
            .max_queue_us
            .max(host.queued_frame_age().as_micros() as u64);
        let now = Instant::now();
        if start.elapsed() >= Duration::from_secs(seconds) {
            if stats.received == stats.sent {
                break;
            }
            thread::sleep(Duration::from_millis(1));
            continue;
        }
        let warming = session_started.elapsed() < Duration::from_secs(warmup);
        if !warming && !full_rate_announced {
            stats.full_rate_start = Some((now, stats.bytes));
            println!(
                "{}",
                json!({"phase":"full_rate","elapsed_s":start.elapsed().as_secs_f64(),"rotation":rotations})
            );
            full_rate_announced = true;
        }
        if now >= next_frame {
            let missed = (now.duration_since(next_frame).as_nanos() / period.as_nanos()) as u32;
            stats.dropped += u64::from(missed);
            sequence += missed;
            next_frame += period * (missed + 1);
            sequence += 1;
            data[..8].copy_from_slice(&(start.elapsed().as_micros() as u64).to_be_bytes());
            data[8..12].copy_from_slice(&sequence.to_be_bytes());
            let frame = InFrame {
                frame: sequence,
                data: data[..if warming { warm_size } else { size }].to_vec(),
                keyframe: true,
                recovery: false,
                host_delay: 0,
                assembly: Duration::ZERO,
            };
            match host.try_send_frame(&frame) {
                Ok(()) => stats.sent += 1,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => stats.dropped += 1,
                Err(error) => return Err(error),
            }
        }
        if now >= next_ping {
            host.try_send_datagram(&(start.elapsed().as_micros() as u64).to_be_bytes())?;
            host.try_send_control(b"paired-probe-control")?;
            next_ping += Duration::from_secs(1);
        }
        if previous.0.elapsed() >= Duration::from_secs(30) {
            report(&mut stats, start, &mut previous, rotations, false);
        }
        if session_started.elapsed() >= renew_after {
            host.shutdown();
            viewer.shutdown();
            (host, viewer, renew_after) = pair(&config, bytes_per_second)?;
            session_started = Instant::now();
            rotations += 1;
            full_rate_announced = false;
            next_frame = session_started;
            next_ping = session_started;
            println!(
                "{}",
                json!({"phase":"fresh_grant","elapsed_s":start.elapsed().as_secs_f64(),"rotation":rotations})
            );
        }
        thread::sleep(Duration::from_millis(1));
    }
    report(&mut stats, start, &mut previous, rotations, true);
    host.shutdown();
    viewer.shutdown();
    if stats.received == 0 || stats.priority_echoes == 0 || stats.control_echoes == 0 {
        return Err(io::Error::other(
            "probe did not deliver frames and both priority echoes",
        ));
    }
    Ok(())
}
fn main() {
    if let Err(error) = run() {
        eprintln!("paired probe failed ({:?}): {error}", error.kind());
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn carrier_selection() {
        assert_eq!(parse_carrier("auto").unwrap(), None);
        assert_eq!(parse_carrier("native").unwrap(), Some(Carrier::Native));
        assert_eq!(
            parse_carrier("websocket").unwrap(),
            Some(Carrier::WebSocket)
        );
        assert!(parse_carrier("unknown").is_err());
    }
    #[test]
    fn bounded_percentiles() {
        assert_eq!(percentile(&mut [], 95), 0);
        assert_eq!(percentile(&mut [9, 1, 5], 50), 5);
        assert_eq!(percentile(&mut [9, 1, 5], 100), 9);
    }
}
