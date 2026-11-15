use super::*;
use latch_proto::{
    audio::AudioPacketizer,
    wire::{self, Datagram},
};

#[path = "audio_repair_reference.rs"]
mod reference;

#[path = "audio_playback_reference.rs"]
mod playback_reference;

fn player() -> Player {
    let (stop, _) = mpsc::channel();
    Player {
        channels: 2,
        rate: 48_000,
        samples_per_frame: 240,
        pcm: Vec::new(),
        link: Arc::default(),
        arrivals: Arrivals::default(),
        arrival_seq: None,
        arrival_seen: 0,
        resume_until: None,
        last: Vec::new(),
        queue: Arc::default(),
        _output_thread: stop,
        output: Arc::new(Mutex::new(Output::Opening)),
        pulled: Arc::default(),
        gain: Arc::new(AtomicU32::new(1.0f32.to_bits())),
        max_queued: 96 * MAX_QUEUED_MS,
        packets: 0,
        lost: 0,
        undecodable: 0,
    }
}

#[derive(Clone, Copy)]
struct Scenario {
    name: &'static str,
    burst: bool,
    loss: f64,
    burst_loss: bool,
}

fn random(state: &mut u64) -> f64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    (*state >> 11) as f64 / (1u64 << 53) as f64
}

fn tape(scenario: Scenario) -> Vec<(u64, Vec<u8>)> {
    tape_for(scenario, 25, 1_000_000..25_000_000)
}

fn tape_for(
    scenario: Scenario,
    seconds: u32,
    loss_window: std::ops::Range<u64>,
) -> Vec<(u64, Vec<u8>)> {
    let mut rng = 0x9e37_79b9_7f4a_7c15;
    let mut jitter_rng = 0x1234_5678_9abc_def0;
    let mut packetizer = AudioPacketizer::new(1);
    let mut coded = Vec::new();
    latch_codec::audio::encode(&[16_384; 480], &mut coded);
    let mut events = Vec::new();
    let mut last_capture = 0;
    let mut jitter = 0;
    for seq in 0..seconds * 200 {
        let capture = if scenario.burst {
            (u64::from((seq + 1) * 240 - 1) / 1024 + 1) * 21_333
        } else {
            u64::from(seq + 1) * 5000
        };
        if capture != last_capture {
            jitter = (random(&mut jitter_rng) * 1000.0) as u64;
            last_capture = capture;
        }
        packetizer.packetize(seq * 5000, &coded, |bytes| {
            let draw = random(&mut rng);
            let lose = loss_window.contains(&capture)
                && (draw < scenario.loss || (scenario.burst_loss && capture % 400_000 < 16_000));
            if !lose {
                events.push((capture + 5000 + jitter, bytes.to_vec()));
            }
        });
    }
    events.sort_by_key(|(at, _)| *at);
    events
}

#[derive(Debug, Default)]
struct Second {
    queued_ms: f64,
    cushion_ms: f64,
    margin_ms: f64,
    pulls: u32,
}

#[derive(Debug, Default)]
struct Measurement {
    repaired: u64,
    concealed: u64,
    silence_ms: f64,
    discarded_ms: f64,
    mean_cushion_ms: f64,
    max_cushion_ms: f64,
    mean_queued_ms: f64,
    max_queued_ms: f64,
    mean_margin_ms: f64,
    cap_hits: u64,
    seconds: Vec<Second>,
}

impl Measurement {
    fn damage_ms(&self) -> f64 {
        self.silence_ms + self.discarded_ms + self.concealed as f64 * 5.0
    }
}

