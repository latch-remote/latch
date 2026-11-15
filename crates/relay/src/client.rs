use crate::{
    crypto::{ensure_supported_cpu, Cipher},
    invalid, timeout,
    wire::{self, TlsSocket, CHUNK, TICK},
    Attachment, Cancel, Grant, Lane, SessionKeys,
};
use latch_proto::video::{InFrame, MAX_FRAME_LEN};
use rustls::pki_types::ServerName;
use std::{
    collections::VecDeque,
    fmt, io,
    net::{SocketAddr, TcpStream, ToSocketAddrs},
    sync::{mpsc, Arc, Condvar, Mutex, OnceLock},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

/// Validated TLS hostname and public TCP port. Supply a public relay DNS hostname.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "EndpointWire", into = "EndpointWire")]
pub struct Endpoint {
    host: String,
    port: u16,
}
#[derive(serde::Serialize, serde::Deserialize)]
struct EndpointWire {
    host: String,
    port: u16,
}
impl From<Endpoint> for EndpointWire {
    fn from(e: Endpoint) -> Self {
        Self {
            host: e.host,
            port: e.port,
        }
    }
}
impl TryFrom<EndpointWire> for Endpoint {
    type Error = io::Error;
    fn try_from(e: EndpointWire) -> io::Result<Self> {
        Self::new(e.host, e.port)
    }
}
impl Endpoint {
    pub fn new(host: impl Into<String>, port: u16) -> io::Result<Self> {
        let host = host.into();
        if port == 0
            || host.is_empty()
            || host.len() > 253
            || !host.is_ascii()
            || host.contains(['/', ':', '@', '\\'])
            || host.bytes().any(|c| c.is_ascii_whitespace())
        {
            return Err(invalid(
                "relay requires a TLS DNS hostname and nonzero port",
            ));
        }
        match ServerName::try_from(host.clone()) {
            Ok(ServerName::DnsName(_)) => Ok(Self { host, port }),
            _ => Err(invalid("relay requires a TLS DNS hostname")),
        }
    }
    pub fn host(&self) -> &str {
        &self.host
    }
    pub fn port(&self) -> u16 {
        self.port
    }
}

#[derive(Clone, Debug)]
pub struct TransportLimits {
    pub priority_queue_bytes: usize,
    pub video_queue_bytes: usize,
    pub event_queue_bytes: usize,
    pub max_frame_bytes: usize,
    pub setup_timeout: Duration,
    /// Hard frame/write/queue deadline; also the floor for the adaptive video
    /// ACK deadline, which uses that lane's RTT and is capped at five seconds.
    pub stall_timeout: Duration,
    pub idle_timeout: Duration,
}
impl Default for TransportLimits {
    fn default() -> Self {
        Self {
            priority_queue_bytes: 256 * 1024,
            video_queue_bytes: 8 * 1024 * 1024,
            event_queue_bytes: 9 * 1024 * 1024,
            max_frame_bytes: (8 * 1024 * 1024).min(MAX_FRAME_LEN),
            setup_timeout: Duration::from_secs(10),
            stall_timeout: Duration::from_millis(750),
            idle_timeout: Duration::from_secs(30),
        }
    }
}
impl TransportLimits {
    fn validate(&self) -> io::Result<()> {
        if self.priority_queue_bytes < 1280
            || self.priority_queue_bytes > 16 * 1024 * 1024
            || self.max_frame_bytes == 0
            || self.max_frame_bytes > MAX_FRAME_LEN
            || self.video_queue_bytes == 0
            || self.video_queue_bytes > MAX_FRAME_LEN
            || self.event_queue_bytes < self.max_frame_bytes + 64
            || self.event_queue_bytes > MAX_FRAME_LEN + 16 * 1024 * 1024
            || self.setup_timeout.is_zero()
            || self.setup_timeout > Duration::from_secs(60)
            || self.stall_timeout.is_zero()
            || self.stall_timeout > Duration::from_secs(5)
            || self.idle_timeout < Duration::from_millis(30)
            || self.idle_timeout > Duration::from_secs(300)
        {
            return Err(invalid("invalid relay transport limits"));
        }
        Ok(())
    }
}

#[derive(Clone)]
pub struct ClientConfig {
    pub endpoint: Endpoint,
    pub limits: TransportLimits,
    tls: Arc<rustls::ClientConfig>,
    public_only: bool,
}
impl fmt::Debug for ClientConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClientConfig")
            .field("endpoint", &self.endpoint)
            .field("limits", &self.limits)
            .finish()
    }
}
impl ClientConfig {
    pub fn production(endpoint: Endpoint) -> io::Result<Self> {
        let mut config = Self::with_roots(
            endpoint,
            rustls::RootCertStore {
                roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
            },
        )?;
        config.public_only = true;
        Ok(config)
    }
    /// Replace trust anchors while preserving endpoint, address policy and transport settings.
    /// Private-PKI measurement callers can use the production configuration path.
    pub fn with_tls_roots(mut self, roots: rustls::RootCertStore) -> io::Result<Self> {
        self.tls = Self::with_roots(self.endpoint.clone(), roots)?.tls;
        Ok(self)
    }
    /// Explicit trust anchors for private PKI and tests. Hostname verification remains enabled.
    pub fn with_roots(endpoint: Endpoint, roots: rustls::RootCertStore) -> io::Result<Self> {
        ensure_supported_cpu()?;
        let mut tls = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls_graviola::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .map_err(|_| invalid("TLS versions unavailable"))?
        .with_root_certificates(roots)
        .with_no_client_auth();
        tls.alpn_protocols = vec![wire::ALPN.to_vec()];
        Ok(Self {
            endpoint,
            limits: TransportLimits::default(),
            tls: Arc::new(tls),
            public_only: false,
        })
    }
}

