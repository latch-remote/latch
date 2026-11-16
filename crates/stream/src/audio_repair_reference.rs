//! Receiver policy copied from main at 45ad4b7 for the playback comparison.
//! Only module paths and the unused legacy constructor were changed.
use latch_proto::{audio::AudioOut, wire::AudioPacket};
use std::time::{Duration, Instant};
const GROUP: u32 = 4;
const AUDIO_HOLD: Duration = Duration::from_millis(20);

/// Puts audio packets in order and rebuilds a lost one from its group's
/// parity. A missing packet is given up on when a packet two or more past
/// the end of its group has arrived, or [`AUDIO_HOLD`] after a later packet
/// or its group's parity showed it missing; parity alone does not, since
/// the group's data may have been reordered behind it. After a gap beyond
/// the 64-packet window, jumps to the arriving packet's group start and
/// clears stale state, rather than playing a long backlog of concealment
/// or stalling forever.
pub struct AudioReceiver {
    session: u32,
    packet_us: u32,
    next: Option<u32>,
    frames: std::collections::HashMap<u32, (u32, Vec<u8>)>,
    parity: std::collections::HashMap<u32, Vec<u8>>,
    latest: Option<u32>,
    /// Since when `next` has been known missing.
    held: Option<Instant>,
    stats: (u64, u64, u64),
}

impl AudioReceiver {
    /// Packet duration from Accept; the legacy constructor retains 10 ms.
    pub fn with_format(session: u32, samples: u32, rate: u32) -> Self {
        Self {
            session,
            packet_us: (u64::from(samples) * 1_000_000 / u64::from(rate.max(1))) as u32,
            next: None,
            frames: Default::default(),
            parity: Default::default(),
            latest: None,
            held: None,
            stats: (0, 0, 0),
        }
    }

    pub fn push(&mut self, packet: &AudioPacket<'_>, now: Instant) {
        let _ = (now, self.session);
        if packet.parity && !packet.seq.is_multiple_of(GROUP) {
            return;
        }
        let mut next = *self.next.get_or_insert(packet.seq - packet.seq % GROUP);
        if latch_proto::video::before(packet.seq, next.wrapping_sub(GROUP - 1)) {
            return;
        }
        if latch_proto::video::before(next, packet.seq) && packet.seq.wrapping_sub(next) > 64 {
            let group = packet.seq - packet.seq % GROUP;
            self.stats.2 += group.wrapping_sub(next) as u64;
            next = group;
            self.next = Some(next);
            self.frames.clear();
            self.parity.clear();
            self.latest = None;
            self.held = None;
        }
        if packet.parity {
            self.parity
                .entry(packet.seq)
                .or_insert_with(|| packet.payload.to_vec());
        } else {
            if latch_proto::video::before(packet.seq, next) || packet.seq.wrapping_sub(next) > 64 {
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
                .is_none_or(|n| latch_proto::video::before(n, packet.seq))
            {
                self.latest = Some(packet.seq);
            }
        }
        self.rebuild(packet.seq - packet.seq % GROUP);
    }

    fn rebuild(&mut self, group: u32) {
        let Some(parity) = self.parity.get(&group) else {
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
            .retain(|&n, _| !latch_proto::video::before(n, seq.wrapping_sub(GROUP)));
        self.parity
            .retain(|&n, _| !latch_proto::video::before(n, seq.wrapping_sub(GROUP)));
        let group = seq - seq % GROUP;
        if let Some((pts, payload)) = self.frames.get(&seq).cloned() {
            self.next = Some(seq.wrapping_add(1));
            self.held = None;
            return Some(AudioOut::Frame { seq, pts, payload });
        }
        if !self
            .latest
            .is_some_and(|n| latch_proto::video::before(seq, n))
            && !self.parity.contains_key(&group)
        {
            // Nothing after it has arrived: it is not late yet.
            self.held = None;
            return None;
        }
        // One timer for a run of missing packets: it restarts only once a
        // packet is delivered.
        let held = *self.held.get_or_insert(now);
        if now.saturating_duration_since(held) >= AUDIO_HOLD
            || self
                .latest
                .is_some_and(|n| !latch_proto::video::before(n, group.wrapping_add(GROUP + 1)))
        {
            self.next = Some(seq.wrapping_add(1));
            self.stats.2 += 1;
            return Some(AudioOut::Missing { seq });
        }
        None
    }

    /// When `pop` may give up on a missing packet even if nothing arrives.
    pub fn next_deadline(&self) -> Option<Instant> {
        self.held.map(|t| t + AUDIO_HOLD)
    }

    /// Counters: packets received, rebuilt from parity, given up on.
    pub fn stats(&self) -> (u64, u64, u64) {
        self.stats
    }
}
