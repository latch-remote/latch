//! Audio packets and their parity. See `docs/PROTOCOL.md`, "Audio (3)".

use crate::wire::AudioPacket;
use std::time::{Duration, Instant};

/// Packets per parity group.
pub const GROUP: u32 = 4;
/// Largest coded frame, leaving room for the audio header and parity's
/// two-byte payload length.
pub const MAX_PAYLOAD: usize = crate::MAX_DATAGRAM - 16 - 2;
/// How long a missing packet may hold back the packets after it before it
/// is given up on, when nothing else settles it first. Covers reordering,
/// and a loss at the end of a sound after which the host sends nothing.
pub const AUDIO_HOLD: Duration = Duration::from_millis(20);
/// Extra repair time after main's release, limited further by queued playback.
pub const AUDIO_REPAIR_EXTENSION: Duration = Duration::from_millis(20);
const MAX_PENDING_REPAIRS: usize = 64;

struct PendingRepair {
    deadline: Instant,
    counted: bool,
    frames: std::collections::HashMap<u32, (u32, Vec<u8>)>,
    parity: Option<Vec<u8>>,
}

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
    /// The in-order release uses concealment for this packet.
    Missing { seq: u32 },
}

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
    repairs: std::collections::HashMap<u32, PendingRepair>,
}

impl AudioReceiver {
    pub fn new(session: u32) -> Self {
        Self::with_format(session, 480, 48_000)
    }

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
            repairs: Default::default(),
        }
    }

    pub fn push(&mut self, packet: &AudioPacket<'_>, now: Instant) {
        let _ = (now, self.session);
        if packet.parity && !packet.seq.is_multiple_of(GROUP) {
            return;
        }
        // Late originals/parity can fill a reserved, still-unplayed frame.
        // This cache never changes the legacy in-order receiver's inputs.
        if packet.payload.len() <= MAX_PAYLOAD + if packet.parity { 2 } else { 0 } {
            let group = packet.seq - packet.seq % GROUP;
            for (&seq, repair) in &mut self.repairs {
                if seq - seq % GROUP != group || now >= repair.deadline {
                    continue;
                }
                if packet.parity {
                    repair.parity.get_or_insert_with(|| packet.payload.to_vec());
                } else {
                    repair
                        .frames
                        .entry(packet.seq)
                        .or_insert_with(|| (packet.pts, packet.payload.to_vec()));
                }
            }
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
            self.stats.2 += self.repairs.len() as u64;
            self.repairs.clear();
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
                self.latest = Some(packet.seq);
            }
        }
        self.rebuild(packet.seq - packet.seq % GROUP);
    }

    fn rebuild(&mut self, group: u32) {
        let Some(parity) = self.parity.get(&group) else {
            return;
        };
        if let Some(AudioOut::Frame { seq, pts, payload }) =
            reconstruct(group, self.packet_us, &self.frames, parity)
        {
            self.frames.insert(seq, (pts, payload));
            self.stats.1 += 1;
            if let Some(repair) = self.repairs.get_mut(&seq) {
                repair.counted = true;
            }
        }
    }

    /// Main's release order, with a bounded repair reservation for a missing
    /// frame that the caller will queue as concealment until its play deadline.
    pub fn pop_for_playback(&mut self, now: Instant, deadline: Instant) -> Option<AudioOut> {
        let out = self.pop(now)?;
        if let AudioOut::Missing { seq } = out {
            let deadline = deadline.min(now + AUDIO_REPAIR_EXTENSION);
            if deadline > now && self.repairs.len() < MAX_PENDING_REPAIRS {
                let group = seq - seq % GROUP;
                let frames: std::collections::HashMap<_, _> = self
                    .frames
                    .iter()
                    .filter(|(n, (_, payload))| {
                        n.wrapping_sub(group) < GROUP && payload.len() <= MAX_PAYLOAD
                    })
                    .map(|(&n, frame)| (n, frame.clone()))
                    .collect();
                let parity = self
                    .parity
                    .get(&group)
                    .filter(|p| p.len() <= MAX_PAYLOAD + 2)
                    .cloned();
                self.repairs.insert(
                    seq,
                    PendingRepair {
                        deadline,
                        counted: false,
                        frames,
                        parity,
                    },
                );
                // Classification is deferred until repair or expiry. This
                // decrement is internal to the opt-in call.
                self.stats.2 -= 1;
            }
        }
        Some(out)
    }

    pub fn repair_deadline(&self, seq: u32) -> Option<Instant> {
        self.repairs.get(&seq).map(|r| r.deadline)
    }

    /// A late frame is usable only after the caller verifies its queued
    /// sample range. Acknowledge that check with `finish_repair`.
    pub fn repair(&self, seq: u32, now: Instant) -> Option<AudioOut> {
        let repair = self.repairs.get(&seq)?;
        if now >= repair.deadline {
            return None;
        }
        if let Some((pts, payload)) = repair.frames.get(&seq) {
            return Some(AudioOut::Frame {
                seq,
                pts: *pts,
                payload: payload.clone(),
            });
        }
        reconstruct(
            seq - seq % GROUP,
            self.packet_us,
            &repair.frames,
            repair.parity.as_deref()?,
        )
    }

    pub fn finish_repair(&mut self, seq: u32, repaired: bool) {
        if let Some(pending) = self.repairs.remove(&seq) {
            if repaired {
                if !pending.frames.contains_key(&seq) && !pending.counted {
                    self.stats.1 += 1;
                }
            } else {
                self.stats.2 += 1;
            }
        }
    }

    /// The next packet in order, or the next one given up on. Call it on
    /// every wakeup, not only when audio arrives: the host sends nothing
    /// while the desktop is silent.
    pub fn pop(&mut self, now: Instant) -> Option<AudioOut> {
        let before = self.repairs.len();
        self.repairs.retain(|_, repair| now < repair.deadline);
        self.stats.2 += (before - self.repairs.len()) as u64;
        let seq = self.next?;
        self.frames
            .retain(|&n, _| !crate::video::before(n, seq.wrapping_sub(GROUP)));
        self.parity
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
        let held = *self.held.get_or_insert(now);
        if now.saturating_duration_since(held) >= AUDIO_HOLD
            || self
                .latest
                .is_some_and(|n| !crate::video::before(n, group.wrapping_add(GROUP + 1)))
        {
            self.next = Some(seq.wrapping_add(1));
            self.stats.2 += 1;
            return Some(AudioOut::Missing { seq });
        }
        None
    }

    /// When `pop` may give up on a missing packet even if nothing arrives.
    pub fn next_deadline(&self) -> Option<Instant> {
        self.held
            .map(|t| t + AUDIO_HOLD)
            .into_iter()
            .chain(self.repairs.values().map(|r| r.deadline))
            .min()
    }

    /// Counters: packets received, rebuilt from parity, given up on.
    pub fn stats(&self) -> (u64, u64, u64) {
        self.stats
    }
}