struct Resolve {
    host: String,
    port: u16,
    cancel: Cancel,
    deadline: Instant,
    reply: mpsc::SyncSender<io::Result<Vec<SocketAddr>>>,
}
fn enqueue_resolve(resolver: &mpsc::SyncSender<Resolve>, mut request: Resolve) -> io::Result<()> {
    loop {
        request.cancel.check()?;
        let remaining = request.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(timeout());
        }
        match resolver.try_send(request) {
            Ok(()) => return Ok(()),
            Err(mpsc::TrySendError::Full(pending)) => {
                request = pending;
                thread::sleep(TICK.min(remaining));
            }
            Err(mpsc::TrySendError::Disconnected(_)) => {
                return Err(io::Error::other("relay DNS worker stopped"));
            }
        }
    }
}
fn resolve(endpoint: &Endpoint, cancel: &Cancel, deadline: Instant) -> io::Result<Vec<SocketAddr>> {
    // ponytail: one process-wide resolver bounds stuck OS DNS to one worker.
    // Caller cancellation never joins this OS-owned blocking operation.
    static RESOLVER: OnceLock<io::Result<mpsc::SyncSender<Resolve>>> = OnceLock::new();
    let resolver = RESOLVER
        .get_or_init(|| {
            let (tx, rx) = mpsc::sync_channel::<Resolve>(16);
            thread::Builder::new()
                .name("relay-dns".into())
                .spawn(move || {
                    // Reuse successful hostname lookups briefly across lanes and ports.
                    // The bounded cache expires quickly so DNS changes remain visible.
                    let mut cache: VecDeque<(String, Instant, Vec<SocketAddr>)> = VecDeque::new();
                    for r in rx {
                        let now = Instant::now();
                        if r.cancel.is_cancelled() || now >= r.deadline {
                            continue;
                        }
                        cache.retain(|(_, expires, _)| *expires > now);
                        let addrs = match cache.iter().find(|(host, _, _)| host == &r.host) {
                            Some((_, _, addresses)) => Ok(addresses.clone()),
                            None => {
                                let result = (r.host.as_str(), 0)
                                    .to_socket_addrs()
                                    .map(|a| a.take(8).collect::<Vec<_>>());
                                if let Ok(addresses) = &result {
                                    if !addresses.is_empty() {
                                        if cache.len() == 16 {
                                            cache.pop_front();
                                        }
                                        cache.push_back((
                                            r.host,
                                            Instant::now() + Duration::from_secs(1),
                                            addresses.clone(),
                                        ));
                                    }
                                }
                                result
                            }
                        }
                        .map(|mut addresses| {
                            for address in &mut addresses {
                                address.set_port(r.port);
                            }
                            addresses
                        });
                        let _ = r.reply.try_send(addrs);
                    }
                })?;
            Ok(tx)
        })
        .as_ref()
        .map_err(|e| io::Error::new(e.kind(), "relay DNS worker creation failed"))?;
    cancel.check()?;
    let (reply, rx) = mpsc::sync_channel(1);
    enqueue_resolve(
        resolver,
        Resolve {
            host: endpoint.host.clone(),
            port: endpoint.port,
            cancel: cancel.clone(),
            deadline,
            reply,
        },
    )?;
    loop {
        cancel.check()?;
        if Instant::now() >= deadline {
            return Err(timeout());
        }
        match rx.recv_timeout(TICK) {
            Ok(r) => return r,
            Err(mpsc::RecvTimeoutError::Timeout) => (),
            Err(_) => return Err(io::Error::other("relay DNS worker stopped")),
        }
    }
}
fn public_address(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(ip) => {
            let [a, b, c, _] = ip.octets();
            !ip.is_private()
                && !ip.is_loopback()
                && !ip.is_link_local()
                && !ip.is_broadcast()
                && !ip.is_multicast()
                && !ip.is_documentation()
                && a != 0
                && a < 240
                && !(a == 100 && (64..128).contains(&b))
                && !(a == 198 && (b == 18 || b == 19))
                && !(a == 192 && b == 0 && c == 0)
        }
        std::net::IpAddr::V6(ip) => {
            let s = ip.segments();
            if s[..6] == [0x64, 0xff9b, 0, 0, 0, 0] {
                // DNS64's well-known NAT64 prefix still targets this IPv4
                // address. Apply the same public-address restrictions to it.
                return public_address(
                    std::net::Ipv4Addr::from((u32::from(s[6]) << 16) | u32::from(s[7])).into(),
                );
            }
            s[0] & 0xe000 == 0x2000 && !(s[0] == 0x2001 && s[1] == 0x0db8)
        }
    }
}
fn connect_tcp(config: &ClientConfig, cancel: &Cancel, deadline: Instant) -> io::Result<TcpStream> {
    let mut addresses = resolve(&config.endpoint, cancel, deadline)?;
    addresses.retain(|addr| !config.public_only || public_address(addr.ip()));
    if addresses.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "relay DNS returned no allowed addresses",
        ));
    }
    // Try the other address family next. A failed IPv6 path must not hold a
    // working IPv4 path behind the OS's multi-second SYN/refusal timer.
    if let Some(other) = addresses
        .iter()
        .position(|addr| addr.is_ipv4() != addresses[0].is_ipv4())
    {
        addresses.swap(1, other);
    }
    let mut remaining = addresses.into_iter();
    let mut next = remaining.next();
    let mut start_next = Instant::now();
    let mut pending: Vec<(TcpStream, Instant)> = Vec::new();
    let mut last_error = io::Error::new(io::ErrorKind::NotFound, "relay has no reachable address");
    loop {
        cancel.check()?;
        let now = Instant::now();
        if now >= deadline {
            return Err(timeout());
        }
        if next.is_some() && (pending.is_empty() || now >= start_next) {
            let addr = next.take().unwrap();
            let result = (|| {
                let socket = socket2::Socket::new(
                    socket2::Domain::for_address(addr),
                    socket2::Type::STREAM,
                    Some(socket2::Protocol::TCP),
                )?;
                socket.set_nonblocking(true)?;
                match socket.connect(&addr.into()) {
                    Ok(()) => (),
                    Err(e)
                        if e.kind() == io::ErrorKind::WouldBlock
                            || matches!(
                                e.raw_os_error(),
                                Some(36 | 115 | 10035 | 10036 | 10037)
                            ) => {}
                    Err(e) => return Err(e),
                }
                Ok::<TcpStream, io::Error>(socket.into())
            })();
            match result {
                Ok(tcp) => pending.push((tcp, now + Duration::from_secs(3))),
                Err(error) => last_error = error,
            }
            next = remaining.next();
            start_next = now + Duration::from_millis(250);
        }
        let mut index = 0;
        while index < pending.len() {
            match latch_proto::transport::tcp_connect_ready(&pending[index].0, Duration::ZERO) {
                Ok(true) => return Ok(pending.swap_remove(index).0),
                Ok(false) if now < pending[index].1 => {
                    index += 1;
                    continue;
                }
                Ok(false) => last_error = timeout(),
                Err(error) => last_error = error,
            }
            pending.swap_remove(index);
        }
        if pending.is_empty() && next.is_none() {
            return Err(last_error);
        }
        thread::sleep(TICK);
    }
}

