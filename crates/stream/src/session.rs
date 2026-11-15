//! One stream from a Latch host (`docs/PROTOCOL.md`): asks the host's
//! control port for a session, then receives video and audio over UDP,
//! decodes them, and sends input. Each session owns its sockets and
//! threads; nothing is global, so a session can start while the last one
//! is still letting go.
//!
//! Threads: the session's own thread runs the TCP control messages and
//! the timers that end a dead session; a receive thread takes every
//! datagram, puts frames together, plays audio and resends input; a decode
//! thread decodes frames in order and publishes the newest to the renderer.

use crate::audio::Player;
use crate::video::{self, Decoder, FrameSlot};
use latch_proto::audio::{AudioOut, AudioReceiver};
use latch_proto::control::{self, Accept, Codec, Message, Report, StreamRequest};
use latch_proto::gamepad::{Feedback, LatestReceiver, PadKind, PadSender, PadState};
use latch_proto::input::{Event as InputEvent, InputSender};
use latch_proto::rtt::RttEstimator;
use latch_proto::video::{
    InFrame, KeyframeRequests, Reassembled, Reassembler, ReassemblyStats, Recovery,
};
use latch_proto::wire::{self, Datagram};
use parking_lot::{Condvar, Mutex};
use std::collections::VecDeque;
use std::io::Write;
use std::net::{IpAddr, Shutdown, SocketAddr, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[cfg(test)]
#[path = "../../../tests/pairs/replay.rs"]
mod pair_interop;

/// Builds the configured production relay with standard certificate validation.
pub fn relay_config(settings: &control::RelaySettings) -> Option<latch_relay::ClientConfig> {
    if !settings.enabled {
        return None;
    }
    latch_relay::Endpoint::new(settings.host.clone(), settings.port)
        .and_then(latch_relay::ClientConfig::production)
        .map_err(|error| tracing::warn!(%error, "relay unavailable"))
        .ok()
}

#[derive(serde::Serialize, serde::Deserialize)]
struct RelayOffer {
    session: u32,
    attempt: u64,
    endpoint: latch_relay::Endpoint,
    viewer: latch_relay::Attachment,
    keys: latch_relay::SessionKeys,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RelayPhase {
    Idle,
    Preparing,
    Offered,
    CommitSent,
    Active,
}
struct PreparedRelay {
    grant_deadline: Instant,
    grant_bytes: u64,
    transport: Arc<latch_relay::Transport>,
    offer: control::RelayPayload,
}
const UDP_REASSESS_DWELL: Duration = Duration::from_secs(60);
const UDP_REASSESS_MAX_BACKOFF: Duration = Duration::from_secs(600);
const UDP_PROBE_SETTLE: Duration = Duration::from_millis(100);
const UDP_PROBE_REPORT: Duration = Duration::from_secs(2);

struct UdpProbe {
    first: u16,
    kbps: u32,
    started: Instant,
    sent: u32,
    seen: Vec<u16>,
    first_received: Option<Instant>,
    last_received: Option<Instant>,
}
impl UdpProbe {
    fn new(first: u16, kbps: u32) -> Self {
        Self {
            first,
            kbps,
            started: Instant::now(),
            sent: 0,
            seen: vec![
                0;
                (kbps as usize * 1000 / 8 / (latch_proto::MAX_DATAGRAM + 80) / 16).max(1)
            ],
            first_received: None,
            last_received: None,
        }
    }
    fn packets(&self) -> u32 {
        self.seen.len() as u32 * 16
    }
    fn healthy(&self, received: u32, span_us: u32) -> bool {
        // Count every missing position, including lost tails. Duplicates never
        // count, and a short burst or a backed-up queue cannot prove capacity.
        self.sent == self.packets()
            && received <= self.packets()
            && received * 1000 >= self.packets() * 995
            && (800_000..=1_200_000).contains(&span_us)
            && u64::from(received) * (latch_proto::MAX_DATAGRAM + 80) as u64 * 8 * 1000
                >= u64::from(self.kbps) * u64::from(span_us) * 9 / 10
    }
}

struct RelayAttempt {
    number: u64,
    phase: RelayPhase,
    transport: Option<Arc<latch_relay::Transport>>,
    ready: bool,
    peer_ready: bool,
    started: Instant,
    retry_at: Instant,
    failures: u32,
    next_probe: Instant,
    probe_delay: Duration,
    probe_serial: u16,
    udp_probe: Option<UdpProbe>,
    returning_direct: bool,
    grant_deadline: Option<Instant>,
    grant_bytes: u64,
    traffic_bytes: u64,
    media_delivered: bool,
    cancel: latch_relay::Cancel,
    preparing: Option<(
        mpsc::Receiver<std::io::Result<PreparedRelay>>,
        std::thread::JoinHandle<()>,
    )>,
}

/// Session-scoped routing shared by the host and viewer. Exactly one receive loop
/// calls `poll`. Negotiation always uses the original authorized TCP connection.
pub struct RelayRoute {
    session: u32,
    role: latch_relay::Role,
    config: Option<latch_relay::ClientConfig>,
    state: Mutex<RelayAttempt>,
    active: AtomicBool,
    failed: AtomicBool,
    fatal: AtomicBool,
    revision: AtomicU64,
}
impl RelayRoute {
    pub fn new(
        session: u32,
        role: latch_relay::Role,
        config: Option<latch_relay::ClientConfig>,
    ) -> Self {
        let now = Instant::now();
        Self {
            session,
            role,
            config,
            state: Mutex::new(RelayAttempt {
                number: 0,
                phase: RelayPhase::Idle,
                transport: None,
                ready: false,
                peer_ready: false,
                started: now,
                retry_at: now,
                failures: 0,
                next_probe: now + UDP_REASSESS_DWELL,
                probe_delay: UDP_REASSESS_DWELL,
                probe_serial: 0,
                udp_probe: None,
                returning_direct: false,
                grant_deadline: None,
                grant_bytes: 0,
                traffic_bytes: 0,
                media_delivered: false,
                cancel: latch_relay::Cancel::new(),
                preparing: None,
            }),
            active: AtomicBool::new(false),
            failed: AtomicBool::new(false),
            fatal: AtomicBool::new(false),
            revision: AtomicU64::new(0),
        }
    }
    pub(crate) fn fail(&self) {
        self.fatal.store(true, Ordering::Release);
    }
    pub fn active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }
    pub fn revision(&self) -> u64 {
        self.revision.load(Ordering::Acquire)
    }
    pub fn pending(&self) -> bool {
        self.state.lock().phase != RelayPhase::Idle
    }
    pub fn number(&self) -> u64 {
        self.state.lock().number
    }
    fn message(&self, attempt: u64, action: control::RelayAction) -> Message {
        Message::Relay(control::RelayControl {
            session: self.session,
            attempt,
            action,
        })
    }
    fn activate(&self, state: &mut RelayAttempt) {
        tracing::info!(role = ?self.role, attempt = state.number, "using Latch TLS relay");
        state.phase = RelayPhase::Active;
        state.started = Instant::now();
        state.next_probe = state.next_probe.max(state.started + UDP_REASSESS_DWELL);
        self.revision.fetch_add(1, Ordering::AcqRel);
        self.active.store(true, Ordering::Release);
    }
    fn reset(&self, state: &mut RelayAttempt) {
        // A close polled just before a TCP handoff/Abort belongs to this reset.
        // Leaving it pending would reset Idle again and move its retry deadline.
        self.failed.store(false, Ordering::Release);
        self.active.store(false, Ordering::Release);
        self.revision.fetch_add(1, Ordering::AcqRel);
        state.cancel.cancel();
        if let Some(transport) = state.transport.take() {
            transport.shutdown();
        }
        // Keep the one cancellable grant worker until it has joined. A new
        // attempt cannot allocate another worker while this one is unwinding.
        let grant_spent = state
            .grant_deadline
            .is_some_and(|deadline| Instant::now() + Duration::from_millis(250) >= deadline)
            || (state.grant_bytes > 0
                && state.traffic_bytes >= state.grant_bytes - state.grant_bytes / 10);
        if state.phase == RelayPhase::Active
            && (state.started.elapsed() >= Duration::from_secs(30) || grant_spent)
        {
            // A finite grant ending normally earns a fresh grant, even when a
            // small byte allowance runs out sooner than the healthy dwell.
            state.failures = 0;
        }
        state.grant_deadline = None;
        state.grant_bytes = 0;
        state.traffic_bytes = 0;
        state.media_delivered = false;
        state.phase = RelayPhase::Idle;
        state.udp_probe = None;
        state.returning_direct = false;
        state.ready = false;
        state.peer_ready = false;
        state.failures = state.failures.saturating_add(1);
        let cooldown = if state.failures >= 3 { 30 } else { 2 };
        state.retry_at = Instant::now() + Duration::from_secs(cooldown);
    }
    /// Called by the host control owner. Padding is bounded to one second,
    /// 50 Mbps, and 64 packets per turn. Media stays exclusively on the relay.
    pub fn reassess_udp(
        &self,
        udp: &UdpSocket,
        addr: SocketAddr,
        requested_kbps: u32,
        mut send: impl FnMut(&Message) -> bool,
    ) -> bool {
        let mut state = self.state.lock();
        if self.role != latch_relay::Role::Host || state.phase != RelayPhase::Active {
            return true;
        }
        let now = Instant::now();
        // 25% headroom covers packet/FEC overhead. Higher requested rates
        // keep the relay until a correspondingly larger check is supported.
        let kbps = requested_kbps.saturating_mul(5).div_ceil(4).max(2000);
        if state.udp_probe.is_none()
            && !state.returning_direct
            && now >= state.next_probe
            && kbps <= 50_000
        {
            state.probe_serial = state.probe_serial.wrapping_add(512);
            let first = state.probe_serial;
            state.udp_probe = Some(UdpProbe::new(first, kbps));
            state.next_probe = now + state.probe_delay;
            state.probe_delay = (state.probe_delay * 2).min(UDP_REASSESS_MAX_BACKOFF);
            if !send(&self.message(state.number, control::RelayAction::UdpProbe { first, kbps })) {
                return false;
            }
        }
        let Some(probe) = state.udp_probe.as_mut() else {
            return true;
        };
        let elapsed = now.saturating_duration_since(probe.started + UDP_PROBE_SETTLE);
        if elapsed > Duration::from_millis(1250) {
            return true;
        }
        let due = (elapsed.as_secs_f64() * f64::from(probe.kbps) * 1000.0
            / 8.0
            / (latch_proto::MAX_DATAGRAM + 80) as f64) as u32;
        let end = due.min(probe.packets()).min(probe.sent + 64);
        let mut bytes = Vec::with_capacity(latch_proto::MAX_DATAGRAM);
        while probe.sent < end {
            wire::write_probe(
                &mut bytes,
                self.session,
                probe.first.wrapping_add((probe.sent / 16) as u16),
                (probe.sent % 16) as u8,
                16,
                latch_proto::MAX_DATAGRAM,
            );
            // A failed send remains a missing position in the result.
            let _ = udp.send_to(&bytes, addr);
            probe.sent += 1;
        }
        true
    }

    /// Only direct UDP enters this meter, separately from relay media reports.
    pub fn observe_udp_probe(&self, datagram: &Datagram<'_>, now: Instant) {
        let Datagram::Probe {
            train,
            index,
            count: 16,
            len,
            ..
        } = datagram
        else {
            return;
        };
        if *len != latch_proto::MAX_DATAGRAM || *index >= 16 {
            return;
        }
        let mut state = self.state.lock();
        if self.role != latch_relay::Role::Viewer || state.phase != RelayPhase::Active {
            return;
        }
        let Some(probe) = state.udp_probe.as_mut() else {
            return;
        };
        if now.duration_since(probe.started) >= UDP_PROBE_REPORT {
            return;
        }
        let train = usize::from(train.wrapping_sub(probe.first));
        if let Some(seen) = probe.seen.get_mut(train) {
            let bit = 1 << index;
            if *seen & bit == 0 {
                *seen |= bit;
                probe.first_received.get_or_insert(now);
                probe.last_received = Some(now);
            }
        }
    }

    pub fn shutdown(&self) {
        let mut state = self.state.lock();
        self.reset(&mut state);
        if let Some((_, worker)) = state.preparing.take() {
            let _ = worker.join();
        }
    }
    /// Priority failure terminates the session so held input is released.
    pub fn send_datagram(&self, bytes: &[u8]) -> std::io::Result<bool> {
        let mut state = self.state.lock();
        if state.phase != RelayPhase::Active {
            return Ok(false);
        }
        match state
            .transport
            .as_ref()
            .ok_or(std::io::ErrorKind::NotConnected)?
            .try_send_datagram(bytes)
        {
            Ok(()) => {
                state.traffic_bytes = state.traffic_bytes.saturating_add((bytes.len() * 2) as u64);
                Ok(true)
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                self.fail();
                Err(error)
            }
            Err(_) => {
                self.failed.store(true, Ordering::Release);
                Ok(false)
            }
        }
    }
    pub fn send_control(&self, message: &Message) -> std::io::Result<bool> {
        // Lifecycle must survive relay teardown. Keep every cursor update on
        // one ordered connection, regardless of serialized shape size.
        if matches!(message, Message::End(_) | Message::Cursor(_)) {
            return Ok(false);
        }
        let mut state = self.state.lock();
        if state.phase != RelayPhase::Active {
            return Ok(false);
        }
        let mut bytes = Vec::new();
        control::write_message(&mut bytes, message)?;
        // Oversized controls retain the existing authorized TCP path.
        if bytes.len() > 16_359 {
            return Ok(false);
        }
        match state
            .transport
            .as_ref()
            .ok_or(std::io::ErrorKind::NotConnected)?
            .try_send_control(&bytes)
        {
            Ok(()) => {
                state.traffic_bytes = state.traffic_bytes.saturating_add((bytes.len() * 2) as u64);
                Ok(true)
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                self.fail();
                Err(error)
            }
            Err(_) => {
                self.failed.store(true, Ordering::Release);
                Ok(false)
            }
        }
    }
    pub fn queued_frame_age(&self) -> Duration {
        self.state
            .lock()
            .transport
            .as_ref()
            .map_or(Duration::ZERO, |transport| transport.queued_frame_age())
    }

    pub fn take_frame_ack_elapsed(&self) -> Option<Duration> {
        let mut state = self.state.lock();
        let elapsed = state.transport.as_ref()?.take_frame_ack_elapsed();
        state.media_delivered |= elapsed.is_some();
        elapsed
    }

    pub fn send_frame(&self, frame: &InFrame) -> std::io::Result<bool> {
        let mut state = self.state.lock();
        if state.phase != RelayPhase::Active {
            return Ok(false);
        }
        match state
            .transport
            .as_ref()
            .ok_or(std::io::ErrorKind::NotConnected)?
            .try_send_frame(frame)
        {
            Ok(()) => {
                state.traffic_bytes = state
                    .traffic_bytes
                    .saturating_add((frame.data.len() * 2) as u64);
                Ok(true)
            }
            Err(error) => {
                if error.kind() != std::io::ErrorKind::WouldBlock {
                    self.failed.store(true, Ordering::Release);
                }
                Err(error)
            }
        }
    }
    /// Consumes transport events in one place. Readiness is E2E on both lanes.
    pub fn poll(&self) -> Option<(u64, latch_relay::Event)> {
        let mut state = self.state.lock();
        let event = state.transport.as_ref()?.recv_timeout(Duration::ZERO)?;
        let received = match &event {
            latch_relay::Event::Frame(frame) => frame.data.len(),
            latch_relay::Event::Datagram(bytes) | latch_relay::Event::Control(bytes) => bytes.len(),
            _ => 0,
        };
        state.traffic_bytes = state.traffic_bytes.saturating_add((received * 2) as u64);
        state.media_delivered |= matches!(event, latch_relay::Event::Frame(_));
        match event {
            latch_relay::Event::Ready => {
                state.ready = true;
                None
            }
            latch_relay::Event::Closed { kind, message } => {
                tracing::warn!(role = ?self.role, ?kind, %message, "relay closed, falling back to UDP");
                self.failed.store(true, Ordering::Release);
                None
            }
            event if state.phase == RelayPhase::Active || state.phase == RelayPhase::CommitSent => {
                Some((self.revision(), event))
            }
            _ => None,
        }
    }
    /// Runs on the TCP control owner. Return false for an authority write failure.
    pub fn maintain(&self, wanted: bool, mut send: impl FnMut(&Message) -> bool) -> bool {
        use control::RelayAction as A;
        if self.fatal.load(Ordering::Acquire) {
            return false;
        }
        let mut state = self.state.lock();
        let renewing = self.role == latch_relay::Role::Host
            && state.phase == RelayPhase::Active
            && (state
                .grant_deadline
                .is_some_and(|deadline| Instant::now() + Duration::from_millis(250) >= deadline)
                || (state.media_delivered
                    && state.grant_bytes > 0
                    && state.traffic_bytes >= state.grant_bytes - state.grant_bytes / 4));
        if renewing {
            // Application bytes omit TLS overhead. Leave a quarter of the
            // advertised combined allowance for records and in-flight media.
            let number = state.number;
            self.reset(&mut state);
            state.failures = 1;
            state.retry_at = Instant::now() + Duration::from_secs(2);
            if !send(&self.message(number, A::Renew)) {
                return false;
            }
        }
        if state.phase == RelayPhase::Active
            && state.udp_probe.as_ref().is_some_and(|p| {
                p.started.elapsed()
                    >= UDP_PROBE_REPORT
                        + if self.role == latch_relay::Role::Host {
                            Duration::from_secs(1)
                        } else {
                            Duration::ZERO
                        }
            })
        {
            let probe = state.udp_probe.take().unwrap();
            if self.role == latch_relay::Role::Viewer {
                let received = probe.seen.iter().map(|bits| bits.count_ones()).sum();
                let span_us = probe
                    .first_received
                    .zip(probe.last_received)
                    .map_or(0, |(first, last)| {
                        last.duration_since(first).as_micros() as u32
                    });
                if !send(&self.message(
                    state.number,
                    A::UdpProbeResult {
                        first: probe.first,
                        received,
                        span_us,
                    },
                )) {
                    return false;
                }
            }
        }
        let failed = self.failed.swap(false, Ordering::AcqRel);
        if failed
            || (state.returning_direct && state.started.elapsed() >= Duration::from_secs(6))
            || (state.phase != RelayPhase::Idle
                && state.phase != RelayPhase::Active
                && state.started.elapsed() >= Duration::from_secs(6))
        {
            let number = state.number;
            let healthy_handoff = failed && state.returning_direct;
            self.reset(&mut state);
            if healthy_handoff {
                state.failures = 0;
                state.retry_at = Instant::now() + Duration::from_secs(2);
            }
            if !send(&self.message(number, A::Abort)) {
                return false;
            }
        }
        if state
            .preparing
            .as_ref()
            .is_some_and(|(_, worker)| worker.is_finished())
        {
            let (result, worker) = state.preparing.take().unwrap();
            let _ = worker.join();
            if state.phase == RelayPhase::Preparing {
                let prepared = result
                    .try_recv()
                    .map_err(std::io::Error::other)
                    .and_then(|prepared| prepared);
                match prepared {
                    Ok(prepared) => {
                        state.grant_deadline = Some(prepared.grant_deadline);
                        state.grant_bytes = prepared.grant_bytes;
                        state.transport = Some(prepared.transport);
                        state.phase = RelayPhase::Offered;
                        if !send(&self.message(state.number, A::Offer(prepared.offer))) {
                            return false;
                        }
                    }
                    Err(error) => {
                        tracing::warn!(%error, "relay preparation failed");
                        let number = state.number;
                        self.reset(&mut state);
                        if !send(&self.message(number, A::Abort)) {
                            return false;
                        }
                    }
                }
            }
        }
        if self.role == latch_relay::Role::Viewer
            && wanted
            && self.config.is_some()
            && state.phase == RelayPhase::Idle
            && state.preparing.is_none()
            && Instant::now() >= state.retry_at
        {
            let Some(number) = state.number.checked_add(1) else {
                return true;
            };
            state.number = number;
            state.phase = RelayPhase::Preparing;
            state.started = Instant::now();
            state.cancel = latch_relay::Cancel::new();
            if !send(&self.message(number, A::Request)) {
                return false;
            }
        }
        if state.ready && state.phase == RelayPhase::Offered {
            if self.role == latch_relay::Role::Viewer && !state.peer_ready {
                state.peer_ready = true;
                if !send(&self.message(state.number, A::Ready)) {
                    return false;
                }
            } else if self.role == latch_relay::Role::Host && state.peer_ready {
                state.phase = RelayPhase::CommitSent;
                if !send(&self.message(state.number, A::Commit)) {
                    return false;
                }
            }
        }
        true
    }
    pub fn handle(
        &self,
        message: control::RelayControl,
        mut send: impl FnMut(&Message) -> bool,
    ) -> bool {
        use control::RelayAction as A;
        if message.session != self.session || message.attempt == 0 {
            return true;
        }
        tracing::debug!(role = ?self.role, attempt = message.attempt, action = ?message.action, "relay negotiation");
        let mut state = self.state.lock();
        if self.role == latch_relay::Role::Host && matches!(message.action, A::Request) {
            if self.config.is_none()
                || state.phase != RelayPhase::Idle
                || state.preparing.is_some()
                || message.attempt <= state.number
            {
                return true;
            }
            if let Some(remaining) = state.retry_at.checked_duration_since(Instant::now()) {
                // Never silently discard a valid request at the cooldown edge.
                // The viewer otherwise waits six seconds for an offer while
                // its media watchdog is already counting the same outage.
                return send(&self.message(
                    message.attempt,
                    A::RetryAfter {
                        millis: remaining.as_millis().clamp(250, 30_000) as u32,
                    },
                ));
            }
            let config = self.config.as_ref().unwrap().clone();
            state.number = message.attempt;
            state.phase = RelayPhase::Preparing;
            state.started = Instant::now();
            state.cancel = latch_relay::Cancel::new();
            let cancel = state.cancel.clone();
            let session = self.session;
            let attempt = state.number;
            let (tx, rx) = mpsc::sync_channel(1);
            match std::thread::Builder::new()
                .name("relay-grant".into())
                .spawn(move || {
                    let prepared = (|| {
                        let granted = Instant::now();
                        let grant = latch_relay::create_grant(&config, &cancel)?;
                        let grant_deadline =
                            granted + Duration::from_secs(u64::from(grant.host.session_seconds));
                        let grant_bytes = grant.host.session_bytes;
                        if cancel.is_cancelled() {
                            return Err(std::io::ErrorKind::Interrupted.into());
                        }
                        let keys = latch_relay::SessionKeys::generate()?;
                        let offer = RelayOffer {
                            session,
                            attempt,
                            endpoint: config.endpoint.clone(),
                            viewer: grant.viewer,
                            keys: keys.clone(),
                        };
                        let offer = control::RelayPayload::new(serde_json::to_string(&offer)?)?;
                        let transport =
                            Arc::new(latch_relay::Transport::connect(config, grant.host, keys)?);
                        if cancel.is_cancelled() {
                            transport.shutdown();
                            return Err(std::io::ErrorKind::Interrupted.into());
                        }
                        Ok(PreparedRelay {
                            transport,
                            offer,
                            grant_deadline,
                            grant_bytes,
                        })
                    })();
                    let _ = tx.send(prepared);
                }) {
                Ok(worker) => state.preparing = Some((rx, worker)),
                Err(_) => self.reset(&mut state),
            }
            return true;
        }
        if message.attempt != state.number {
            return true;
        }
        match message.action {
            A::Offer(payload)
                if self.role == latch_relay::Role::Viewer
                    && state.phase == RelayPhase::Preparing =>
            {
                let offer: Result<RelayOffer, _> = serde_json::from_str(payload.as_str());
                let config = self.config.as_ref();
                if let (Ok(offer), Some(config)) = (offer, config) {
                    if offer.session == self.session
                        && offer.attempt == state.number
                        && offer.viewer.role() == latch_relay::Role::Viewer
                        && offer.endpoint.host() == config.endpoint.host()
                        && offer.endpoint.port() == config.endpoint.port()
                    {
                        state.grant_deadline = Some(
                            state.started
                                + Duration::from_secs(u64::from(offer.viewer.session_seconds)),
                        );
                        state.grant_bytes = offer.viewer.session_bytes;
                        if let Ok(transport) = latch_relay::Transport::connect(
                            config.clone(),
                            offer.viewer,
                            offer.keys,
                        ) {
                            state.transport = Some(Arc::new(transport));
                            state.phase = RelayPhase::Offered;
                            return true;
                        }
                    }
                }
                self.reset(&mut state);
                return send(&self.message(message.attempt, A::Abort));
            }
            A::Ready
                if self.role == latch_relay::Role::Host && state.phase == RelayPhase::Offered =>
            {
                state.peer_ready = true
            }
            A::Commit
                if self.role == latch_relay::Role::Viewer
                    && state.phase == RelayPhase::Offered
                    && state.ready =>
            {
                self.activate(&mut state);
                return send(&self.message(message.attempt, A::Committed));
            }
            A::Committed
                if self.role == latch_relay::Role::Host
                    && state.phase == RelayPhase::CommitSent
                    && state.ready
                    && state.peer_ready =>
            {
                self.activate(&mut state)
            }
            A::UdpProbe { first, kbps }
                if self.role == latch_relay::Role::Viewer
                    && state.phase == RelayPhase::Active
                    && state.udp_probe.is_none()
                    && (2000..=50_000).contains(&kbps) =>
            {
                state.udp_probe = Some(UdpProbe::new(first, kbps));
            }
            A::UdpProbeResult {
                first,
                received,
                span_us,
            } if self.role == latch_relay::Role::Host && state.phase == RelayPhase::Active => {
                if let Some(probe) = state.udp_probe.take() {
                    if probe.first != first {
                        state.udp_probe = Some(probe);
                    } else if probe.healthy(received, span_us) {
                        state.returning_direct = true;
                        state.started = Instant::now();
                        return send(&self.message(message.attempt, A::Direct));
                    }
                }
            }
            A::Direct
                if self.role == latch_relay::Role::Viewer && state.phase == RelayPhase::Active =>
            {
                self.reset(&mut state);
                // A measured, healthy handoff is not a failed relay attempt.
                state.failures = 0;
                state.retry_at = Instant::now() + Duration::from_secs(2);
                return send(&self.message(message.attempt, A::DirectReady));
            }
            A::DirectReady if self.role == latch_relay::Role::Host && state.returning_direct => {
                self.reset(&mut state);
                state.failures = 0;
                state.retry_at = Instant::now() + Duration::from_secs(2);
            }
            A::RetryAfter { millis }
                if self.role == latch_relay::Role::Viewer
                    && state.phase == RelayPhase::Preparing =>
            {
                let failures = state.failures;
                self.reset(&mut state);
                // Waiting for the host's cooldown did not test the relay path.
                state.failures = failures;
                state.retry_at =
                    Instant::now() + Duration::from_millis(u64::from(millis.clamp(250, 30_000)));
            }
            A::Renew
                if self.role == latch_relay::Role::Viewer
                    && (state.phase == RelayPhase::Active || state.phase == RelayPhase::Idle) =>
            {
                if state.phase == RelayPhase::Active {
                    self.reset(&mut state);
                }
                state.failures = 1;
                state.retry_at = Instant::now() + Duration::from_secs(2);
            }
            A::Abort if state.phase != RelayPhase::Idle => self.reset(&mut state),
            _ => {}
        }
        true
    }
}
impl Drop for RelayRoute {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Includes takeover (up to 30 s), this viewer's prep (15 s), and capture open.
const START_TIMEOUT: Duration = Duration::from_secs(60);
/// After the host accepts, its first decoded picture must arrive within this.
const MEDIA_TIMEOUT: Duration = Duration::from_secs(8);
/// Still desktops produce idle frames every 100 ms. Give loss recovery and
/// decoder recovery eight seconds, independently of TCP keepalives.
const MEDIA_STALL_TIMEOUT: Duration = Duration::from_secs(8);
/// Four seconds of 1 kHz input, or roughly 256 KiB of pasted text. Beyond
/// this, end the session so its cleanup releases held keys and buttons.
const MAX_PENDING_INPUT: usize = 4096;
/// Allows the host's two-second injection retry grace plus network recovery.
const INPUT_ACK_TIMEOUT: Duration = Duration::from_secs(5);
/// A host heard from neither over TCP nor UDP for this long is gone.
const SILENT: Duration = Duration::from_secs(10);
const KEEPALIVE: Duration = Duration::from_secs(1);
const HELLO_FAST: Duration = Duration::from_millis(50);
const HELLO_SLOW: Duration = Duration::from_secs(1);
const PING_EVERY: Duration = Duration::from_millis(500);
/// Complete frames waiting for the decoder. A network stall delivers the
/// frames it held back all at once, and a decoder that is fast enough works
/// through them, so the count only has to bound memory. What says the
/// decoder cannot keep up is the age of the oldest frame waiting
/// ([`decode_max_age`]): past that the queue is dropped and the stream
/// restarts at a keyframe, rather than the picture falling further behind.
const DECODE_QUEUE: usize = 8;
/// Three frame intervals, between 12 and 100 ms.
fn decode_max_age(fps: u32) -> Duration {
    Duration::from_secs_f64((3.0 / f64::from(fps.max(1))).clamp(0.012, 0.100))
}
const REPORT_EVERY: Duration = Duration::from_millis(50);
const STATS_EVERY: Duration = Duration::from_millis(250);
/// How long a stopping session waits for a decode in progress. A platform
/// decoder can block inside the OS (VideoToolbox after a sleep or a GPU
/// reset); the window must still be able to leave the stream.
const DECODER_STOP: Duration = Duration::from_secs(1);
/// Access units an AV1, HEVC or H.264 High decoder may take without a
/// picture, or decode errors it may make since its last picture, before
/// the session stops trusting it and asks the host again without that
/// codec. These
/// have no `latch-codec` decoder behind them as Baseline does (see
/// [`video::Fallback`]); one that gave pictures and then fails is judged
/// the same way. Ten access units is a sixth of a second at 60 fps.
const HARDWARE_PROBATION: u32 = 10;
const HARDWARE_ERRORS: u32 = 3;
/// How long the end waits for the receive thread, which also plays sound.
const RECEIVER_STOP: Duration = Duration::from_secs(2);
/// A control message the host does not take within this breaks the
/// session, rather than blocking whoever sent it (the UI, for a cursor
/// change).
const CONTROL_WRITE_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// A connection stage is starting.
    Stage(String),
    Connected,
    /// Setup failed before the stream started. The `connecting` stage
    /// denotes a recoverable connection/media failure. Other stages need
    /// the user to resolve a refusal or local setup failure before retrying.
    Failed {
        stage: String,
        message: String,
    },
    /// The stream ended after it started. `code` 0 means the host or this
    /// side ended it on purpose; the others are `latch_proto::control::end`
    /// codes, or -1 when the connection broke.
    Terminated {
        code: i32,
        message: String,
    },
    /// The network is struggling (`true`) or fine again (`false`).
    Poor(bool),
    /// This machine cannot play the stream's sound; the reason.
    NoAudio(String),
    /// Something the host wants the person to know (it cannot inject
    /// input, say).
    Notice(String),
}

/// Where the host is.
#[derive(Debug, Clone)]
pub struct Server {
    pub address: IpAddr,
    /// The host's control port, which is also its media port.
    pub port: u16,
}

#[derive(Debug, Clone)]
pub struct Settings {
    /// Host advertised joint colour ranking and the user chose Auto.
    pub prefer_color: bool,
    pub color: latch_proto::color::ColorCapabilities,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    /// Ask for HEVC when both sides can; otherwise H.264.
    pub hevc: bool,
    /// Ask for AV1 when both sides can.
    pub av1: bool,
    pub audio: bool,
    /// This machine's name, for the host's log.
    pub name: String,
    /// Ask the host to stream cursor shapes and omit the pointer from video.
    pub cursor: bool,
    /// Offer gamepads.
    pub gamepads: bool,
    /// Send this machine's microphone, where the host can play it.
    pub mic: bool,
}

/// Where a session sends the host's rumble and lights: slot and feedback.
pub type FeedbackFn = Box<dyn Fn(u8, Feedback) + Send + Sync>;

#[derive(Debug, Clone, Default)]
pub struct CursorState {
    pub visible: bool,
    pub shape: Option<Arc<latch_proto::control::CursorShape>>,
}

#[derive(Debug, Clone, Default)]
pub struct Stats {
    pub color: latch_proto::color::ColorDescription,
    pub fps: f32,
    /// Encoded video in the frames that came together, in Mbps: what the
    /// bitrate setting caps. FEC parity, headers and audio are not in it.
    pub mbps: f32,
    /// Received media in Mbps: UDP includes FEC and Latch headers; relay
    /// includes complete video and priority payloads, excluding TLS/TCP headers.
    pub received_mbps: f32,
    pub rtt_ms: u32,
    pub rtt_var_ms: u32,
    pub decode_ms: f32,
    pub decode_p95_ms: f32,
    pub frame_budget_ms: f32,
    /// Sustained decoder throughput; zero until overload is detected.
    pub decode_capacity_fps: u32,
    /// Capture to send on the host.
    pub host_ms: f32,
    /// First shard to whole frame.
    pub assembly_ms: f32,
    /// Whole frame to the decoder starting on it.
    pub queue_ms: f32,
    pub width: u32,
    pub height: u32,
    pub decoder: &'static str,
    /// The host's encoder, in its own words.
    pub encoder: String,
    pub codec: &'static str,
    /// Share of video packets in the last second that arrived too late or
    /// not at all and could not be rebuilt from FEC.
    pub loss_pct: f32,
    /// Packets FEC did rebuild in the last second.
    pub fec_recovered: u32,
    /// Where sound goes ("audio → MacBook Pro Speakers"), or why it does
    /// not; empty until the first second of video.
    pub audio: String,
    /// Where the delay between an audio packet arriving and its sound
    /// leaving the speaker goes.
    pub audio_latency: crate::audio::Latency,
    /// Where this machine's microphone goes, or why it does not; empty
    /// when it was not asked for.
    pub mic: String,
    /// Persistent video diagnosis, even while the transport is connected.
    pub video_problem: Option<String>,
    pub applied_bitrate_kbps: u32,
    pub wire_budget_kbps: u32,
    pub application_limited: bool,
    pub requested_bitrate_kbps: u32,
    pub pacing_queue_ms: f32,
    pub send_errors: u64,
    pub send_retries: u64,
    pub socket_send_bytes: usize,
    pub socket_receive_bytes: usize,
    pub host_socket_send_bytes: usize,
    pub host_socket_receive_bytes: usize,
    pub frames_lost: u64,
    pub window_ms: u32,
    /// Whole frames put together a second, before the decoder: what the
    /// network brought, where [`Stats::fps`] is what was decoded.
    pub received_fps: f32,
    /// Share of video packets in the last second the network lost,
    /// whether FEC rebuilt them or not; [`Stats::loss_pct`] is what was
    /// left after FEC.
    pub raw_loss_pct: f32,
    /// Frames that arrived whole in the last second but were never
    /// decoded: thrown away while the decoder caught up or while the
    /// stream waited for a keyframe.
    pub frames_dropped: u64,
    /// How unevenly whole frames arrive: the mean distance of each gap
    /// between two from the mean gap, in ms.
    pub frame_jitter_ms: f32,
}

/// Every loss of the session and how it ended, for tools that measure
/// recovery ([`Session::recovery_log`]).
#[derive(Debug, Clone, Default)]
pub struct RecoveryLog {
    /// Keyframes and recovery frames delivered, and their bytes.
    pub keyframes: u64,
    pub keyframe_bytes: u64,
    pub recovery_frames: u64,
    pub recovery_bytes: u64,
    /// Oldest first; at most [`RecoveryLog::KEPT`].
    pub losses: Vec<LossEvent>,
}

impl RecoveryLog {
    pub const KEPT: usize = 100_000;
}

/// One loss, from the frame given up on to the frame that ended it.
#[derive(Debug, Clone, PartialEq)]
pub struct LossEvent {
    /// From the loss being declared to the next frame delivered.
    pub resume_ms: f32,
    /// From the last frame delivered before the loss to the next one:
    /// how long the picture stood still.
    pub freeze_ms: f32,
    /// Ended by a keyframe rather than a recovery frame.
    pub keyframe: bool,
    /// The size of the frame that ended it.
    pub bytes: usize,
}

/// Loss bookkeeping for [`RecoveryLog`].
#[derive(Default)]
struct Losses {
    /// When the current loss was declared, and the delivery before it.
    open: Option<(Instant, Option<Instant>)>,
    last_delivery: Option<Instant>,
}

impl Losses {
    fn lost(&mut self, now: Instant) {
        if self.open.is_none() {
            self.open = Some((now, self.last_delivery));
        }
    }

    fn delivered(&mut self, log: &Mutex<RecoveryLog>, frame: &InFrame, now: Instant) {
        let ended = self.open.take();
        self.last_delivery = Some(now);
        if !frame.keyframe && !frame.recovery && ended.is_none() {
            return;
        }
        let mut log = log.lock();
        if frame.keyframe {
            log.keyframes += 1;
            log.keyframe_bytes += frame.data.len() as u64;
        }
        if frame.recovery {
            log.recovery_frames += 1;
            log.recovery_bytes += frame.data.len() as u64;
        }
        if let Some((at, before)) = ended.filter(|_| log.losses.len() < RecoveryLog::KEPT) {
            let ms = |t: Instant| now.saturating_duration_since(t).as_secs_f32() * 1000.0;
            log.losses.push(LossEvent {
                resume_ms: ms(at),
                freeze_ms: before.map_or(0.0, ms),
                keyframe: frame.keyframe,
                bytes: frame.data.len(),
            });
        }
    }
}

/// Test knob: `LATCH_TEST_VIDEO_LOSS=<percent>[:<burst>]` drops that share
/// of the host's video datagrams on arrival, `burst` in a row (default 1),
/// so loss recovery can be measured on a clean network. Seeded, so runs
/// compare.
struct TestLoss {
    chance: f64,
    burst: u32,
    left: u32,
    seed: u64,
}

impl TestLoss {
    fn from_env() -> Option<Self> {
        let value = std::env::var("LATCH_TEST_VIDEO_LOSS").ok()?;
        let (percent, burst) = value.split_once(':').unwrap_or((&value, "1"));
        let percent: f64 = percent.trim().parse().ok()?;
        let burst: u32 = burst.trim().parse().ok().filter(|&b| b > 0)?;
        (percent > 0.0).then(|| Self {
            // A burst starts this often, so the share dropped is `percent`.
            chance: percent / 100.0 / f64::from(burst),
            burst,
            left: 0,
            seed: 0x9e37_79b9_7f4a_7c15,
        })
    }

    fn drops(&mut self) -> bool {
        if self.left > 0 {
            self.left -= 1;
            return true;
        }
        self.seed ^= self.seed << 13;
        self.seed ^= self.seed >> 7;
        self.seed ^= self.seed << 17;
        let draw = (self.seed >> 11) as f64 / (1u64 << 53) as f64;
        if draw < self.chance {
            self.left = self.burst - 1;
            return true;
        }
        false
    }
}

/// Test knob: `LATCH_TEST_NO_RECOVERY=1` asks for keyframes after a loss,
/// as viewers before reference invalidation did.
fn recovery_wanted() -> bool {
    std::env::var_os("LATCH_TEST_NO_RECOVERY").is_none()
}

#[derive(Default)]
struct VideoHealth {
    connected_at: Option<Instant>,
    last_frame: Option<Instant>,
    last_encoded: Option<Instant>,
    black_since: Option<Instant>,
    last_error: Option<String>,
}

impl VideoHealth {
    fn frame(&mut self, now: Instant, black: bool) {
        self.last_frame = Some(now);
        self.last_error = None;
        if black {
            self.black_since.get_or_insert(now);
        } else {
            self.black_since = None;
        }
    }

    fn stalled(&self, now: Instant) -> bool {
        self.last_frame
            .is_some_and(|last| now.saturating_duration_since(last) >= MEDIA_STALL_TIMEOUT)
    }

    fn stall_message(&self) -> &'static str {
        if self.last_encoded > self.last_frame {
            "Video is arriving, but decoding stopped producing pictures. Reconnecting…"
        } else {
            "Video stopped arriving from the PC, including idle frames. Reconnecting…"
        }
    }

    fn problem(&self, now: Instant) -> Option<String> {
        let connected = self.connected_at?;
        if now.duration_since(self.last_frame.unwrap_or(connected)) >= Duration::from_secs(5) {
            return Some(if let Some(error) = &self.last_error {
                format!("Video cannot be decoded: {error}. Try restarting the stream or selecting H.264 in Settings.")
            } else if self.last_frame.is_some() {
                self.stall_message().into()
            } else {
                "Connected, but no video has arrived from the PC. Check its display, or restart the stream.".into()
            });
        }
        if self
            .black_since
            .is_some_and(|since| now.duration_since(since) >= Duration::from_secs(5))
        {
            return Some("The PC is sending a black picture: the connection and the video are healthy, but every frame is blank. Asking the PC why…".into());
        }
        None
    }
}

