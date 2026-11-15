use latch_proto::{
    audio::{AudioOut, AudioPacketizer, AudioReceiver},
    wire::{self, Datagram},
};
use std::time::{Duration, Instant};

// Unbudgeted callers must retain main's exact 20 ms/progress policy.
#[path = "../../stream/src/audio_repair_reference.rs"]
mod reference;

const GROUPS: u32 = 16;

fn schedule(spacing: [u64; 4], period: u64) -> Vec<u64> {
    (0..GROUPS * 4)
        .map(|seq| u64::from(seq / 4) * period + spacing[(seq % 4) as usize])
        .collect()
}

fn tape(arrivals: &[u64], parity_delay: u64) -> Vec<(u64, Vec<u8>)> {
    let mut packetizer = AudioPacketizer::new(1);
    let mut tape = Vec::new();
    for (seq, &arrival) in arrivals.iter().enumerate() {
        let seq = seq as u32;
        packetizer.packetize(seq * 5000, &[seq as u8], |bytes| {
            let Datagram::Audio { packet, .. } = wire::parse(bytes).unwrap() else {
                panic!()
            };
            tape.push((
                arrival + if packet.parity { parity_delay } else { 0 },
                bytes.to_vec(),
            ));
        });
    }
    tape.sort_by_key(|(arrival, _)| *arrival);
    tape
}

fn drain(receiver: &mut AudioReceiver, now: Instant, output: &mut Vec<AudioOut>) {
    while let Some(frame) = receiver.pop(now) {
        output.push(frame);
    }
}

fn replay(tape: Vec<(u64, Vec<u8>)>, lost: u32) {
    let start = Instant::now();
    let mut receiver = AudioReceiver::with_format(1, 240, 48_000);
    let mut output = Vec::new();
    let mut original = reference::AudioReceiver::with_format(1, 240, 48_000);
    let mut expected = Vec::new();
    let mut events = tape.iter().peekable();
    // Advance the injected clock even between arrivals, exercising quiet
    // receive-loop wakeups and expiry before a delayed parity arrives.
    for ms in 0..=tape.last().unwrap().0 + 100 {
        let now = start + Duration::from_millis(ms);
        while let Some((_, bytes)) = events.next_if(|(arrival, _)| *arrival == ms) {
            let Datagram::Audio { packet, .. } = wire::parse(bytes).unwrap() else {
                panic!()
            };
            if !packet.parity && packet.seq % 4 == lost {
                continue;
            }
            receiver.push(&packet, now);
            original.push(&packet, now);
            drain(&mut receiver, now, &mut output);
            while let Some(frame) = original.pop(now) {
                expected.push(frame);
            }
        }
        drain(&mut receiver, now, &mut output);
        while let Some(frame) = original.pop(now) {
            expected.push(frame);
        }
    }
    // Keep every cadence, delay and loss-position input, and require exact
    // parity with main on the restored legacy path, including expiry.
    let intact: Vec<_> = (0..GROUPS * 4)
        .map(|seq| AudioOut::Frame {
            seq,
            pts: seq * 5000,
            payload: vec![seq as u8],
        })
        .collect();
    assert_eq!(output, expected, "lost group position {}", lost + 1);
    assert_eq!(output.len(), intact.len());
    for (actual, full) in output.iter().zip(&intact) {
        if matches!(actual, AudioOut::Frame { .. }) {
            assert_eq!(actual, full);
        }
    }
    assert_eq!(receiver.stats(), original.stats());
    assert_eq!(original.next_deadline(), None);
    assert_eq!(receiver.next_deadline(), None);
}

#[test]
fn audio_repair_even_five_ms_spacing_at_every_position() {
    for lost in 0..4 {
        replay(tape(&schedule([0, 5, 10, 15], 20), 0), lost);
        // Parity delayed within the reordering allowance after group end.
        replay(tape(&schedule([0, 5, 10, 15], 20), 19), lost);
    }
}

#[test]
fn audio_repair_bursty_capture_at_every_position() {
    for lost in 0..4 {
        // Four together every 20 ms, and two together every 10 ms.
        for spacing in [[0, 0, 0, 0], [0, 0, 10, 10]] {
            replay(tape(&schedule(spacing, 20), 0), lost);
            replay(tape(&schedule(spacing, 20), 19), lost);
        }
    }
}

#[test]
fn audio_repair_jittered_spacing_at_every_position() {
    for lost in 0..4 {
        replay(tape(&schedule([0, 7, 18, 32], 40), 0), lost);
        replay(tape(&schedule([0, 7, 18, 32], 40), 19), lost);
    }
}

#[test]
fn audio_repair_no_loss_is_delivered_immediately() {
    let start = Instant::now();
    for spacing in [[0, 5, 10, 15], [0, 0, 0, 0], [0, 0, 10, 10], [0, 7, 18, 32]] {
        let mut receiver = AudioReceiver::with_format(1, 240, 48_000);
        for (ms, bytes) in tape(&schedule(spacing, 40), 0) {
            let now = start + Duration::from_millis(ms);
            let Datagram::Audio { packet, .. } = wire::parse(&bytes).unwrap() else {
                panic!()
            };
            receiver.push(&packet, now);
            if !packet.parity {
                assert_eq!(
                    receiver.pop(now),
                    Some(AudioOut::Frame {
                        seq: packet.seq,
                        pts: packet.pts,
                        payload: packet.payload.to_vec(),
                    })
                );
            }
            assert_eq!(receiver.pop(now), None);
            assert_eq!(receiver.next_deadline(), None);
        }
        assert_eq!(receiver.stats(), (u64::from(GROUPS * 4), 0, 0));
    }
}