fn open(config: &ClientConfig, cancel: &Cancel, deadline: Instant) -> io::Result<TlsSocket> {
    ensure_supported_cpu()?;
    let tcp = connect_tcp(config, cancel, deadline)?;
    let name = ServerName::try_from(config.endpoint.host.clone())
        .map_err(|_| invalid("invalid TLS hostname"))?;
    let conn = rustls::ClientConnection::new(config.tls.clone(), name)
        .map_err(|_| invalid("TLS connection setup failed"))?;
    let mut socket = TlsSocket::new(conn.into(), tcp)?;
    while socket.conn.is_handshaking() || !socket.idle_write() {
        cancel.check()?;
        if Instant::now() >= deadline {
            return Err(timeout());
        }
        socket.pump(None)?;
        thread::sleep(TICK);
    }
    Ok(socket)
}

fn exchange(
    socket: &mut TlsSocket,
    request: &[u8],
    cancel: &Cancel,
    deadline: Instant,
) -> io::Result<Vec<u8>> {
    socket.queue(request)?;
    loop {
        cancel.check()?;
        if Instant::now() >= deadline {
            return Err(timeout());
        }
        socket.pump(None)?;
        if let Some(reply) = socket.read_record()? {
            return Ok(reply);
        }
        thread::sleep(TICK);
    }
}
/// Cancellable create, including DNS, TCP connect and TLS handshake.
pub fn create_grant(config: &ClientConfig, cancel: &Cancel) -> io::Result<Grant> {
    config.limits.validate()?;
    let deadline = Instant::now() + config.limits.setup_timeout;
    let mut socket = open(config, cancel, deadline)?;
    Grant::decode(&exchange(&mut socket, &[1], cancel, deadline)?)
}