fn evaluate(
    events: &[(u64, Vec<u8>)],
    old: bool,
    window: std::ops::Range<u64>,
    fixed: Option<usize>,
) -> Measurement {
    let mut p = player();
    let mut mixer = Mixer::new(
        48_000,
        2,
        48_000,
        2,
        p.queue.clone(),
        p.pulled.clone(),
        fixed,
        p.gain.clone(),
    )
    .linked(p.link.clone());
    let mut original_mixer = playback_reference::Mixer::new(
        48_000,
        2,
        48_000,
        2,
        p.queue.clone(),
        p.pulled.clone(),
        fixed,
        p.gain.clone(),
    )
    .linked(p.link.clone());
    if let Some(samples) = fixed {
        p.link
            .target_us
            .store((samples as f64 / 96.0 * 1000.0) as u32, Ordering::Relaxed);
    }
    let mut receiver = AudioReceiver::with_format(1, 240, 48_000);
    let mut original = reference::AudioReceiver::with_format(1, 240, 48_000);
    let base = Instant::now();
    let original_data: std::collections::HashSet<u32> = events
        .iter()
        .filter_map(|(_, bytes)| {
            let Datagram::Audio { packet, .. } = wire::parse(bytes).unwrap() else {
                panic!()
            };
            (!packet.parity).then_some(packet.seq)
        })
        .collect();
    let mut events = events.iter().peekable();
    let mut out = [0.0; 240];
    let mut result = Measurement {
        seconds: (0..window.end.div_ceil(1_000_000))
            .map(|_| Second::default())
            .collect(),
        ..Default::default()
    };
    let mut cushion_sum = 0.0;
    let mut queued_sum = 0.0;
    let mut pulls = 0;
    let mut initial_stats = (0, 0, 0);
    let mut initial_delivered = 0;
    let mut initial_discarded = 0;
    for us in (0..window.end).step_by(500) {
        let now = base + Duration::from_micros(us);
        while let Some((arrival, bytes)) = events.next_if(|(at, _)| *at <= us) {
            let Datagram::Audio { packet, .. } = wire::parse(bytes).unwrap() else {
                panic!()
            };
            let arrival = base + Duration::from_micros(*arrival);
            if old {
                original.push(&packet, arrival);
            } else {
                p.observe_packet(&packet, arrival);
                receiver.push(&packet, arrival);
            }
        }
        if old {
            let _ = original.next_deadline();
            while let Some(frame) = original.pop(now) {
                match frame {
                    AudioOut::Frame { seq, payload, .. } => {
                        p.link.delivered.lock().push(seq);
                        p.push_at(&payload, Some(now));
                    }
                    AudioOut::Missing { .. } => p.push_at(&[], Some(now)),
                }
            }
        } else {
            p.receive(&mut receiver, now);
        }
        if us == window.start {
            initial_stats = if old {
                original.stats()
            } else {
                receiver.stats()
            };
            initial_delivered = p.link.delivered.lock().len();
            initial_discarded = p.link.discarded_samples.load(Ordering::Relaxed);
        }
        if us % 2500 == 0 {
            let before = p.queue.lock().len();
            if old {
                original_mixer.fill_at(&mut out, now);
            } else {
                mixer.fill_at(&mut out, now);
            }
            if us >= window.start {
                result.silence_ms += out.iter().filter(|&&s| s == 0.0).count() as f64 / 96.0;
                let (cushion, margin) = if old {
                    (original_mixer.cushion(242), original_mixer.margin_ms)
                } else {
                    (mixer.cushion(242), mixer.margin_ms)
                };
                let cushion = cushion as f64 / 96.0;
                let queued = (before + p.queue.lock().len()) as f64 / 2.0 / 96.0 + 2.5;
                let second = &mut result.seconds[(us / 1_000_000) as usize];
                second.queued_ms += queued;
                second.cushion_ms += cushion;
                second.margin_ms += margin;
                second.pulls += 1;
                result.mean_margin_ms += margin;
                queued_sum += queued;
                result.max_queued_ms = result.max_queued_ms.max(queued);
                cushion_sum += cushion;
                pulls += 1;
                result.max_cushion_ms = result.max_cushion_ms.max(cushion);
            }
        }
    }
    let stats = if old {
        original.stats()
    } else {
        receiver.stats()
    };
    result.repaired = p.link.delivered.lock()[initial_delivered..]
        .iter()
        .filter(|seq| !original_data.contains(seq))
        .count() as u64;
    result.concealed = stats.2 - initial_stats.2;
    result.discarded_ms =
        (p.link.discarded_samples.load(Ordering::Relaxed) - initial_discarded) as f64 / 96.0;
    result.cap_hits = p.link.queue_cap_hits.load(Ordering::Relaxed);
    result.mean_margin_ms /= f64::from(pulls);
    for second in &mut result.seconds {
        if second.pulls > 0 {
            second.queued_ms /= f64::from(second.pulls);
            second.cushion_ms /= f64::from(second.pulls);
            second.margin_ms /= f64::from(second.pulls);
        }
    }
    result.mean_cushion_ms = cushion_sum / f64::from(pulls);
    result.mean_queued_ms = queued_sum / f64::from(pulls);
    result
}

