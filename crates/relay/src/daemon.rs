//! Bounded anonymous pilot daemon. No outbound destinations or public admin API.
use crate::{
    crypto::{ensure_supported_cpu, random},
    denied, invalid,
    quota::Quota,
    wire::{self, TlsSocket, TICK},
    Attachment, Cancel, Grant, Role,
};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use std::{
    io,
    net::{IpAddr, SocketAddr, TcpListener},
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PilotLimits {
    pub max_connections: usize,
    pub max_sessions: usize,
    pub connections_per_ip: usize,
    pub sessions_per_ip: usize,
    pub accepts_per_ip_per_minute: u32,
    pub accepts_per_minute: u32,
    pub max_rate_entries: usize,
    pub grant_ttl: Duration,
    pub session_lifetime: Duration,
    pub session_bytes: u64,
    pub handshake_timeout: Duration,
    pub stall_timeout: Duration,
    pub idle_timeout: Duration,
}
impl Default for PilotLimits {
    fn default() -> Self {
        Self {
            max_connections: 24,
            max_sessions: 4,
            connections_per_ip: 8,
            sessions_per_ip: 2,
            accepts_per_ip_per_minute: 30,
            accepts_per_minute: 120,
            max_rate_entries: 256,
            grant_ttl: Duration::from_secs(60),
            session_lifetime: Duration::from_secs(900),
            session_bytes: 512 * 1024 * 1024,
            handshake_timeout: Duration::from_secs(10),
            stall_timeout: Duration::from_millis(750),
            idle_timeout: Duration::from_secs(30),
        }
    }
}
impl PilotLimits {
    fn validate(&self) -> io::Result<()> {
        if !(4..=512).contains(&self.max_connections)
            || self.max_sessions == 0
            || self.max_sessions > 64
            || self.connections_per_ip == 0
            || self.connections_per_ip > self.max_connections
            || self.sessions_per_ip == 0
            || self.sessions_per_ip > self.max_sessions
            || self.accepts_per_ip_per_minute == 0
            || self.accepts_per_minute == 0
            || self.max_rate_entries == 0
            || self.max_rate_entries > 4096
            || self.grant_ttl < Duration::from_secs(1)
            || self.grant_ttl > Duration::from_secs(300)
            || self.session_lifetime < Duration::from_secs(1)
            || self.session_lifetime > Duration::from_secs(24 * 3600)
            || self.session_bytes < 1024 * 1024
            || self.handshake_timeout.is_zero()
            || self.handshake_timeout > Duration::from_secs(60)
            || self.stall_timeout.is_zero()
            || self.stall_timeout > Duration::from_secs(5)
            || self.idle_timeout.is_zero()
            || self.idle_timeout > Duration::from_secs(300)
        {
            return Err(invalid("invalid relay pilot limits"));
        }
        Ok(())
    }
}

/// TLS server configuration and persistent, mandatory economic limits.
pub struct DaemonConfig {
    pub listen: SocketAddr,
    pub http_listen: Option<SocketAddr>,
    pub acme_webroot: Option<PathBuf>,
    pub limits: PilotLimits,
    pub quota_path: PathBuf,
    pub monthly_bytes: u64,
    pub initialize_quota: bool,
    tls: Arc<rustls::ServerConfig>,
}
impl DaemonConfig {
    pub fn new(
        listen: SocketAddr,
        certificates: Vec<CertificateDer<'static>>,
        key: PrivateKeyDer<'static>,
        quota_path: PathBuf,
        monthly_bytes: u64,
    ) -> io::Result<Self> {
        ensure_supported_cpu()?;
        let mut tls = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls_graviola::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .map_err(|_| invalid("TLS versions unavailable"))?
        .with_no_client_auth()
        .with_single_cert(certificates, key)
        .map_err(|_| invalid("invalid relay TLS identity"))?;
        tls.alpn_protocols = vec![wire::ALPN.to_vec(), crate::carrier::HTTP_ALPN.to_vec()];
        // Explicitly bound sessions and disable TLS resumption state and tickets.
        tls.session_storage = Arc::new(rustls::server::NoServerSessionStorage {});
        tls.send_tls13_tickets = 0;
        Ok(Self {
            listen,
            http_listen: None,
            acme_webroot: None,
            limits: PilotLimits::default(),
            quota_path,
            monthly_bytes,
            initialize_quota: false,
            tls: Arc::new(tls),
        })
    }
}
#[derive(Default)]
pub struct Metrics {
    /// Application-observed encrypted TCP bytes, counting ingress plus egress.
    pub network_bytes: AtomicU64,
    /// Durable upper bound, including unused byte leases and TLS setup traffic.
    pub quota_reserved_bytes: AtomicU64,
    pub accepted_connections: AtomicU64,
    pub rejected_connections: AtomicU64,
    pub grants_created: AtomicU64,
    pub http_responses: AtomicU64,
    pub active_connections: AtomicU64,
    pub active_sessions: AtomicU64,
}
impl Metrics {
    /// Fixed public counters. Contains no identifiers, tickets, keys or IPs.
    pub fn snapshot(&self) -> String {
        format!("network_bytes={} quota_reserved_bytes={} accepted={} rejected={} grants={} http_responses={} connections={} sessions={}",
            self.network_bytes.load(Ordering::Relaxed), self.quota_reserved_bytes.load(Ordering::Relaxed),
            self.accepted_connections.load(Ordering::Relaxed), self.rejected_connections.load(Ordering::Relaxed),
            self.grants_created.load(Ordering::Relaxed), self.http_responses.load(Ordering::Relaxed), self.active_connections.load(Ordering::Relaxed), self.active_sessions.load(Ordering::Relaxed))
    }
}
struct Rate {
    ip: IpAddr,
    start: Instant,
    accepts: u32,
}
#[derive(Clone, Copy)]
enum Phase {
    Request,
    ReplyAndClose,
    Waiting {
        session: usize,
        role: usize,
        lane: usize,
    },
    Attached {
        session: usize,
        role: usize,
        lane: usize,
    },
}
impl Phase {
    fn session(self) -> Option<usize> {
        match self {
            Self::Waiting { session, .. } | Self::Attached { session, .. } => Some(session),
            _ => None,
        }
    }
}
struct Connection {
    socket: TlsSocket,
    ip: IpAddr,
    opened: Instant,
    phase: Phase,
}
struct Session {
    grant: Grant,
    ip: IpAddr,
    created: Instant,
    attach_until: Instant,
    used: [u8; 2],
    lanes: [[Option<usize>; 2]; 2],
    bytes: u64,
}
impl Session {
    fn claim(&mut self, ticket: &[u8], role: usize, lane: usize) -> io::Result<()> {
        let expected = if role == 0 {
            &self.grant.host.tickets[lane]
        } else {
            &self.grant.viewer.tickets[lane]
        };
        // Equal-length capability comparison visits every byte.
        let mismatch = ticket
            .iter()
            .zip(expected)
            .fold(0u8, |v, (a, b)| v | (a ^ b));
        if ticket.len() != 32
            || mismatch != 0
            || self.used[role] & (1 << lane) != 0
            || Instant::now() >= self.attach_until
        {
            return Err(denied());
        }
        self.used[role] |= 1 << lane;
        Ok(())
    }
}

/// Single nonblocking event loop, fixed slot counts and fixed forwarding buffers.
pub struct Daemon {
    listener: TcpListener,
    probe: Option<crate::probe::ProbeServer>,
    config: DaemonConfig,
    quota: Quota,
    metrics: Arc<Metrics>,
    connections: Vec<Option<Connection>>,
    sessions: Vec<Option<Session>>,
    rates: Vec<Rate>,
    rate_start: Instant,
    accepts: u32,
    diagnostic_start: Instant,
    diagnostics: u32,
}
impl Daemon {
    pub fn bind(config: DaemonConfig) -> io::Result<Self> {
        ensure_supported_cpu()?;
        config.limits.validate()?;
        let probe = config
            .http_listen
            .map(|address| crate::probe::ProbeServer::bind(address, config.acme_webroot.as_deref()))
            .transpose()?;
        let quota = Quota::open(
            &config.quota_path,
            config.monthly_bytes,
            config.initialize_quota,
        )?;
        let socket = socket2::Socket::new(
            socket2::Domain::for_address(config.listen),
            socket2::Type::STREAM,
            Some(socket2::Protocol::TCP),
        )?;
        #[cfg(unix)]
        socket.set_reuse_address(true)?;
        if config.listen.is_ipv6() {
            socket.set_only_v6(false)?;
        }
        socket.bind(&config.listen.into())?;
        socket.listen(128)?;
        let listener: TcpListener = socket.into();
        listener.set_nonblocking(true)?;
        let connections = (0..config.limits.max_connections).map(|_| None).collect();
        let sessions = (0..config.limits.max_sessions).map(|_| None).collect();
        Ok(Self {
            listener,
            probe,
            config,
            quota,
            metrics: Arc::new(Metrics::default()),
            connections,
            sessions,
            rates: Vec::new(),
            rate_start: Instant::now(),
            accepts: 0,
            diagnostic_start: Instant::now(),
            diagnostics: 0,
        })
    }
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.listener.local_addr()
    }
    pub fn http_addr(&self) -> io::Result<Option<SocketAddr>> {
        self.probe
            .as_ref()
            .map(|probe| probe.local_addr())
            .transpose()
    }
    pub fn metrics(&self) -> Arc<Metrics> {
        self.metrics.clone()
    }
    /// Run until cancellation. Quota persistence failure stops the daemon fail closed.
    pub fn run(mut self, cancel: &Cancel) -> io::Result<()> {
        while !cancel.is_cancelled() {
            let before = self.metrics.network_bytes.load(Ordering::Relaxed);
            if let Some(probe) = &mut self.probe {
                probe.poll();
            }
            self.accept()?;
            for index in 0..self.connections.len() {
                if self.connections[index].is_none() {
                    continue;
                }
                if let Err(error) = self.step(index) {
                    self.connection_error(index, &error);
                }
            }
            self.quota.check()?;
            self.pair_waiting();
            for index in 0..self.sessions.len() {
                let expired = self.sessions[index].as_ref().is_some_and(|s| {
                    s.created.elapsed() >= self.config.limits.session_lifetime
                        || s.bytes >= self.config.limits.session_bytes
                        || (Instant::now() >= s.attach_until && s.used != [3, 3])
                });
                if expired {
                    self.remove_session(index);
                }
            }
            self.metrics
                .quota_reserved_bytes
                .store(self.quota.reserved(), Ordering::Relaxed);
            self.metrics.active_connections.store(
                self.connections.iter().filter(|c| c.is_some()).count() as u64,
                Ordering::Relaxed,
            );
            self.metrics.active_sessions.store(
                self.sessions.iter().filter(|s| s.is_some()).count() as u64,
                Ordering::Relaxed,
            );
            if self.metrics.network_bytes.load(Ordering::Relaxed) == before {
                thread::sleep(TICK);
            }
        }
        self.metrics.active_connections.store(0, Ordering::Relaxed);
        self.metrics.active_sessions.store(0, Ordering::Relaxed);
        Ok(())
    }
    fn connection_error(&mut self, index: usize, error: &io::Error) {
        let conn = self.connections[index].as_ref().unwrap();
        let phase = conn.phase;
        let decoy = matches!(conn.socket.carrier, crate::carrier::State::Closing);
        if !decoy
            && !(matches!(phase, Phase::ReplyAndClose)
                && error.kind() == io::ErrorKind::ConnectionAborted)
        {
            self.diagnose(Some(phase), error);
        }
        let conn = self.connections[index].as_mut().unwrap();
        // Only send a rejection from existing credit that the next pump can use.
        // A per-connection queue failure always retires this connection.
        if error.kind() == io::ErrorKind::PermissionDenied
            && matches!(phase, Phase::Request)
            && conn.socket.credit >= (2 * wire::RECORD_MAX) as u64
            && conn.socket.ready()
            && conn.socket.queue(&[0]).is_ok()
        {
            conn.phase = Phase::ReplyAndClose;
        } else {
            self.remove_connection(index);
        }
    }
    fn diagnose(&mut self, phase: Option<Phase>, error: &io::Error) {
        if self.diagnostic_start.elapsed() >= Duration::from_secs(60) {
            self.diagnostic_start = Instant::now();
            self.diagnostics = 0;
        }
        // One global bound also covers unauthenticated callers and quota refusals.
        if self.diagnostics >= 9 {
            return;
        }
        self.diagnostics += 1;
        if self.diagnostics == 9 {
            eprintln!("relay diagnostics suppressed until next minute");
            return;
        }
        let (phase, role, lane) = match phase {
            None => ("admission", "none", "none"),
            Some(Phase::Request) => ("request", "none", "none"),
            Some(Phase::ReplyAndClose) => ("reply", "none", "none"),
            Some(Phase::Waiting { role, lane, .. }) => {
                ("waiting", role_name(role), lane_name(lane))
            }
            Some(Phase::Attached { role, lane, .. }) => {
                ("attached", role_name(role), lane_name(lane))
            }
        };
        // Never emit arbitrary error text, addresses, slot/session IDs or capabilities.
        eprintln!(
            "relay connection closed phase={phase} role={role} lane={lane} category={} kind={:?}",
            error_category(error),
            error.kind()
        );
    }
    fn rate_allowed(&mut self, ip: IpAddr) -> bool {
        if self.rate_start.elapsed() >= Duration::from_secs(60) {
            self.rate_start = Instant::now();
            self.accepts = 0;
        }
        if self.accepts >= self.config.limits.accepts_per_minute {
            return false;
        }
        self.rates
            .retain(|r| r.start.elapsed() < Duration::from_secs(60));
        if let Some(r) = self.rates.iter_mut().find(|r| r.ip == ip) {
            if r.accepts >= self.config.limits.accepts_per_ip_per_minute {
                return false;
            }
            r.accepts += 1;
            self.accepts += 1;
            return true;
        }
        if self.rates.len() >= self.config.limits.max_rate_entries {
            // Oldest-window eviction lets a new source compete for global admission.
            let oldest = self
                .rates
                .iter()
                .enumerate()
                .min_by_key(|(_, r)| r.start)
                .unwrap()
                .0;
            self.rates.swap_remove(oldest);
        }
        self.accepts += 1;
        self.rates.push(Rate {
            ip,
            start: Instant::now(),
            accepts: 1,
        });
        true
    }
    fn accept(&mut self) -> io::Result<()> {
        for _ in 0..8 {
            let (tcp, address) = match self.listener.accept() {
                Ok(c) => c,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => return Err(e),
            };
            let ip = normalize_ip(address.ip());
            let slot = self.connections.iter().position(|c| c.is_none());
            if !self.rate_allowed(ip)
                || slot.is_none()
                || self
                    .connections
                    .iter()
                    .flatten()
                    .filter(|c| c.ip == ip)
                    .count()
                    >= self.config.limits.connections_per_ip
            {
                self.metrics
                    .rejected_connections
                    .fetch_add(1, Ordering::Relaxed);
                continue;
            }
            // Reserve durable transfer credit before even the TLS handshake.
            let mut credit = 0;
            let mut credit_epoch = 0;
            match self.quota.credit(&mut credit, &mut credit_epoch) {
                Ok(()) => (),
                // The bounded connection slot waits for durable credit before
                // its TLS pump can read or write any network bytes.
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => (),
                Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                    self.metrics
                        .rejected_connections
                        .fetch_add(1, Ordering::Relaxed);
                    self.diagnose(None, &error);
                    continue;
                }
                Err(error) => return Err(error),
            }
            let socket = rustls::ServerConnection::new(self.config.tls.clone())
                .map_err(|_| invalid("TLS server setup failed"))
                .and_then(|tls| TlsSocket::new(tls.into(), tcp));
            let mut socket = match socket {
                Ok(socket) => socket,
                Err(error) => {
                    self.diagnose(None, &error);
                    continue;
                }
            };
            socket.carrier = crate::carrier::State::Detect;
            socket.credit = credit;
            socket.credit_epoch = credit_epoch;
            self.connections[slot.unwrap()] = Some(Connection {
                socket,
                ip,
                opened: Instant::now(),
                phase: Phase::Request,
            });
            self.metrics
                .accepted_connections
                .fetch_add(1, Ordering::Relaxed);
        }
        Ok(())
    }
    fn step(&mut self, index: usize) -> io::Result<()> {
        let conn = self.connections[index].as_mut().unwrap();
        if let Some(session) = conn.phase.session() {
            let s = self.sessions[session].as_ref().ok_or_else(denied)?;
            if s.bytes
                > self
                    .config
                    .limits
                    .session_bytes
                    .saturating_sub((2 * wire::CHUNK) as u64)
            {
                return Err(denied());
            }
        }
        let before = conn.socket.transferred;
        let pumped = conn.socket.pump(Some(&mut self.quota));
        if matches!(&pumped, Err(error) if error.kind() == io::ErrorKind::PermissionDenied)
            && matches!(conn.phase, Phase::Request)
        {
            self.metrics
                .rejected_connections
                .fetch_add(1, Ordering::Relaxed);
        }
        let delta = conn.socket.transferred - before;
        self.metrics
            .network_bytes
            .fetch_add(delta, Ordering::Relaxed);
        if let Some(session) = conn.phase.session() {
            let s = self.sessions[session].as_mut().ok_or_else(denied)?;
            s.bytes = s.bytes.saturating_add(delta);
            if s.bytes >= self.config.limits.session_bytes {
                return Err(denied());
            }
        }
        if matches!(conn.phase, Phase::Request)
            && matches!(conn.socket.carrier, crate::carrier::State::Closing)
        {
            self.metrics.http_responses.fetch_add(1, Ordering::Relaxed);
            conn.phase = Phase::ReplyAndClose;
        }
        pumped?;
        conn.socket.check_deadlines(
            self.config.limits.stall_timeout,
            self.config.limits.idle_timeout,
        )?;
        if !matches!(conn.phase, Phase::Attached { .. })
            && conn.opened.elapsed() >= self.config.limits.handshake_timeout
        {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "relay handshake deadline expired",
            ));
        }
        let phase = conn.phase;
        match phase {
            Phase::Request => {
                if !conn.socket.idle_write() {
                    return Ok(());
                }
                if let Some(request) = conn.socket.read_record()? {
                    match request.first() {
                        Some(1) if request.len() == 1 => self.create(index)?,
                        Some(2) if request.len() == 67 => self.attach(index, &request)?,
                        _ => return Err(denied()),
                    }
                }
            }
            Phase::ReplyAndClose => {
                if conn.socket.idle_write() {
                    return Err(io::ErrorKind::ConnectionAborted.into());
                }
            }
            Phase::Waiting { .. } => (),
            Phase::Attached {
                session,
                role,
                lane,
            } => {
                let peer = self.sessions[session].as_ref().ok_or_else(denied)?.lanes[1 - role]
                    [lane]
                    .ok_or_else(denied)?;
                // Backpressure leaves bytes in TLS/socket buffers. One record is forwarded only
                // when its peer has completely flushed the previous record.
                if self.connections[peer]
                    .as_ref()
                    .is_some_and(|c| c.socket.idle_write())
                {
                    if let Some(record) = self.connections[index]
                        .as_mut()
                        .unwrap()
                        .socket
                        .read_record()?
                    {
                        if record.len() < 25 {
                            return Err(invalid("short encrypted relay record"));
                        }
                        self.connections[peer]
                            .as_mut()
                            .unwrap()
                            .socket
                            .queue(&record)?;
                    }
                }
            }
        }
        Ok(())
    }
    fn create(&mut self, index: usize) -> io::Result<()> {
        let connection = self.connections[index].as_ref().unwrap();
        let ip = connection.ip;
        if self
            .sessions
            .iter()
            .flatten()
            .filter(|s| s.ip == ip)
            .count()
            >= self.config.limits.sessions_per_ip
        {
            return Err(denied());
        }
        let slot = self
            .sessions
            .iter()
            .position(|s| s.is_none())
            .ok_or_else(denied)?;
        let id = random()?;
        if self
            .sessions
            .iter()
            .flatten()
            .any(|s| s.grant.host.id == id)
        {
            return Err(io::Error::other("rendezvous collision"));
        }
        let expires_unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| invalid("system clock invalid"))?
            .as_secs()
            .checked_add(self.config.limits.grant_ttl.as_secs())
            .ok_or_else(denied)?;
        let host = Attachment {
            id,
            tickets: [random()?, random()?],
            role: Role::Host,
            expires_unix,
            session_seconds: self.config.limits.session_lifetime.as_secs() as u32,
            session_bytes: self.config.limits.session_bytes,
        };
        let viewer = Attachment {
            tickets: [random()?, random()?],
            role: Role::Viewer,
            ..host.clone()
        };
        let grant = Grant { host, viewer };
        let connection = self.connections[index].as_mut().unwrap();
        // The request may arrive before the last handshake bytes flush.
        if !connection.socket.idle_write() {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        connection.socket.queue(&grant.encode())?;
        connection.phase = Phase::ReplyAndClose;
        self.sessions[slot] = Some(Session {
            grant,
            ip,
            created: Instant::now(),
            attach_until: Instant::now() + self.config.limits.grant_ttl,
            used: [0, 0],
            lanes: [[None; 2]; 2],
            bytes: connection.socket.transferred,
        });
        self.metrics.grants_created.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
    fn attach(&mut self, index: usize, request: &[u8]) -> io::Result<()> {
        let role = request[65] as usize;
        let lane = request[66] as usize;
        if role > 1 || lane > 1 {
            return Err(denied());
        }
        let slot = self
            .sessions
            .iter()
            .position(|s| {
                s.as_ref()
                    .is_some_and(|s| s.grant.host.id == request[1..33])
            })
            .ok_or_else(denied)?;
        let session = self.sessions[slot].as_mut().unwrap();
        session.claim(&request[33..65], role, lane)?;
        session.lanes[role][lane] = Some(index);
        session.bytes = session
            .bytes
            .saturating_add(self.connections[index].as_ref().unwrap().socket.transferred);
        self.connections[index].as_mut().unwrap().phase = Phase::Waiting {
            session: slot,
            role,
            lane,
        };
        Ok(())
    }
    fn pair_waiting(&mut self) {
        for session in self.sessions.iter().flatten() {
            for lane in 0..2 {
                let (Some(a), Some(b)) = (session.lanes[0][lane], session.lanes[1][lane]) else {
                    continue;
                };
                let can_pair = [a, b].iter().all(|i| {
                    self.connections[*i].as_ref().is_some_and(|c| {
                        matches!(c.phase, Phase::Waiting { .. }) && c.socket.idle_write()
                    })
                });
                if !can_pair {
                    continue;
                }
                for index in [a, b] {
                    let c = self.connections[index].as_mut().unwrap();
                    if let Phase::Waiting {
                        session,
                        role,
                        lane,
                    } = c.phase
                    {
                        if c.socket.queue(&[3]).is_ok() {
                            c.phase = Phase::Attached {
                                session,
                                role,
                                lane,
                            };
                        }
                    }
                }
            }
        }
    }
    fn remove_connection(&mut self, index: usize) {
        if let Some(conn) = self.connections[index].take() {
            if let Some(session) = conn.phase.session() {
                self.remove_session(session);
            }
        }
    }
    fn remove_session(&mut self, index: usize) {
        if let Some(session) = self.sessions[index].take() {
            for connection in session.lanes.into_iter().flatten().flatten() {
                self.connections[connection] = None;
            }
        }
    }
}
fn role_name(role: usize) -> &'static str {
    match role {
        0 => "host",
        _ => "viewer",
    }
}
fn lane_name(lane: usize) -> &'static str {
    match lane {
        0 => "priority",
        _ => "video",
    }
}
fn error_category(error: &io::Error) -> &'static str {
    match error.kind() {
        io::ErrorKind::TimedOut => match error.to_string().as_str() {
            "TLS write deadline expired" => "tls_write_timeout",
            "TLS record deadline expired" => "partial_record_timeout",
            "TLS socket idle deadline expired" => "idle_timeout",
            "relay handshake deadline expired" => "handshake_timeout",
            _ => "tcp_timeout",
        },
        io::ErrorKind::PermissionDenied => "denied_or_quota_exhausted",
        io::ErrorKind::UnexpectedEof => "peer_closed",
        io::ErrorKind::ConnectionReset => "connection_reset",
        io::ErrorKind::InvalidData | io::ErrorKind::InvalidInput => "invalid_protocol",
        _ => "io_error",
    }
}
pub(crate) fn normalize_ip(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(ip) => {
            let bytes = ip.octets();
            if bytes[..12] == [0, 0x64, 0xff, 0x9b, 0, 0, 0, 0, 0, 0, 0, 0] {
                return IpAddr::V4(std::net::Ipv4Addr::new(
                    bytes[12], bytes[13], bytes[14], bytes[15],
                ));
            }
            if let Some(v4) = ip.to_ipv4_mapped() {
                return IpAddr::V4(v4);
            }
            let mut prefix = bytes;
            prefix[8..].fill(0);
            IpAddr::V6(std::net::Ipv6Addr::from(prefix))
        }
        ip => ip,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn test_daemon() -> (Daemon, PathBuf) {
        use rustls::pki_types::pem::PemObject;
        let path = std::env::temp_dir().join(format!(
            "latch-admission-{}-{}",
            std::process::id(),
            u64::from_be_bytes(random().unwrap())
        ));
        let mut config = DaemonConfig::new(
            "127.0.0.1:0".parse().unwrap(),
            vec![
                CertificateDer::from_pem_slice(include_bytes!("../tests/certs/server.pem"))
                    .unwrap(),
            ],
            PrivateKeyDer::from_pem_slice(include_bytes!("../tests/certs/server-key.pem")).unwrap(),
            path.clone(),
            16 * 1024 * 1024,
        )
        .unwrap();
        config.initialize_quota = true;
        (Daemon::bind(config).unwrap(), path)
    }
    #[test]
    fn ipv6_prefixes_share_all_admission_buckets_and_rate_rejections_preserve_global_slots() {
        let (mut daemon, path) = test_daemon();
        daemon.config.limits.accepts_per_ip_per_minute = 2;
        let a = normalize_ip("2001:4860:1:2::1".parse().unwrap());
        let b = normalize_ip("2001:4860:1:2:ffff:ffff:ffff:ffff".parse().unwrap());
        assert_eq!(a, b);
        assert_ne!(a, normalize_ip("2001:4860:1:3::1".parse().unwrap()));
        assert!(daemon.rate_allowed(a));
        assert!(daemon.rate_allowed(b));
        for _ in 0..1000 {
            assert!(!daemon.rate_allowed(b));
        }
        assert_eq!(daemon.rates.len(), 1);
        assert_eq!(daemon.accepts, 2);
        assert!(daemon.rate_allowed("8.8.8.8".parse().unwrap()));
        for ip in ["::ffff:192.0.2.1", "64:ff9b::c000:201"] {
            assert_eq!(
                normalize_ip(ip.parse().unwrap()),
                "192.0.2.1".parse::<IpAddr>().unwrap()
            );
        }
        // Unknown network-specific source translation uses the gateway's /64 bucket.
        assert_eq!(
            normalize_ip("64:ff9b:1:1234::1".parse().unwrap()),
            normalize_ip("64:ff9b:1:1234::2".parse().unwrap())
        );
        drop(daemon);
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn full_rate_table_evicts_oldest_window_and_admits_new_clients() {
        let (mut daemon, path) = test_daemon();
        daemon.config.limits.max_rate_entries = 2;
        for ip in ["1.1.1.1", "8.8.8.8", "9.9.9.9"] {
            assert!(daemon.rate_allowed(ip.parse().unwrap()));
        }
        assert_eq!(daemon.rates.len(), 2);
        assert!(!daemon
            .rates
            .iter()
            .any(|r| r.ip == "1.1.1.1".parse::<IpAddr>().unwrap()));
        assert_eq!(daemon.accepts, 3);
        daemon.accepts = daemon.config.limits.accepts_per_minute;
        assert!(!daemon.rate_allowed("4.2.2.2".parse().unwrap()));
        drop(daemon);
        std::fs::remove_file(path).unwrap();
    }
    fn accept_until(daemon: &mut Daemon, count: usize) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while daemon.connections.iter().flatten().count() < count {
            daemon.accept().unwrap();
            assert!(
                Instant::now() < deadline,
                "connections did not reach listener"
            );
            thread::sleep(TICK);
        }
    }
    #[test]
    fn rotating_ipv6_subnets_still_obey_global_admission() {
        let (mut daemon, path) = test_daemon();
        daemon.config.limits.max_rate_entries = 2;
        daemon.config.limits.accepts_per_minute = 12;
        for subnet in 0..20 {
            let address = format!("2606:4700:1234:{subnet:x}::1").parse().unwrap();
            assert_eq!(daemon.rate_allowed(normalize_ip(address)), subnet < 12);
        }
        assert_eq!(daemon.accepts, 12);
        assert!(daemon.rates.len() <= 2);
        drop(daemon);
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn connection_error_retires_one_slot_and_decoys_preserve_diagnostic_budget() {
        use std::net::TcpStream;
        let (mut daemon, path) = test_daemon();
        let tcp = TcpStream::connect(daemon.local_addr().unwrap()).unwrap();
        accept_until(&mut daemon, 1);
        for _ in 0..20 {
            daemon.connections[0].as_mut().unwrap().socket.carrier = crate::carrier::State::Closing;
            // step counts the HTTP response once, including when the write fails.
            let _ = daemon.step(0);
        }
        assert_eq!(daemon.metrics.http_responses.load(Ordering::Relaxed), 1);
        daemon.connection_error(0, &io::ErrorKind::ConnectionReset.into());
        assert_eq!(daemon.diagnostics, 0);
        assert!(daemon.metrics.snapshot().contains("http_responses=1"));
        let second = TcpStream::connect(daemon.local_addr().unwrap()).unwrap();
        accept_until(&mut daemon, 1);
        daemon.connection_error(0, &denied());
        assert!(daemon.connections[0].is_none());
        assert_eq!(daemon.diagnostics, 1);
        let third = TcpStream::connect(daemon.local_addr().unwrap()).unwrap();
        accept_until(&mut daemon, 1);
        assert!(daemon.connections[0].is_some());
        drop((tcp, second, third, daemon));
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn exhausted_admission_preserves_existing_credit_and_keeps_running() {
        use rustls::pki_types::pem::PemObject;
        use std::net::TcpStream;
        let path = std::env::temp_dir().join(format!(
            "latch-daemon-quota-{}-{:x}",
            std::process::id(),
            u64::from_be_bytes(random().unwrap())
        ));
        let cert =
            CertificateDer::from_pem_slice(include_bytes!("../tests/certs/server.pem")).unwrap();
        let key =
            PrivateKeyDer::from_pem_slice(include_bytes!("../tests/certs/server-key.pem")).unwrap();
        let mut config = DaemonConfig::new(
            "127.0.0.1:0".parse().unwrap(),
            vec![cert],
            key,
            path.clone(),
            64 * 1024,
        )
        .unwrap();
        config.initialize_quota = true;
        let mut daemon = Daemon::bind(config).unwrap();
        let address = daemon.local_addr().unwrap();
        let first = TcpStream::connect(address).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while daemon.connections[0].is_none() {
            daemon.accept().unwrap();
            assert!(Instant::now() < deadline);
            thread::sleep(TICK);
        }
        while daemon.connections[0].as_ref().unwrap().socket.credit == 0 {
            daemon.step(0).unwrap();
            assert!(Instant::now() < deadline);
            thread::sleep(TICK);
        }
        let credit = daemon.connections[0].as_ref().unwrap().socket.credit;
        assert_eq!(credit, 64 * 1024);
        let second = TcpStream::connect(address).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while daemon.metrics.rejected_connections.load(Ordering::Relaxed) == 0 {
            daemon.accept().unwrap();
            for index in 0..daemon.connections.len() {
                if daemon.connections[index].is_some() {
                    if let Err(error) = daemon.step(index) {
                        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
                        daemon.remove_connection(index);
                    }
                }
            }
            assert!(Instant::now() < deadline);
            thread::sleep(TICK);
        }
        assert_eq!(daemon.connections.iter().flatten().count(), 1);
        assert_eq!(
            daemon.connections[0].as_ref().unwrap().socket.credit,
            credit
        );
        assert_eq!(daemon.quota.reserved(), 64 * 1024);
        for _ in 0..100 {
            daemon.diagnose(None, &denied());
        }
        assert_eq!(daemon.diagnostics, 9);
        daemon.diagnostic_start = Instant::now() - Duration::from_secs(60);
        daemon.diagnose(None, &denied());
        assert_eq!(daemon.diagnostics, 1);

        let cancel = Cancel::new();
        let worker_cancel = cancel.clone();
        let metrics = daemon.metrics();
        let worker = thread::spawn(move || daemon.run(&worker_cancel));
        let third = TcpStream::connect(address).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while metrics.rejected_connections.load(Ordering::Relaxed) < 2 {
            assert!(
                Instant::now() < deadline,
                "daemon stopped processing admission"
            );
            thread::sleep(TICK);
        }
        assert!(!worker.is_finished());
        cancel.cancel();
        worker.join().unwrap().unwrap();
        drop((first, second, third));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn diagnostics_classify_deadlines_without_exposing_error_text() {
        for (message, expected) in [
            ("TLS write deadline expired", "tls_write_timeout"),
            ("TLS record deadline expired", "partial_record_timeout"),
            ("TLS socket idle deadline expired", "idle_timeout"),
            ("relay handshake deadline expired", "handshake_timeout"),
            ("secret attacker-controlled text", "tcp_timeout"),
        ] {
            assert_eq!(
                error_category(&io::Error::new(io::ErrorKind::TimedOut, message)),
                expected
            );
        }
        assert_eq!(error_category(&denied()), "denied_or_quota_exhausted");
        assert_eq!(error_category(&io::Error::other("secret")), "io_error");
    }

    #[test]
    fn pairing_role_capabilities_are_single_use_per_lane() {
        let a = Attachment {
            id: [1; 32],
            tickets: [[2; 32], [4; 32]],
            role: Role::Host,
            expires_unix: 99,
            session_seconds: 20,
            session_bytes: 1024 * 1024,
        };
        let b = Attachment {
            tickets: [[3; 32], [5; 32]],
            role: Role::Viewer,
            ..a.clone()
        };
        let mut session = Session {
            grant: Grant { host: a, viewer: b },
            ip: "127.0.0.1".parse().unwrap(),
            created: Instant::now(),
            attach_until: Instant::now() + Duration::from_secs(10),
            used: [0, 0],
            lanes: [[None; 2]; 2],
            bytes: 0,
        };
        assert!(session.claim(&[3; 32], 0, 0).is_err());
        session.claim(&[2; 32], 0, 0).unwrap();
        assert!(session.claim(&[2; 32], 0, 0).is_err());
        assert!(session.claim(&[2; 32], 0, 1).is_err());
        session.claim(&[4; 32], 0, 1).unwrap();
        session.claim(&[3; 32], 1, 0).unwrap();
        session.attach_until = Instant::now();
        assert!(session.claim(&[5; 32], 1, 1).is_err());
    }
    #[test]
    fn wildcard_ipv6_listener_accepts_both_families_and_normalizes_ip_limits() {
        match TcpListener::bind("[::1]:0") {
            Ok(listener) => drop(listener),
            Err(error)
                if error.kind() == io::ErrorKind::AddrNotAvailable
                    || matches!(error.raw_os_error(), Some(47 | 97 | 10047 | 10049)) =>
            {
                eprintln!("IPv6 loopback unavailable: {error}");
                return;
            }
            Err(error) => panic!("IPv6 loopback probe failed: {error}"),
        }

        use rustls::pki_types::pem::PemObject;
        use std::net::TcpStream;
        let path = std::env::temp_dir().join(format!(
            "latch-dual-{}-{}",
            std::process::id(),
            u64::from_be_bytes(random().unwrap())
        ));
        let mut config = DaemonConfig::new(
            "[::]:0".parse().unwrap(),
            vec![
                CertificateDer::from_pem_slice(include_bytes!("../tests/certs/server.pem"))
                    .unwrap(),
            ],
            PrivateKeyDer::from_pem_slice(include_bytes!("../tests/certs/server-key.pem")).unwrap(),
            path.clone(),
            1024 * 1024,
        )
        .unwrap();
        config.initialize_quota = true;
        let mut daemon = Daemon::bind(config).unwrap();
        let port = daemon.local_addr().unwrap().port();
        let v4 = TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port)).unwrap();
        let v6 = TcpStream::connect((std::net::Ipv6Addr::LOCALHOST, port)).unwrap();
        accept_until(&mut daemon, 2);
        assert_eq!(daemon.connections.iter().flatten().count(), 2);
        for ip in ["::ffff:192.0.2.1", "64:ff9b::c000:201"] {
            assert_eq!(
                normalize_ip(ip.parse().unwrap()),
                "192.0.2.1".parse::<IpAddr>().unwrap()
            );
        }
        assert!(daemon
            .rates
            .iter()
            .any(|r| r.ip == "127.0.0.1".parse::<IpAddr>().unwrap()));
        drop((v4, v6, daemon));
        std::fs::remove_file(path).unwrap();
    }
}
