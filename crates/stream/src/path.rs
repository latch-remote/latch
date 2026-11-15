//! Session media policy and its viewer-facing state. Tailscale route discovery
//! can refine `UdpPath` independently of media transport selection.
use latch_proto::control::{PathAction, Report};
use latch_proto::wire::{self, Datagram};
use std::collections::VecDeque;
use std::time::{Duration, Instant};

#[non_exhaustive]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum UdpPath {
    #[default]
    Unknown,
    Direct,
    PeerRelay,
    Derp,
}

#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MediaPath {
    TailscaleUdp(UdpPath),
    LatchRelay,
}

#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Carrier {
    Native,
    Tls,
    WebSocket,
    HttpConnect,
}

impl Carrier {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::Tls => "tls",
            Self::WebSocket => "websocket",
            Self::HttpConnect => "http-connect",
        }
    }
}

impl std::fmt::Display for Carrier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Native => "Native",
            Self::Tls => "TLS",
            Self::WebSocket => "WebSocket",
            Self::HttpConnect => "HTTP CONNECT",
        })
    }
}

#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelayFailure {
    Closed,
    Stalled,
    Backpressure,
    Preparation,
}

#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathReason {
    Initial,
    UdpBlocked,
    UdpStalled,
    UdpLoss { percent: u8 },
    UdpFrameLoss { percent: u8 },
    UdpLimited { kbps: u32 },
    UdpRecovered,
    DatagramSize { bytes: u16 },
    RelayUnavailable(RelayFailure),
    RelayRenewal,
    CaptivePortal,
    TlsInterception,
    ProxyAuth,
}

impl PathReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Initial => "initial",
            Self::UdpBlocked => "udp-blocked",
            Self::UdpStalled => "udp-stalled",
            Self::UdpLoss { .. } => "udp-loss",
            Self::UdpFrameLoss { .. } => "udp-frame-loss",
            Self::UdpLimited { .. } => "udp-limited",
            Self::UdpRecovered => "udp-recovered",
            Self::DatagramSize { .. } => "datagram-size",
            Self::RelayUnavailable(RelayFailure::Closed) => "relay-closed",
            Self::RelayUnavailable(RelayFailure::Stalled) => "relay-stalled",
            Self::RelayUnavailable(RelayFailure::Backpressure) => "relay-backpressure",
            Self::RelayUnavailable(RelayFailure::Preparation) => "relay-preparation",
            Self::RelayRenewal => "relay-renewal",
            Self::CaptivePortal => "captive-portal",
            Self::TlsInterception => "tls-interception",
            Self::ProxyAuth => "proxy-auth",
        }
    }

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
            Self::DatagramSize { bytes } => format!("UDP uses {bytes}-byte datagrams"),
            Self::RelayUnavailable(failure) => match failure {
                RelayFailure::Closed => "The relay connection closed",
                RelayFailure::Stalled => "The relay stopped acknowledging video",
                RelayFailure::Backpressure => "The relay priority queue filled",
                RelayFailure::Preparation => "The relay could not connect",
            }
            .into(),
            Self::RelayRenewal => "Renewing the relay connection".into(),
            Self::CaptivePortal => "This network requires a browser sign-in".into(),
            Self::TlsInterception => "This network intercepts TLS connections".into(),
            Self::ProxyAuth => "The network proxy requires authentication".into(),
        }
    }
}

/// Local progress toward a possible transport change. The committed path
/// and its age remain available while negotiation or measurement runs.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathActivity {
    PreparingRelay,
    TestingUdp,
}

impl PathActivity {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PreparingRelay => "preparing-relay",
            Self::TestingUdp => "testing-udp",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::PreparingRelay => "Preparing the relay",
            Self::TestingUdp => "Testing the direct path",
        }
    }
}

#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaPathState {
    pub path: MediaPath,
    pub reason: PathReason,
    pub carrier: Option<Carrier>,
    /// The relay carrier is tunneled through an HTTP CONNECT proxy.
    pub via_proxy: bool,
    pub since: Instant,
    pub activity: Option<PathActivity>,
    /// Announced peak wire rate of the current UDP return probe.
    pub udp_probe_kbps: Option<u32>,
}

impl MediaPathState {
    /// Time on the committed path, suitable for duration fields in logs.
    pub fn age(&self) -> Duration {
        self.since.elapsed()
    }
}

