use super::*;
use latch_proto::{
    audio::AudioPacketizer,
    wire::{self, Datagram},
};

#[path = "audio_repair_reference.rs"]
mod reference;

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
    let mut rng = 0x9e37_79b9_7f4a_7c15;
    let mut jitter_rng = 0x1234_5678_9abc_def0;
    let mut packetizer = AudioPacketizer::new(1);
    let mut coded = Vec::new();
    latch_codec::audio::encode(&[16_384; 480], &mut coded);
    let mut events = Vec::new();
    let mut last_capture = 0;
    let mut jitter = 0;
    for seq in 0..5000u32 {
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
            let lose = capture >= 1_000_000
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
struct Measurement {
    repaired: u64,
    concealed: u64,
    silence_ms: f64,
    discarded_ms: f64,
    mean_cushion_ms: f64,
    max_cushion_ms: f64,
    mean_queued_ms: f64,
    max_queued_ms: f64,
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
    let mut result = Measurement::default();
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
            mixer.fill_at(&mut out, now);
            if us >= window.start {
                result.silence_ms += out.iter().filter(|&&s| s == 0.0).count() as f64 / 96.0;
                let cushion = mixer.cushion(242) as f64 / 96.0;
                let queued = (before + p.queue.lock().len()) as f64 / 2.0 / 96.0 + 2.5;
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