/// Incoming records contain application data. Debug output omits their contents.
pub enum Event {
    Ready,
    Datagram(Vec<u8>),
    Frame(InFrame),
    Control(Vec<u8>),
    Closed {
        kind: io::ErrorKind,
        message: String,
    },
}
impl fmt::Debug for Event {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ready => f.write_str("Ready"),
            Self::Datagram(b) => f.debug_tuple("Datagram bytes").field(&b.len()).finish(),
            Self::Control(b) => f.debug_tuple("Control bytes").field(&b.len()).finish(),
            Self::Frame(frame) => f
                .debug_struct("Frame")
                .field("frame", &frame.frame)
                .field("bytes", &frame.data.len())
                .finish(),
            Self::Closed { kind, message } => f
                .debug_struct("Closed")
                .field("kind", kind)
                .field("message", message)
                .finish(),
        }
    }
}
impl Event {
    fn cost(&self) -> usize {
        64 + match self {
            Self::Datagram(b) | Self::Control(b) => b.len(),
            Self::Frame(f) => f.data.len(),
            _ => 0,
        }
    }
}
#[derive(Default)]
struct State {
    priority: VecDeque<(Vec<u8>, Instant)>,
    priority_bytes: usize,
    frame: Option<(InFrame, Instant)>,
    events: VecDeque<Event>,
    event_bytes: usize,
    ready: u8,
    closed: Option<(io::ErrorKind, String)>,
    closed_delivered: bool,
    frame_ack_elapsed: Option<Duration>,
}
struct Shared {
    state: Mutex<State>,
    wake: Condvar,
    cancel: Cancel,
    limits: TransportLimits,
}
impl Shared {
    fn close(&self, e: io::Error) {
        self.cancel.cancel();
        let mut state = self.state.lock().unwrap();
        if state.closed.is_none() {
            state.closed = Some((e.kind(), e.to_string()));
        }
        state.priority.clear();
        state.priority_bytes = 0;
        state.frame = None;
        state.events.clear();
        state.event_bytes = 0;
        self.wake.notify_all();
    }
    fn event(&self, event: Event) -> io::Result<()> {
        let mut state = self.state.lock().unwrap();
        if state.closed.is_some() {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        let cost = event.cost();
        if cost > self.limits.event_queue_bytes - state.event_bytes {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "relay receive queue full",
            ));
        }
        state.event_bytes += cost;
        state.events.push_back(event);
        self.wake.notify_all();
        Ok(())
    }
    fn ready(&self, lane: Lane) -> io::Result<()> {
        let mut state = self.state.lock().unwrap();
        if state.closed.is_some() {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        state.ready |= 1 << lane as u8;
        let ready = state.ready == 3;
        drop(state);
        if ready {
            self.event(Event::Ready)?;
        }
        Ok(())
    }
}
/// Thread-safe, byte-bounded transport with one nonblocking TLS owner per lane.
pub struct Transport {
    shared: Arc<Shared>,
    workers: Mutex<Vec<JoinHandle<()>>>,
}
impl fmt::Debug for Transport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Transport { .. }")
    }
}
impl Transport {
    /// Returns immediately. Await Ready or Closed using recv_timeout.
    pub fn connect(
        config: ClientConfig,
        attachment: Attachment,
        keys: SessionKeys,
    ) -> io::Result<Self> {
        ensure_supported_cpu()?;
        config.limits.validate()?;
        if !(1..=86400).contains(&attachment.session_seconds)
            || attachment.session_bytes < 1024 * 1024
        {
            return Err(invalid("invalid attachment lifetime"));
        }
        let shared = Arc::new(Shared {
            state: Mutex::new(State::default()),
            wake: Condvar::new(),
            cancel: Cancel::new(),
            limits: config.limits.clone(),
        });
        let result = Self {
            shared,
            workers: Mutex::new(Vec::new()),
        };
        for lane in [Lane::Priority, Lane::Video] {
            let (config, attachment, keys, shared) = (
                config.clone(),
                attachment.clone(),
                keys.clone(),
                result.shared.clone(),
            );
            let worker = thread::Builder::new()
                .name(format!("relay-{lane:?}"))
                .spawn(move || {
                    if let Err(e) = run_lane(&config, &attachment, &keys, lane, &shared) {
                        shared.close(io::Error::new(e.kind(), format!("relay {lane:?}: {e}")));
                    }
                })?;
            result.workers.lock().unwrap().push(worker);
        }
        Ok(result)
    }
    pub fn try_send_datagram(&self, bytes: &[u8]) -> io::Result<()> {
        if bytes.is_empty() || bytes.len() > 1200 {
            return Err(invalid("priority datagram must contain 1..1200 bytes"));
        }
        self.send_priority(1, bytes)
    }
    pub fn try_send_control(&self, bytes: &[u8]) -> io::Result<()> {
        if bytes.len() >= CHUNK {
            return Err(invalid("control exceeds encrypted record bound"));
        }
        self.send_priority(2, bytes)
    }
    fn send_priority(&self, kind: u8, bytes: &[u8]) -> io::Result<()> {
        let mut state = self.shared.state.lock().unwrap();
        self.can_send(&state)?;
        let cost = bytes.len() + 65;
        if cost > self.shared.limits.priority_queue_bytes - state.priority_bytes {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        let mut packet = Vec::with_capacity(bytes.len() + 1);
        packet.push(kind);
        packet.extend_from_slice(bytes);
        state.priority.push_back((packet, Instant::now()));
        state.priority_bytes += cost;
        Ok(())
    }
    pub fn try_send_frame(&self, frame: &InFrame) -> io::Result<()> {
        if frame.data.len() > self.shared.limits.max_frame_bytes {
            return Err(invalid("frame exceeds configured limit"));
        }
        let mut state = self.shared.state.lock().unwrap();
        self.can_send(&state)?;
        if state.frame.is_some() || frame.data.len() > self.shared.limits.video_queue_bytes {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        state.frame = Some((frame.clone(), Instant::now()));
        Ok(())
    }
    /// Age of the whole unsent frame, or zero when that queue is empty.
    pub fn queued_frame_age(&self) -> Duration {
        self.shared
            .state
            .lock()
            .unwrap()
            .frame
            .as_ref()
            .map_or(Duration::ZERO, |(_, queued)| queued.elapsed())
    }

    /// Longest enqueue-to-authenticated-video-ACK duration since the last call.
    /// Includes queue residence, transmission and the peer's acknowledgment.
    /// Compare with this lane's own baseline; the priority socket can have a different RTT.
    pub fn take_frame_ack_elapsed(&self) -> Option<Duration> {
        self.shared.state.lock().unwrap().frame_ack_elapsed.take()
    }

    /// Discard a queued whole frame. An in-flight frame always finishes or closes.
    pub fn discard_queued_frame(&self) -> bool {
        self.shared.state.lock().unwrap().frame.take().is_some()
    }
    fn can_send(&self, state: &State) -> io::Result<()> {
        if state.closed.is_some() || self.shared.cancel.is_cancelled() {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        if state.ready != 3 {
            return Err(io::ErrorKind::NotConnected.into());
        }
        Ok(())
    }
    /// Returns None on timeout or after Closed has already been consumed.
    pub fn recv_timeout(&self, duration: Duration) -> Option<Event> {
        let started = Instant::now();
        let mut state = self.shared.state.lock().unwrap();
        loop {
            if let Some(event) = state.events.pop_front() {
                state.event_bytes -= event.cost();
                return Some(event);
            }
            if let Some((kind, message)) = state.closed.clone() {
                if state.closed_delivered {
                    return None;
                }
                state.closed_delivered = true;
                return Some(Event::Closed { kind, message });
            }
            let remaining = duration.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                return None;
            }
            state = self
                .shared
                .wake
                .wait_timeout(state, remaining.min(Duration::from_secs(60)))
                .unwrap()
                .0;
        }
    }
    /// Idempotent. Joins only bounded, cancellable network workers.
    pub fn shutdown(&self) {
        self.shared.close(io::Error::new(
            io::ErrorKind::Interrupted,
            "relay shut down",
        ));
        for worker in self.workers.lock().unwrap().drain(..) {
            let _ = worker.join();
        }
    }
}
impl Drop for Transport {
    fn drop(&mut self) {
        self.shutdown();
    }
}

struct OutgoingFrame {
    frame: InFrame,
    queued: Instant,
    offset: usize,
    complete: bool,
}
struct IncomingFrame {
    frame: InFrame,
    total: usize,
    started: Instant,
}
fn application_record(
    lane: Lane,
    plaintext: &[u8],
    incoming: &mut Option<IncomingFrame>,
    shared: &Shared,
) -> io::Result<Option<u32>> {
    match (lane, plaintext[0]) {
        (_, 5) if plaintext.len() == 1 => Ok(None),
        (Lane::Priority, 1) if (2..=1201).contains(&plaintext.len()) => shared
            .event(Event::Datagram(plaintext[1..].to_vec()))
            .map(|_| None),
        (Lane::Priority, 2) => shared
            .event(Event::Control(plaintext[1..].to_vec()))
            .map(|_| None),
        (Lane::Video, 3) if plaintext.len() >= 12 && incoming.is_none() => {
            let total = u32::from_be_bytes(plaintext[1..5].try_into().unwrap()) as usize;
            if total > shared.limits.max_frame_bytes
                || plaintext[9] > 3
                || plaintext.len() - 12 > total
            {
                return Err(invalid("invalid complete-frame header"));
            }
            let mut data = Vec::with_capacity(total);
            data.extend_from_slice(&plaintext[12..]);
            *incoming = Some(IncomingFrame {
                frame: InFrame {
                    frame: u32::from_be_bytes(plaintext[5..9].try_into().unwrap()),
                    data,
                    keyframe: plaintext[9] & 1 != 0,
                    recovery: plaintext[9] & 2 != 0,
                    host_delay: u16::from_be_bytes(plaintext[10..12].try_into().unwrap()),
                    assembly: Duration::ZERO,
                },
                total,
                started: Instant::now(),
            });
            finish_frame(incoming, shared)
        }
        (Lane::Video, 4) if plaintext.len() > 1 && incoming.is_some() => {
            let pending = incoming.as_mut().unwrap();
            if plaintext.len() - 1 > pending.total - pending.frame.data.len() {
                return Err(invalid("video chunk exceeds frame length"));
            }
            pending.frame.data.extend_from_slice(&plaintext[1..]);
            finish_frame(incoming, shared)
        }
        _ => Err(invalid("unexpected authenticated record type or lane")),
    }
}
fn finish_frame(incoming: &mut Option<IncomingFrame>, shared: &Shared) -> io::Result<Option<u32>> {
    if incoming
        .as_ref()
        .is_some_and(|p| p.frame.data.len() == p.total)
    {
        let mut pending = incoming.take().unwrap();
        pending.frame.assembly = pending.started.elapsed();
        let number = pending.frame.frame;
        shared.event(Event::Frame(pending.frame))?;
        return Ok(Some(number));
    }
    Ok(None)
}
fn run_lane(
    config: &ClientConfig,
    attachment: &Attachment,
    keys: &SessionKeys,
    lane: Lane,
    shared: &Shared,
) -> io::Result<()> {
    let setup_deadline = Instant::now() + config.limits.setup_timeout;
    let mut socket = open(config, &shared.cancel, setup_deadline)?;
    if exchange(
        &mut socket,
        &attachment.request(lane),
        &shared.cancel,
        setup_deadline,
    )? != [3]
    {
        return Err(crate::denied());
    }
    while !socket.idle_write() {
        shared.cancel.check()?;
        if Instant::now() >= setup_deadline {
            return Err(timeout());
        }
        socket.pump(None)?;
        thread::sleep(TICK);
    }
    let mut tx = Cipher::new(keys, &attachment.id, lane, attachment.role)?;
    let mut rx = Cipher::new(keys, &attachment.id, lane, attachment.role.peer())?;
    socket.queue(&tx.seal(&[0])?)?;
    let mut confirmed = false;
    let mut incoming = None;
    let mut outgoing: Option<OutgoingFrame> = None;
    // Bound ACK metadata without imposing an eight-frames-per-RTT ceiling on
    // long paths. Byte-bounded frame queues still limit stale media; ACK
    // metadata can outlive a delivered frame when the return path is delayed.
    const ACK_WINDOW: usize = 256;
    let mut unacked: VecDeque<(u32, Instant)> = VecDeque::new();
    let mut video_rtt = latch_proto::rtt::RttEstimator::new();
    let mut pending_acks: VecDeque<u32> = VecDeque::new();
    let mut last_sent = Instant::now();
    let mut last_peer = Instant::now();
    let lifetime = Instant::now() + Duration::from_secs(attachment.session_seconds as u64);
    loop {
        shared.cancel.check()?;
        let now = Instant::now();
        if now >= lifetime
            || (!confirmed && now >= setup_deadline)
            || last_peer.elapsed() >= config.limits.idle_timeout
        {
            return Err(timeout());
        }
        let before = socket.transferred;
        let mut application_work = false;
        socket.pump(None)?;
        socket.check_deadlines(config.limits.stall_timeout, config.limits.idle_timeout)?;
        if let Some(record) = socket.read_record()? {
            application_work = true;
            let plaintext = rx.open(&record)?;
            last_peer = Instant::now();
            if !confirmed {
                if plaintext != [0] {
                    return Err(invalid("relay key confirmation required"));
                }
                confirmed = true;
                shared.ready(lane)?;
            } else if lane == Lane::Video && plaintext[0] == 6 {
                if plaintext.len() != 5 {
                    return Err(invalid("invalid video acknowledgment"));
                }
                let number = u32::from_be_bytes(plaintext[1..].try_into().unwrap());
                let Some((frame, queued)) = unacked.pop_front() else {
                    return Err(invalid("unexpected video acknowledgment"));
                };
                if frame != number {
                    return Err(invalid("unexpected video acknowledgment"));
                }
                let elapsed = queued.elapsed();
                video_rtt.sample(elapsed);
                let mut state = shared.state.lock().unwrap();
                state.frame_ack_elapsed = Some(
                    state
                        .frame_ack_elapsed
                        .map_or(elapsed, |old| old.max(elapsed)),
                );
            } else if let Some(number) =
                application_record(lane, &plaintext, &mut incoming, shared)?
            {
                if pending_acks.len() == ACK_WINDOW {
                    return Err(invalid("video acknowledgment window exceeded"));
                }
                pending_acks.push_back(number);
            }
        }
        if unacked.front().is_some_and(|(_, queued)| {
            queued.elapsed() >= video_ack_deadline(&video_rtt, config.limits.stall_timeout)
        }) {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "video acknowledgment deadline expired",
            ));
        }
        if incoming
            .as_ref()
            .is_some_and(|p: &IncomingFrame| p.started.elapsed() >= config.limits.stall_timeout)
        {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "incoming video frame deadline expired",
            ));
        }
        if outgoing
            .as_ref()
            .is_some_and(|p| p.queued.elapsed() >= config.limits.stall_timeout)
        {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "outgoing video frame deadline expired",
            ));
        }
        if confirmed && socket.idle_write() {
            let mut state = shared.state.lock().unwrap();
            if lane == Lane::Video
                && state
                    .frame
                    .as_ref()
                    .is_some_and(|(_, queued)| queued.elapsed() >= config.limits.stall_timeout)
            {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "queued video frame deadline expired",
                ));
            }
            let plaintext = match lane {
                Lane::Priority => {
                    if let Some((packet, queued)) = state.priority.pop_front() {
                        state.priority_bytes -= packet.len() + 64;
                        if queued.elapsed() >= config.limits.stall_timeout {
                            return Err(io::Error::new(
                                io::ErrorKind::TimedOut,
                                "priority send queue deadline expired",
                            ));
                        }
                        Some(packet)
                    } else {
                        None
                    }
                }
                Lane::Video => {
                    if outgoing.as_ref().is_some_and(|p| p.complete) {
                        outgoing = None;
                    }
                    if outgoing.is_none() && unacked.len() < ACK_WINDOW {
                        outgoing = state.frame.take().map(|(frame, queued)| OutgoingFrame {
                            frame,
                            queued,
                            offset: 0,
                            complete: false,
                        });
                    }
                    if let Some(number) = pending_acks.pop_front() {
                        let mut packet = vec![6];
                        packet.extend(number.to_be_bytes());
                        Some(packet)
                    } else if let Some(pending) = outgoing.as_mut() {
                        let frame = &pending.frame;
                        if pending.queued.elapsed() >= config.limits.stall_timeout {
                            return Err(io::Error::new(
                                io::ErrorKind::TimedOut,
                                "outgoing video frame deadline expired",
                            ));
                        }
                        let mut packet = if pending.offset == 0 {
                            let mut b = vec![3];
                            b.extend((frame.data.len() as u32).to_be_bytes());
                            b.extend(frame.frame.to_be_bytes());
                            b.push(u8::from(frame.keyframe) | u8::from(frame.recovery) << 1);
                            b.extend(frame.host_delay.to_be_bytes());
                            b
                        } else {
                            vec![4]
                        };
                        let end = frame.data.len().min(pending.offset + CHUNK - packet.len());
                        packet.extend_from_slice(&frame.data[pending.offset..end]);
                        pending.offset = end;
                        pending.complete = end == frame.data.len();
                        if pending.complete {
                            unacked.push_back((frame.frame, pending.queued));
                        }
                        Some(packet)
                    } else {
                        None
                    }
                }
            };
            drop(state);
            if let Some(plaintext) = plaintext {
                application_work = true;
                socket.queue(&tx.seal(&plaintext)?)?;
                last_sent = Instant::now();
            } else if last_sent.elapsed() >= config.limits.idle_timeout / 3 {
                socket.queue(&tx.seal(&[5])?)?;
                last_sent = Instant::now();
            }
        }
        if !application_work && before == socket.transferred {
            thread::sleep(TICK);
        }
    }
}