/// What the decode thread adds up between two stats updates.
#[derive(Default)]
struct DecodeWindow {
    frames: u32,
    decode_us: u64,
    samples_us: Vec<u64>,
    queue_us: u64,
    host_tenths: u64,
    assembly_us: u64,
}

/// What input needs to reach the host.
struct Link {
    udp: UdpSocket,
    host: SocketAddr,
    relay: Arc<RelayRoute>,
    sender: InputSender,
    input_progress: Option<Instant>,
    input_failed: bool,
    pads: PadSender,
    buf: Vec<u8>,
}

impl Link {
    fn flush(&mut self, now: Instant) {
        if self
            .input_progress
            .is_some_and(|since| now.saturating_duration_since(since) >= INPUT_ACK_TIMEOUT)
        {
            self.input_failed = true;
        }
        if self.input_failed {
            return;
        }
        if self.sender.poll(now, &mut self.buf)
            && routed_datagram(&self.relay, &self.udp, &self.buf, self.host).is_err()
        {
            self.input_failed = true;
        }
        if self.pads.poll(now, &mut self.buf)
            && routed_datagram(&self.relay, &self.udp, &self.buf, self.host).is_err()
        {
            self.input_failed = true;
        }
    }

    fn next_deadline(&self) -> Option<Instant> {
        self.sender
            .next_deadline()
            .into_iter()
            .chain(self.pads.next_deadline())
            .min()
    }
}