#[test]
fn seeded_loss_replays_real_receiver_and_playback_for_twenty_three_seconds() {
    let mut results = Vec::new();
    for (burst, name) in [(false, "steady"), (true, "21.333ms")] {
        for loss in [0.01, 0.05, 0.15, 0.0] {
            let scenario = Scenario {
                name,
                burst,
                loss,
                burst_loss: loss == 0.0,
            };
            let events = tape(scenario);
            let old = evaluate(&events, true, 1_000_000..24_000_000, None);
            let new = evaluate(&events, false, 1_000_000..24_000_000, None);
            println!("{} {} | repaired {} -> {} | concealed {} -> {} | silence {:.2} -> {:.2} | discarded {:.2} -> {:.2} | cushion mean {:.2} -> {:.2}, max {:.2} -> {:.2} | queued mean {:.2} -> {:.2}, max {:.2} -> {:.2} | damage {:.2} -> {:.2}",
                scenario.name, if scenario.burst_loss { "bursty loss".to_string() } else { format!("{:.0}%", loss * 100.0) },
                old.repaired, new.repaired, old.concealed, new.concealed,
                old.silence_ms, new.silence_ms, old.discarded_ms, new.discarded_ms,
                old.mean_cushion_ms, new.mean_cushion_ms, old.max_cushion_ms, new.max_cushion_ms,
                old.mean_queued_ms, new.mean_queued_ms, old.max_queued_ms, new.max_queued_ms,
                old.damage_ms(), new.damage_ms());
            results.push((scenario, old, new));
        }
    }
    for (scenario, old, new) in results {
        assert!(
            new.damage_ms() <= old.damage_ms(),
            "{} {} damage {old:?} -> {new:?}",
            scenario.name,
            scenario.loss
        );
        assert!(
            new.mean_cushion_ms <= old.mean_cushion_ms,
            "{} {} cushion {old:?} -> {new:?}",
            scenario.name,
            scenario.loss
        );
        assert!(
            new.discarded_ms <= old.discarded_ms,
            "intact audio discarded"
        );
        assert!(new.mean_queued_ms <= old.mean_queued_ms + 5.0);
        assert_eq!(new.cap_hits, 0);
        assert_eq!(old.cap_hits, 0);
        if scenario.burst {
            assert!(new.damage_ms() < old.damage_ms());
        }
    }
}

#[test]
fn pause_then_each_loss_with_thirteen_ms_cushion_never_adds_audible_damage() {
    let clean = tape(Scenario {
        name: "steady",
        burst: false,
        loss: 0.0,
        burst_loss: false,
    });
    for group in [200, 400] {
        for lost in 0..4 {
            for lose_parity in [false, true] {
                let events: Vec<_> = clean
                    .iter()
                    .filter_map(|(at, bytes)| {
                        let Datagram::Audio { packet, .. } = wire::parse(bytes).unwrap() else {
                            panic!()
                        };
                        if (!packet.parity && packet.seq == group + lost)
                            || (packet.parity && packet.seq == group && lose_parity)
                        {
                            return None;
                        }
                        Some((
                            at + if packet.seq >= 200 { 1_000_000 } else { 0 },
                            bytes.clone(),
                        ))
                    })
                    .collect();
                let start = if group == 200 { 1_900_000 } else { 2_900_000 };
                let old = evaluate(&events, true, start..4_500_000, Some(13 * 96));
                let new = evaluate(&events, false, start..4_500_000, Some(13 * 96));
                assert!(
                    new.damage_ms() <= old.damage_ms(),
                    "position {} parity lost={} {old:?} -> {new:?}",
                    lost + 1,
                    lose_parity
                );
                assert!(new.discarded_ms <= old.discarded_ms);
                if lost == 3 && lose_parity {
                    assert!(new.silence_ms <= old.silence_ms);
                    if group == 400 {
                        println!("13ms cushion after pause, fourth packet and parity lost {old:?} -> {new:?}");
                        assert_eq!(new.discarded_ms, 0.0);
                    }
                    assert_eq!(new.concealed, 1);
                }
            }
        }
    }
}