/// Only committed media changes reach the notice stream. Diagnostic refinement
/// stays available through media_path(), and flapping coalesces to the latest state.
pub(crate) struct PathNotices {
    previous: MediaPathState,
    last_notice: Option<Instant>,
    last_emitted: Option<MediaPathState>,
    pending: Option<MediaPathState>,
}

impl PathNotices {
    pub(crate) fn new(previous: MediaPathState) -> Self {
        Self {
            previous,
            last_notice: None,
            last_emitted: None,
            pending: None,
        }
    }

    pub(crate) fn observe(
        &mut self,
        current: MediaPathState,
        now: Instant,
    ) -> Option<MediaPathState> {
        let changed_carrier = matches!(self.previous.path, MediaPath::LatchRelay)
            != matches!(current.path, MediaPath::LatchRelay);
        let changed_reason = self.previous.reason.as_str() != current.reason.as_str()
            && matches!(
                current.reason,
                PathReason::CaptivePortal | PathReason::TlsInterception | PathReason::ProxyAuth
            );
        if current.reason == PathReason::RelayRenewal {
            self.pending = None;
        } else if (changed_carrier || changed_reason)
            && !(self.previous.reason == PathReason::RelayRenewal
                && matches!(current.path, MediaPath::LatchRelay))
        {
            self.pending = Some(current.clone());
        }
        if self.pending.is_some() {
            self.pending = Some(current.clone());
        }
        self.previous = current;
        if self
            .last_notice
            .is_some_and(|last| now.saturating_duration_since(last) < Duration::from_secs(20))
        {
            return None;
        }
        let notice = self.pending.take()?;
        if self.last_emitted.as_ref().is_some_and(|last| {
            matches!(last.path, MediaPath::LatchRelay)
                == matches!(notice.path, MediaPath::LatchRelay)
                && !matches!(
                    notice.reason,
                    PathReason::CaptivePortal | PathReason::TlsInterception | PathReason::ProxyAuth
                )
        }) {
            return None;
        }
        self.last_emitted = Some(notice.clone());
        self.last_notice = Some(now);
        Some(notice)
    }
}