struct Inner {
    #[cfg(test)]
    pair_probe: Option<Arc<pair_interop::Probe>>,
    configured_relay: Mutex<Option<latch_relay::ClientConfig>>,
    relay: Mutex<Arc<RelayRoute>>,
    relay_messages: Mutex<VecDeque<(u64, Message)>>,
    /// This machine's microphone, while it is being sent.
    mic: Mutex<Option<crate::mic::Mic>>,
    events: Sender<Event>,
    wake: Box<dyn Fn() + Send + Sync>,
    frames: Arc<FrameSlot>,
    stats: Mutex<Stats>,
    recovery: Mutex<RecoveryLog>,
    /// Persists across the receive thread's periodic player description.
    host_audio_problem: Mutex<String>,
    video_health: Mutex<VideoHealth>,
    decode: Mutex<DecodeWindow>,
    link: Mutex<Option<Link>>,
    /// Frames waiting for the decoder, and whether it must skip to a
    /// keyframe.
    queue: Mutex<DecodeQueue>,
    queue_cv: Condvar,
    /// The TCP connection, so `stop` can unblock its reader.
    tcp: Mutex<Option<TcpStream>>,
    epoch: Instant,
    /// When anything last came from the host, µs since `epoch`.
    heard_us: AtomicU64,
    /// Video packets the network lost in the last second, rebuilt or not,
    /// in thousandths of a percent: what the host sizes its FEC by.
    feedback: Mutex<Report>,
    /// When the decoder last published a picture, µs since `epoch`.
    published_us: AtomicU64,
    /// Frames thrown away before decoding since the last stats window.
    dropped: AtomicU64,
    connected: AtomicBool,
    /// The viewer has been told the stream is up; a renegotiated attempt
    /// does not tell it again.
    announced: AtomicBool,
    /// The user's stop.
    stop: AtomicBool,
    /// This attempt at the stream is over: stopped, or given up for
    /// another codec.
    attempt_over: AtomicBool,
    /// Which attempt this is; a decode thread left behind in the OS ends
    /// when it comes back to a later one.
    attempt: AtomicU64,
    /// A codec this machine failed to decode, and why: the session asks
    /// the host again without it.
    renegotiate: Mutex<Option<(Codec, String, video::FailureKind)>>,
    finished: AtomicBool,
    audio_gain: Arc<AtomicU32>,
    done: Mutex<bool>,
    done_cv: Condvar,
    cursor_supported: AtomicBool,
    cursor_state: Mutex<Option<CursorState>>,
    control_writer: Mutex<Option<TcpStream>>,
    /// Gamepad slots the host takes; 0 until it accepts, or if it takes none.
    pad_slots: AtomicU32,
    pad_problem: Mutex<String>,
    pad_feedback: Mutex<Option<FeedbackFn>>,
    /// Pads' newest physical states, retained across connection/route changes.
    /// Locked after `link`, never before.
    pending_pads: Mutex<[Option<(PadKind, PadState)>; latch_proto::gamepad::MAX_PADS]>,
}

#[derive(Default)]
struct DecodeLoad {
    samples: VecDeque<Duration>,
    reported: bool,
}
impl DecodeLoad {
    fn sample(&mut self, service: Duration, budget: Duration) -> Option<u32> {
        self.samples.push_back(service);
        if self.samples.len() > 60 {
            self.samples.pop_front();
        }
        if self.reported || self.samples.len() < 60 {
            return None;
        }
        let mean = self.samples.iter().sum::<Duration>() / 60;
        if mean > budget.mul_f32(1.1) {
            self.reported = true;
            return Some((1.0 / mean.as_secs_f64()).floor().max(1.0) as u32);
        }
        None
    }
}

#[derive(Default)]
struct DecodeQueue {
    route_revision: u64,
    frames: VecDeque<(InFrame, Instant)>,
    want_keyframe: bool,
}

impl Inner {
    fn new(
        events: Sender<Event>,
        frames: Arc<FrameSlot>,
        wake: Box<dyn Fn() + Send + Sync>,
    ) -> Self {
        Self {
            configured_relay: Mutex::new(None),
            #[cfg(test)]
            pair_probe: None,
            relay: Mutex::new(Arc::new(RelayRoute::new(
                0,
                latch_relay::Role::Viewer,
                None,
            ))),
            relay_messages: Mutex::new(VecDeque::new()),
            events,
            wake,
            frames,
            stats: Mutex::new(Stats::default()),
            recovery: Mutex::default(),
            host_audio_problem: Mutex::new(String::new()),
            mic: Mutex::new(None),
            video_health: Mutex::new(VideoHealth::default()),
            decode: Mutex::new(DecodeWindow::default()),
            link: Mutex::new(None),
            queue: Mutex::new(DecodeQueue::default()),
            queue_cv: Condvar::new(),
            tcp: Mutex::new(None),
            epoch: Instant::now(),
            heard_us: AtomicU64::new(0),
            feedback: Mutex::new(Report::default()),
            published_us: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            connected: AtomicBool::new(false),
            announced: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            attempt_over: AtomicBool::new(false),
            attempt: AtomicU64::new(0),
            renegotiate: Mutex::new(None),
            finished: AtomicBool::new(false),
            audio_gain: Arc::new(AtomicU32::new(1.0f32.to_bits())),
            done: Mutex::new(false),
            done_cv: Condvar::new(),
            cursor_supported: AtomicBool::new(false),
            cursor_state: Mutex::new(None),
            control_writer: Mutex::new(None),
            pad_slots: AtomicU32::new(0),
            pad_problem: Mutex::new(String::new()),
            pad_feedback: Mutex::new(None),
            pending_pads: Mutex::new([None; latch_proto::gamepad::MAX_PADS]),
        }
    }

    fn emit(&self, e: Event) {
        let _ = self.events.send(e);
        (self.wake)();
    }

    fn now_us(&self) -> u64 {
        self.epoch.elapsed().as_micros() as u64
    }

    fn heard(&self) {
        self.heard_us.store(self.now_us(), Ordering::Relaxed);
    }

    fn stopping(&self) -> bool {
        self.stop.load(Ordering::Acquire) || self.attempt_over.load(Ordering::Acquire)
    }

    fn finish(&self) {
        self.finished.store(true, Ordering::Release);
        *self.done.lock() = true;
        self.done_cv.notify_all();
        (self.wake)();
    }
}

pub struct Session {
    inner: Arc<Inner>,
}

/// The keyboard-and-mouse side of a session, cheap to clone and safe to
/// use from any thread: a worker that has to type on the PC after a
/// network round trip holds one of these instead of the session.
#[derive(Clone)]
pub struct Input {
    inner: Arc<Inner>,
}

impl Input {
    pub fn connected(&self) -> bool {
        self.inner.connected.load(Ordering::Acquire)
    }

    fn send(&self, event: InputEvent) {
        if !self.connected() {
            return;
        }
        if let Some(link) = self.inner.link.lock().as_mut() {
            if link.input_failed || self.inner.stopping() {
                return;
            }
            // UTF-8 splitting can leave up to three bytes unused in each
            // MAX_TEXT chunk. Reserve that upper bound before allocating.
            let count = match &event {
                InputEvent::Text(text) => text.len().div_ceil(latch_proto::input::MAX_TEXT - 3),
                _ => 1,
            };
            if count > MAX_PENDING_INPUT.saturating_sub(link.sender.unacked()) {
                link.input_failed = true;
                return;
            }
            let now = Instant::now();
            link.input_progress.get_or_insert(now);
            link.sender.push(event);
            link.flush(now);
        }
    }

    pub fn mouse_move(&self, dx: i16, dy: i16) {
        if dx != 0 || dy != 0 {
            self.send(InputEvent::MouseMove { dx, dy });
        }
    }

    /// `x, y` in a `width × height` picture.
    pub fn mouse_position(&self, x: i16, y: i16, width: i16, height: i16) {
        let clamp = |v: i16| v.max(0) as u16;
        self.send(InputEvent::MousePosition {
            x: clamp(x),
            y: clamp(y),
            width: clamp(width).max(1),
            height: clamp(height).max(1),
        });
    }

    /// `button` is one of `crate::input::BUTTON_*`.
    pub fn mouse_button(&self, button: u8, down: bool) {
        self.send(InputEvent::MouseButton { button, down });
    }

    /// `vk` is a Windows virtual-key code, `modifiers` the
    /// `crate::input::MOD_*` bits held.
    pub fn key(&self, vk: i16, down: bool, modifiers: u8) {
        self.send(InputEvent::Key {
            vk: vk as u16,
            down,
            modifiers,
        });
    }

    pub fn text(&self, text: &str) {
        if !text.is_empty() {
            self.send(InputEvent::Text(text.to_string()));
        }
    }

    /// 120 per notch; up and right positive.
    pub fn scroll(&self, vertical: i16, horizontal: i16) {
        if vertical != 0 || horizontal != 0 {
            self.send(InputEvent::Scroll {
                vertical,
                horizontal,
            });
        }
    }

    /// Lets go of every key and button the host holds for this viewer.
    pub fn release_all(&self) {
        self.send(InputEvent::ReleaseAll);
    }

    /// Gamepad slots the host takes (0 while connecting, or when it takes
    /// none).
    pub fn pad_slots(&self) -> u8 {
        self.inner.pad_slots.load(Ordering::Acquire) as u8
    }

    /// The pad in `slot` looks like this now. Sent at once, then again until
    /// the host has it; before the host has accepted, kept until it has. A
    /// slot the host does not take is ignored.
    pub fn pad(&self, slot: u8, kind: PadKind, state: PadState) {
        self.set_pad(slot, Some((kind, state)));
    }

    /// The pad in `slot` went away.
    pub fn pad_gone(&self, slot: u8) {
        self.set_pad(slot, None);
    }

    fn set_pad(&self, slot: u8, pad: Option<(PadKind, PadState)>) {
        let mut link = self.inner.link.lock();
        if let Some(p) = self.inner.pending_pads.lock().get_mut(slot as usize) {
            *p = pad;
        }
        let Some(link) = link.as_mut() else {
            return;
        };
        if slot >= self.pad_slots() {
            return;
        }
        match pad {
            Some((kind, state)) => link.pads.update(slot, kind, state),
            None => link.pads.remove(slot),
        }
        link.flush(Instant::now());
    }
}

impl Session {
    pub fn start(
        server: Server,
        settings: Settings,
        frames: Arc<FrameSlot>,
        events: Sender<Event>,
        wake: impl Fn() + Send + Sync + 'static,
    ) -> Self {
        Self::start_with_relay(
            server,
            settings,
            frames,
            events,
            wake,
            relay_config(&control::RelaySettings::default()),
        )
    }

    pub fn start_with_relay(
        server: Server,
        settings: Settings,
        frames: Arc<FrameSlot>,
        events: Sender<Event>,
        wake: impl Fn() + Send + Sync + 'static,
        relay: Option<latch_relay::ClientConfig>,
    ) -> Self {
        let inner = Arc::new(Inner::new(events, frames, Box::new(wake)));
        *inner.configured_relay.lock() = relay;
        let worker = inner.clone();
        let spawned = std::thread::Builder::new()
            .name("stream".into())
            .spawn(move || {
                run(&worker, &server, &settings);
                worker.finish();
            });
        if let Err(e) = spawned {
            inner.emit(Event::Failed {
                stage: "starting".into(),
                message: format!("could not start the stream thread: {e}"),
            });
            inner.finish();
        }
        Self { inner }
    }

    pub fn connected(&self) -> bool {
        self.inner.connected.load(Ordering::Acquire)
    }

    /// The session has ended and let go of everything.
    pub fn finished(&self) -> bool {
        self.inner.finished.load(Ordering::Acquire)
    }

    /// Whether the host agreed to stream cursor shapes and omit the host pointer.
    pub fn cursor_supported(&self) -> bool {
        self.inner.cursor_supported.load(Ordering::Acquire)
    }