#[test]
fn an_owned_repair_underrun_preserves_intact_samples_and_the_margin() {
    let p = player();
    p.link.jitter_us.store(1, Ordering::Relaxed);
    p.link.burst_us.store(5000, Ordering::Relaxed);
    let mut mixer = Mixer::new(
        48_000,
        2,
        48_000,
        2,
        p.queue.clone(),
        p.pulled.clone(),
        None,
        p.gain.clone(),
    )
    .linked(p.link.clone());
    let now = Instant::now();
    let mut out = [0.0; 240];
    p.queue.lock().extend([0.5; 96 * 20]);
    mixer.fill_at(&mut out, now);
    p.link.discarded_samples.store(0, Ordering::Relaxed);
    p.queue.lock().clear();
    p.link.repairing.store(true, Ordering::Relaxed);
    mixer.fill_at(&mut out, now + Duration::from_millis(10));
    assert!(mixer.dry_repair);
    p.link.repairing.store(false, Ordering::Relaxed);
    p.queue.lock().extend([0.5; 96 * 20]);
    mixer.fill_at(&mut out, now + Duration::from_micros(12_500));
    assert_eq!(p.link.discarded_samples.load(Ordering::Relaxed), 0);
    assert!(p.queue.lock().len() >= 96 * 16);
    assert_eq!(mixer.margin_ms, MARGIN_MS);
    assert_eq!(p.link.jitter_us.load(Ordering::Relaxed), 1);
}

#[test]
fn network_arrival_clock_ignores_duplicates_and_receiver_release_time() {
    let mut p = player();
    let now = Instant::now();
    let packet = |seq| AudioPacket {
        parity: false,
        seq,
        pts: seq * 5000,
        payload: &[],
    };
    p.observe_packet(&packet(0), now);
    p.observe_packet(&packet(1), now + Duration::from_millis(5));
    let media = p.arrivals.media;
    p.observe_packet(&packet(1), now + Duration::from_secs(1));
    assert_eq!(p.arrivals.last, Some(now + Duration::from_millis(5)));
    assert_eq!(p.arrivals.media, media);
    assert_eq!(p.resume_until, None);
    let mut coded = Vec::new();
    latch_codec::audio::encode(&[16_384; 480], &mut coded);
    for _ in 0..10 {
        p.push_at(&coded, None);
    }
    assert_eq!(p.arrivals.last, Some(now + Duration::from_millis(5)));
    assert_eq!(p.arrivals.media, media);
}

#[test]
fn loss_free_receiver_and_playback_add_no_hold_or_cushion() {
    for burst in [false, true] {
        let events = tape(Scenario {
            name: "loss free",
            burst,
            loss: 0.0,
            burst_loss: false,
        });
        let old = evaluate(&events, true, 1_000_000..24_000_000, None);
        let new = evaluate(&events, false, 1_000_000..24_000_000, None);
        assert_eq!(new.concealed, 0);
        assert_eq!(new.repaired, 0);
        assert_eq!(new.silence_ms, 0.0);
        assert_eq!(new.discarded_ms, 0.0);
        assert!(
            new.mean_cushion_ms <= old.mean_cushion_ms,
            "{burst} {old:?} -> {new:?}"
        );
        assert!(
            new.mean_queued_ms <= old.mean_queued_ms,
            "{burst} {old:?} -> {new:?}"
        );
    }
}

