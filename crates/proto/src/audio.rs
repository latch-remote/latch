//! Audio packets and their parity. See `docs/PROTOCOL.md`, "Audio (3)".

use crate::wire::AudioPacket;
use std::time::{Duration, Instant};

/// Packets per parity group.
pub const GROUP: u32 = 4;
/// Largest coded frame, leaving room for the audio header and parity's
/// two-byte payload length.
pub const MAX_PAYLOAD: usize = crate::MAX_DATAGRAM - 16 - 2;
/// Reordering allowance after a group's estimated parity arrival.
pub const AUDIO_HOLD: Duration = Duration::from_millis(20);
/// Fallback hold for callers without a playback deadline. Desktop output
/// additionally applies its queue reserve and [`AUDIO_PLAYBACK_HOLD`].
pub const AUDIO_MAX_HOLD: Duration = Duration::from_millis(60);
/// Desktop playback never waits longer than a capture callback and one
/// small device pull, even when its queue has more room.
pub const AUDIO_PLAYBACK_HOLD: Duration = Duration::from_millis(25);
const PAUSE: Duration = AUDIO_MAX_HOLD;
const SPACING_GROUPS: usize = 8;

/// Numbers encoded audio frames and sends a parity packet after every
/// [`GROUP`] of them.
pub struct AudioPacketizer {
    session: u32,
    seq: u32,
    parity: Vec<u8>,
    write: fn(&mut Vec<u8>, u32, &AudioPacket<'_>),
}

impl AudioPacketizer {
    pub fn new(session: u32) -> Self {
        Self {
            session,
            seq: 0,
            parity: Vec::new(),
            write: crate::wire::write_audio,
        }
    }

    /// The same for the viewer's microphone (datagram kind 13).
    pub fn mic(session: u32) -> Self {
        Self {
            write: crate::wire::write_mic,
            ..Self::new(session)
        }
    }