    /// The latest cursor shape and visibility received from the host, or None
    /// if cursor shapes are not supported or none have arrived yet.
    pub fn cursor(&self) -> Option<CursorState> {
        if !self.inner.cursor_supported.load(Ordering::Acquire) {
            return None;
        }
        self.inner.cursor_state.lock().clone()
    }

    /// Tell the host whether to draw the cursor into its video stream.
    pub fn draw_cursor(&self, draw: bool) {
        let _ = send_control(
            &self.inner,
            &Message::DrawCursor(latch_proto::control::DrawCursor { draw }),
        );
    }

    /// Ends the session; returns at once. [`Session::finished`] says when
    /// it is over.
    pub fn stop(&self) {
        self.inner.stop.store(true, Ordering::Release);
        if let Some(tcp) = self.inner.tcp.lock().as_ref() {
            let _ = tcp.shutdown(Shutdown::Both);
        }
        self.inner.queue_cv.notify_all();
    }

    pub fn relay_active(&self) -> bool {
        self.inner.relay.lock().active()
    }

    pub fn stats(&self) -> Stats {
        let mut stats = self.inner.stats.lock().clone();
        stats.video_problem = self.inner.video_health.lock().problem(Instant::now());
        stats
    }

    /// Every loss so far and how it ended.
    pub fn recovery_log(&self) -> RecoveryLog {
        self.inner.recovery.lock().clone()
    }

    /// When the newest picture was published to the [`FrameSlot`]: the
    /// renderer takes how long it waited from this.
    pub fn published_at(&self) -> Option<Instant> {
        match self.inner.published_us.load(Ordering::Relaxed) {
            0 => None,
            us => Some(self.inner.epoch + Duration::from_micros(us)),
        }
    }

    pub fn set_volume(&self, gain: f32) {
        self.inner
            .audio_gain
            .store(gain.clamp(0.0, 2.0).to_bits(), Ordering::Relaxed);
    }

    pub fn volume(&self) -> f32 {
        f32::from_bits(self.inner.audio_gain.load(Ordering::Relaxed))
    }

    pub fn input(&self) -> Input {
        Input {
            inner: self.inner.clone(),
        }
    }

    /// Why the host takes no gamepads, once it has said so.
    pub fn pad_problem(&self) -> String {
        self.inner.pad_problem.lock().clone()
    }

    /// Where the host's rumble and lights go, from the receive thread.
    pub fn on_pad_feedback(&self, f: impl Fn(u8, Feedback) + Send + Sync + 'static) {
        *self.inner.pad_feedback.lock() = Some(Box::new(f));
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The codecs to offer, best first.
fn offer(settings: &Settings) -> Vec<Codec> {
    video::decodable()
        .into_iter()
        .filter(|&c| {
            (settings.hevc || c != Codec::Hevc)
                && (settings.av1 || c != Codec::Av1)
                && video::decodable_at(c, settings.width, settings.height)
        })
        .collect()
}

/// The sentence in a refusal's `Ack` body, or the status.
fn refusal(status: u16, body: &str) -> String {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(str::to_string))
        .filter(|e| !e.is_empty())
        .unwrap_or_else(|| format!("the host answered HTTP {status}"))
}

/// The stream, asked for again without a codec whose decoder failed here,
/// down to H.264 Constrained Baseline, which `latch-codec` always decodes.
fn run(inner: &Arc<Inner>, server: &Server, settings: &Settings) {
    let mut excluded = Vec::new();
    loop {
        attempt(inner, server, settings, &excluded);
        let Some((codec, why, kind)) = inner.renegotiate.lock().take() else {
            return;
        };
        if inner.stop.load(Ordering::Acquire) {
            return;
        }
        excluded.push(codec);
        video::decoder_failed_for(SocketAddr::new(server.address, server.port), codec, kind);
        let codec = match codec {
            Codec::H264High => "H.264 High",
            other => other.name(),
        };
        tracing::warn!("{codec} decoding failed ({why}); asking the host again without it");
        inner.emit(Event::Notice(format!(
            "This computer could not decode {codec} ({why}), so the stream starts again without it."
        )));
        inner.attempt.fetch_add(1, Ordering::AcqRel);
        *inner.queue.lock() = DecodeQueue::default();
        *inner.video_health.lock() = VideoHealth::default();
        inner.attempt_over.store(false, Ordering::Release);
    }
}

fn session_decoder(inner: &Inner, accept: &control::Accept) -> anyhow::Result<Box<dyn Decoder>> {
    #[cfg(test)]
    if let Some(probe) = &inner.pair_probe {
        return video::pair_decoder(probe.native, accept.width, accept.height);
    }
    let _ = inner;
    video::new_decoder_for_color(accept.codec, accept.width, accept.height, accept.color)
}

/// One request for the stream, and the stream until it ends.
fn attempt(inner: &Arc<Inner>, server: &Server, settings: &Settings, excluded: &[Codec]) {
    if inner.stopping() {
        return;
    }
    let fail = |stage: &str, message: String| {
        if !inner.stopping() {
            inner.emit(Event::Failed {
                stage: stage.into(),
                message,
            });
        }
    };
    inner.emit(Event::Stage("Asking for the stream…".into()));
    let bind: SocketAddr = if server.address.is_ipv6() {
        "[::]:0".parse().expect("address")
    } else {
        "0.0.0.0:0".parse().expect("address")
    };
    let udp = match UdpSocket::bind(bind) {
        Ok(u) => u,
        Err(e) => return fail("connecting", format!("no UDP socket for the picture: {e}")),
    };
    match latch_proto::transport::configure_socket(&udp, 8 * 1024 * 1024, 8 * 1024 * 1024) {
        Ok(buffers) => {
            tracing::info!(send_bytes = buffers.send, receive_bytes = buffers.receive, "viewer UDP buffers (requested 8388608/8388608; Linux may cap at net.core.wmem_max/rmem_max)");
            let mut stats = inner.stats.lock();
            stats.socket_send_bytes = buffers.send;
            stats.socket_receive_bytes = buffers.receive;
        }
        Err(e) => return fail("connecting", format!("cannot configure media buffers: {e}")),
    }
    let udp_port = udp.local_addr().map(|a| a.port()).unwrap_or(0);
    let codecs: Vec<_> = offer(settings)
        .into_iter()
        .filter(|&c| {
            !excluded.contains(&c)
                && video::decoder_available_for(SocketAddr::new(server.address, server.port), c)
        })
        .collect();
    let audio_channels = match settings.audio.then(crate::audio::output_channels) {
        Some(n) if n > 2 => n,
        _ => 0,
    };
    #[cfg(test)]
    let audio_channels = inner
        .pair_probe
        .as_ref()
        .map_or(audio_channels, |probe| probe.channels);
    let color = video::decoder_color(settings.color, settings.width, settings.height);
    let request = StreamRequest {
        relay: inner.configured_relay.lock().is_some(),
        prefer_color: settings.prefer_color,
        color,
        protocol: latch_proto::PROTOCOL,
        width: settings.width,
        height: settings.height,
        fps: settings.fps,
        bitrate_kbps: settings.bitrate_kbps,
        codecs: codecs.clone(),
        udp_port,
        audio: settings.audio,
        name: settings.name.clone(),
        cursor: settings.cursor,
        gamepads: settings.gamepads,
        reference_invalidation: recovery_wanted(),
        mic: settings.mic,
        probe_trains: true,
        // 5.1 or 7.1 when this machine's speakers have them; stereo is the
        // field's absence, which every host understands.
        audio_channels,
    };
    let body = serde_json::to_vec(&request).unwrap_or_default();
    let host = SocketAddr::new(server.address, server.port);
    let mut tcp = match latch_core::http::upgrade_cancellable(
        host,
        control::PATH,
        control::UPGRADE,
        &body,
        START_TIMEOUT,
        |stream| {
            *inner.tcp.lock() = Some(stream.try_clone()?);
            Ok(())
        },
        || inner.stopping(),
    ) {
        Ok(Ok(tcp)) => tcp,
        Ok(Err(reply)) => {
            return fail(
                if matches!(reply.status, 408 | 429 | 500..=599) {
                    "connecting"
                } else {
                    "starting"
                },
                if reply.status == 404 {
                    "the host runs a Latch older than 5.0 and needs updating".into()
                } else {
                    refusal(reply.status, &reply.body)
                },
            )
        }
        Err(e) => {
            if inner.stopping() {
                return;
            }
            return fail("connecting", format!("{e:#}"));
        }
    };
    let _ = tcp.set_nodelay(true);
    let _ = tcp.set_read_timeout(Some(START_TIMEOUT));
    let _ = tcp.set_write_timeout(Some(CONTROL_WRITE_TIMEOUT));
    *inner.tcp.lock() = tcp.try_clone().ok();
    *inner.control_writer.lock() = tcp.try_clone().ok();
    if inner.stopping() {
        let _ = tcp.shutdown(Shutdown::Both);
        return;
    }
    let accept = match control::read_message(&mut tcp) {
        Ok(Message::Accept(a)) => a,
        Ok(Message::End(e)) => return fail("starting", e.message),
        Ok(other) => return fail("starting", format!("the host said {other:?} first")),
        Err(e) => {
            return fail(
                if e.kind() == std::io::ErrorKind::InvalidData {
                    "starting"
                } else {
                    "connecting"
                },
                format!("the host did not start the stream: {e}"),
            )
        }
    };
    *inner.relay.lock() = Arc::new(RelayRoute::new(
        accept.session,
        latch_relay::Role::Viewer,
        accept
            .relay
            .then(|| inner.configured_relay.lock().clone())
            .flatten(),
    ));
    inner.relay_messages.lock().clear();
    inner
        .cursor_supported
        .store(accept.cursor, Ordering::Release);
    let pad_slots = accept.gamepads.min(latch_proto::gamepad::MAX_PADS as u8);
    inner.pad_slots.store(
        pad_slots as u32 * settings.gamepads as u32,
        Ordering::Release,
    );
    if settings.gamepads && pad_slots == 0 {
        *inner.pad_problem.lock() = if accept.gamepad_problem.is_empty() {
            "the host takes no gamepads".into()
        } else {
            accept.gamepad_problem.clone()
        };
    }
    let Some(token) = control::token_from_hex(&accept.token) else {
        return fail("starting", "the host sent a malformed session token".into());
    };
    if !codecs.contains(&accept.codec) {
        return fail(
            "starting",
            format!("the host chose {:?}, which was not offered", accept.codec),
        );
    }
    if !accept.color.supported()
        || (accept.color.bit_depth == 10 && !color.ten_bit)
        || (accept.color.is_hdr() && (!color.hdr || color.peak_nits == 0))
    {
        return fail("starting", "host selected unoffered colour format".into());
    }
    let decoder = match session_decoder(inner, &accept) {
        Ok(d) => d,
        Err(e) if accept.codec != Codec::H264Baseline => {
            *inner.renegotiate.lock() =
                Some((accept.codec, format!("{e:#}"), video::FailureKind::Open));
            let _ = tcp.shutdown(Shutdown::Both);
            return;
        }
        Err(e) => return fail("decoding", format!("{e:#}")),
    };
    {
        let mut stats = inner.stats.lock();
        stats.width = accept.width;
        stats.height = accept.height;
        stats.decoder = decoder.name();
        stats.encoder = accept.encoder.clone();
        stats.codec = accept.codec.name();
        stats.color = accept.color;
        stats.frame_budget_ms = 1000.0 / accept.fps.max(1) as f32;
        if accept.audio.is_none() && settings.audio {
            stats.audio = if accept.audio_problem.is_empty() {
                "no sound from the host".into()
            } else {
                format!("no sound from the host: {}", accept.audio_problem)
            };
        }
    }
    let player = accept.audio.as_ref().and_then(|a| {
        #[cfg(test)]
        if inner.pair_probe.is_some() {
            return None;
        }
        match Player::with_gain(
            a.rate,
            a.channels as usize,
            a.frame_samples as usize,
            inner.audio_gain.clone(),
        ) {
            Ok(p) => Some(p),
            Err(e) => {
                inner.emit(Event::NoAudio(format!("{e:#}")));
                None
            }
        }
    });
    let media = SocketAddr::new(server.address, server.port);
    let send_udp = match udp.try_clone() {
        Ok(u) => u,
        Err(e) => return fail("connecting", format!("{e}")),
    };
    {
        let mut link = Link {
            udp: send_udp,
            host: media,
            relay: inner.relay.lock().clone(),
            sender: InputSender::new(accept.session),
            input_progress: None,
            input_failed: false,
            pads: PadSender::new(accept.session),
            buf: Vec::with_capacity(latch_proto::MAX_DATAGRAM),
        };
        let taken = inner.pad_slots.load(Ordering::Acquire);
        let mut guard = inner.link.lock();
        let pending = *inner.pending_pads.lock();
        for (slot, pad) in pending.into_iter().enumerate() {
            if let Some((kind, state)) = pad.filter(|_| (slot as u32) < taken) {
                link.pads.update(slot as u8, kind, state);
            }
        }
        link.flush(Instant::now());
        *guard = Some(link);
    }
    if settings.mic {
        let started = match (&accept.mic, udp.try_clone()) {
            (Some(format), Ok(udp)) => crate::mic::Mic::start_routed(
                format,
                accept.session,
                udp,
                media,
                inner.relay.lock().clone(),
            )
            .map_err(|e| format!("{e:#}")),
            (Some(_), Err(e)) => Err(e.to_string()),
            (None, _) if accept.mic_problem.is_empty() => {
                Err("the host does not take a microphone".into())
            }
            (None, _) => Err(format!("the host cannot play it: {}", accept.mic_problem)),
        };
        match started {
            Ok(mic) => *inner.mic.lock() = Some(mic),
            Err(why) => inner.stats.lock().mic = format!("microphone not sent: {why}"),
        }
    }
    inner.heard();
    let receiver = {
        let inner = inner.clone();
        let accept = accept.clone();
        Worker::spawn("stream-receive", move || {
            receive(&inner, udp, media, &accept, token, player)
        })
    };
    let decoding = {
        let inner = inner.clone();
        let (codec, color) = (accept.codec, accept.color);
        Worker::spawn("stream-decode", move || {
            decode(&inner, decoder, codec, color)
        })
    };
    let outcome = control_loop(inner, &mut tcp, &accept);
    inner.mic.lock().take();
    inner.relay.lock().shutdown();
    inner.attempt_over.store(true, Ordering::Release);
    inner.connected.store(false, Ordering::Release);
    inner.queue_cv.notify_all();
    let _ = tcp.shutdown(Shutdown::Both);
    // After the host's FIN macOS refuses that shutdown: only closing every
    // handle sends ours, which lets the host finish without waiting.
    *inner.control_writer.lock() = None;
    inner.tcp.lock().take();
    drop(tcp);
    if let Ok(r) = receiver {
        if !r.join_within(RECEIVER_STOP) {
            tracing::warn!(
                "the receiver did not return within {RECEIVER_STOP:?}; leaving it behind"
            );
        }
    }
    if let Ok(d) = decoding {
        if !d.join_within(DECODER_STOP) {
            tracing::warn!("the decoder did not return within {DECODER_STOP:?}; leaving it behind");
        }
    }
    *inner.link.lock() = None;
    if let Some(event) = outcome.event() {
        inner.emit(event);
    }
}

/// A thread that can be waited for with a time limit.
struct Worker {
    handle: std::thread::JoinHandle<()>,
    exited: mpsc::Receiver<()>,
}

impl Worker {
    fn spawn(name: &str, f: impl FnOnce() + Send + 'static) -> std::io::Result<Self> {
        let (alive, exited) = mpsc::channel();
        let handle = std::thread::Builder::new()
            .name(name.into())
            .spawn(move || {
                // Dropped when `f` returns or panics.
                let _alive: Sender<()> = alive;
                f()
            })?;
        Ok(Self { handle, exited })
    }