#[test]
fn reordered_originals_measure_network_lateness_once() {
    let mut p = player();
    let now = Instant::now();
    let packet = |seq| AudioPacket {
        parity: false,
        seq,
        pts: seq * 5000,
        payload: &[],
    };
    p.observe_packet(&packet(0), now);
    p.observe_packet(&packet(2), now + Duration::from_millis(10));
    let media = p.arrivals.media;
    p.observe_packet(&packet(1), now + Duration::from_millis(20));
    let block = p.arrivals.block.unwrap();
    assert!(block.2 >= 0.01499);
    assert_eq!(p.arrivals.media, media);
    p.observe_packet(&packet(1), now + Duration::from_secs(1));
    assert_eq!(p.arrivals.block.unwrap(), block);
    assert_eq!(p.resume_until, None);
    p.observe_packet(&packet(4), now + Duration::from_secs(2));
    let resume = p.resume_until;
    p.observe_packet(&packet(3), now + Duration::from_millis(2005));
    let block = p.arrivals.block.unwrap();
    assert!(block.2 >= 0.00999);
    p.observe_packet(&packet(2), now + Duration::from_secs(3));
    assert_eq!(p.arrivals.block.unwrap(), block);
    assert_eq!(p.resume_until, resume);
}

fn mean_latency(measurement: &Measurement, range: std::ops::Range<usize>) -> (f64, f64, f64) {
    let seconds = &measurement.seconds[range];
    let n = seconds.len() as f64;
    (
        seconds.iter().map(|s| s.queued_ms).sum::<f64>() / n,
        seconds.iter().map(|s| s.cushion_ms).sum::<f64>() / n,
        seconds.iter().map(|s| s.margin_ms).sum::<f64>() / n,
    )
}

fn assert_recovers(new: &Measurement, baseline: &Measurement, after: usize, name: &str) {
    assert_eq!(new.cap_hits, 0, "{name} hit the queue cap");
    for second in after..new.seconds.len() {
        let measured = &new.seconds[second];
        let clean = &baseline.seconds[second];
        assert!(
            (measured.queued_ms - clean.queued_ms).abs() <= 2.0,
            "{name} at {second}s queued {:.3}, baseline {:.3}",
            measured.queued_ms,
            clean.queued_ms
        );
        assert!(
            (measured.margin_ms - clean.margin_ms).abs() <= 2.0,
            "{name} at {second}s margin {:.3}, baseline {:.3}",
            measured.margin_ms,
            clean.margin_ms
        );
    }
}

#[test]
fn loss_stops_at_twelve_seconds_and_latency_recovers_within_ten_seconds() {
    for (burst, cadence) in [(false, "steady"), (true, "21.333ms")] {
        let clean = Scenario {
            name: cadence,
            burst,
            loss: 0.0,
            burst_loss: false,
        };
        let events = tape_for(clean, 97, 1_000_000..12_000_000);
        let baseline = evaluate(&events, false, 1_000_000..96_000_000, None);
        let old_baseline = evaluate(&events, true, 1_000_000..96_000_000, None);
        println!(
            "BASELINE {cadence} queued {:.3} -> {:.3}, margin {:.3} -> {:.3}",
            old_baseline.mean_queued_ms,
            baseline.mean_queued_ms,
            old_baseline.mean_margin_ms,
            baseline.mean_margin_ms
        );
        for at in [1, 12, 22, 32, 44, 72, 92] {
            println!(
                "BASELINE_AT {cadence} {at}s {:?} -> {:?}",
                mean_latency(&old_baseline, at..at + 1),
                mean_latency(&baseline, at..at + 1)
            );
        }
        assert!(baseline.mean_queued_ms <= old_baseline.mean_queued_ms);
        assert!(baseline.mean_margin_ms <= old_baseline.mean_margin_ms);
        assert_eq!(baseline.cap_hits, 0);
        assert_eq!(old_baseline.cap_hits, 0);
        for loss in [0.01, 0.05, 0.15, 0.0] {
            let scenario = Scenario {
                loss,
                burst_loss: loss == 0.0,
                ..clean
            };
            let events = tape_for(scenario, 97, 1_000_000..12_000_000);
            let old = evaluate(&events, true, 1_000_000..96_000_000, None);
            let new = evaluate(&events, false, 1_000_000..96_000_000, None);
            let name = format!(
                "{cadence} {}",
                if scenario.burst_loss {
                    "burst loss".to_string()
                } else {
                    format!("{:.0}%", loss * 100.0)
                }
            );
            for (label, range) in [
                ("loss", 1..12),
                ("+10s", 22..23),
                ("+30s", 42..43),
                ("+60s", 72..73),
            ] {
                println!(
                    "RECOVERY {name} {label} {:?} -> {:?}",
                    mean_latency(&old, range.clone()),
                    mean_latency(&new, range)
                );
            }
            assert!(
                mean_latency(&new, 1..12).0 <= mean_latency(&old, 1..12).0 + 5.0,
                "{name} loss latency"
            );
            assert_eq!(old.cap_hits, 0, "{name} reference cap");
            assert_recovers(&new, &baseline, 22, &name);
        }
    }
}