fn reconstruct(
    group: u32,
    packet_us: u32,
    frames: &std::collections::HashMap<u32, (u32, Vec<u8>)>,
    parity: &[u8],
) -> Option<AudioOut> {
    let missing: Vec<_> = (0..GROUP)
        .map(|i| group.wrapping_add(i))
        .filter(|s| !frames.contains_key(s))
        .collect();
    if missing.len() != 1 {
        return None;
    }
    let mut data = parity.to_vec();
    for i in 0..GROUP {
        if let Some((_, payload)) = frames.get(&group.wrapping_add(i)) {
            let mut coded = Vec::new();
            coded.extend((payload.len() as u16).to_le_bytes());
            coded.extend(payload);
            for (a, b) in data.iter_mut().zip(coded) {
                *a ^= b;
            }
        }
    }
    if data.len() < 2 {
        return None;
    }
    let len = u16::from_le_bytes([data[0], data[1]]) as usize;
    if len > data.len() - 2 {
        return None;
    }
    let seq = missing[0];
    let pts = (0..GROUP)
        .find_map(|i| {
            let n = group.wrapping_add(i);
            frames
                .get(&n)
                .map(|(p, _)| p.wrapping_add(seq.wrapping_sub(n).wrapping_mul(packet_us)))
        })
        .unwrap_or(0);
    Some(AudioOut::Frame {
        seq,
        pts,
        payload: data[2..2 + len].to_vec(),
    })
}