    /// Joins the thread if it ends within `limit`; otherwise leaves it to
    /// end by itself and returns false.
    fn join_within(self, limit: Duration) -> bool {
        if let Err(mpsc::RecvTimeoutError::Timeout) = self.exited.recv_timeout(limit) {
            return false;
        }
        let _ = self.handle.join();
        true
    }
}

enum Outcome {
    /// This side stopped.
    Stopped,
    /// Never got going.
    Failed(String, String),
    /// Ended after it started.
    Ended(i32, String),
}

impl Outcome {
    fn event(self) -> Option<Event> {
        match self {
            Self::Stopped => None,
            Self::Failed(stage, message) => Some(Event::Failed { stage, message }),
            Self::Ended(code, message) => Some(Event::Terminated { code, message }),
        }
    }
}

/// The control connection: keepalives and reports out, notices and the end
/// in; and the clock that gives up on a silent host.
fn send_control(inner: &Inner, message: &Message) -> bool {
    let relay = inner.relay.lock().clone();
    match relay.send_control(message) {
        Ok(true) => return true,
        Err(_) => {
            relay.fail();
            return false;
        }
        Ok(false) => {}
    }
    send_tcp(inner, message)
}
fn send_tcp(inner: &Inner, message: &Message) -> bool {
    inner.control_writer.lock().as_mut().is_some_and(|writer| {
        control::write_message(writer, message).is_ok() && writer.flush().is_ok()
    })
}

fn relay_bandwidth_limited(stats: &Stats) -> bool {
    // FEC can conceal every missing frame while a UDP policer still forces
    // poor picture quality. Compare useful video with the encoder's reduced
    // target: a quiet desktop must not allocate a relay just for being small.
    stats.requested_bitrate_kbps >= 4000
        && stats.wire_budget_kbps > 0
        && stats.wire_budget_kbps.saturating_mul(5) < stats.requested_bitrate_kbps.saturating_mul(3)
        && stats.applied_bitrate_kbps > 0
        && stats.received_fps > 0.0
        && stats.mbps.is_finite()
        && stats.mbps * 1000.0 >= stats.applied_bitrate_kbps as f32 * 0.75
}

fn control_loop(inner: &Arc<Inner>, tcp: &mut TcpStream, accept: &Accept) -> Outcome {
    let (messages, inbox) = mpsc::sync_channel(128);
    if let Ok(mut reader) = tcp.try_clone() {
        let _ = reader.set_read_timeout(None);
        let _ = std::thread::Builder::new()
            .name("stream-control".into())
            .spawn(move || loop {
                match control::read_message(&mut reader) {
                    Ok(m) => {
                        if messages.send(m).is_err() {
                            return;
                        }
                    }
                    Err(_) => return,
                }
            });
    }
    let accepted = Instant::now();
    let mut last_report = Instant::now();
    let mut last_keepalive = Instant::now();
    let mut damage_since: Option<Instant> = None;
    let mut bandwidth_since: Option<Instant> = None;
    loop {
        if inner.stopping() {
            if let Some(w) = inner.control_writer.lock().as_mut() {
                let _ = control::write_message(
                    w,
                    &Message::End(control::End {
                        code: control::end::ENDED,
                        message: String::new(),
                    }),
                );
            }
            *inner.control_writer.lock() = None;
            return Outcome::Stopped;
        }
        let incoming = match inbox.try_recv() {
            Ok(message) => Ok(message),
            Err(mpsc::TryRecvError::Disconnected) => Err(mpsc::RecvTimeoutError::Disconnected),
            Err(mpsc::TryRecvError::Empty) => {
                let message = inner.relay_messages.lock().pop_front();
                message
                    .and_then(|(revision, message)| {
                        (revision == inner.relay.lock().revision()).then_some(message)
                    })
                    .map(Ok)
                    .unwrap_or_else(|| inbox.recv_timeout(Duration::from_millis(5)))
            }
        };
        match incoming {
            Ok(Message::Relay(message)) if accept.relay => {
                if !inner
                    .relay
                    .lock()
                    .handle(message, |message| send_tcp(inner, message))
                {
                    return Outcome::Ended(-1, "The authorized control connection broke.".into());
                }
            }
            Ok(Message::End(e)) => {
                let message = if e.message.is_empty() {
                    "The host ended the stream.".to_string()
                } else {
                    e.message
                };
                return if inner.connected.load(Ordering::Acquire)
                    || e.code == control::end::TRANSPORT_FAILED
                {
                    Outcome::Ended(e.code as i32, message)
                } else {
                    Outcome::Failed("starting".into(), message)
                };
            }
            Ok(Message::Report(report)) => {
                inner.heard();
                let mut stats = inner.stats.lock();
                stats.applied_bitrate_kbps = report.applied_bitrate_kbps;
                stats.wire_budget_kbps = report.wire_budget_kbps;
                stats.application_limited = report.application_limited;
                stats.requested_bitrate_kbps = report.requested_bitrate_kbps;
                stats.pacing_queue_ms = report.pacing_queue_ms;
                stats.send_errors = report.send_errors;
                stats.send_retries = report.send_retries;
                stats.host_socket_send_bytes = report.socket_send_bytes;
                stats.host_socket_receive_bytes = report.socket_receive_bytes;
            }
            Ok(Message::Cursor(c)) => {
                inner.heard();
                let mut state = inner.cursor_state.lock();
                let prev_shape = state.as_ref().and_then(|s| s.shape.clone());
                let shape = c.shape.map(Arc::new).or(prev_shape);
                *state = Some(CursorState {
                    visible: c.visible,
                    shape,
                });
                (inner.wake)();
            }
            Ok(Message::Notice(n)) => {
                inner.heard();
                match n.kind.as_str() {
                    "audio" => {
                        *inner.host_audio_problem.lock() = n.message.clone();
                        if !n.message.is_empty() {
                            inner.stats.lock().audio =
                                format!("no sound from the host: {}", n.message);
                        } else {
                            inner.stats.lock().audio.clear();
                        }
                    }
                    _ if !n.message.is_empty() => inner.emit(Event::Notice(n.message)),
                    _ => {}
                }
            }
            Ok(_) => inner.heard(),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                if inner.stopping() {
                    return Outcome::Stopped;
                }
                return if inner.connected.load(Ordering::Acquire) {
                    Outcome::Ended(-1, "The connection to the host closed.".into())
                } else {
                    Outcome::Failed("connecting".into(), "the host closed the connection".into())
                };
            }
        }
        let now = Instant::now();
        let published = inner.published_us.load(Ordering::Relaxed);
        let missing = if published == 0 {
            accepted.elapsed() >= Duration::from_secs(1)
        } else {
            inner.now_us().saturating_sub(published) >= 1_000_000
        };
        let report = inner.feedback.lock().clone();
        if report.frames_lost > 0 || report.unrecovered_loss_pct.unwrap_or(0.0) > 1.0 {
            damage_since.get_or_insert(now);
        } else if report.fps > 0.0 {
            damage_since = None;
        }
        let damaged = damage_since.is_some_and(|since| since.elapsed() >= Duration::from_secs(2));
        if !inner.relay.lock().active() && relay_bandwidth_limited(&inner.stats.lock()) {
            bandwidth_since.get_or_insert(now);
        } else {
            bandwidth_since = None;
        }
        let limited =
            bandwidth_since.is_some_and(|since| since.elapsed() >= Duration::from_secs(5));
        if !inner
            .relay
            .lock()
            .maintain(missing || damaged || limited, |message| {
                send_tcp(inner, message)
            })
        {
            return Outcome::Ended(
                control::end::TRANSPORT_FAILED as i32,
                "The stream transport failed. Reconnecting…".into(),
            );
        }
        if now - last_keepalive >= KEEPALIVE {
            last_keepalive = now;
            if (!send_control(inner, &Message::Keepalive) || !send_tcp(inner, &Message::Keepalive))
                && !inner.stopping()
            {
                return Outcome::Ended(
                    control::end::TRANSPORT_FAILED as i32,
                    "The connection to the host broke.".into(),
                );
            }
        }
        if now - last_report >= REPORT_EVERY {
            last_report = now;
            let report = inner.feedback.lock().clone();
            if !send_control(inner, &Message::Report(report)) && !inner.stopping() {
                return Outcome::Ended(
                    control::end::TRANSPORT_FAILED as i32,
                    "The connection to the host broke.".into(),
                );
            }
        }
        {
            let mut link = inner.link.lock();
            if let Some(link) = link.as_mut() {
                if link.input_failed
                    || link.input_progress.is_some_and(|since| {
                        now.saturating_duration_since(since) >= INPUT_ACK_TIMEOUT
                    })
                {
                    link.input_failed = true;
                    return Outcome::Ended(-1, "The PC stopped acknowledging input. Reconnecting to release held keys and buttons.".into());
                }
            }
        }
        let health = inner.video_health.lock();
        if health.stalled(now) {
            return Outcome::Ended(-1, health.stall_message().into());
        }
        drop(health);
        if !inner.connected.load(Ordering::Acquire) && accepted.elapsed() >= MEDIA_TIMEOUT {
            if inner.video_health.lock().last_encoded.is_some() {
                return Outcome::Failed(
                    "decoding".into(),
                    "The host is sending complete video frames, but this machine has not decoded a picture. Try H.264 or restart the stream.".into(),
                );
            }
            return Outcome::Failed(
                "connecting".into(),
                format!(
                    "the host started the stream ({}), but no picture arrived: UDP port {} may be blocked between the two machines",
                    accept.encoder,
                    latch_core::CONTROL_PORT
                ),
            );
        }
        let silent = inner
            .now_us()
            .saturating_sub(inner.heard_us.load(Ordering::Relaxed));
        if silent >= SILENT.as_micros() as u64 {
            return Outcome::Ended(
                control::end::VIEWER_SILENT as i32,
                "The host stopped answering.".into(),
            );
        }
    }
}

fn receive_relay_control(inner: &Inner, revision: u64, bytes: &[u8]) {
    let relay = inner.relay.lock().clone();
    let mut source = bytes;
    match control::read_message(&mut source) {
        Ok(
            message @ (Message::Report(_)
            | Message::Keepalive
            | Message::Notice(_)
            | Message::Cursor(_)
            | Message::End(_)),
        ) if source.is_empty() => {
            let mut messages = inner.relay_messages.lock();
            if messages.len() >= 128 {
                relay.fail();
            } else {
                messages.push_back((revision, message));
            }
        }
        _ => relay.fail(),
    }
}

/// The receive thread: every datagram from the host, the hello and ping
/// timers, input resends and keyframe requests, and the statistics.
fn receive(
    inner: &Arc<Inner>,
    udp: UdpSocket,
    host: SocketAddr,
    accept: &Accept,
    token: [u8; 16],
    mut player: Option<Player>,
) {
    crate::priority::media_thread();
    let session = accept.session;
    // Readiness wakes immediately for datagrams on macOS, Linux and Windows;
    // socket receive timeouts round up to scheduler ticks on Windows.
    let Ok(mut poll) = mio::Poll::new() else {
        return;
    };
    if udp.set_nonblocking(true).is_err() {
        return;
    }
    let Ok(clone) = udp.try_clone() else {
        return;
    };
    let mut ready_socket = mio::net::UdpSocket::from_std(clone);
    if poll
        .registry()
        .register(&mut ready_socket, mio::Token(0), mio::Interest::READABLE)
        .is_err()
    {
        return;
    }
    let mut events = mio::Events::with_capacity(4);
    let accepted = Instant::now();
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    let mut buf = vec![0u8; latch_proto::MAX_DATAGRAM + 1];
    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    let mut batch = latch_proto::transport::ReceiveBatch::default();
    let mut out = Vec::with_capacity(64);
    let mut reassembler = Reassembler::new(session);
    let mut audio_rx = accept.audio.as_ref().map_or_else(
        || AudioReceiver::new(session),
        |format| AudioReceiver::with_format(session, format.frame_samples, format.rate),
    );
    let mut keyframes = KeyframeRequests::new();
    let mut recovery = accept.reference_invalidation.then(Recovery::new);
    let mut losses = Losses::default();
    let mut test_loss = TestLoss::from_env();
    let mut feedback = LatestReceiver::new();
    let mut rtt = RttEstimator::new();
    let mut media_seen = false;
    let mut last_hello: Option<Instant> = None;
    let mut last_ping = Instant::now();
    let mut window_start = Instant::now();
    let mut throughput = Throughput::default();
    let mut arrivals = Arrivals::default();
    let mut window_base = reassembler.stats();
    let mut report_start = Instant::now();
    let mut report_base = window_base;
    let mut poor = false;
    let mut good_windows = 0u32;
    // The socket ran dry on the last pass, after which everything that had
    // arrived was handled: wait for the next datagram or timer.
    let relay = inner.relay.lock().clone();
    let mut route_revision = relay.revision();
    let mut relay_frames = 0u64;
    let mut relay_report_frames = 0u64;
    let mut last_relay_frame = None;
    let mut drained = false;
    while !inner.stopping() {
        if route_revision != relay.revision() {
            route_revision = relay.revision();
            reassembler = Reassembler::new(session);
            rtt = RttEstimator::new();
            keyframes = KeyframeRequests::new();
            recovery = accept.reference_invalidation.then(Recovery::new);
            window_base = reassembler.stats();
            report_base = window_base;
            throughput = Throughput::default();
            relay_report_frames = relay_frames;
            last_relay_frame = None;
            if let Some(link) = inner.link.lock().as_mut() {
                for (slot, pad) in inner.pending_pads.lock().iter().enumerate() {
                    if let Some((kind, state)) = pad.filter(|_| slot < accept.gamepads as usize) {
                        // A fallback unplugs the host's devices. Advance the
                        // snapshot sequence so even a stationary pad plugs in
                        // again, without accepting an old route's ACK.
                        link.pads.remove(slot as u8);
                        link.pads.update(slot as u8, kind, state);
                    }
                }
                link.flush(Instant::now());
            }
            let mut queue = inner.queue.lock();
            queue.frames.clear();
            queue.route_revision = route_revision;
            queue.want_keyframe = true;
        }
        for _ in 0..64 {
            match relay.poll() {
                Some((revision, latch_relay::Event::Frame(frame))) => {
                    if revision != relay.revision() {
                        continue;
                    }
                    inner.heard();
                    media_seen = true;
                    throughput.received += frame.data.len() as u64;
                    throughput.feedback_received += frame.data.len() as u64;
                    relay_frames += 1;
                    last_relay_frame = Some(frame.frame);
                    deliver_frame(
                        inner,
                        frame,
                        &mut throughput,
                        &mut arrivals,
                        &mut losses,
                        Instant::now(),
                    );
                }
                Some((revision, latch_relay::Event::Datagram(bytes))) => {
                    if revision != relay.revision() {
                        continue;
                    }
                    if let Ok(datagram) = wire::parse(&bytes) {
                        if datagram.session() == session {
                            inner.heard();
                            handle(
                                inner,
                                &datagram,
                                &udp,
                                host,
                                &mut out,
                                &mut reassembler,
                                &mut audio_rx,
                                &mut feedback,
                                &mut rtt,
                                &mut media_seen,
                                &mut throughput,
                                bytes.len(),
                            );
                        }
                    }
                }
                Some((revision, latch_relay::Event::Control(bytes))) => {
                    if revision != relay.revision() {
                        continue;
                    }
                    receive_relay_control(inner, revision, &bytes);
                }
                Some(_) => {}
                None => break,
            }
        }
        if drained {
            let now = Instant::now();
            let deadline = reassembler
                .next_deadline()
                .into_iter()
                .chain(audio_rx.next_deadline())
                .chain(inner.link.lock().as_ref().and_then(Link::next_deadline))
                .min()
                .unwrap_or(now + Duration::from_millis(20));
            let wait = deadline
                .saturating_duration_since(now)
                .min(Duration::from_millis(if relay.active() { 2 } else { 20 }));
            let _ = poll.poll(&mut events, Some(wait));
            drained = false;
        }
        let now = Instant::now();
        let hello_every = if media_seen { HELLO_SLOW } else { HELLO_FAST };
        if last_hello.is_none_or(|t| now - t >= hello_every) {
            wire::write_hello(&mut out, session, Some(&token));
            let _ = udp.send_to(&out, host);
            last_hello = Some(now);
        }
        if media_seen && now - last_ping >= PING_EVERY {
            wire::write_ping(&mut out, session, inner.now_us());
            let _ = routed_datagram(&relay, &udp, &out, host);
            last_ping = now;
        }
        if let Some(link) = inner.link.lock().as_mut() {
            link.flush(now);
        }
        #[cfg(any(target_os = "linux", target_os = "macos", windows))]
        match receive_ready(&ready_socket, &mut batch, &udp, |bytes, from| {
            if from.ip() == host.ip() {
                if let Ok(datagram) = wire::parse(bytes) {
                    if datagram.session() == session {
                        if relay.active() {
                            relay.observe_udp_probe(&datagram, Instant::now());
                            if let Datagram::Ping { time, .. } = datagram {
                                wire::write_pong(&mut out, session, time);
                                let _ = udp.send_to(&out, host);
                            }
                            return;
                        }
                        if matches!(datagram, Datagram::Video { .. })
                            && test_loss.as_mut().is_some_and(TestLoss::drops)
                        {
                            return;
                        }
                        if let (Datagram::RecoveryAck { ack, .. }, Some(r)) =
                            (&datagram, recovery.as_mut())
                        {
                            r.ack(*ack, Instant::now());
                        }
                        inner.heard();
                        handle(
                            inner,
                            &datagram,
                            &udp,
                            from,
                            &mut out,
                            &mut reassembler,
                            &mut audio_rx,
                            &mut feedback,
                            &mut rtt,
                            &mut media_seen,
                            &mut throughput,
                            bytes.len(),
                        );
                        hand_off_video(
                            inner,
                            &mut reassembler,
                            &mut keyframes,
                            &mut throughput,
                            &mut arrivals,
                            &mut losses,
                            Instant::now(),
                        );
                    }
                }
            }
        }) {
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => drained = true,
            _ => {}
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
        // Drain a bounded batch before timers and feedback bookkeeping.
        for _ in 0..64 {
            match ready_socket.recv_from(&mut buf) {
                Ok((n, from)) if from.ip() == host.ip() => {
                    if let Ok(datagram) = wire::parse(&buf[..n]) {
                        if datagram.session() == session {
                            if relay.active() {
                                relay.observe_udp_probe(&datagram, Instant::now());
                                if let Datagram::Ping { time, .. } = datagram {
                                    wire::write_pong(&mut out, session, time);
                                    let _ = udp.send_to(&out, host);
                                }
                                continue;
                            }
                            if let (Datagram::RecoveryAck { ack, .. }, Some(r)) =
                                (&datagram, recovery.as_mut())
                            {
                                r.ack(*ack, Instant::now());
                            }
                            inner.heard();
                            handle(
                                inner,
                                &datagram,
                                &udp,
                                from,
                                &mut out,
                                &mut reassembler,
                                &mut audio_rx,
                                &mut feedback,
                                &mut rtt,
                                &mut media_seen,
                                &mut throughput,
                                n,
                            );
                        }
                    }
                }
                Ok(_) => {}
                // Hand on what completed before waiting for more: a frame's
                // last datagram must not wait for the next frame's first.
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    drained = true;
                    break;
                }
                Err(_) => {}
            }
        }
        let now = Instant::now();
        // Also when no audio arrived: a lost packet at the end of a sound
        // must not hold the rest of it until the next sound.
        while let Some(out) = audio_rx.pop(now) {
            #[cfg(test)]
            if let Some(probe) = &inner.pair_probe {
                probe.audio(&out);
            }
            if let Some(player) = player.as_mut() {
                match out {
                    AudioOut::Frame { payload, .. } => player.push(&payload),
                    AudioOut::Missing { .. } => player.push(&[]),
                }
            }
        }
        if route_revision != relay.revision() {
            continue;
        }
        hand_off_video(
            inner,
            &mut reassembler,
            &mut keyframes,
            &mut throughput,
            &mut arrivals,
            &mut losses,
            now,
        );
        let lost = reassembler.waiting_for_keyframe();
        if let Some(r) = recovery.as_mut().filter(|_| !relay.active()) {
            if let Some(request) = r.due(
                lost,
                reassembler.last_delivered(),
                reassembler.next_expected(),
                reassembler.newest_seen(),
                now,
                rtt.srtt(),
            ) {
                wire::write_recovery_request(&mut out, session, &request);
                let _ = routed_datagram(&relay, &udp, &out, host);
            }
        }
        // With recovery frames, a loss asks for a keyframe only once the
        // host could not recover it.
        let waiting = (!relay.active()
            && reassembler.needs_keyframe(accepted, now)
            && recovery
                .as_ref()
                .is_none_or(|r| r.wants_keyframe() || reassembler.last_delivered().is_none()))
            || inner.queue.lock().want_keyframe;
        if keyframes.due(waiting, now, rtt.srtt()) {
            wire::write_keyframe_request(
                &mut out,
                session,
                if relay.active() {
                    last_relay_frame.unwrap_or(0)
                } else {
                    reassembler.last_delivered().unwrap_or(0)
                },
            );
            let _ = routed_datagram(&relay, &udp, &out, host);
        }
        if now - report_start >= REPORT_EVERY {
            let stats = reassembler.stats();
            let (loss, _, raw_loss) = loss_in_window(&report_base, &stats);
            let secs = (now - report_start).as_secs_f32();
            *inner.feedback.lock() = Report {
                fps: if relay.active() {
                    (relay_frames - relay_report_frames) as f32 / secs
                } else {
                    (stats.frames_delivered - report_base.frames_delivered) as f32 / secs
                },
                loss_pct: raw_loss,
                decode_ms: inner.stats.lock().decode_ms,
                rtt_ms: rtt.srtt().map_or(0.0, |d| d.as_secs_f32() * 1000.0),
                received_kbps: (std::mem::take(&mut throughput.feedback_received) as f32 * 8.0
                    / secs
                    / 1000.0) as u32,
                frames_lost: stats.frames_lost - report_base.frames_lost,
                unrecovered_loss_pct: Some(loss),
                loss_burst_packets: reassembler.take_loss_burst(),
                probe_kbps: throughput.probes.take_kbps(now),
                window_ms: (secs * 1000.0) as u32,
                queue_ms: inner
                    .queue
                    .lock()
                    .frames
                    .front()
                    .map_or(0.0, |(_, at)| at.elapsed().as_secs_f32() * 1000.0),
                ..Default::default()
            };
            report_start = now;
            report_base = stats;
            relay_report_frames = relay_frames;
        }
        if now - window_start >= STATS_EVERY {
            let stats = reassembler.stats();
            let (loss, recovered, raw_loss) = loss_in_window(&window_base, &stats);
            let lost_count = stats.frames_lost.saturating_sub(window_base.frames_lost);
            let skipped = stats
                .frames_skipped
                .saturating_sub(window_base.frames_skipped);
            let lost_frames = lost_count > 0;
            window_base = stats;
            let secs = (now - window_start).as_secs_f32();
            window_start = now;
            let mut decode = std::mem::take(&mut *inner.decode.lock());
            decode.samples_us.sort_unstable();
            let frames = decode.frames.max(1) as f32;
            {
                let mut s = inner.stats.lock();
                s.window_ms = (secs * 1000.0) as u32;
                s.frames_lost = lost_count;
                s.fps = decode.frames as f32 / secs;
                (s.received_fps, s.frame_jitter_ms) = arrivals.take(secs);
                s.raw_loss_pct = raw_loss;
                s.frames_dropped = skipped + inner.dropped.swap(0, Ordering::Relaxed);
                (s.mbps, s.received_mbps) = throughput.take(secs);
                s.decode_ms = decode.decode_us as f32 / frames / 1000.0;
                if !decode.samples_us.is_empty() {
                    s.decode_p95_ms =
                        decode.samples_us[(decode.samples_us.len() * 95).div_ceil(100) - 1] as f32
                            / 1000.0;
                }
                s.queue_ms = decode.queue_us as f32 / frames / 1000.0;
                s.host_ms = decode.host_tenths as f32 / frames / 10.0;
                s.assembly_ms = decode.assembly_us as f32 / frames / 1000.0;
                s.loss_pct = loss;
                s.fec_recovered = recovered;
                s.rtt_ms = rtt.srtt().map_or(0, |d| d.as_millis() as u32);
                s.rtt_var_ms = rtt.rttvar().as_millis() as u32;
                if let Some(mic) = inner.mic.lock().as_ref() {
                    s.mic = match mic.state() {
                        crate::mic::MicState::Opening => "microphone: opening".into(),
                        crate::mic::MicState::Sending { device } => {
                            format!("microphone {device} → host")
                        }
                        crate::mic::MicState::Failed(why) => format!("microphone not sent: {why}"),
                    };
                }
                if let Some(p) = &player {
                    s.audio_latency = p.latency();
                    let problem = inner.host_audio_problem.lock();
                    s.audio = if problem.is_empty() {
                        p.describe()
                    } else {
                        format!("no sound from the host: {problem}")
                    };
                }
            }
            let bad = loss > 1.0 || lost_frames;
            if bad && !poor {
                poor = true;
                good_windows = 0;
                inner.emit(Event::Poor(true));
            } else if !bad && poor {
                good_windows += 1;
                if good_windows >= 3 {
                    poor = false;
                    inner.emit(Event::Poor(false));
                }
            }
        }
    }
}