fn video_ack_deadline(rtt: &latch_proto::rtt::RttEstimator, floor: Duration) -> Duration {
    // An authenticated ACK proves that the frame arrived. Use that lane's
    // measured round trip and variation for later ACKs, rather than closing a
    // healthy forward path because its return traffic briefly took longer.
    // Frame queues, partial records and writes retain the configured deadline.
    rtt.srtt()
        .map_or(floor, |srtt| {
            srtt.saturating_add(rtt.rttvar().saturating_mul(4))
                .max(floor)
        })
        .min(Duration::from_secs(5))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn resolution_request(cancel: Cancel, deadline: Instant) -> Resolve {
        let (reply, _) = mpsc::sync_channel(1);
        Resolve {
            host: "localhost".into(),
            port: 443,
            cancel,
            deadline,
            reply,
        }
    }

    #[test]
    fn resolver_queue_waits_for_capacity_and_observes_deadline_and_cancel() {
        let (tx, rx) = mpsc::sync_channel(1);
        let deadline = Instant::now() + Duration::from_secs(2);
        assert!(tx
            .try_send(resolution_request(Cancel::new(), deadline))
            .is_ok());
        let (done, result) = mpsc::sync_channel(1);
        let producer = tx.clone();
        let worker = thread::spawn(move || {
            done.send(enqueue_resolve(
                &producer,
                resolution_request(Cancel::new(), deadline),
            ))
            .unwrap();
        });
        assert!(matches!(
            result.recv_timeout(Duration::from_millis(20)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));
        rx.recv().unwrap();
        result
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .unwrap();
        worker.join().unwrap();
        assert_eq!(
            enqueue_resolve(
                &tx,
                resolution_request(Cancel::new(), Instant::now() + Duration::from_millis(10)),
            )
            .unwrap_err()
            .kind(),
            io::ErrorKind::TimedOut
        );
        let cancel = Cancel::new();
        let pending = cancel.clone();
        let worker =
            thread::spawn(move || enqueue_resolve(&tx, resolution_request(pending, deadline)));
        cancel.cancel();
        assert_eq!(
            worker.join().unwrap().unwrap_err().kind(),
            io::ErrorKind::Interrupted
        );
        assert_eq!(rx.try_iter().count(), 1);
    }

    #[test]
    fn resolver_reuses_hostname_addresses_with_each_requested_port() {
        let cancel = Cancel::new();
        for port in [12345, 23456, 34567] {
            let endpoint = Endpoint::new("localhost", port).unwrap();
            let addresses =
                resolve(&endpoint, &cancel, Instant::now() + Duration::from_secs(5)).unwrap();
            assert!(!addresses.is_empty());
            assert!(addresses.iter().all(|address| address.port() == port));
        }
    }

    #[test]
    fn acknowledgment_deadline_tracks_video_rtt_and_is_always_bounded() {
        let mut rtt = latch_proto::rtt::RttEstimator::new();
        let floor = Duration::from_millis(750);
        assert_eq!(video_ack_deadline(&rtt, floor), floor);
        for _ in 0..100 {
            rtt.sample(Duration::from_millis(180));
        }
        assert_eq!(video_ack_deadline(&rtt, floor), floor);
        rtt.sample(Duration::from_millis(725));
        assert!(video_ack_deadline(&rtt, floor) > floor);
        for _ in 0..100 {
            rtt.sample(Duration::from_secs(10));
            assert!(video_ack_deadline(&rtt, floor) <= Duration::from_secs(5));
        }
        for _ in 0..200 {
            rtt.sample(Duration::from_millis(180));
        }
        assert_eq!(video_ack_deadline(&rtt, floor), floor);
    }

    fn shared() -> Shared {
        let limits = TransportLimits {
            max_frame_bytes: 64,
            video_queue_bytes: 64,
            priority_queue_bytes: 1280,
            event_queue_bytes: 2048,
            ..TransportLimits::default()
        };
        Shared {
            state: Mutex::new(State::default()),
            wake: Condvar::new(),
            cancel: Cancel::new(),
            limits,
        }
    }
    fn header(total: u32, flags: u8, data: &[u8]) -> Vec<u8> {
        let mut b = vec![3];
        b.extend(total.to_be_bytes());
        b.extend(99u32.to_be_bytes());
        b.push(flags);
        b.extend(17u16.to_be_bytes());
        b.extend(data);
        b
    }
    #[test]
    fn incomplete_oversized_and_wrong_lane_frames_never_deliver() {
        let shared = shared();
        let mut incoming = None;
        assert!(
            application_record(Lane::Video, &header(65, 0, &[]), &mut incoming, &shared).is_err()
        );
        assert!(
            application_record(Lane::Video, &header(4, 4, &[]), &mut incoming, &shared).is_err()
        );
        assert!(
            application_record(Lane::Priority, &header(4, 0, &[]), &mut incoming, &shared).is_err()
        );
        application_record(Lane::Video, &header(4, 3, &[1, 2]), &mut incoming, &shared).unwrap();
        assert!(shared.state.lock().unwrap().events.is_empty());
        assert!(
            application_record(Lane::Video, &header(4, 0, &[]), &mut incoming, &shared).is_err()
        );
        assert!(application_record(Lane::Video, &[4, 3, 4, 5], &mut incoming, &shared).is_err());
        application_record(Lane::Video, &[4, 3, 4], &mut incoming, &shared).unwrap();
        match shared.state.lock().unwrap().events.pop_front().unwrap() {
            Event::Frame(f) => {
                assert_eq!(f.data, [1, 2, 3, 4]);
                assert_eq!(f.frame, 99);
                assert!(f.keyframe && f.recovery);
                assert_eq!(f.host_delay, 17);
            }
            e => panic!("{e:?}"),
        }
        assert!(incoming.is_none());
    }
    #[test]
    fn priority_backpressure_and_whole_unsent_frame_discard() {
        let transport = Transport {
            shared: Arc::new(shared()),
            workers: Mutex::new(Vec::new()),
        };
        assert_eq!(
            transport.try_send_datagram(&[1]).unwrap_err().kind(),
            io::ErrorKind::NotConnected
        );
        transport.shared.state.lock().unwrap().ready = 3;
        transport.try_send_datagram(&[1; 1200]).unwrap();
        assert_eq!(
            transport.try_send_datagram(&[2; 1200]).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        assert_eq!(transport.shared.state.lock().unwrap().priority.len(), 1);
        let f = InFrame {
            frame: 1,
            data: vec![0; 64],
            keyframe: true,
            recovery: false,
            host_delay: 0,
            assembly: Duration::ZERO,
        };
        transport.try_send_frame(&f).unwrap();
        assert_eq!(
            transport.try_send_frame(&f).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        assert!(transport.discard_queued_frame());
        assert!(!transport.discard_queued_frame());
        transport.try_send_frame(&f).unwrap();
        transport.shutdown();
        assert!(matches!(
            transport.recv_timeout(Duration::ZERO),
            Some(Event::Closed { .. })
        ));
    }
    #[test]
    fn production_resolution_rejects_tailnet_and_private_addresses() {
        let mut config =
            ClientConfig::production(Endpoint::new("relay.example", 443).unwrap()).unwrap();
        config.limits.max_frame_bytes = 12345;
        let config = config
            .with_tls_roots(rustls::RootCertStore::empty())
            .unwrap();
        assert!(config.public_only);
        assert_eq!(config.limits.max_frame_bytes, 12345);
        for address in [
            "100.64.0.1",
            "100.127.255.255",
            "10.1.2.3",
            "127.0.0.1",
            "169.254.1.2",
            "192.168.1.2",
            "::1",
            "fd7a:115c:a1e0::1",
            "2001:db8::1",
            "64:ff9b::a01:203",
            "64:ff9b::6440:1",
            "64:ff9b::7f00:1",
            "64:ff9b:1::808:808",
        ] {
            assert!(!public_address(address.parse().unwrap()), "{address}");
        }
        for address in [
            "1.1.1.1",
            "8.8.8.8",
            "2606:4700:4700::1111",
            "64:ff9b::808:808",
        ] {
            assert!(public_address(address.parse().unwrap()));
        }
        assert!(
            serde_json::from_str::<Endpoint>(r#"{"host":"https://relay.example","port":443}"#)
                .is_err()
        );
    }
}
