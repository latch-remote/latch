//! Session media policy and its viewer-facing state. Tailscale route discovery
//! can refine `UdpPath` independently of media transport selection.
use latch_proto::control::{PathAction, Report};
use latch_proto::wire::{self, Datagram};
use std::collections::VecDeque;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum UdpPath {
    #[default]
    Unknown,
    Direct,
    PeerRelay,
    Derp,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MediaPath {
    TailscaleUdp(UdpPath),
    LatchRelay,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathReason {
    Initial,
    UdpBlocked,
    UdpStalled,
    UdpLoss {
        percent: u8,
    },
    UdpFrameLoss {
        percent: u8,
    },
    UdpLimited {
        kbps: u32,
    },
    UdpRecovered,
    DatagramSize {
        bytes: u16,
    },
    RelayUnavailable(String),
    /// Carrier/proxy/captive-portal/TLS diagnostics supplied by future wiring.
    Environment {
        code: String,
        detail: String,
    },
}

impl PathReason {
    pub fn description(&self) -> String {
        match self {
            Self::Initial => "Media uses Tailscale UDP".into(),
            Self::UdpBlocked => "UDP is blocked on this network".into(),
            Self::UdpStalled => "UDP stopped delivering video".into(),
            Self::UdpLoss { percent } => format!("UDP is losing {percent}% of packets"),
            Self::UdpFrameLoss { percent } => format!("UDP is losing {percent}% of video frames"),
            Self::UdpLimited { kbps } => {
                format!(
                    "UDP is limited to about {:.1} Mbps",
                    f64::from(*kbps) / 1000.0
                )
            }
            Self::UdpRecovered => "UDP recovered".into(),
            Self::DatagramSize { bytes } => format!("UDP recovered with {bytes}-byte datagrams"),
            Self::RelayUnavailable(detail) => format!("Latch relay unavailable. {detail}"),
            Self::Environment { detail, .. } => detail.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaPathState {
    pub path: MediaPath,
    pub reason: PathReason,
    /// Carrier display name. Transport wiring may supply another negotiated name.
    pub carrier: Option<String>,
    pub since: Instant,
}

impl Default for MediaPathState {
    fn default() -> Self {
        Self {
            path: MediaPath::TailscaleUdp(UdpPath::Unknown),
            reason: PathReason::Initial,
            carrier: None,
            since: Instant::now(),
        }
    }
}

/// A trailing second of actual receive windows. Clean windows contribute to
/// the denominator, allowing bursty damage to accumulate without treating a
/// single short burst or FEC-repaired loss as a reason to allocate a relay.
#[derive(Default)]
pub(crate) struct Policy {
    windows: VecDeque<(Instant, Report, bool, bool)>,
}

impl Policy {
    pub(crate) fn observe(
        &mut self,
        now: Instant,
        report: Report,
        bandwidth_limited: bool,
        fec_limited: bool,
        budget: u32,
    ) -> Option<PathReason> {
        self.windows
            .push_back((now, report, bandwidth_limited, fec_limited));
        while self
            .windows
            .front()
            .is_some_and(|(at, ..)| now.saturating_duration_since(*at) > Duration::from_secs(1))
        {
            self.windows.pop_front();
        }
        let mut duration = 0;
        let mut damaged = 0;
        let mut limited = 0;
        let mut raw_loss = 0.0;
        let mut quality_limited = 0;
        let mut lost_frames = 0;
        let mut delivered_frames = 0.0;
        let mut first_damage = None;
        let mut last_damage = None;
        for (at, report, short, fec_cost) in &self.windows {
            let ms = report.window_ms.clamp(1, 250);
            duration += ms;
            raw_loss += report.loss_pct.clamp(0.0, 100.0) * ms as f32;
            if report.frames_lost > 0 || report.unrecovered_loss_pct.unwrap_or(0.0) > 1.0 {
                damaged += ms;
                first_damage.get_or_insert(*at);
                last_damage = Some(*at);
            }
            lost_frames += report.frames_lost;
            delivered_frames += report.fps.max(0.0) * ms as f32 / 1000.0;
            if *fec_cost {
                quality_limited += ms;
            }
            if *short {
                limited += ms;
            }
        }
        if duration < 800 {
            return None;
        }
        if limited * 4 >= duration * 3 {
            return Some(PathReason::UdpLimited { kbps: budget });
        }
        let raw_loss = raw_loss / duration as f32;
        let ongoing = last_damage
            .is_some_and(|at| now.saturating_duration_since(at) <= Duration::from_millis(250));
        let repeated = first_damage.zip(last_damage).is_some_and(|(first, last)| {
            last.saturating_duration_since(first) >= Duration::from_millis(250)
        });
        // A receive batch can account for several lost frames in one report.
        // Count those frames as well as the number of damaged time windows.
        let frame_damage = repeated
            && lost_frames >= 3
            && raw_loss >= 10.0
            && lost_frames as f32 * 10.0 >= lost_frames as f32 + delivered_frames;
        if ongoing && (damaged * 4 >= duration || frame_damage)
            || quality_limited * 4 >= duration * 3 && raw_loss >= 10.0
        {
            return Some(if raw_loss >= 1.0 {
                PathReason::UdpLoss {
                    percent: raw_loss.round() as u8,
                }
            } else {
                PathReason::UdpFrameLoss {
                    percent: (lost_frames as f32 * 100.0
                        / (lost_frames as f32 + delivered_frames).max(1.0))
                    .round() as u8,
                }
            });
        }
        None
    }
}

/// Interleaved, deduplicated size tests use existing negotiated Probe datagrams.
/// Three rounds avoid mistaking one random drop for a size-dependent black hole.
pub const MTU_SIZES: [usize; 4] = [1200, 1000, 800, 576];
const MTU_REPORT: Duration = Duration::from_millis(600);
const MTU_INTERVAL: Duration = Duration::from_secs(10);
const MTU_EARLY_WINDOW: Duration = Duration::from_secs(1);

pub struct MtuProbe {
    serial: u16,
    started: Instant,
    next: Instant,
    pending: bool,
    rounds: u8,
    received: [u8; 4],
    early: Option<(Instant, [u8; 4])>,
}

impl MtuProbe {
    pub fn new(now: Instant) -> Self {
        Self {
            serial: 0x8000,
            started: now,
            next: now,
            pending: false,
            rounds: 0,
            received: [0; 4],
            early: None,
        }
    }

    pub fn start(&mut self, serial: u16, now: Instant) {
        self.received = self
            .early
            .take()
            .filter(|(at, _)| {
                serial == self.serial.wrapping_add(1)
                    && now.saturating_duration_since(*at) <= MTU_EARLY_WINDOW
            })
            .map_or([0; 4], |(_, received)| received);
        self.serial = serial;
        self.started = now;
        self.pending = true;
        self.rounds = 0;
    }

    /// Called on the host control owner. At most four datagrams per turn.
    pub fn send(
        &mut self,
        now: Instant,
        session: u32,
        mut control: impl FnMut(PathAction) -> bool,
        mut datagram: impl FnMut(&[u8]),
    ) -> bool {
        if now >= self.next {
            self.start(self.serial.wrapping_add(1), now);
            self.next = now + MTU_INTERVAL;
            if !control(PathAction::MtuProbe {
                serial: self.serial,
            }) {
                return false;
            }
        }
        if self.pending
            && self.rounds < 3
            && now.saturating_duration_since(self.started)
                >= Duration::from_millis(u64::from(self.rounds) * 100)
        {
            let mut bytes = Vec::new();
            for (index, size) in MTU_SIZES.iter().enumerate() {
                wire::write_probe(
                    &mut bytes,
                    session,
                    self.serial,
                    index as u8 * 3 + self.rounds,
                    16,
                    *size,
                );
                datagram(&bytes);
            }
            self.rounds += 1;
        }
        true
    }

    pub fn observe(&mut self, datagram: &Datagram<'_>) -> bool {
        if let Datagram::Probe {
            train,
            index,
            count: 16,
            len,
            ..
        } = datagram
        {
            let slot = usize::from(*index / 3);
            if slot < 4 && *len == MTU_SIZES[slot] {
                if self.pending && *train == self.serial {
                    self.received[slot] |= 1 << (*index % 3);
                    return true;
                }
                // UDP can beat its TCP announcement. Cache only the next serial,
                // and leave unannounced positions available to other probe meters.
                if *train == self.serial.wrapping_add(1) {
                    let now = Instant::now();
                    let (at, received) = self.early.get_or_insert((now, [0; 4]));
                    if now.saturating_duration_since(*at) > MTU_EARLY_WINDOW {
                        *at = now;
                        *received = [0; 4];
                    }
                    received[slot] |= 1 << (*index % 3);
                }
            }
        }
        false
    }

    pub fn report(&mut self, now: Instant) -> Option<PathAction> {
        if !self.pending || now.saturating_duration_since(self.started) < MTU_REPORT {
            return None;
        }
        self.pending = false;
        Some(PathAction::MtuResult {
            serial: self.serial,
            received: self.received.map(|bits| bits.count_ones() as u8),
        })
    }

    pub fn result(&mut self, serial: u16, received: [u8; 4]) -> Option<usize> {
        if !self.pending
            || self.serial != serial
            || self.rounds != 3
            || received.iter().any(|count| *count > 3)
        {
            return None;
        }
        self.pending = false;
        let index = received.iter().position(|count| *count >= 2)?;
        // Partial delivery of a larger size is random loss, not an MTU limit.
        received[..index]
            .iter()
            .all(|count| *count == 0)
            .then_some(MTU_SIZES[index])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intermittent_damage_accumulates_but_repaired_loss_keeps_udp() {
        for loss in [15.0, 17.0, 18.0] {
            for repaired in [false, true] {
                let mut policy = Policy::default();
                let start = Instant::now();
                let mut selected = None;
                for sample in 0..24 {
                    selected = policy.observe(
                        start + Duration::from_millis(sample * 50),
                        Report {
                            window_ms: 50,
                            fps: 60.0,
                            loss_pct: loss,
                            unrecovered_loss_pct: Some(0.0),
                            frames_lost: u64::from(!repaired && sample % 2 == 0),
                            ..Default::default()
                        },
                        false,
                        false,
                        20_000,
                    );
                    if selected.is_some() {
                        break;
                    }
                }
                assert_eq!(
                    selected,
                    (!repaired).then_some(PathReason::UdpLoss {
                        percent: loss as u8
                    })
                );
            }
        }
    }

    #[test]
    fn busy_udp_policer_is_detected_in_eight_hundred_milliseconds() {
        assert_eq!(
            PathReason::UdpLimited { kbps: 500 }.description(),
            "UDP is limited to about 0.5 Mbps"
        );
        let mut policy = Policy::default();
        let start = Instant::now();
        for sample in 0..16 {
            let reason = policy.observe(
                start + Duration::from_millis(sample * 50),
                Report {
                    window_ms: 50,
                    fps: 60.0,
                    ..Default::default()
                },
                true,
                false,
                3000,
            );
            assert_eq!(
                reason,
                (sample == 15).then_some(PathReason::UdpLimited { kbps: 3000 })
            );
        }
    }

    #[test]
    fn mtu_sizes_are_interleaved_deduplicated_and_bound_to_the_current_test() {
        let start = Instant::now();
        let mut host = MtuProbe::new(start);
        let mut viewer = MtuProbe::new(start);
        let mut serial = 0;
        for round in 0..3 {
            let now = start + Duration::from_millis(round * 100);
            assert!(host.send(
                now,
                7,
                |action| {
                    let PathAction::MtuProbe { serial: id } = action else {
                        panic!("probe");
                    };
                    serial = id;
                    viewer.start(id, now);
                    true
                },
                |_| {}
            ));
            // Feed the same wire positions, twice, with a size black hole.
            for (index, size) in MTU_SIZES.iter().enumerate() {
                let mut bytes = Vec::new();
                wire::write_probe(
                    &mut bytes,
                    7,
                    serial,
                    index as u8 * 3 + round as u8,
                    16,
                    *size,
                );
                if *size <= 850 {
                    let datagram = wire::parse(&bytes).unwrap();
                    assert!(viewer.observe(&datagram));
                    assert!(viewer.observe(&datagram));
                }
            }
        }
        let PathAction::MtuResult { serial, received } = viewer.report(start + MTU_REPORT).unwrap()
        else {
            panic!("result");
        };
        assert_eq!(received, [0, 0, 3, 3]);
        assert_eq!(host.result(serial.wrapping_sub(1), received), None);
        assert_eq!(host.result(serial, received), Some(800));
        assert_eq!(host.result(serial, received), None);
    }

    #[test]
    fn fec_overhead_only_requests_relay_when_it_costs_requested_quality() {
        for (loss, low_quality) in [(17.0, true), (17.0, false), (0.0, true)] {
            let mut policy = Policy::default();
            let start = Instant::now();
            let mut reason = None;
            for sample in 0..16 {
                reason = policy.observe(
                    start + Duration::from_millis(sample * 50),
                    Report {
                        window_ms: 50,
                        fps: 60.0,
                        loss_pct: loss,
                        unrecovered_loss_pct: Some(0.0),
                        ..Default::default()
                    },
                    false,
                    low_quality,
                    20_000,
                );
            }
            assert_eq!(
                reason,
                (low_quality && loss > 10.0).then_some(PathReason::UdpLoss { percent: 17 })
            );
        }
    }

    #[test]
    fn mtu_positions_that_beat_tcp_are_retained_until_announcement_and_expire() {
        for delay in [Duration::from_millis(350), Duration::from_secs(2)] {
            let start = Instant::now();
            let mut viewer = MtuProbe::new(start);
            for round in 0..3 {
                for (index, size) in MTU_SIZES.iter().enumerate() {
                    if *size <= 850 {
                        let mut bytes = Vec::new();
                        wire::write_probe(
                            &mut bytes,
                            7,
                            0x8001,
                            index as u8 * 3 + round,
                            16,
                            *size,
                        );
                        let datagram = wire::parse(&bytes).unwrap();
                        assert!(!viewer.observe(&datagram));
                        assert!(!viewer.observe(&datagram));
                    }
                }
            }
            viewer.start(0x8001, start + delay);
            let Some(PathAction::MtuResult { received, .. }) =
                viewer.report(start + delay + MTU_REPORT)
            else {
                panic!("result");
            };
            assert_eq!(
                received,
                if delay < MTU_EARLY_WINDOW {
                    [0, 0, 3, 3]
                } else {
                    [0; 4]
                }
            );
        }
    }

    #[test]
    fn batched_frame_losses_count_even_between_clean_windows() {
        let mut policy = Policy::default();
        let start = Instant::now();
        let mut reason = None;
        for sample in 0..16 {
            reason = policy.observe(
                start + Duration::from_millis(sample * 50),
                Report {
                    window_ms: 50,
                    fps: 30.0,
                    loss_pct: 17.0,
                    frames_lost: if sample % 6 == 0 { 2 } else { 0 },
                    unrecovered_loss_pct: Some(0.0),
                    ..Default::default()
                },
                false,
                false,
                20_000,
            );
        }
        assert_eq!(reason, Some(PathReason::UdpLoss { percent: 17 }));
    }

    #[test]
    fn an_initial_burst_that_fec_has_resolved_keeps_udp() {
        let mut policy = Policy::default();
        let start = Instant::now();
        let mut reason = None;
        for sample in 0..16 {
            reason = policy.observe(
                start + Duration::from_millis(sample * 50),
                Report {
                    window_ms: 50,
                    fps: 30.0,
                    loss_pct: 17.0,
                    frames_lost: u64::from(sample < 4),
                    unrecovered_loss_pct: Some(0.0),
                    ..Default::default()
                },
                false,
                false,
                20_000,
            );
        }
        assert_eq!(reason, None);
    }
}