fn hand_off_video(
    inner: &Arc<Inner>,
    reassembler: &mut Reassembler,
    keyframes: &mut KeyframeRequests,
    throughput: &mut Throughput,
    arrivals: &mut Arrivals,
    losses: &mut Losses,
    now: Instant,
) {
    while let Some(event) = reassembler.poll(now) {
        match event {
            Reassembled::Frame(frame) => {
                deliver_frame(inner, frame, throughput, arrivals, losses, now);
            }
            Reassembled::Lost { frame } => {
                losses.lost(now);
                tracing::debug!("video frame {frame} lost");
                // Perhaps the keyframe asked for: ask again.
                keyframes.lost();
            }
        }
    }
}

fn deliver_frame(
    inner: &Arc<Inner>,
    frame: InFrame,
    throughput: &mut Throughput,
    arrivals: &mut Arrivals,
    losses: &mut Losses,
    now: Instant,
) {
    losses.delivered(&inner.recovery, &frame, now);
    throughput.frame(&frame);
    arrivals.frame(now);
    let max_age = decode_max_age((1000.0 / inner.stats.lock().frame_budget_ms.max(1.0)) as u32);
    let mut q = inner.queue.lock();
    if q.frames.len() >= DECODE_QUEUE
        || q.frames
            .front()
            .is_some_and(|(_, at)| now.saturating_duration_since(*at) >= max_age)
    {
        tracing::warn!("the decoder fell behind; restarting at a keyframe");
        inner
            .dropped
            .fetch_add(q.frames.len() as u64, Ordering::Relaxed);
        q.frames.clear();
        q.want_keyframe = true;
    }
    inner.video_health.lock().last_encoded = Some(now);
    if q.route_revision != inner.relay.lock().revision() {
        return;
    }
    q.frames.push_back((frame, now));
    drop(q);
    inner.queue_cv.notify_one();
}

pub(crate) fn routed_datagram(
    relay: &RelayRoute,
    udp: &UdpSocket,
    bytes: &[u8],
    host: SocketAddr,
) -> std::io::Result<()> {
    match relay.send_datagram(bytes) {
        Ok(true) => Ok(()),
        Ok(false) => {
            let _ = udp.send_to(bytes, host);
            Ok(())
        }
        Err(error) => {
            relay.fail();
            Err(error)
        }
    }
}

/// When whole frames arrived in the current statistics window.
#[derive(Default)]
struct Arrivals {
    last: Option<Instant>,
    frames: u32,
    /// Gaps between consecutive frames, in µs.
    gaps: Vec<u64>,
}

impl Arrivals {
    fn frame(&mut self, now: Instant) {
        if let Some(last) = self.last.replace(now) {
            self.gaps
                .push(now.saturating_duration_since(last).as_micros() as u64);
        }
        self.frames += 1;
    }

    /// Frames a second over `secs`, and the mean absolute deviation of the
    /// gaps between them in ms; the window starts again.
    fn take(&mut self, secs: f32) -> (f32, f32) {
        let fps = self.frames as f32 / secs.max(f32::EPSILON);
        let jitter = if self.gaps.len() < 2 {
            0.0
        } else {
            let mean = self.gaps.iter().sum::<u64>() as f64 / self.gaps.len() as f64;
            let spread = self
                .gaps
                .iter()
                .map(|&g| (g as f64 - mean).abs())
                .sum::<f64>()
                / self.gaps.len() as f64;
            (spread / 1000.0) as f32
        };
        self.frames = 0;
        self.gaps.clear();
        (fps, jitter)
    }
}

/// Media bytes in the current statistics window.
#[derive(Default)]
struct Throughput {
    /// Received media payload: UDP video/audio packets or complete relay frames.
    /// IP, TCP and TLS framing are outside this application counter.
    received: u64,
    feedback_received: u64,
    /// Encoded video in the frames put back together.
    video: u64,
    /// Host probe trains: their rate, for the next report.
    probes: latch_proto::probe::TrainMeter,
}

impl Throughput {
    fn datagram(&mut self, datagram: &Datagram<'_>, len: usize) {
        if matches!(datagram, Datagram::Video { .. } | Datagram::Audio { .. }) {
            self.received += len as u64;
            self.feedback_received += len as u64;
        }
    }

    fn frame(&mut self, frame: &InFrame) {
        self.video += frame.data.len() as u64;
    }

    /// Video and received Mbps over `secs`; starts the next window.
    fn take(&mut self, secs: f32) -> (f32, f32) {
        let probes = std::mem::take(&mut self.probes);
        let window = std::mem::replace(
            self,
            Self {
                probes,
                ..Default::default()
            },
        );
        let mbps = |bytes: u64| bytes as f32 * 8.0 / secs / 1e6;
        (mbps(window.video), mbps(window.received))
    }
}

/// Unrecoverable loss in percent, shards FEC rebuilt, and the raw loss the
/// network caused (rebuilt or not), between two snapshots.
fn loss_in_window(before: &ReassemblyStats, after: &ReassemblyStats) -> (f32, u32, f32) {
    let received = after.shards_received.saturating_sub(before.shards_received);
    let recovered = after
        .shards_recovered
        .saturating_sub(before.shards_recovered);
    let missing = after.shards_missing.saturating_sub(before.shards_missing);
    let total = received + recovered + missing;
    if total == 0 {
        return (0.0, 0, 0.0);
    }
    (
        missing as f32 * 100.0 / total as f32,
        recovered.min(u32::MAX as u64) as u32,
        (recovered + missing) as f32 * 100.0 / total as f32,
    )
}

#[allow(clippy::too_many_arguments)]
fn handle(
    inner: &Arc<Inner>,
    datagram: &Datagram<'_>,
    udp: &UdpSocket,
    from: SocketAddr,
    out: &mut Vec<u8>,
    reassembler: &mut Reassembler,
    audio_rx: &mut AudioReceiver,
    feedback: &mut LatestReceiver,
    rtt: &mut RttEstimator,
    media_seen: &mut bool,
    throughput: &mut Throughput,
    len: usize,
) {
    let session = datagram.session();
    let now = Instant::now();
    throughput.datagram(datagram, len);
    match datagram {
        Datagram::Hello { .. } => {}
        Datagram::Video { shard, .. } => {
            *media_seen = true;
            reassembler.push(shard, now);
            #[cfg(test)]
            if let Some(probe) = &inner.pair_probe {
                probe
                    .fec
                    .store(reassembler.stats().shards_recovered, Ordering::Relaxed);
            }
        }
        Datagram::Audio { packet, .. } => audio_rx.push(packet, now),
        Datagram::InputAck { next, .. } => {
            if let Some(link) = inner.link.lock().as_mut() {
                let before = link.sender.unacked();
                link.sender.ack(*next);
                if link.sender.unacked() < before {
                    link.input_progress = (link.sender.unacked() > 0).then_some(now);
                }
            }
        }
        Datagram::PadAck { acks, .. } => {
            if let Some(link) = inner.link.lock().as_mut() {
                for ack in acks.iter() {
                    link.pads.ack(ack);
                }
            }
        }
        Datagram::PadFeedback { updates, .. } => {
            let mut acks = Vec::with_capacity(updates.count());
            let deliver = inner.pad_feedback.lock();
            for u in updates.iter() {
                if feedback.accept(u.slot, u.seq) {
                    if let Some(f) = deliver.as_ref() {
                        f(u.slot, u.feedback);
                    }
                }
                acks.extend(feedback.ack(u.slot));
            }
            drop(deliver);
            if !acks.is_empty() {
                wire::write_pad_feedback_ack(out, session, &acks);
                let _ = routed_datagram(&inner.relay.lock(), udp, out, from);
            }
        }
        Datagram::Ping { time, .. } => {
            wire::write_pong(out, session, *time);
            let _ = routed_datagram(&inner.relay.lock(), udp, out, from);
        }
        Datagram::Pong { time, .. } => {
            let sample = inner.now_us().saturating_sub(*time);
            rtt.sample(Duration::from_micros(sample));
        }
        Datagram::Probe {
            train,
            index,
            count,
            len,
            ..
        } => throughput.probes.push(*train, *index, *count, *len, now),
        _ => {}
    }
}

