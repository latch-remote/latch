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
        queued_end: 0,
        reservations: Default::default(),
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

#[derive(Clone, Copy)]
struct CapturePath {
    callback_samples: u32,
    host_ppm: f64,
    heavy_tail: bool,
}

fn tape_for(
    scenario: Scenario,
    seconds: u32,
    loss_window: std::ops::Range<u64>,
) -> Vec<(u64, Vec<u8>)> {
    tape_on_path(scenario, seconds, loss_window, None)
}

fn tape_on_path(
    scenario: Scenario,
    seconds: u32,
    loss_window: std::ops::Range<u64>,
    path: Option<CapturePath>,
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
        let capture = if let Some(path) = path {
            let callback = ((seq + 1) * 240).div_ceil(path.callback_samples);
            (f64::from(callback) * f64::from(path.callback_samples) * 1_000_000.0
                / 48_000.0
                / (1.0 + path.host_ppm * 1e-6))
                .round() as u64
        } else if scenario.burst {
            (u64::from((seq + 1) * 240 - 1) / 1024 + 1) * 21_333
        } else {
            u64::from(seq + 1) * 5000
        };
        if capture != last_capture {
            jitter = (random(&mut jitter_rng) * 1000.0) as u64;
            if path.is_some_and(|p| p.heavy_tail) && random(&mut jitter_rng) < 0.02 {
                jitter += 10_000 + (random(&mut jitter_rng) * 20_000.0) as u64;
            }
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
    silence_ms: f64,
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
    queue_history: Vec<(usize, usize, u64)>,
    samples: Vec<(u32, usize)>,
    missing: u64,
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
    pull_us: u64,
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
    let mut out = vec![0.0; (pull_us * 96 / 1000) as usize];
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
                receiver.push(&packet, arrival);
            }
        }
        if old {
            let _ = original.next_deadline();
            while let Some(frame) = original.pop(now) {
                match frame {
                    AudioOut::Frame { seq, payload, .. } => {
                        p.link.delivered.lock().push(seq);
                        p.push_at(&payload, now);
                    }
                    AudioOut::Missing { .. } => {
                        result.missing += 1;
                        p.push_at(&[], now);
                    }
                }
            }
        } else {
            let lost_before = p.lost;
            p.receive(&mut receiver, now);
            result.missing += p.lost.saturating_sub(lost_before);
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
        if us % pull_us == 0 {
            let before = p.queue.lock().len();
            if old {
                original_mixer.fill_at(&mut out, now);
            } else {
                mixer.fill_at(&mut out, now);
            }
            result.queue_history.push((
                before,
                p.queue.lock().len(),
                p.link.underruns.load(Ordering::Relaxed),
            ));
            for &sample in &out {
                let bits = sample.to_bits();
                if let Some((_, count)) =
                    result.samples.last_mut().filter(|(last, _)| *last == bits)
                {
                    *count += 1;
                } else {
                    result.samples.push((bits, 1));
                }
            }
            if us >= window.start {
                result.silence_ms += out.iter().filter(|&&s| s == 0.0).count() as f64 / 96.0;
                let (cushion, margin) = if old {
                    (
                        original_mixer.cushion(out.len() + 2),
                        original_mixer.margin_ms,
                    )
                } else {
                    (mixer.cushion(out.len() + 2), mixer.margin_ms)
                };
                let cushion = cushion as f64 / 96.0;
                let queued =
                    (before + p.queue.lock().len()) as f64 / 2.0 / 96.0 + pull_us as f64 / 1000.0;
                let second = &mut result.seconds[(us / 1_000_000) as usize];
                second.silence_ms += out.iter().filter(|&&s| s == 0.0).count() as f64 / 96.0;
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
    for &seq in p.reservations.keys() {
        receiver.finish_repair(seq, false);
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

// Queue/controller state is bit-exact. The damage sum uses a 1e-9 ms
// floating-point summation epsilon, with no audio/sample tolerance.
const DAMAGE_EPSILON_MS: f64 = 1e-9;

fn no_worse(old: &Measurement, new: &Measurement) {
    assert_eq!(
        new.queue_history, old.queue_history,
        "queue depth and controller underruns"
    );
    assert_eq!(new.mean_queued_ms, old.mean_queued_ms);
    assert_eq!(new.mean_cushion_ms, old.mean_cushion_ms);
    assert_eq!(new.mean_margin_ms, old.mean_margin_ms);
    assert_eq!(new.cap_hits, old.cap_hits);
    for (new, old) in new.seconds.iter().zip(&old.seconds) {
        assert_eq!(new.queued_ms, old.queued_ms);
        assert_eq!(new.cushion_ms, old.cushion_ms);
        assert_eq!(new.margin_ms, old.margin_ms);
    }
    let windows = |m: &Measurement| m.seconds.iter().filter(|s| s.silence_ms > 0.0).count();
    assert!(windows(new) <= windows(old), "underrun windows");
    assert!(
        new.concealed <= old.concealed,
        "concealments {} -> {}",
        old.concealed,
        new.concealed
    );
    assert!(
        new.damage_ms() <= old.damage_ms() + DAMAGE_EPSILON_MS,
        "damage {:.9} -> {:.9}",
        old.damage_ms(),
        new.damage_ms()
    );
    if old.missing == 0 && new.missing == 0 {
        assert_eq!(new.samples, old.samples, "lossless exact emitted samples");
    }
}

fn pair(
    events: &[(u64, Vec<u8>)],
    window: std::ops::Range<u64>,
    pull_us: u64,
) -> (Measurement, Measurement) {
    let old = evaluate(events, true, window.clone(), None, pull_us);
    let new = evaluate(events, false, window, None, pull_us);
    no_worse(&old, &new);
    (old, new)
}

fn mean_latency(m: &Measurement, range: std::ops::Range<usize>) -> (f64, f64, f64) {
    let seconds = &m.seconds[range];
    let n = seconds.len() as f64;
    (
        seconds.iter().map(|s| s.queued_ms).sum::<f64>() / n,
        seconds.iter().map(|s| s.cushion_ms).sum::<f64>() / n,
        seconds.iter().map(|s| s.margin_ms).sum::<f64>() / n,
    )
}

#[test]
fn seeded_loss_replays_real_receiver_and_playback_for_twenty_three_seconds() {
    for pull in [2500, 5000, 10_000, 20_000] {
        for (burst, name) in [(false, "steady"), (true, "21.333ms")] {
            for loss in [0.01, 0.05, 0.15, 0.0] {
                let scenario = Scenario {
                    name,
                    burst,
                    loss,
                    burst_loss: loss == 0.0,
                };
                let (old, new) = pair(&tape(scenario), 1_000_000..24_000_000, pull);
                if burst && !scenario.burst_loss {
                    assert!(new.damage_ms() < old.damage_ms());
                    assert!(new.repaired > old.repaired);
                }
                println!("MATRIX pull={pull} {} loss={loss} repaired {} -> {}, concealed {} -> {}, silence {:.3} -> {:.3}, discard {:.3} -> {:.3}, damage {:.3} -> {:.3}, queue {:.3} -> {:.3}, cushion {:.3} -> {:.3}, max {:.3} -> {:.3}", scenario.name, old.repaired, new.repaired, old.concealed, new.concealed, old.silence_ms, new.silence_ms, old.discarded_ms, new.discarded_ms, old.damage_ms(), new.damage_ms(), old.mean_queued_ms, new.mean_queued_ms, old.mean_cushion_ms, new.mean_cushion_ms, old.max_cushion_ms, new.max_cushion_ms);
            }
        }
    }
}

#[test]
fn isolated_losses_at_every_position_and_pull_size_never_add_damage() {
    for pull in [2500, 5000, 10_000, 20_000] {
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
                    let (old, new) = pair(&events, 1_000_000..24_000_000, pull);
                    println!("ISOLATED pull={pull} {name} pos={} parity_lost={lose_parity} damage {:.3} -> {:.3}, queue {:.3} -> {:.3}, repairs {} -> {}", position+1, old.damage_ms(), new.damage_ms(), old.mean_queued_ms, new.mean_queued_ms, old.repaired, new.repaired);
                }
            }
        }
    }
}

#[test]
fn loss_stops_at_twelve_seconds_and_latency_matches_main_through_ninety_six() {
    for pull in [2500, 5000, 10_000, 20_000] {
        for (burst, name) in [(false, "steady"), (true, "21.333ms")] {
            for loss in [0.01, 0.05, 0.15, 0.0] {
                let events = tape_for(
                    Scenario {
                        name,
                        burst,
                        loss,
                        burst_loss: loss == 0.0,
                    },
                    97,
                    1_000_000..12_000_000,
                );
                let (old, new) = pair(&events, 1_000_000..96_000_000, pull);
                for (label, range) in [
                    ("loss", 1..12),
                    ("+10s", 22..23),
                    ("+30s", 42..43),
                    ("+60s", 72..73),
                ] {
                    println!(
                        "RECOVERY pull={pull} {name} loss={loss} {label} {:?} -> {:?}",
                        mean_latency(&old, range.clone()),
                        mean_latency(&new, range)
                    );
                }
            }
        }
    }
}

#[test]
fn ten_loss_free_minutes_are_exact_at_all_pulls_cadences_and_drift_rates() {
    let started = Instant::now();
    for pull in [2500, 5000, 10_000, 20_000] {
        for callback_samples in [480, 1024] {
            for host_ppm in [15.625, -15.625, 50.0, -50.0, 100.0, -100.0] {
                let events = tape_on_path(
                    Scenario {
                        name: "clock drift",
                        burst: true,
                        loss: 0.0,
                        burst_loss: false,
                    },
                    602,
                    0..0,
                    Some(CapturePath {
                        callback_samples,
                        host_ppm,
                        heavy_tail: false,
                    }),
                );
                let (old, new) = pair(&events, 1_000_000..600_000_000, pull);
                assert_eq!(old.missing, 0);
                assert_eq!(new.missing, 0);
                let windows = old.seconds.iter().filter(|s| s.silence_ms > 0.0).count();
                println!("DRIFT pull={pull} samples={callback_samples} ppm={host_ppm} windows={windows} silence={:.3} queue={:.3} exact", old.silence_ms, old.mean_queued_ms);
            }
        }
    }
    println!("DRIFT_RUNTIME {:.3}s", started.elapsed().as_secs_f64());
}

#[test]
fn heavy_tailed_jitter_and_ten_ms_capture_never_add_damage_or_latency() {
    for pull in [2500, 5000, 10_000, 20_000] {
        for callback_samples in [480, 1024] {
            for host_ppm in [-100.0, 100.0] {
                for heavy_tail in [false, true] {
                    for loss in [0.0, 0.05] {
                        let events = tape_on_path(
                            Scenario {
                                name: "extended",
                                burst: true,
                                loss,
                                burst_loss: false,
                            },
                            25,
                            1_000_000..25_000_000,
                            Some(CapturePath {
                                callback_samples,
                                host_ppm,
                                heavy_tail,
                            }),
                        );
                        let (old, new) = pair(&events, 1_000_000..24_000_000, pull);
                        println!("PATH pull={pull} samples={callback_samples} ppm={host_ppm} tail={heavy_tail} loss={loss} damage {:.3} -> {:.3}, queue {:.3} -> {:.3}", old.damage_ms(), new.damage_ms(), old.mean_queued_ms, new.mean_queued_ms);
                        if heavy_tail && loss == 0.0 {
                            // Also replay the tail through a FIFO transport, ensuring
                            // no sequence gap at all for the exact differential.
                            let mut ordered = events.clone();
                            ordered.sort_by_key(|(_, bytes)| {
                                let Datagram::Audio { packet, .. } = wire::parse(bytes).unwrap()
                                else {
                                    panic!()
                                };
                                (
                                    packet.seq + if packet.parity { 3 } else { 0 },
                                    packet.parity,
                                )
                            });
                            let mut latest = 0;
                            for (at, _) in &mut ordered {
                                latest = latest.max(*at);
                                *at = latest;
                            }
                            let (old, new) = pair(&ordered, 1_000_000..24_000_000, pull);
                            assert_eq!(old.missing, 0);
                            assert_eq!(new.samples, old.samples);
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn pause_duplicates_and_stalls_keep_mains_controller_timeline() {
    let clean = tape_for(
        Scenario {
            name: "pause",
            burst: false,
            loss: 0.0,
            burst_loss: false,
        },
        97,
        0..0,
    );
    for pull in [2500, 5000, 10_000, 20_000] {
        for position in 0..4 {
            for lose_parity in [false, true] {
                let mut events: Vec<_> = clean
                    .iter()
                    .filter_map(|(at, bytes)| {
                        let Datagram::Audio { packet, .. } = wire::parse(bytes).unwrap() else {
                            panic!()
                        };
                        if (!packet.parity && packet.seq == 400 + position)
                            || (packet.parity && packet.seq == 400 && lose_parity)
                        {
                            return None;
                        }
                        let at = *at + if packet.seq >= 200 { 1_000_000 } else { 0 };
                        Some((
                            if (6_000_000..6_080_000).contains(&at) {
                                6_080_000
                            } else {
                                at
                            },
                            bytes.clone(),
                        ))
                    })
                    .collect();
                let duplicates: Vec<_> = events
                    .iter()
                    .map(|(at, bytes)| (at + 1000, bytes.clone()))
                    .collect();
                events.extend(duplicates);
                events.sort_by_key(|(at, _)| *at);
                pair(&events, 1_000_000..96_000_000, pull);
            }
        }
    }
}

#[test]
fn loss_free_steady_and_burst_capture_are_exact_at_every_pull_size() {
    for pull in [2500, 5000, 10_000, 20_000] {
        for burst in [false, true] {
            let events = tape(Scenario {
                name: "loss free",
                burst,
                loss: 0.0,
                burst_loss: false,
            });
            let (old, new) = pair(&events, 1_000_000..24_000_000, pull);
            assert_eq!(old.missing, 0);
            assert_eq!(new.samples, old.samples);
            println!(
                "LOSS_FREE pull={pull} burst={burst} queue={:.3} cushion={:.3} exact",
                old.mean_queued_ms, old.mean_cushion_ms
            );
        }
    }
}

#[test]
fn late_parity_replaces_only_a_complete_safely_queued_reservation() {
    for consume in [false, true] {
        let mut p = player();
        let mut receiver = AudioReceiver::with_format(1, 240, 48_000);
        let now = Instant::now();
        let mut coded = Vec::new();
        latch_codec::audio::encode(&[16_384; 480], &mut coded);
        for _ in 0..8 {
            p.push_at(&coded, now);
        }
        let mut packetizer = AudioPacketizer::new(1);
        let mut packets = Vec::new();
        for seq in 0..4 {
            packetizer.packetize(seq * 5000, &coded, |b| packets.push(b.to_vec()));
        }
        for bytes in &packets[1..4] {
            let Datagram::Audio { packet, .. } = wire::parse(bytes).unwrap() else {
                panic!()
            };
            receiver.push(&packet, now);
        }
        p.receive(&mut receiver, now);
        p.receive(&mut receiver, now + Duration::from_millis(20));
        let reserved = &p.reservations[&0];
        assert_eq!(reserved.deadline, now + Duration::from_millis(40));
        let start = reserved.start;
        let before = p.queue.lock().len();
        if consume {
            // Device consumption or main's restart trim invalidates a range.
            p.queue.lock().drain(..start as usize + 2);
        }
        let Datagram::Audio { packet, .. } = wire::parse(packets.last().unwrap()).unwrap() else {
            panic!()
        };
        receiver.push(&packet, now + Duration::from_millis(26));
        p.receive(&mut receiver, now + Duration::from_millis(26));
        assert!(p.reservations.is_empty());
        assert_eq!(p.lost, u64::from(consume));
        if !consume {
            let q = p.queue.lock();
            assert_eq!(q.len(), before);
            assert!(q.iter().skip(start as usize).take(480).all(|&s| s == 0.5));
        }
        assert_eq!(receiver.stats().2, u64::from(consume));
    }
}

#[test]
fn pause_then_each_loss_with_thirteen_ms_cushion_matches_main() {
    let clean = tape(Scenario {
        name: "pause",
        burst: false,
        loss: 0.0,
        burst_loss: false,
    });
    for group in [200, 400] {
        for position in 0..4 {
            for lose_parity in [false, true] {
                let events: Vec<_> = clean
                    .iter()
                    .filter_map(|(at, bytes)| {
                        let Datagram::Audio { packet, .. } = wire::parse(bytes).unwrap() else {
                            panic!()
                        };
                        if (!packet.parity && packet.seq == group + position)
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
                let old = evaluate(&events, true, start..4_500_000, Some(13 * 96), 2500);
                let new = evaluate(&events, false, start..4_500_000, Some(13 * 96), 2500);
                no_worse(&old, &new);
            }
        }
    }
}

#[test]
fn single_stall_and_stall_starting_during_a_repair_wait_match_main() {
    for burst in [false, true] {
        let clean = tape_for(
            Scenario {
                name: "stall",
                burst,
                loss: 0.0,
                burst_loss: false,
            },
            97,
            0..0,
        );
        for stall in [6_000_000..6_080_000, 2_016_000..2_096_000] {
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
                        if stall.contains(at) { stall.end } else { *at },
                        bytes.clone(),
                    ))
                })
                .collect();
            events.sort_by_key(|(at, _)| *at);
            for pull in [2500, 5000, 10_000, 20_000] {
                pair(&events, 1_000_000..96_000_000, pull);
            }
        }
    }
}

#[test]
fn distinct_burst_frames_play_in_their_original_slots_after_late_repair() {
    const LOST: usize = 32;
    let frames: Vec<Vec<i16>> = (0..48)
        .map(|seq| {
            (0..480)
                .map(|i| (seq * 512 + i * 2 - 16_000) as i16)
                .collect()
        })
        .collect();
    let original: Vec<f32> = frames
        .iter()
        .flatten()
        .map(|&s| f32::from(s) / 32768.0)
        .collect();
    let mut packetizer = AudioPacketizer::new(1);
    let mut tape = Vec::new();
    for (seq, frame) in frames.iter().enumerate() {
        let mut coded = Vec::new();
        latch_codec::audio::encode(frame, &mut coded);
        // The modeled 1024-sample callbacks put packets 32/33 in one
        // burst and 34/35 plus parity in the next, 21.333 ms later.
        let at = ((seq + 1) * 240).div_ceil(1024) as u64 * 21_333 + 5000;
        assert!(packetizer.packetize(seq as u32 * 5000, &coded, |bytes| tape
            .push((at, bytes.to_vec()))));
    }
    let mut concealed_playback = None;
    // First establish the parity-lost reference, then repair, then deliver
    // parity after the frame has already played and its bound has expired.
    for parity_delay in [None, Some(0), Some(30_000)] {
        let mut events: Vec<_> = tape
            .iter()
            .filter_map(|(at, bytes)| {
                let Datagram::Audio { packet, .. } = wire::parse(bytes).unwrap() else {
                    panic!()
                };
                if !packet.parity && packet.seq == LOST as u32 {
                    return None;
                }
                let at = if packet.parity && packet.seq == LOST as u32 {
                    at + parity_delay?
                } else {
                    *at
                };
                Some((at, bytes.clone()))
            })
            .collect();
        events.sort_by_key(|(at, _)| *at);
        let mut events = events.iter().peekable();
        let mut p = player();
        let mut receiver = AudioReceiver::with_format(1, 240, 48_000);
        let mut mixer = Mixer::new(
            48_000,
            2,
            48_000,
            2,
            p.queue.clone(),
            p.pulled.clone(),
            Some(40 * 96),
            p.gain.clone(),
        )
        .linked(p.link.clone());
        let base = Instant::now();
        let mut played = Vec::new();
        let mut first_sample = None;
        let mut reserved_start = None;
        let mut reserved_at = None;
        let mut out = [0.0; 240];
        for us in (0..400_000).step_by(500) {
            let now = base + Duration::from_micros(us);
            while let Some((at, bytes)) = events.next_if(|(at, _)| *at <= us) {
                let Datagram::Audio { packet, .. } = wire::parse(bytes).unwrap() else {
                    panic!()
                };
                if packet.parity && packet.seq == LOST as u32 {
                    if parity_delay == Some(0) {
                        let reserved = &p.reservations[&(LOST as u32)];
                        // The normal 20 ms cutoff already released the gap
                        // on a previous tick, before this callback's parity.
                        assert!(now > reserved_at.unwrap());
                        assert!(now < reserved.deadline);
                    } else {
                        assert!(!p.reservations.contains_key(&(LOST as u32)));
                        assert!(
                            p.queued_end - p.queue.lock().len() as u64 > reserved_start.unwrap()
                        );
                    }
                }
                receiver.push(&packet, base + Duration::from_micros(*at));
            }
            p.receive(&mut receiver, now);
            if let Some(reserved) = p.reservations.get(&(LOST as u32)) {
                reserved_start = Some(reserved.start);
                reserved_at.get_or_insert(now);
            }
            if us % 2500 == 0 {
                mixer.fill_at(&mut out, now);
                if first_sample.is_none() && mixer.started {
                    // At unity rate the interpolation delays by one stereo
                    // sample frame. All subsequent samples are bit-exact PCM.
                    first_sample = Some(played.len() + 2);
                }
                played.extend_from_slice(&out);
            }
        }
        assert!(reserved_start.is_some());
        let start = first_sample.unwrap();
        let actual = &played[start..start + original.len()];
        let mut expected = original.clone();
        if parity_delay != Some(0) {
            for i in 0..480 {
                expected[LOST * 480 + i] =
                    original[(LOST - 1) * 480 + i] * (0.5 * (1.0 - i as f32 / 480.0));
            }
        }
        let bits = |samples: &[f32]| samples.iter().map(|s| s.to_bits()).collect::<Vec<_>>();
        for (index, (&actual, &expected)) in actual.iter().zip(&expected).enumerate() {
            assert_eq!(
                actual.to_bits(),
                expected.to_bits(),
                "parity delay {parity_delay:?}, played sample {index}"
            );
        }
        // Pin both neighbours separately as well as the entire played stream.
        for neighbour in [LOST - 1, LOST + 1] {
            let range = neighbour * 480..(neighbour + 1) * 480;
            assert_eq!(bits(&actual[range.clone()]), bits(&original[range]));
        }
        assert_eq!(p.lost, u64::from(parity_delay != Some(0)));
        if parity_delay.is_none() {
            concealed_playback = Some(bits(&played));
        }
        if parity_delay == Some(30_000) {
            assert_eq!(bits(&played), *concealed_playback.as_ref().unwrap());
        }
    }
}

#[test]
fn player_play_deadline_expires_even_when_the_device_keeps_the_full_range_queued() {
    let base = Instant::now();
    let mut p = player();
    let mut receiver = AudioReceiver::with_format(1, 240, 48_000);
    let mut coded = Vec::new();
    latch_codec::audio::encode(&[12_345; 480], &mut coded);
    // Ten milliseconds precede the missing frame. The device is then
    // suspended, so elapsed time and sample-range validity are independent.
    for _ in 0..2 {
        p.push_at(&coded, base);
    }
    let mut packetizer = AudioPacketizer::new(1);
    let mut packets = Vec::new();
    for seq in 0..4 {
        packetizer.packetize(seq * 5000, &coded, |bytes| packets.push(bytes.to_vec()));
    }
    for bytes in &packets[1..4] {
        let Datagram::Audio { packet, .. } = wire::parse(bytes).unwrap() else {
            panic!()
        };
        receiver.push(&packet, base);
    }
    p.receive(&mut receiver, base);
    let release = base + Duration::from_millis(20);
    p.receive(&mut receiver, release);
    let reserved = &p.reservations[&0];
    let deadline = reserved.deadline;
    let arrival = base + Duration::from_millis(30);
    assert!(
        arrival < release + latch_proto::audio::AUDIO_REPAIR_EXTENSION,
        "the hard cap must still allow repair"
    );
    let before: Vec<_> = p.queue.lock().iter().map(|s| s.to_bits()).collect();
    let front = p.queued_end - before.len() as u64;
    assert!(
        reserved.start >= front + 97,
        "the unchanged atomic range check must still allow repair"
    );
    assert!(reserved.end <= p.queued_end);
    let Datagram::Audio { packet, .. } = wire::parse(packets.last().unwrap()).unwrap() else {
        panic!()
    };
    receiver.push(&packet, arrival);
    p.receive(&mut receiver, arrival);
    assert_eq!(
        p.lost, 1,
        "the player-side time budget must reject this repair"
    );
    assert_eq!(
        p.queue
            .lock()
            .iter()
            .map(|s| s.to_bits())
            .collect::<Vec<_>>(),
        before
    );
    assert!(deadline < arrival);
    assert!(p.reservations.is_empty());
    assert_eq!(receiver.stats().2, 1);
}