impl Default for MediaPathState {
    fn default() -> Self {
        Self {
            path: MediaPath::TailscaleUdp(UdpPath::Unknown),
            reason: PathReason::Initial,
            carrier: None,
            via_proxy: false,
            since: Instant::now(),
            activity: None,
            udp_probe_kbps: None,
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
        udp_damage: bool,
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
        if duration < 800 || !udp_damage {
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
                serial == (self.serial.wrapping_add(1) | 0x8000)
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
            self.start(self.serial.wrapping_add(1) | 0x8000, now);
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
                    12,
                    *size,
                );
                datagram(&bytes);
            }
            self.rounds += 1;
        }
        true
    }

    pub fn observe(&mut self, datagram: &Datagram<'_>, now: Instant) -> bool {
        if let Datagram::Probe {
            train,
            index,
            count: 12,
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
                if *train == (self.serial.wrapping_add(1) | 0x8000) {
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
    fn capacity_limiting_and_congestion_loss_need_independent_udp_damage_evidence() {
        let start = Instant::now();
        let mut policy = Policy::default();
        for tick in 0..200 {
            assert_eq!(
                policy.observe(
                    start + Duration::from_millis(tick * 50),
                    Report {
                        window_ms: 50,
                        fps: 60.0,
                        loss_pct: 25.0,
                        frames_lost: 3,
                        unrecovered_loss_pct: Some(20.0),
                        ..Default::default()
                    },
                    true,
                    true,
                    22_000,
                    false
                ),
                None
            );
        }
    }

    #[test]
    fn notices_ignore_startup_refinement_failed_preparation_mtu_and_renewal_and_coalesce_flapping()
    {
        let now = Instant::now();
        let mut state = MediaPathState::default();
        let mut notices = PathNotices::new(state.clone());
        state.path = MediaPath::TailscaleUdp(UdpPath::Direct);
        assert!(notices.observe(state.clone(), now).is_none());
        state.reason = PathReason::DatagramSize { bytes: 800 };
        assert!(notices.observe(state.clone(), now).is_none());
        // Failed preparation leaves the state untouched.
        assert!(notices.observe(state.clone(), now).is_none());
        state.path = MediaPath::LatchRelay;
        state.reason = PathReason::UdpBlocked;
        assert_eq!(notices.observe(state.clone(), now), Some(state.clone()));
        state.path = MediaPath::TailscaleUdp(UdpPath::Direct);
        state.reason = PathReason::RelayRenewal;
        assert!(notices
            .observe(state.clone(), now + Duration::from_secs(1))
            .is_none());
        state.path = MediaPath::LatchRelay;
        state.reason = PathReason::UdpBlocked;
        assert!(notices
            .observe(state.clone(), now + Duration::from_secs(2))
            .is_none());
        assert!(notices
            .observe(state.clone(), now + Duration::from_secs(20))
            .is_none());
        state.path = MediaPath::TailscaleUdp(UdpPath::Direct);
        state.reason = PathReason::UdpRecovered;
        assert!(notices
            .observe(state.clone(), now + Duration::from_secs(21))
            .is_some());
        state.path = MediaPath::LatchRelay;
        state.reason = PathReason::UdpLoss { percent: 17 };
        assert!(notices
            .observe(state.clone(), now + Duration::from_secs(22))
            .is_none());
        state.reason = PathReason::UdpLoss { percent: 18 };
        assert!(notices
            .observe(state.clone(), now + Duration::from_secs(23))
            .is_none());
        assert!(notices
            .observe(state.clone(), now + Duration::from_secs(41))
            .is_some());
        state.reason = PathReason::CaptivePortal;
        assert!(notices
            .observe(state.clone(), now + Duration::from_secs(42))
            .is_none());
        assert_eq!(
            notices.observe(state.clone(), now + Duration::from_secs(61)),
            Some(state)
        );
    }

    #[test]
    fn a_coalesced_round_trip_does_not_repeat_the_last_emitted_transport() {
        let now = Instant::now();
        let mut state = MediaPathState::default();
        let mut notices = PathNotices::new(state.clone());
        state.path = MediaPath::LatchRelay;
        state.reason = PathReason::UdpBlocked;
        assert!(notices.observe(state.clone(), now).is_some());
        state.path = MediaPath::TailscaleUdp(UdpPath::Direct);
        state.reason = PathReason::UdpRecovered;
        assert!(notices
            .observe(state.clone(), now + Duration::from_secs(5))
            .is_none());
        state.path = MediaPath::LatchRelay;
        state.reason = PathReason::UdpLoss { percent: 17 };
        assert!(notices
            .observe(state.clone(), now + Duration::from_secs(10))
            .is_none());
        assert!(notices
            .observe(state, now + Duration::from_secs(20))
            .is_none());
    }

    #[test]
    fn typed_path_extensions_have_stable_machine_strings_and_plain_descriptions() {
        assert_eq!(PathActivity::PreparingRelay.as_str(), "preparing-relay");
        assert_eq!(PathActivity::TestingUdp.as_str(), "testing-udp");
        assert_eq!(Carrier::Native.as_str(), "native");
        assert_eq!(Carrier::Tls.as_str(), "tls");
        assert_eq!(Carrier::WebSocket.as_str(), "websocket");
        assert_eq!(Carrier::HttpConnect.as_str(), "http-connect");
        for (reason, code) in [
            (PathReason::CaptivePortal, "captive-portal"),
            (PathReason::TlsInterception, "tls-interception"),
            (PathReason::ProxyAuth, "proxy-auth"),
        ] {
            assert_eq!(reason.as_str(), code);
            assert!(!reason.description().is_empty());
        }
        assert_eq!(
            PathReason::DatagramSize { bytes: 800 }.description(),
            "UDP uses 800-byte datagrams"
        );
    }

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
                        true,
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
                true,
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
                    12,
                    *size,
                );
                if *size <= 850 {
                    let datagram = wire::parse(&bytes).unwrap();
                    assert!(viewer.observe(&datagram, start));
                    assert!(viewer.observe(&datagram, start));
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
                    true,
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
                            12,
                            *size,
                        );
                        let datagram = wire::parse(&bytes).unwrap();
                        assert!(!viewer.observe(&datagram, start));
                        assert!(!viewer.observe(&datagram, start));
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
                true,
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
                true,
            );
        }
        assert_eq!(reason, None);
    }
}