#[test]
fn isolated_loss_at_each_position_prioritizes_latency_with_bounded_damage() {
    for (burst, name) in [(false, "steady"), (true, "21.333ms")] {
        let clean = tape(Scenario {
            name,
            burst,
            loss: 0.0,
            burst_loss: false,
        });
        for position in 0..4 {
            for lose_parity in [false, true] {
                let mut losses = 0;
                let events: Vec<_> = clean
                    .iter()
                    .filter(|(_, bytes)| {
                        let Datagram::Audio { packet, .. } = wire::parse(bytes).unwrap() else {
                            panic!()
                        };
                        let group = packet.seq / 4;
                        let selected = group > 50 && group < 1200 && group.is_multiple_of(10);
                        let lost = selected
                            && ((!packet.parity && packet.seq % 4 == position)
                                || (packet.parity && lose_parity));
                        if lost && !packet.parity {
                            losses += 1;
                        }
                        !lost
                    })
                    .cloned()
                    .collect();
                assert_eq!(losses, 114);
                let old = evaluate(&events, true, 1_000_000..24_000_000, None);
                let new = evaluate(&events, false, 1_000_000..24_000_000, None);
                println!("ISOLATED {name} pos={} parity_lost={lose_parity} damage {:.3} -> {:.3}, queued {:.3} -> {:.3}, repaired {} -> {}, concealed {} -> {}",
                    position+1, old.damage_ms(), new.damage_ms(), old.mean_queued_ms, new.mean_queued_ms,
                    old.repaired, new.repaired, old.concealed, new.concealed);
                assert!(
                    new.mean_queued_ms <= old.mean_queued_ms,
                    "{name} pos={} parity_lost={lose_parity} latency {:.3} -> {:.3}",
                    position + 1,
                    old.mean_queued_ms,
                    new.mean_queued_ms
                );
                assert!(
                    new.damage_ms() <= old.damage_ms() + 20.0,
                    "{name} pos={} parity_lost={lose_parity} damage {:.3} -> {:.3}",
                    position + 1,
                    old.damage_ms(),
                    new.damage_ms()
                );
                assert_eq!(new.cap_hits, 0);
                assert_eq!(old.cap_hits, 0);
            }
        }
    }
}

#[test]
fn one_loss_followed_by_an_eighty_ms_network_stall_recovers() {
    for (burst, name) in [(false, "steady"), (true, "21.333ms")] {
        let clean = tape_for(
            Scenario {
                name,
                burst,
                loss: 0.0,
                burst_loss: false,
            },
            97,
            0..0,
        );
        let baseline = evaluate(&clean, false, 1_000_000..96_000_000, None);
        let mut events: Vec<_> = clean
            .iter()
            .filter_map(|(at, bytes)| {
                let Datagram::Audio { packet, .. } = wire::parse(bytes).unwrap() else {
                    panic!()
                };
                // Lose the original and parity so the first event conceals.
                if packet.seq == 400 {
                    return None;
                }
                Some((
                    if (6_000_000..6_080_000).contains(at) {
                        6_080_000
                    } else {
                        *at
                    },
                    bytes.clone(),
                ))
            })
            .collect();
        events.sort_by_key(|(at, _)| *at);
        let old = evaluate(&events, true, 1_000_000..96_000_000, None);
        let new = evaluate(&events, false, 1_000_000..96_000_000, None);
        for (label, range) in [
            ("stall", 1..7),
            ("+10s", 16..17),
            ("+30s", 36..37),
            ("+60s", 66..67),
        ] {
            println!(
                "STALL {name} {label} {:?} -> {:?}",
                mean_latency(&old, range.clone()),
                mean_latency(&new, range)
            );
        }
        assert!(mean_latency(&new, 1..7).0 <= mean_latency(&old, 1..7).0 + 5.0);
        assert_eq!(old.cap_hits, 0);
        assert_recovers(&new, &baseline, 16, name);
    }
}