#[test]
fn audio_repair_lost_parity_releases_at_hard_bound() {
    let start = Instant::now();
    let mut receiver = AudioReceiver::with_format(1, 240, 48_000);
    let mut output = Vec::new();
    // Packet zero and parity are lost. Even a growing capture interval
    // cannot extend the unbudgeted path beyond main's 20 ms bound.
    let packets = tape(&schedule([0, 0, 25, 45], 100), 0);
    let mut events = packets.iter().peekable();
    for ms in 0..20 {
        let now = start + Duration::from_millis(ms);
        while let Some((_, bytes)) = events.next_if(|(at, _)| *at == ms) {
            let Datagram::Audio { packet, .. } = wire::parse(bytes).unwrap() else {
                panic!()
            };
            if !packet.parity && packet.seq != 0 {
                receiver.push(&packet, now);
            }
        }
        drain(&mut receiver, now, &mut output);
        assert!(output.is_empty());
    }
    let bound = start + Duration::from_millis(20);
    assert_eq!(receiver.next_deadline(), Some(bound));
    assert_eq!(receiver.pop(bound - Duration::from_nanos(1)), None);
    assert_eq!(receiver.pop(bound), Some(AudioOut::Missing { seq: 0 }));
    drain(&mut receiver, bound, &mut output);
    assert_eq!(output.len(), 1);
    assert_eq!(receiver.stats(), (1, 0, 1));
    assert_eq!(receiver.next_deadline(), None);
}

#[test]
fn audio_repair_deadlines_follow_group_position() {
    let start = Instant::now();
    for (lost, deadline_ms) in [(0, 25), (1, 30), (2, 35), (3, 40)] {
        let mut receiver = AudioReceiver::with_format(1, 240, 48_000);
        let mut output = Vec::new();
        for (ms, bytes) in tape(&schedule([0, 5, 10, 15], 20), 0) {
            if ms > 20 {
                break;
            }
            let Datagram::Audio { packet, .. } = wire::parse(&bytes).unwrap() else {
                panic!()
            };
            if packet.parity || packet.seq == lost {
                continue;
            }
            let now = start + Duration::from_millis(ms);
            receiver.push(&packet, now);
            drain(&mut receiver, now, &mut output);
        }
        let deadline = start + Duration::from_millis(deadline_ms);
        assert_eq!(receiver.next_deadline(), Some(deadline));
        assert_eq!(receiver.pop(deadline - Duration::from_nanos(1)), None);
        assert_eq!(
            receiver.pop(deadline),
            Some(AudioOut::Missing { seq: lost })
        );
        drain(&mut receiver, deadline, &mut output);
        assert_eq!(receiver.next_deadline(), None);
        assert_eq!(receiver.stats().2, 1);
    }
}

#[test]
fn audio_repair_measured_capture_cadence_at_every_position() {
    // The local three-second probe observed a 21.321 ms median callback
    // interval. Model 1024 samples per callback at 48 kHz (21.333 ms),
    // yielding four or five 240-sample packets. Quantize arrivals upward
    // to the replay clock's milliseconds. The 1024-sample callback size
    // is an assumption used to model the measured cadence.
    let arrivals: Vec<_> = (0..GROUPS * 4)
        .map(|seq| {
            let callback = u64::from((seq + 1) * 240 - 1) / 1024;
            (callback * 21_333).div_ceil(1000)
        })
        .collect();
    for lost in 0..4 {
        replay(tape(&arrivals, 0), lost);
        replay(tape(&arrivals, 19), lost);
    }
}

#[test]
fn audio_repair_parity_before_reordered_data_at_every_position() {
    for lost in 0..4 {
        let reordered = if lost == 3 { 2 } else { 3 };
        let mut packets = tape(&schedule([0, 5, 10, 15], 20), 0);
        for (arrival, bytes) in &mut packets {
            let Datagram::Audio { packet, .. } = wire::parse(bytes).unwrap() else {
                panic!()
            };
            if !packet.parity && packet.seq % 4 == reordered {
                *arrival += 19;
            }
        }
        packets.sort_by_key(|(arrival, _)| *arrival);
        replay(packets, lost);
    }
}

#[test]
fn audio_repair_startup_allows_a_three_plus_one_capture_split() {
    for lost in 0..4 {
        replay(tape(&schedule([0, 0, 0, 22], 40), 5), lost);
    }
}

#[test]
fn audio_repair_duplicates_do_not_change_delivery_or_deadlines() {
    for lost in 0..4 {
        let mut packets = tape(&schedule([0, 5, 10, 15], 20), 19);
        let duplicates: Vec<_> = packets
            .iter()
            .map(|(at, bytes)| (at + 1, bytes.clone()))
            .collect();
        packets.extend(duplicates);
        packets.sort_by_key(|(at, _)| *at);
        replay(packets, lost);
    }
}