    /// Calls `emit` with the packet for `payload` and, when it ends a group,
    /// the group's parity packet after it. Returns false if `payload`
    /// exceeds [`MAX_PAYLOAD`]; emits nothing and leaves the sequence and
    /// parity unchanged in that case.
    pub fn packetize(&mut self, pts: u32, payload: &[u8], emit: impl FnMut(&[u8])) -> bool {
        if payload.len() > MAX_PAYLOAD {
            return false;
        }
        let mut emit = emit;
        let mut buf = Vec::new();
        (self.write)(
            &mut buf,
            self.session,
            &AudioPacket {
                parity: false,
                seq: self.seq,
                pts,
                payload,
            },
        );
        emit(&buf);
        let mut coded = Vec::with_capacity(payload.len() + 2);
        coded.extend((payload.len() as u16).to_le_bytes());
        coded.extend(payload);
        self.parity.resize(self.parity.len().max(coded.len()), 0);
        for (a, b) in self.parity.iter_mut().zip(coded) {
            *a ^= b;
        }
        if self.seq % GROUP == GROUP - 1 {
            (self.write)(
                &mut buf,
                self.session,
                &AudioPacket {
                    parity: true,
                    seq: self.seq.wrapping_sub(GROUP - 1),
                    pts,
                    payload: &self.parity,
                },
            );
            emit(&buf);
            self.parity.clear();
        }
        self.seq = self.seq.wrapping_add(1);
        true
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AudioOut {
    Frame {
        seq: u32,
        pts: u32,
        payload: Vec<u8>,
    },
    /// This packet is gone for good; the player conceals it.
    Missing { seq: u32 },
}

/// Puts audio packets in order and rebuilds a lost one from its group's
/// parity. Missing packets wait until [`AUDIO_HOLD`] after their group's
/// estimated parity arrival, bounded by [`AUDIO_MAX_HOLD`] from detection.
/// Intact packets are delivered immediately. After a gap beyond
/// the 64-packet window, jumps to the arriving packet's group start and
/// clears stale state, rather than playing a long backlog of concealment
/// or stalling forever.
pub struct AudioReceiver {
    session: u32,
    packet_us: u32,
    next: Option<u32>,
    frames: std::collections::HashMap<u32, (u32, Vec<u8>)>,
    parity: std::collections::HashMap<u32, (Instant, Vec<u8>)>,
    latest: Option<u32>,
    last_arrival: Option<Instant>,
    spacing: [(u32, Duration); SPACING_GROUPS],
    /// Last observed data position and arrival in each group.
    group_arrival: std::collections::HashMap<u32, (u32, Instant)>,
    /// Since when `next` has been known missing.
    held: Option<Instant>,
    playout_deadline: Option<Instant>,
    stats: (u64, u64, u64),
}

impl AudioReceiver {
    pub fn new(session: u32) -> Self {
        Self::with_format(session, 480, 48_000)
    }

    /// Packet duration from Accept; the legacy constructor retains 10 ms.
    pub fn with_format(session: u32, samples: u32, rate: u32) -> Self {
        let packet_us = (u64::from(samples) * 1_000_000 / u64::from(rate.max(1))) as u32;
        Self {
            session,
            packet_us,
            next: None,
            frames: Default::default(),
            parity: Default::default(),
            latest: None,
            last_arrival: None,
            spacing: [(0, Duration::ZERO); SPACING_GROUPS],
            group_arrival: Default::default(),
            held: None,
            playout_deadline: None,
            stats: (0, 0, 0),
        }
    }

    pub fn push(&mut self, packet: &AudioPacket<'_>, now: Instant) {
        let _ = self.session;
        if packet.parity && !packet.seq.is_multiple_of(GROUP) {
            return;
        }
        let mut next = *self.next.get_or_insert(packet.seq - packet.seq % GROUP);
        if crate::video::before(packet.seq, next.wrapping_sub(GROUP - 1)) {
            return;
        }
        if crate::video::before(next, packet.seq) && packet.seq.wrapping_sub(next) > 64 {
            let group = packet.seq - packet.seq % GROUP;
            self.stats.2 += group.wrapping_sub(next) as u64;
            next = group;
            self.next = Some(next);
            self.frames.clear();
            self.parity.clear();
            self.latest = None;
            self.last_arrival = None;
            self.spacing.fill((0, Duration::ZERO));
            self.group_arrival.clear();
            self.held = None;
        }
        if packet.parity {
            self.parity
                .entry(packet.seq)
                .or_insert_with(|| (now, packet.payload.to_vec()));
        } else {
            if crate::video::before(packet.seq, next) || packet.seq.wrapping_sub(next) > 64 {
                return;
            }
            if self.frames.contains_key(&packet.seq) {
                return;
            }
            self.frames
                .insert(packet.seq, (packet.pts, packet.payload.to_vec()));
            self.stats.0 += 1;
            if self
                .latest
                .is_none_or(|n| crate::video::before(n, packet.seq))
            {
                if let Some(arrival) = self.last_arrival {
                    let spacing = now.saturating_duration_since(arrival);
                    if spacing >= PAUSE {
                        // Sequence numbers do not advance during silence.
                        self.spacing.fill((0, Duration::ZERO));
                    } else if !spacing.is_zero() {
                        let group = packet.seq - packet.seq % GROUP;
                        let entry = &mut self.spacing[(group / GROUP) as usize % SPACING_GROUPS];
                        if entry.0 != group {
                            *entry = (group, Duration::ZERO);
                        }
                        entry.1 = entry.1.max(spacing);
                    }
                }
                self.last_arrival = Some(now);
                self.latest = Some(packet.seq);
            }
            let position = packet.seq % GROUP;
            self.group_arrival
                .insert(packet.seq - position, (position, now));
        }
        self.rebuild(packet.seq - packet.seq % GROUP);
    }

    fn spacing(&self) -> Duration {
        let latest = self.latest.unwrap_or(0);
        let group = latest - latest % GROUP;
        self.spacing
            .iter()
            .filter(|&&(n, interval)| {
                group.wrapping_sub(n) < GROUP * SPACING_GROUPS as u32 && !interval.is_zero()
            })
            .map(|&(_, interval)| interval)
            // Before the first callback interval, allow a capture burst.
            .max()
            .unwrap_or(AUDIO_HOLD)
            .max(Duration::from_micros(u64::from(self.packet_us)))
            .min(AUDIO_MAX_HOLD)
    }

    /// The output queue's latest safe release time. Update before each
    /// `pop`, including timer wakeups. A pending gap's deadline can only
    /// move earlier. This enables the low-latency playback policy.
    pub fn set_playout_deadline(&mut self, deadline: Instant) {
        self.playout_deadline = Some(if self.held.is_some() {
            self.playout_deadline
                .map_or(deadline, |old| old.min(deadline))
        } else {
            deadline
        });
    }

    fn rebuild(&mut self, group: u32) {
        let Some((_, parity)) = self.parity.get(&group) else {
            return;
        };
        let missing: Vec<_> = (0..GROUP)
            .map(|i| group.wrapping_add(i))
            .filter(|s| !self.frames.contains_key(s))
            .collect();
        if missing.len() != 1 {
            return;
        }
        let mut data = parity.clone();
        for i in 0..GROUP {
            if let Some((_, payload)) = self.frames.get(&group.wrapping_add(i)) {
                let mut coded = Vec::new();
                coded.extend((payload.len() as u16).to_le_bytes());
                coded.extend(payload);
                for (a, b) in data.iter_mut().zip(coded) {
                    *a ^= b;
                }
            }
        }
        if data.len() < 2 {
            return;
        }
        let len = u16::from_le_bytes([data[0], data[1]]) as usize;
        if len > data.len() - 2 {
            return;
        }
        let seq = missing[0];
        let pts = (0..GROUP)
            .find_map(|i| {
                let n = group.wrapping_add(i);
                self.frames
                    .get(&n)
                    .map(|(p, _)| p.wrapping_add(seq.wrapping_sub(n).wrapping_mul(self.packet_us)))
            })
            .unwrap_or(0);
        self.frames.insert(seq, (pts, data[2..2 + len].to_vec()));
        self.stats.1 += 1;
    }
    /// The next packet in order, or the next one given up on. Call it on
    /// every wakeup, not only when audio arrives: the host sends nothing
    /// while the desktop is silent.
    pub fn pop(&mut self, now: Instant) -> Option<AudioOut> {
        let seq = self.next?;
        self.frames
            .retain(|&n, _| !crate::video::before(n, seq.wrapping_sub(GROUP)));
        self.parity
            .retain(|&n, _| !crate::video::before(n, seq.wrapping_sub(GROUP)));
        self.group_arrival
            .retain(|&n, _| !crate::video::before(n, seq.wrapping_sub(GROUP)));
        let group = seq - seq % GROUP;
        if let Some((pts, payload)) = self.frames.get(&seq).cloned() {
            self.next = Some(seq.wrapping_add(1));
            self.held = None;
            return Some(AudioOut::Frame { seq, pts, payload });
        }
        if !self.latest.is_some_and(|n| crate::video::before(seq, n))
            && !self.parity.contains_key(&group)
        {
            // Nothing after it has arrived: it is not late yet.
            self.held = None;
            return None;
        }
        // One timer for a run of missing packets: it restarts only once a
        // packet is delivered.
        self.held.get_or_insert(now);
        if self.next_deadline().is_some_and(|deadline| now >= deadline) {
            self.next = Some(seq.wrapping_add(1));
            self.stats.2 += 1;
            return Some(AudioOut::Missing { seq });
        }
        None
    }

    /// When `pop` may give up on a missing packet even if nothing arrives.
    pub fn next_deadline(&self) -> Option<Instant> {
        let held = self.held?;
        let seq = self.next?;
        let group = seq - seq % GROUP;
        let cap = self
            .playout_deadline
            .map_or(held + AUDIO_MAX_HOLD, |deadline| {
                deadline.min(held + AUDIO_PLAYBACK_HOLD)
            });
        if self.playout_deadline.is_some()
            && self
                .latest
                .is_some_and(|n| !crate::video::before(n, group.wrapping_add(GROUP + 1)))
        {
            // Retain main's progress cutoff in playback. Extra waiting
            // after two packets of the next group risks its intact audio.
            return Some(held);
        }
        if let Some((arrival, _)) = self.parity.get(&group) {
            if self.playout_deadline.is_some() {
                // A single loss was already rebuilt by push. Playback
                // conceals an unrepairable group immediately.
                return Some(held);
            }
            // Parity is already here. Only reordered data can still help.
            return Some((*arrival + AUDIO_HOLD).min(cap));
        }
        let due = self
            .group_arrival
            .get(&group)
            .map_or(held, |&(position, arrival)| {
                arrival + self.spacing() * (GROUP - 1 - position)
            });
        Some((due + AUDIO_HOLD).min(cap))
    }

    /// Counters: packets received, rebuilt from parity, given up on.
    pub fn stats(&self) -> (u64, u64, u64) {
        self.stats
    }
}

#[cfg(test)]
mod timing_tests {
    use super::*;

    #[test]
    fn spacing_shrinks_after_eight_groups_and_resets_after_resynchronization() {
        let mut receiver = AudioReceiver::with_format(1, 240, 48_000);
        let start = Instant::now();
        for seq in 0..48 {
            let ms = if seq < 4 {
                seq * 30
            } else {
                90 + (seq - 3) * 5
            };
            receiver.push(
                &AudioPacket {
                    parity: false,
                    seq,
                    pts: seq * 5000,
                    payload: &[0],
                },
                start + Duration::from_millis(u64::from(ms)),
            );
            if seq == 3 {
                assert_eq!(receiver.spacing(), Duration::from_millis(30));
            }
        }
        assert_eq!(receiver.spacing(), Duration::from_millis(5));
        receiver.push(
            &AudioPacket {
                parity: false,
                seq: 200,
                pts: 1_000_000,
                payload: &[0],
            },
            start + Duration::from_secs(2),
        );
        assert_eq!(receiver.spacing(), AUDIO_HOLD);
    }

    #[test]
    fn playback_deadline_only_tightens_and_never_exceeds_twenty_five_ms() {
        let start = Instant::now();
        for reserve in [Duration::from_millis(13), Duration::from_secs(1)] {
            let mut receiver = AudioReceiver::with_format(1, 240, 48_000);
            receiver.set_playout_deadline(start + reserve);
            receiver.push(
                &AudioPacket {
                    parity: false,
                    seq: 1,
                    pts: 5000,
                    payload: &[1],
                },
                start,
            );
            assert_eq!(receiver.pop(start), None);
            let deadline = start + reserve.min(AUDIO_PLAYBACK_HOLD);
            assert_eq!(receiver.next_deadline(), Some(deadline));
            receiver.set_playout_deadline(start + Duration::from_secs(2));
            assert_eq!(receiver.next_deadline(), Some(deadline));
            assert_eq!(receiver.pop(deadline - Duration::from_nanos(1)), None);
            assert_eq!(receiver.pop(deadline), Some(AudioOut::Missing { seq: 0 }));
        }
    }

    #[test]
    fn playback_conceals_an_unrepairable_group_as_soon_as_parity_arrives() {
        let start = Instant::now();
        let mut packets = Vec::new();
        let mut sender = AudioPacketizer::new(1);
        for seq in 0..GROUP {
            sender.packetize(seq * 5000, &[seq as u8], |bytes| {
                packets.push(bytes.to_vec())
            });
        }
        let mut receiver = AudioReceiver::with_format(1, 240, 48_000);
        receiver.set_playout_deadline(start + AUDIO_PLAYBACK_HOLD);
        for index in [0, 3, 4] {
            let crate::wire::Datagram::Audio { packet, .. } =
                crate::wire::parse(&packets[index]).unwrap()
            else {
                panic!()
            };
            receiver.push(&packet, start);
        }
        assert!(matches!(
            receiver.pop(start),
            Some(AudioOut::Frame { seq: 0, .. })
        ));
        assert_eq!(receiver.pop(start), Some(AudioOut::Missing { seq: 1 }));
        assert_eq!(receiver.pop(start), Some(AudioOut::Missing { seq: 2 }));
        assert!(matches!(
            receiver.pop(start),
            Some(AudioOut::Frame { seq: 3, .. })
        ));
        assert_eq!(receiver.pop(start), None);
        assert_eq!(receiver.next_deadline(), None);
    }
}