/// The decode thread: frames in order, newest picture out.
fn decode(
    inner: &Arc<Inner>,
    mut decoder: Box<dyn Decoder>,
    codec: Codec,
    color: latch_proto::color::ColorDescription,
) {
    crate::priority::media_thread();
    let attempt = inner.attempt.load(Ordering::Acquire);
    let over = || inner.stopping() || inner.attempt.load(Ordering::Acquire) != attempt;
    // A decoder knows whether it runs on hardware only once it has a
    // session, after the first keyframe; the stats follow it.
    let mut named = decoder.name();
    // Since the last picture: access units decoded, and errors.
    let mut tries = 0u32;
    let mut errors = 0u32;
    let budget_ms = inner.stats.lock().frame_budget_ms.max(1.0);
    let max_age = decode_max_age((1000.0 / budget_ms) as u32);
    let mut load = DecodeLoad::default();
    loop {
        let ((frame, ready), route_revision) = {
            let mut q = inner.queue.lock();
            loop {
                if over() {
                    return;
                }
                if q.frames
                    .front()
                    .is_some_and(|(_, at)| at.elapsed() >= max_age)
                {
                    inner
                        .dropped
                        .fetch_add(q.frames.len() as u64, Ordering::Relaxed);
                    q.frames.clear();
                    q.want_keyframe = true;
                }
                if let Some(next) = q.frames.pop_front() {
                    if q.want_keyframe && !next.0.keyframe {
                        inner.dropped.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }
                    if next.0.keyframe {
                        q.want_keyframe = false;
                    }
                    break (next, q.route_revision);
                }
                inner.queue_cv.wait_for(&mut q, Duration::from_millis(100));
            }
        };
        let start = Instant::now();
        let spare = inner.frames.spare();
        let decoded = decoder.decode(&frame.data, frame.keyframe, spare);
        if over() {
            // The session may have finished without waiting for this.
            return;
        }
        if inner.relay.lock().revision() != route_revision {
            continue;
        }
        let service = start.elapsed();
        if let Some(fps) = load.sample(service, Duration::from_secs_f32(budget_ms / 1000.0)) {
            inner.stats.lock().decode_capacity_fps = fps;
            inner.emit(Event::Notice(format!("This machine decodes about {fps} fps at this size. Lower the frame rate in Stream settings.")));
        }
        match &decoded {
            Ok(Some(_)) => (tries, errors) = (0, 0),
            Ok(None) => tries += 1,
            Err(_) => (tries, errors) = (tries + 1, errors + 1),
        }
        if codec != Codec::H264Baseline
            && (tries >= HARDWARE_PROBATION
                || errors >= HARDWARE_ERRORS
                || decoded
                    .as_ref()
                    .is_err_and(|e| e.is::<video::HardwareFailed>()))
        {
            let why = match &decoded {
                Err(e) if errors >= HARDWARE_ERRORS || e.is::<video::HardwareFailed>() => {
                    format!(
                        "{} failed {errors} times without a picture: {e:#}",
                        decoder.name()
                    )
                }
                _ => format!("{} gave no picture for {tries} frames", decoder.name()),
            };
            *inner.renegotiate.lock() = Some((codec, why, video::FailureKind::Decode));
            inner.attempt_over.store(true, Ordering::Release);
            inner.queue_cv.notify_all();
            return;
        }
        match decoded {
            Ok(Some(mut picture)) => {
                // Serialize publication with the TCP owner's route commit.
                let route = inner.relay.lock();
                if route.revision() != route_revision {
                    continue;
                }
                if picture.color.bit_depth != color.bit_depth {
                    // Pictures at another depth would be drawn wrongly;
                    // the stream starts again without this codec.
                    *inner.renegotiate.lock() = Some((
                        codec,
                        format!(
                            "{} gave {}-bit pictures for a {}-bit stream",
                            decoder.name(),
                            picture.color.bit_depth,
                            color.bit_depth
                        ),
                        video::FailureKind::Decode,
                    ));
                    inner.attempt_over.store(true, Ordering::Release);
                    inner.queue_cv.notify_all();
                    return;
                }
                picture.color = color;
                let decoded = Instant::now();
                picture.decoded_at = Some(decoded);
                if decoder.name() != named {
                    named = decoder.name();
                    inner.stats.lock().decoder = named;
                }
                if picture.width > 0 && picture.height > 0 {
                    let mut st = inner.stats.lock();
                    st.width = picture.width;
                    st.height = picture.height;
                }
                let black = picture.is_black();
                #[cfg(test)]
                if let Some(probe) = &inner.pair_probe {
                    probe.picture(frame.frame, &picture);
                }
                inner.frames.publish(picture);
                inner
                    .published_us
                    .store(inner.now_us().max(1), Ordering::Relaxed);
                (inner.wake)();
                {
                    let mut health = inner.video_health.lock();
                    health.connected_at.get_or_insert(decoded);
                    health.frame(decoded, black);
                }
                inner.connected.store(true, Ordering::Release);
                if !inner.announced.swap(true, Ordering::AcqRel) {
                    inner.emit(Event::Connected);
                }
                let mut w = inner.decode.lock();
                w.frames += 1;
                w.decode_us += (decoded - start).as_micros() as u64;
                w.samples_us.push((decoded - start).as_micros() as u64);
                w.queue_us += (start - ready).as_micros() as u64;
                w.host_tenths += u64::from(frame.host_delay);
                w.assembly_us += frame.assembly.as_micros() as u64;
            }
            Ok(None) => {}
            Err(e) => {
                tracing::debug!("decode: {e:#}");
                inner.video_health.lock().last_error = Some(format!("{e:#}"));
                inner.queue.lock().want_keyframe = true;
            }
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "macos", windows))]
fn receive_ready(
    ready: &mio::net::UdpSocket,
    batch: &mut latch_proto::transport::ReceiveBatch,
    udp: &UdpSocket,
    handle: impl FnMut(&[u8], SocketAddr),
) -> std::io::Result<usize> {
    // Mio must see WouldBlock even when I/O uses the standard-socket clone:
    // on Windows this rearms the interests disabled by the readable event.
    ready.try_io(|| batch.receive(udp, handle))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct ViewerRelay {
        inner: Arc<Inner>,
        events: mpsc::Receiver<Event>,
        tcp: TcpStream,
        _control_peer: TcpStream,
        peer: latch_relay::Transport,
        cancel: latch_relay::Cancel,
        daemon: Option<std::thread::JoinHandle<std::io::Result<()>>>,
        quota: std::path::PathBuf,
    }

    impl ViewerRelay {
        fn new() -> Self {
            use latch_relay::daemon::{Daemon, DaemonConfig};
            use rustls::pki_types::{pem::PemObject, CertificateDer, PrivateKeyDer};

            let quota = std::env::temp_dir().join(format!(
                "latch-viewer-failure-{}-{}",
                std::process::id(),
                rand::random::<u64>()
            ));
            let mut config = DaemonConfig::new(
                "127.0.0.1:0".parse().unwrap(),
                vec![CertificateDer::from_pem_slice(include_bytes!(
                    "../../relay/tests/certs/server.pem"
                ))
                .unwrap()],
                PrivateKeyDer::from_pem_slice(include_bytes!(
                    "../../relay/tests/certs/server-key.pem"
                ))
                .unwrap(),
                quota.clone(),
                32 * 1024 * 1024,
            )
            .unwrap();
            config.initialize_quota = true;
            let daemon = Daemon::bind(config).unwrap();
            let endpoint =
                latch_relay::Endpoint::new("localhost", daemon.local_addr().unwrap().port())
                    .unwrap();
            let mut roots = rustls::RootCertStore::empty();
            roots
                .add(
                    CertificateDer::from_pem_slice(include_bytes!(
                        "../../relay/tests/certs/ca.pem"
                    ))
                    .unwrap(),
                )
                .unwrap();
            let mut config = latch_relay::ClientConfig::with_roots(endpoint, roots).unwrap();
            config.limits.priority_queue_bytes = 1280;
            let cancel = latch_relay::Cancel::new();
            let run_cancel = cancel.clone();
            let worker = std::thread::spawn(move || daemon.run(&run_cancel));
            let grant = latch_relay::create_grant(&config, &cancel).unwrap();
            let keys = latch_relay::SessionKeys::generate().unwrap();
            let peer =
                latch_relay::Transport::connect(config.clone(), grant.host, keys.clone()).unwrap();
            let viewer =
                Arc::new(latch_relay::Transport::connect(config, grant.viewer, keys).unwrap());
            for transport in [&peer, viewer.as_ref()] {
                assert!(matches!(
                    transport.recv_timeout(Duration::from_secs(5)),
                    Some(latch_relay::Event::Ready)
                ));
            }
            let (tx, events) = mpsc::channel();
            let inner = Arc::new(Inner::new(
                tx,
                Arc::new(FrameSlot::default()),
                Box::new(|| {}),
            ));
            let route = Arc::new(RelayRoute::new(7, latch_relay::Role::Viewer, None));
            route.state.lock().transport = Some(viewer);
            route.activate(&mut route.state.lock());
            *inner.relay.lock() = route;
            // Start at the established-session boundary, after first publication.
            inner.connected.store(true, Ordering::Release);
            inner.announced.store(true, Ordering::Release);
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let tcp = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
            let (control_peer, _) = listener.accept().unwrap();
            *inner.control_writer.lock() = Some(tcp.try_clone().unwrap());
            *inner.tcp.lock() = Some(tcp.try_clone().unwrap());
            Self {
                inner,
                events,
                tcp,
                _control_peer: control_peer,
                peer,
                cancel,
                daemon: Some(worker),
                quota,
            }
        }
    }

    impl Drop for ViewerRelay {
        fn drop(&mut self) {
            let _ = self.tcp.shutdown(Shutdown::Both);
            self.inner.relay.lock().shutdown();
            self.peer.shutdown();
            self.cancel.cancel();
            let _ = self.daemon.take().unwrap().join();
            let _ = std::fs::remove_file(&self.quota);
        }
    }

    #[test]
    fn viewer_relay_backpressure_retries_unless_stopped_or_renegotiating() {
        let accept: Accept = serde_json::from_value(serde_json::json!({
            "session": 7, "token": "00000000000000000000000000000000",
            "codec": "h264-baseline", "width": 64, "height": 64, "fps": 30,
            "bitrate_kbps": 2000, "encoder": "test", "audio": null, "relay": true
        }))
        .unwrap();
        for inbox_overflow in [false, true] {
            for action in ["recover", "stop", "codec fallback"] {
                let mut viewer = ViewerRelay::new();
                let route = viewer.inner.relay.lock().clone();
                if inbox_overflow {
                    let mut bytes = Vec::new();
                    control::write_message(&mut bytes, &Message::Keepalive).unwrap();
                    for _ in 0..=128 {
                        receive_relay_control(&viewer.inner, route.revision(), &bytes);
                    }
                    assert_eq!(viewer.inner.relay_messages.lock().len(), 128);
                } else {
                    // A single record exceeds this fixture's queue, so the
                    // actual TLS writer cannot race away the capacity failure.
                    let message = Message::Notice(control::Notice {
                        kind: "video".into(),
                        message: "x".repeat(2000),
                    });
                    assert!(!send_control(&viewer.inner, &message));
                }
                match action {
                    "stop" => Session {
                        inner: viewer.inner.clone(),
                    }
                    .stop(),
                    "codec fallback" => {
                        *viewer.inner.renegotiate.lock() = Some((
                            Codec::Hevc,
                            "test decoder failure".into(),
                            video::FailureKind::Decode,
                        ));
                        viewer.inner.attempt_over.store(true, Ordering::Release);
                    }
                    _ => {}
                }
                let started = Instant::now();
                let outcome = control_loop(&viewer.inner, &mut viewer.tcp, &accept);
                if let Some(event) = outcome.event() {
                    viewer.inner.emit(event);
                }
                assert!(started.elapsed() < Duration::from_secs(1));
                if action == "recover" {
                    assert!(
                        matches!(
                            viewer.events.try_recv(),
                            Ok(Event::Terminated { code, .. })
                                if code == control::end::TRANSPORT_FAILED as i32
                        ),
                        "inbox overflow: {inbox_overflow}"
                    );
                } else {
                    assert!(viewer.events.try_recv().is_err(), "{action}");
                }
            }
        }
    }

    #[test]
    fn host_transport_failure_before_publication_remains_retryable() {
        let accept: Accept = serde_json::from_value(serde_json::json!({
            "session": 7, "token": "00000000000000000000000000000000",
            "codec": "h264-baseline", "width": 64, "height": 64, "fps": 30,
            "bitrate_kbps": 2000, "encoder": "test", "audio": null, "relay": true
        }))
        .unwrap();
        for code in [
            control::end::TRANSPORT_FAILED,
            control::end::ENDED,
            control::end::TAKEN_OVER,
            control::end::HOST_STOPPING,
            control::end::ENCODER_FAILED,
        ] {
            let (tx, _events) = mpsc::channel();
            let inner = Arc::new(Inner::new(
                tx,
                Arc::new(FrameSlot::default()),
                Box::new(|| {}),
            ));
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let mut tcp = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
            let (mut host, _) = listener.accept().unwrap();
            control::write_message(
                &mut host,
                &Message::End(control::End {
                    code,
                    message: "before the first frame".into(),
                }),
            )
            .unwrap();
            assert!(!inner.connected.load(Ordering::Acquire));
            let event = control_loop(&inner, &mut tcp, &accept).event();
            let _ = tcp.shutdown(Shutdown::Both);
            if code == control::end::TRANSPORT_FAILED {
                assert!(
                    matches!(event, Some(Event::Terminated { code: actual, .. }) if actual == code as i32)
                );
            } else {
                assert!(
                    matches!(event, Some(Event::Failed { ref stage, .. }) if stage == "starting")
                );
            }
        }
    }

    #[test]
    fn terminal_and_cursor_controls_always_keep_original_ordered_connection() {
        let route = RelayRoute::new(7, latch_relay::Role::Host, None);
        route.activate(&mut route.state.lock());
        for message in [
            Message::End(control::End {
                code: control::end::HOST_STOPPING,
                message: "stop".into(),
            }),
            Message::Cursor(control::Cursor {
                visible: true,
                shape: None,
            }),
        ] {
            assert!(!route.send_control(&message).unwrap());
        }
    }

    #[test]
    fn relay_close_before_direct_ready_is_a_healthy_handoff() {
        use control::{RelayAction as A, RelayControl};
        let host = RelayRoute::new(7, latch_relay::Role::Host, None);
        {
            let mut state = host.state.lock();
            state.number = 1;
            host.activate(&mut state);
            state.returning_direct = true;
            state.failures = 2;
        }
        host.failed.store(true, Ordering::Release);
        assert!(host.maintain(false, |_| true));
        assert!(!host.active());
        assert_eq!(host.state.lock().failures, 0);
        assert!(host.handle(
            RelayControl {
                session: 7,
                attempt: 1,
                action: A::DirectReady
            },
            |_| true
        ));
        assert_eq!(host.state.lock().failures, 0);
        assert!(
            host.state
                .lock()
                .retry_at
                .saturating_duration_since(Instant::now())
                <= Duration::from_secs(2)
        );
    }

    #[test]
    fn udp_reassessment_requires_full_capacity_and_counts_loss_and_duplicates() {
        use control::{RelayAction as A, RelayControl};
        for loss_pct in [0, 15, 18] {
            let host = RelayRoute::new(7, latch_relay::Role::Host, None);
            let viewer = RelayRoute::new(7, latch_relay::Role::Viewer, None);
            for route in [&host, &viewer] {
                let mut state = route.state.lock();
                state.number = 1;
                route.activate(&mut state);
            }
            let udp = UdpSocket::bind("127.0.0.1:0").unwrap();
            let sink = UdpSocket::bind("127.0.0.1:0").unwrap();
            let addr = sink.local_addr().unwrap();
            assert!(host.reassess_udp(&udp, addr, 2000, |_| panic!("minimum dwell")));
            host.state.lock().next_probe = Instant::now();
            assert!(host.reassess_udp(&udp, addr, 2000, |message| {
                let Message::Relay(message) = message else {
                    panic!("control")
                };
                viewer.handle(message.clone(), |_| true)
            }));
            let mut bytes = Vec::new();
            let mut state = host.state.lock();
            let probe = state.udp_probe.as_mut().unwrap();
            let first = probe.first;
            let packets = probe.packets();
            probe.sent = packets;
            drop(state);
            let start = viewer.state.lock().udp_probe.as_ref().unwrap().started;
            for packet in 0..packets {
                if packet * 37 % 100 < loss_pct {
                    continue;
                }
                wire::write_probe(
                    &mut bytes,
                    7,
                    first.wrapping_add((packet / 16) as u16),
                    (packet % 16) as u8,
                    16,
                    latch_proto::MAX_DATAGRAM,
                );
                let datagram = wire::parse(&bytes).unwrap();
                let now = start + Duration::from_secs_f64(f64::from(packet) / f64::from(packets));
                // Duplicate traffic cannot hide the 15–18% erased positions.
                viewer.observe_udp_probe(&datagram, now);
                viewer.observe_udp_probe(&datagram, now);
            }
            viewer.state.lock().udp_probe.as_mut().unwrap().started -= UDP_PROBE_REPORT;
            let mut messages = Vec::new();
            assert!(viewer.maintain(false, |message| {
                messages.push(message.clone());
                true
            }));
            let Message::Relay(result) = messages.pop().unwrap() else {
                panic!("result")
            };
            let mut direct = false;
            assert!(host.handle(result, |message| {
                direct = matches!(
                    message,
                    Message::Relay(RelayControl {
                        action: A::Direct,
                        ..
                    })
                );
                true
            }));
            assert_eq!(direct, loss_pct == 0);
            assert!(
                host.active(),
                "host waits for the direct handoff acknowledgement"
            );
            assert!(host.state.lock().probe_delay >= Duration::from_secs(120));
            assert!(host.reassess_udp(&udp, addr, 2000, |_| panic!("backoff")));
            if direct {
                host.handle(
                    RelayControl {
                        session: 7,
                        attempt: 0,
                        action: A::DirectReady,
                    },
                    |_| true,
                );
                assert!(host.active(), "stale epoch ended relay");
                host.failed.store(true, Ordering::Release);
                host.handle(
                    RelayControl {
                        session: 7,
                        attempt: 1,
                        action: A::DirectReady,
                    },
                    |_| true,
                );
                assert!(!host.active());
                assert_eq!(host.state.lock().failures, 0, "healthy handoff failed");
                assert!(host.maintain(false, |_| panic!("consumed close reset Idle twice")));
            }
        }
        let mut probe = UdpProbe::new(0, 25_000);
        probe.sent = probe.packets();
        assert!(probe.healthy(probe.packets(), 1_000_000));
        for span in [0, 20_000, 2_000_000, u32::MAX] {
            assert!(
                !probe.healthy(probe.packets(), span),
                "unvalidated capacity {span}"
            );
        }
        assert!(!probe.healthy(probe.packets() + 1, 1_000_000));
        probe.sent -= 1;
        assert!(
            !probe.healthy(probe.packets(), 1_000_000),
            "incomplete local send"
        );
    }

    #[test]
    fn host_cooldown_defers_a_fresh_request_without_stranding_the_viewer() {
        let config = relay_config(&control::RelaySettings::default()).unwrap();
        let host = RelayRoute::new(7, latch_relay::Role::Host, Some(config.clone()));
        let viewer = RelayRoute::new(7, latch_relay::Role::Viewer, Some(config));
        host.state.lock().retry_at = Instant::now() + Duration::from_secs(2);
        let mut responses = Vec::new();
        assert!(viewer.maintain(true, |request| {
            let Message::Relay(request) = request else {
                panic!("request")
            };
            host.handle(request.clone(), |response| {
                responses.push(response.clone());
                true
            })
        }));
        let Message::Relay(response) = responses.pop().expect("bounded deferral") else {
            panic!("response")
        };
        assert!(matches!(
            response.action,
            control::RelayAction::RetryAfter { millis: 250..=2000 }
        ));
        assert!(viewer.handle(response, |_| true));
        assert_eq!(
            viewer.state.lock().failures,
            0,
            "deferral counted as failure"
        );
        assert!(!viewer.pending());
        assert!(viewer.maintain(true, |_| panic!("hot retry")));
        assert!(!host.pending());
    }

    #[test]
    fn relay_retry_has_monotonic_attempts_cooldown_and_a_circuit_breaker() {
        let config = relay_config(&control::RelaySettings::default()).unwrap();
        let route = RelayRoute::new(7, latch_relay::Role::Viewer, Some(config));
        for attempt in 1..=3 {
            route.state.lock().retry_at = Instant::now();
            let mut requested = None;
            assert!(route.maintain(true, |message| {
                if let Message::Relay(control::RelayControl {
                    attempt,
                    action: control::RelayAction::Request,
                    ..
                }) = message
                {
                    requested = Some(*attempt);
                }
                true
            }));
            assert_eq!(requested, Some(attempt));
            if attempt > 1 {
                route.handle(
                    control::RelayControl {
                        session: 7,
                        attempt: attempt - 1,
                        action: control::RelayAction::Abort,
                    },
                    |_| true,
                );
                assert!(route.pending(), "stale Abort canceled the current attempt");
            }
            route.failed.store(true, Ordering::Release);
            assert!(route.maintain(false, |_| true));
            assert!(!route.pending());
            assert!(route.maintain(true, |_| panic!("cooldown was bypassed")));
        }
        assert!(
            route
                .state
                .lock()
                .retry_at
                .saturating_duration_since(Instant::now())
                > Duration::from_secs(29)
        );
        route.state.lock().retry_at = Instant::now();
        assert!(route.maintain(false, |_| panic!(
            "healthy UDP must cancel the need to retry"
        )));
        assert_eq!(route.number(), 3);
    }

    #[test]
    fn relay_can_help_busy_rate_limited_udp_even_when_fec_repairs_every_frame() {
        let mut stats = Stats {
            requested_bitrate_kbps: 20_000,
            wire_budget_kbps: 6000,
            applied_bitrate_kbps: 4000,
            mbps: 3.5,
            received_fps: 60.0,
            ..Stats::default()
        };
        assert!(relay_bandwidth_limited(&stats));
        stats.mbps = 0.1;
        assert!(!relay_bandwidth_limited(&stats), "quiet desktop");
        stats.mbps = 3.5;
        stats.wire_budget_kbps = 15_000;
        assert!(!relay_bandwidth_limited(&stats), "healthy path");
        stats.wire_budget_kbps = 0;
        assert!(!relay_bandwidth_limited(&stats), "no host telemetry yet");
        stats.requested_bitrate_kbps = 2000;
        stats.wire_budget_kbps = 1000;
        assert!(!relay_bandwidth_limited(&stats), "intentionally low rate");
    }

    #[test]
    fn input_backlog_is_bounded_before_enqueue_and_large_text_is_rejected() {
        let (tx, _) = mpsc::channel();
        let inner = Arc::new(Inner::new(tx, Arc::default(), Box::new(|| {})));
        let udp = UdpSocket::bind("127.0.0.1:0").unwrap();
        let host = udp.local_addr().unwrap();
        *inner.link.lock() = Some(Link {
            udp,
            host,
            relay: inner.relay.lock().clone(),
            sender: InputSender::new(1),
            input_progress: None,
            input_failed: false,
            pads: PadSender::new(1),
            buf: Vec::new(),
        });
        inner.connected.store(true, Ordering::Release);
        let input = Input {
            inner: inner.clone(),
        };
        for _ in 0..MAX_PENDING_INPUT {
            input.key(65, true, 0);
        }
        assert_eq!(
            inner.link.lock().as_ref().unwrap().sender.unacked(),
            MAX_PENDING_INPUT
        );
        input.key(65, false, 0);
        input.release_all();
        {
            let mut guard = inner.link.lock();
            let link = guard.as_mut().unwrap();
            assert!(
                link.input_failed,
                "a key-up beyond the bound ends the session"
            );
            assert_eq!(link.sender.unacked(), MAX_PENDING_INPUT);
            link.sender = InputSender::new(1);
            link.input_progress = None;
            link.input_failed = false;
        }
        input.text(&"🎮".repeat(MAX_PENDING_INPUT * latch_proto::input::MAX_TEXT / 4 + 1));
        let guard = inner.link.lock();
        let link = guard.as_ref().unwrap();
        assert!(link.input_failed);
        assert_eq!(
            link.sender.unacked(),
            0,
            "oversized paste is rejected before splitting"
        );
    }

    #[test]
    fn media_deadline_tracks_pictures_even_while_complete_frames_arrive() {
        let start = Instant::now();
        let mut health = VideoHealth::default();
        health.frame(start, false);
        let before = start + MEDIA_STALL_TIMEOUT - Duration::from_millis(1);
        health.last_encoded = Some(before);
        assert!(!health.stalled(before));
        assert!(health.stalled(start + MEDIA_STALL_TIMEOUT));
        assert!(health.stall_message().contains("decoding stopped"));
        health.frame(start + MEDIA_STALL_TIMEOUT, false);
        assert!(!health.stalled(start + MEDIA_STALL_TIMEOUT));
        assert!(health.stall_message().contains("including idle frames"));
    }

    #[test]
    fn queue_budget_tracks_frame_interval_with_bounds() {
        for (fps, ms) in [(30, 100), (60, 50), (120, 25), (240, 12), (500, 12)] {
            assert_eq!(decode_max_age(fps).as_millis(), ms);
        }
        assert_eq!(decode_max_age(0), Duration::from_millis(100));
    }

    #[test]
    fn slow_test_decoder_emits_capacity_notice_and_stats() {
        struct Slow;
        impl video::Decoder for Slow {
            fn name(&self) -> &'static str {
                "slow test decoder"
            }
            fn decode(
                &mut self,
                _: &[u8],
                _: bool,
                _: Option<video::Frame>,
            ) -> anyhow::Result<Option<video::Frame>> {
                std::thread::sleep(Duration::from_millis(20));
                Ok(Some(video::Frame::default()))
            }
        }
        let (tx, rx) = mpsc::channel();
        let inner = Arc::new(Inner::new(
            tx,
            Arc::new(FrameSlot::default()),
            Box::new(|| {}),
        ));
        inner.stats.lock().frame_budget_ms = 10.0;
        let worker = {
            let inner = inner.clone();
            std::thread::spawn(move || {
                decode(
                    &inner,
                    Box::new(Slow),
                    Codec::H264Baseline,
                    Default::default(),
                )
            })
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        for frame in 0..60 {
            inner.queue.lock().frames.push_back((
                InFrame {
                    frame,
                    data: vec![],
                    keyframe: true,
                    recovery: false,
                    host_delay: 0,
                    assembly: Duration::ZERO,
                },
                Instant::now(),
            ));
            inner.queue_cv.notify_one();
            while inner.frames.seq() <= u64::from(frame) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        inner.stop.store(true, Ordering::Release);
        inner.queue_cv.notify_all();
        worker.join().unwrap();
        let capacity = inner.stats.lock().decode_capacity_fps;
        assert!(capacity > 0 && capacity <= 50, "{capacity}");
        assert!(rx
            .try_iter()
            .any(|e| matches!(e, Event::Notice(s) if s.contains("This machine decodes about"))));
    }

    #[test]
    fn sustained_slow_decoder_is_detected_once_and_bursts_are_not() {
        let budget = Duration::from_millis(10);
        let mut load = DecodeLoad::default();
        for _ in 0..59 {
            assert_eq!(load.sample(Duration::from_millis(20), budget), None);
        }
        assert_eq!(load.sample(Duration::from_millis(20), budget), Some(50));
        assert_eq!(load.sample(Duration::from_millis(20), budget), None);
        let mut load = DecodeLoad::default();
        for i in 0..120 {
            let ms = if i == 30 { 30 } else { 5 };
            assert_eq!(load.sample(Duration::from_millis(ms), budget), None);
        }
    }

    #[cfg(windows)]
    #[test]
    fn batch_receive_rearms_after_a_drained_burst() {
        for offload in [false, true] {
            let udp = UdpSocket::bind("127.0.0.1:0").unwrap();
            udp.set_nonblocking(true).unwrap();
            let mut ready = mio::net::UdpSocket::from_std(udp.try_clone().unwrap());
            let mut poll = mio::Poll::new().unwrap();
            poll.registry()
                .register(&mut ready, mio::Token(0), mio::Interest::READABLE)
                .unwrap();
            let mut events = mio::Events::with_capacity(4);
            let mut batch = latch_proto::transport::ReceiveBatch::with_offload(offload);
            let tx = UdpSocket::bind("127.0.0.1:0").unwrap();
            let addr = udp.local_addr().unwrap();
            for burst in 0..4 {
                let sender = tx.try_clone().unwrap();
                let child = std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(10));
                    for _ in 0..8 {
                        sender.send_to(&[burst], addr).unwrap();
                    }
                });
                let start = Instant::now();
                poll.poll(&mut events, Some(Duration::from_secs(1)))
                    .unwrap();
                assert!(
                    events.iter().any(|event| event.is_readable()),
                    "missed burst {burst}, offload={offload}"
                );
                assert!(
                    start.elapsed() < Duration::from_millis(200),
                    "timer-driven receive"
                );
                child.join().unwrap();
                let mut count = 0;
                loop {
                    match receive_ready(&ready, &mut batch, &udp, |bytes, _| {
                        assert_eq!(bytes, &[burst]);
                        count += 1;
                    }) {
                        Ok(_) => {}
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                        Err(e) => panic!("{e}"),
                    }
                }
                assert_eq!(count, 8);
            }
        }
    }

    #[test]
    fn arrivals_count_frames_and_how_unevenly_they_come() {
        let mut a = Arrivals::default();
        let t = Instant::now();
        for ms in [0, 10, 20, 30, 40] {
            a.frame(t + Duration::from_millis(ms));
        }
        let (fps, jitter) = a.take(0.05);
        assert_eq!(fps, 100.0);
        assert!(jitter < 0.01, "even gaps: {jitter}");
        for ms in [50, 52, 70, 72, 90] {
            a.frame(t + Duration::from_millis(ms));
        }
        let (_, jitter) = a.take(0.05);
        assert!(
            (jitter - 6.4).abs() < 0.01,
            "gaps 10, 2, 18, 2, 18 ms: {jitter}"
        );
        assert_eq!(a.take(1.0), (0.0, 0.0), "a new window");
    }

    #[test]
    fn video_health_distinguishes_missing_black_frozen_and_recovered_video() {
        let start = Instant::now();
        let mut h = VideoHealth::default();
        assert_eq!(h.problem(start), None);
        h.connected_at = Some(start);
        let later = start + Duration::from_secs(6);
        assert!(h.problem(later).unwrap().contains("no video has arrived"));
        h.frame(later, true);
        assert_eq!(h.problem(later), None);
        let black = later + Duration::from_secs(6);
        h.frame(black, true);
        assert!(h.problem(black).unwrap().contains("black picture"));
        h.frame(black, false);
        assert_eq!(h.problem(black), None);
        let frozen = black + Duration::from_secs(6);
        assert!(h.problem(frozen).unwrap().contains("stopped arriving"));
        h.last_error = Some("bad slice".into());
        assert!(h
            .problem(frozen)
            .unwrap()
            .contains("cannot be decoded: bad slice"));
    }

    #[test]
    fn loss_is_the_share_of_packets_fec_could_not_save() {
        let before = ReassemblyStats::default();
        let after = ReassemblyStats {
            shards_received: 90,
            shards_recovered: 8,
            shards_missing: 2,
            ..Default::default()
        };
        let (loss, recovered, raw) = loss_in_window(&before, &after);
        assert!((loss - 2.0).abs() < 1e-3);
        assert_eq!(recovered, 8);
        assert!((raw - 10.0).abs() < 1e-3);
        assert_eq!(loss_in_window(&after, &after), (0.0, 0, 0.0));
    }

    #[test]
    fn a_decoder_stuck_in_the_os_does_not_hold_the_session_open() {
        let (release, stuck) = mpsc::channel::<()>();
        let decoding = Worker::spawn("stuck-decode", move || {
            let _ = stuck.recv();
        })
        .unwrap();
        assert!(!decoding.join_within(Duration::from_millis(50)));
        drop(release);
        let decoding = Worker::spawn("decode", || {}).unwrap();
        assert!(decoding.join_within(Duration::from_secs(10)));
        let decoding = Worker::spawn("panicking-decode", || panic!("decoder bug")).unwrap();
        assert!(decoding.join_within(Duration::from_secs(10)));
    }

    #[test]
    fn the_bitrate_is_encoded_video_and_received_counts_fec_and_audio() {
        use latch_proto::audio::AudioPacketizer;
        use latch_proto::video::{OutFrame, Packetizer};
        let mut datagrams = Vec::new();
        let mut video = Packetizer::new(1);
        video.set_fec_percent(50);
        let frame = OutFrame {
            data: &[7; 100_000],
            keyframe: true,
            host_delay: 0,
        };
        video
            .packetize(&frame, |dg| datagrams.push(dg.to_vec()))
            .unwrap();
        let mut audio = AudioPacketizer::new(1);
        for seq in 0..8 {
            audio.packetize(seq * 5000, &[1; 500], |dg| datagrams.push(dg.to_vec()));
        }
        let now = Instant::now();
        let mut reassembler = Reassembler::new(1);
        let mut throughput = Throughput::default();
        for dg in &datagrams {
            let datagram = wire::parse(dg).unwrap();
            throughput.datagram(&datagram, dg.len());
            if let Datagram::Video { shard, .. } = datagram {
                reassembler.push(&shard, now);
            }
        }
        let mut hello = Vec::new();
        wire::write_hello(&mut hello, 1, None);
        throughput.datagram(&wire::parse(&hello).unwrap(), hello.len());
        while let Some(Reassembled::Frame(f)) = reassembler.poll(now) {
            throughput.frame(&f);
        }
        let (mbps, received) = throughput.take(1.0);
        assert_eq!(mbps, 0.8, "100 000 bytes of video in one second");
        let all: usize = datagrams.iter().map(Vec::len).sum();
        assert_eq!(received, all as f32 * 8.0 / 1e6);
        assert!(
            received > 1.5 * mbps,
            "parity, headers and audio: {received}"
        );
        assert_eq!(throughput.take(1.0), (0.0, 0.0), "a new window");
    }

    #[test]
    fn refusals_read_the_hosts_sentence() {
        assert_eq!(
            refusal(406, r#"{"ok":false,"error":"this host encodes HEVC"}"#),
            "this host encodes HEVC"
        );
        assert_eq!(refusal(500, "not json"), "the host answered HTTP 500");
    }

    #[test]
    fn hevc_is_offered_only_when_the_settings_allow_it() {
        let mut s = Settings {
            prefer_color: false,
            color: Default::default(),
            cursor: false,
            gamepads: false,
            mic: false,
            width: 1920,
            height: 1080,
            fps: 60,
            bitrate_kbps: 20_000,
            hevc: false,
            av1: false,
            audio: true,
            name: "test".into(),
        };
        assert!(!offer(&s).contains(&Codec::Hevc));
        assert!(offer(&s).contains(&Codec::H264Baseline));
        s.hevc = true;
        assert_eq!(
            offer(&s).contains(&Codec::Hevc),
            video::decodable().contains(&Codec::Hevc)
        );
    }

    #[test]
    fn stop_interrupts_blocked_upgrade() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let session = Session::start(
            Server {
                address: "127.0.0.1".parse().unwrap(),
                port,
            },
            Settings {
                prefer_color: false,
                color: Default::default(),
                cursor: false,
                gamepads: false,
                mic: false,
                width: 1920,
                height: 1080,
                fps: 60,
                bitrate_kbps: 10_000,
                hevc: false,
                av1: false,
                audio: false,
                name: "test".into(),
            },
            Arc::new(FrameSlot::default()),
            std::sync::mpsc::channel().0,
            || {},
        );
        let (mut server_sock, _) = listener.accept().unwrap();
        server_sock
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        use std::io::Read;
        let mut head = Vec::new();
        while !head.ends_with(b"\r\n\r\n") {
            let mut byte = [0];
            server_sock.read_exact(&mut byte).unwrap();
            head.push(byte[0]);
        }
        let start = Instant::now();
        session.stop();
        while !session.finished() {
            if start.elapsed() > Duration::from_secs(3) {
                panic!("session did not finish within 3 seconds of stop");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        drop(server_sock);
    }

    #[test]
    fn decoded_picture_updates_session_stats_dimensions() {
        let (tx, _rx) = mpsc::channel();
        let (wake_tx, wake_rx) = mpsc::channel();
        let frames = Arc::new(FrameSlot::default());
        let inner = Arc::new(Inner::new(
            tx,
            frames,
            Box::new(move || {
                let _ = wake_tx.send(());
            }),
        ));
        {
            let mut stats = inner.stats.lock();
            stats.width = 64;
            stats.height = 64;
        }

        struct FakeDecoder;
        impl video::Decoder for FakeDecoder {
            fn decode(
                &mut self,
                _annexb: &[u8],
                _idr: bool,
                spare: Option<video::Frame>,
            ) -> anyhow::Result<Option<video::Frame>> {
                let mut f = spare.unwrap_or_default();
                f.width = 128;
                f.height = 64;
                Ok(Some(f))
            }
            fn name(&self) -> &'static str {
                "fake"
            }
        }

        let in_frame = InFrame {
            frame: 1,
            data: vec![0, 0, 0, 1],
            keyframe: true,
            recovery: false,
            host_delay: 0,
            assembly: Duration::ZERO,
        };
        inner
            .queue
            .lock()
            .frames
            .push_back((in_frame, Instant::now()));

        let inner_clone = inner.clone();
        let handle = std::thread::spawn(move || {
            decode(
                &inner_clone,
                Box::new(FakeDecoder),
                Codec::H264Baseline,
                Default::default(),
            );
        });

        let decoded = wake_rx.recv_timeout(Duration::from_secs(10));
        inner.stop.store(true, Ordering::Release);
        inner.queue_cv.notify_all();
        handle.join().unwrap();

        decoded.expect("decoder did not publish a frame");
        assert_eq!(inner.stats.lock().width, 128);
        assert_eq!(inner.stats.lock().height, 64);
    }

    /// Feeds `decoder` `frames` access units, every one a keyframe, as a
    /// stream of `codec`, and returns what the session would renegotiate.
    fn give_up_on(
        codec: Codec,
        decoder: Box<dyn video::Decoder>,
        frames: u32,
    ) -> Option<(Codec, String)> {
        let (tx, _rx) = mpsc::channel();
        let inner = Arc::new(Inner::new(
            tx,
            Arc::new(FrameSlot::default()),
            Box::new(|| {}),
        ));
        for frame in 0..frames {
            inner.queue.lock().frames.push_back((
                InFrame {
                    frame,
                    data: vec![0, 0, 0, 1],
                    keyframe: true,
                    recovery: false,
                    host_delay: 0,
                    assembly: Duration::ZERO,
                },
                Instant::now(),
            ));
        }
        let worker = {
            let inner = inner.clone();
            std::thread::spawn(move || decode(&inner, decoder, codec, Default::default()))
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        while !inner.queue.lock().frames.is_empty()
            && !inner.attempt_over.load(Ordering::Acquire)
            && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(5));
        }
        std::thread::sleep(Duration::from_millis(20));
        inner.stop.store(true, Ordering::Release);
        inner.queue_cv.notify_all();
        worker.join().unwrap();
        let given_up = inner.renegotiate.lock().take().map(|(c, why, _)| (c, why));
        given_up
    }

    struct Failing(bool);
    impl video::Decoder for Failing {
        fn decode(
            &mut self,
            _: &[u8],
            _: bool,
            _: Option<video::Frame>,
        ) -> anyhow::Result<Option<video::Frame>> {
            anyhow::ensure!(self.0, "the driver fell over");
            Ok(None)
        }
        fn name(&self) -> &'static str {
            "test hardware"
        }
    }

    #[test]
    fn a_fatal_device_failure_renegotiates_without_repeated_driver_calls() {
        struct Removed;
        impl video::Decoder for Removed {
            fn decode(
                &mut self,
                _: &[u8],
                _: bool,
                _: Option<video::Frame>,
            ) -> anyhow::Result<Option<video::Frame>> {
                Err(video::HardwareFailed("device removed".into()).into())
            }
            fn name(&self) -> &'static str {
                "removed hardware"
            }
        }
        let (codec, why) = give_up_on(Codec::Hevc, Box::new(Removed), 1).expect("renegotiation");
        assert_eq!(codec, Codec::Hevc);
        assert!(why.contains("device removed"));
    }

    #[test]
    fn a_failing_hardware_decoder_is_given_up_for_another_codec() {
        let (codec, why) = give_up_on(Codec::Hevc, Box::new(Failing(false)), 3).expect("gave up");
        assert_eq!(codec, Codec::Hevc);
        assert!(why.contains("the driver fell over"), "{why}");
        let (codec, why) = give_up_on(Codec::Av1, Box::new(Failing(true)), 10).expect("gave up");
        assert_eq!(codec, Codec::Av1);
        assert!(why.contains("no picture for 10"), "{why}");
        // Short of the limits it is still trusted.
        assert!(give_up_on(Codec::H264High, Box::new(Failing(false)), 2).is_none());
        assert!(give_up_on(Codec::H264High, Box::new(Failing(true)), 9).is_none());
        // Baseline has latch-codec behind it, not another codec.
        assert!(give_up_on(Codec::H264Baseline, Box::new(Failing(false)), 12).is_none());
    }
}