#[test]
fn audio_repair_pause_then_each_loss_position_with_and_without_parity() {
    let start = Instant::now();
    for pause in [0, 61, 1000] {
        for lost in 0..4 {
            for lose_parity in [false, true] {
                let mut packets = tape(&schedule([0, 5, 10, 15], 20), 0);
                for (at, bytes) in &mut packets {
                    let Datagram::Audio { packet, .. } = wire::parse(bytes).unwrap() else {
                        panic!()
                    };
                    if packet.seq >= 32 {
                        *at += pause;
                    }
                }
                let mut events = packets.iter().peekable();
                let mut receiver = AudioReceiver::with_format(1, 240, 48_000);
                let mut output = Vec::new();
                let mut concealed_at = None;
                for ms in 0..=packets.last().unwrap().0 + 100 {
                    let now = start + Duration::from_millis(ms);
                    while let Some((_, bytes)) = events.next_if(|(at, _)| *at == ms) {
                        let Datagram::Audio { packet, .. } = wire::parse(bytes).unwrap() else {
                            panic!()
                        };
                        if (!packet.parity && packet.seq == 32 + lost)
                            || (packet.parity && packet.seq == 32 && lose_parity)
                        {
                            continue;
                        }
                        receiver.push(&packet, now);
                    }
                    while let Some(frame) = receiver.pop(now) {
                        if matches!(frame, AudioOut::Missing { .. }) {
                            assert_eq!(frame, AudioOut::Missing { seq: 32 + lost });
                            concealed_at = Some(ms);
                        }
                        output.push(frame);
                    }
                }
                assert_eq!(output.len(), (GROUPS * 4) as usize);
                assert_eq!(receiver.stats().1, u64::from(!lose_parity));
                assert_eq!(receiver.stats().2, u64::from(lose_parity));
                assert_eq!(concealed_at, lose_parity.then_some(160 + pause + 25));
                assert_eq!(receiver.next_deadline(), None);
            }
        }
    }
}

#[test]
fn playback_reservations_expire_at_the_earlier_budget_or_twenty_ms_extension() {
    for budget_ms in [0, 7, 20, 60] {
        let start = Instant::now();
        let mut receiver = AudioReceiver::with_format(1, 240, 48_000);
        let packet = latch_proto::wire::AudioPacket {
            parity: false,
            seq: 1,
            pts: 5000,
            payload: &[1],
        };
        receiver.push(&packet, start);
        assert_eq!(receiver.pop(start), None);
        let release = start + Duration::from_millis(20);
        assert_eq!(
            receiver.pop_for_playback(release, release + Duration::from_millis(budget_ms)),
            Some(AudioOut::Missing { seq: 0 })
        );
        assert_eq!(
            receiver.pop(release),
            Some(AudioOut::Frame {
                seq: 1,
                pts: 5000,
                payload: vec![1]
            })
        );
        assert_eq!(receiver.pop(release), None);
        if budget_ms == 0 {
            assert_eq!(receiver.stats(), (1, 0, 1));
            assert_eq!(receiver.next_deadline(), None);
        } else {
            let bound = release + Duration::from_millis(budget_ms.min(20));
            assert_eq!(receiver.next_deadline(), Some(bound));
            assert_eq!(receiver.stats(), (1, 0, 0));
            assert_eq!(receiver.pop(bound - Duration::from_nanos(1)), None);
            assert_eq!(receiver.stats().2, 0);
            assert_eq!(receiver.pop(bound), None);
            assert_eq!(receiver.stats().2, 1);
            assert_eq!(receiver.next_deadline(), None);
        }
    }
}

#[test]
fn playback_reservation_cache_is_bounded_even_without_clock_progress() {
    let now = Instant::now();
    let mut receiver = AudioReceiver::with_format(1, 240, 48_000);
    let mut missing = Vec::new();
    for seq in 0..512 {
        if seq % 4 == 0 {
            continue;
        }
        receiver.push(
            &latch_proto::wire::AudioPacket {
                parity: false,
                seq,
                pts: seq * 5000,
                payload: &[1],
            },
            now,
        );
        while let Some(out) = receiver.pop_for_playback(now, now + Duration::from_secs(3600)) {
            if let AudioOut::Missing { seq } = out {
                missing.push(seq);
            }
        }
    }
    assert_eq!(missing.len(), 127);
    assert_eq!(
        missing
            .iter()
            .filter(|&&seq| receiver.repair_deadline(seq).is_some())
            .count(),
        64
    );
    let bound = now + Duration::from_millis(20);
    assert!(missing
        .iter()
        .filter_map(|&seq| receiver.repair_deadline(seq))
        .all(|deadline| deadline == bound));
    while receiver.pop(bound).is_some() {}
    assert_eq!(receiver.stats().2, 128);
    assert!(missing
        .iter()
        .all(|&seq| receiver.repair_deadline(seq).is_none()));
    assert_eq!(receiver.next_deadline(), None);
}