#[test]
fn a_network_stall_outlasting_a_pending_repair_rebuilds_the_cushion() {
    let p = player();
    p.link.jitter_us.store(1, Ordering::Relaxed);
    p.link.burst_us.store(5000, Ordering::Relaxed);
    let mut mixer = Mixer::new(
        48_000,
        2,
        48_000,
        2,
        p.queue.clone(),
        p.pulled.clone(),
        None,
        p.gain.clone(),
    )
    .linked(p.link.clone());
    let now = Instant::now();
    let mut out = [0.0; 240];
    p.queue.lock().extend([0.5; 96 * 20]);
    mixer.fill_at(&mut out, now);
    p.queue.lock().clear();
    p.link.repairing.store(true, Ordering::Relaxed);
    mixer.fill_at(&mut out, now + Duration::from_millis(10));
    assert!(mixer.dry_repair);
    p.link.repairing.store(false, Ordering::Relaxed);
    p.queue.lock().extend([0.5; 96 * 80]);
    mixer.fill_at(&mut out, now + Duration::from_millis(90));
    assert!(!mixer.dry_repair);
    assert!(p.queue.lock().len() < 96 * 10);
    assert!(mixer.margin_ms <= MARGIN_MS + MAX_UNDERRUN_STEP_MS);
}

#[test]
fn exact_1024_sample_callbacks_reproduce_main_growth_and_the_controller_fix() {
    let mut events = tape_for(
        Scenario {
            name: "exact callback",
            burst: true,
            loss: 0.0,
            burst_loss: false,
        },
        97,
        0..0,
    );
    // Remove the rounded model's 15.6 ppm clock error. Keep the same
    // callback batching and seeded transit jitter.
    for (at, _) in &mut events {
        let callback = (*at - 5000) / 21_333;
        *at += callback * 1024 * 1_000_000 / 48_000 - callback * 21_333;
    }
    let old = evaluate(&events, true, 1_000_000..96_000_000, None);
    let new = evaluate(&events, false, 1_000_000..96_000_000, None);
    for second in [1, 12, 22, 32, 44, 72, 92] {
        println!(
            "EXACT_CALLBACK {second}s {:?} -> {:?}",
            mean_latency(&old, second..second + 1),
            mean_latency(&new, second..second + 1)
        );
    }
    assert!(old.seconds[22].queued_ms >= old.seconds[1].queued_ms + 25.0);
    assert!(new.seconds[22].queued_ms <= new.seconds[1].queued_ms + 2.0);
    assert_eq!(new.cap_hits, 0);
}

#[test]
fn a_stall_starting_during_a_real_receiver_wait_does_not_keep_the_backlog() {
    let clean = tape_for(
        Scenario {
            name: "pending then stall",
            burst: false,
            loss: 0.0,
            burst_loss: false,
        },
        97,
        0..0,
    );
    let baseline = evaluate(&clean, false, 1_000_000..96_000_000, None);
    let mut events: Vec<_> = clean
        .iter()
        .filter_map(|(at, bytes)| {
            let Datagram::Audio { packet, .. } = wire::parse(bytes).unwrap() else {
                panic!()
            };
            if packet.seq == 400 {
                return None;
            }
            Some((
                if (2_016_000..2_096_000).contains(at) {
                    2_096_000
                } else {
                    *at
                },
                bytes.clone(),
            ))
        })
        .collect();
    events.sort_by_key(|(at, _)| *at);
    let old = evaluate(&events, true, 1_000_000..96_000_000, None);
    let new = evaluate(&events, false, 1_000_000..96_000_000, None);
    println!(
        "PENDING_STALL 12s {:?} -> {:?}",
        mean_latency(&old, 12..13),
        mean_latency(&new, 12..13)
    );
    assert_recovers(&new, &baseline, 12, "pending repair then stall");
}
