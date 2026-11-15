use crate::invalid;
#[cfg(test)]
use crate::timeout;
use serde::{Deserialize, Serialize};
use std::{
    fmt,
    io::{self, Read, Write},
    net::TcpStream,
    time::{Duration, Instant},
};

pub(crate) const ALPN: &[u8] = b"latch-relay/1";
pub(crate) const CHUNK: usize = 16 * 1024 - 24;
pub(crate) const RECORD_MAX: usize = CHUNK + 24;
const TLS_BUFFER: usize = 2 * (RECORD_MAX + 4);
pub(crate) const TICK: Duration = Duration::from_millis(2);

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[repr(u8)]
pub enum Role {
    Host = 0,
    Viewer = 1,
}
impl Role {
    pub(crate) fn peer(self) -> Self {
        match self {
            Self::Host => Self::Viewer,
            Self::Viewer => Self::Host,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Lane {
    Priority = 0,
    Video = 1,
}

/// Role attachment containing two independent single-use lane capabilities.
#[derive(Clone, Serialize, Deserialize)]
pub struct Attachment {
    pub(crate) id: [u8; 32],
    pub(crate) tickets: [[u8; 32]; 2],
    pub(crate) role: Role,
    pub expires_unix: u64,
    pub session_seconds: u32,
    pub session_bytes: u64,
}
impl Attachment {
    pub fn role(&self) -> Role {
        self.role
    }
    pub(crate) fn request(&self, lane: Lane) -> Vec<u8> {
        let mut b = vec![2];
        b.extend(self.id);
        b.extend(self.tickets[lane as usize]);
        b.extend([self.role as u8, lane as u8]);
        b
    }
}
impl fmt::Debug for Attachment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Attachment")
            .field("role", &self.role)
            .field("capability", &"[REDACTED]")
            .finish()
    }
}
#[derive(Clone, Debug)]
pub struct Grant {
    pub host: Attachment,
    pub viewer: Attachment,
}
impl Grant {
    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut b = vec![1];
        b.extend(self.host.id);
        for ticket in self.host.tickets.iter().chain(&self.viewer.tickets) {
            b.extend(ticket);
        }
        b.extend(self.host.expires_unix.to_be_bytes());
        b.extend(self.host.session_seconds.to_be_bytes());
        b.extend(self.host.session_bytes.to_be_bytes());
        b
    }
    pub(crate) fn decode(b: &[u8]) -> io::Result<Self> {
        if b.len() != 181 || b[0] != 1 {
            return Err(invalid("relay rejected create request"));
        }
        let id = b[1..33].try_into().unwrap();
        let expires_unix = u64::from_be_bytes(b[161..169].try_into().unwrap());
        let session_seconds = u32::from_be_bytes(b[169..173].try_into().unwrap());
        let session_bytes = u64::from_be_bytes(b[173..181].try_into().unwrap());
        if !(1..=86400).contains(&session_seconds) || session_bytes < 1024 * 1024 {
            return Err(invalid("invalid grant lifetime"));
        }
        Ok(Self {
            host: Attachment {
                id,
                tickets: [b[33..65].try_into().unwrap(), b[65..97].try_into().unwrap()],
                role: Role::Host,
                expires_unix,
                session_seconds,
                session_bytes,
            },
            viewer: Attachment {
                id,
                tickets: [
                    b[97..129].try_into().unwrap(),
                    b[129..161].try_into().unwrap(),
                ],
                role: Role::Viewer,
                expires_unix,
                session_seconds,
                session_bytes,
            },
        })
    }
}

/// One bounded, incrementally read length-prefixed application record.
#[derive(Default)]
pub(crate) struct Framer {
    header: [u8; 4],
    header_read: usize,
    body: Vec<u8>,
    body_read: usize,
    started: Option<Instant>,
}
impl Framer {
    pub fn read(&mut self, reader: &mut dyn Read) -> io::Result<Option<Vec<u8>>> {
        if self.header_read < 4 {
            match reader.read(&mut self.header[self.header_read..]) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "relay truncated record",
                    ))
                }
                Ok(n) => {
                    self.started.get_or_insert_with(Instant::now);
                    self.header_read += n;
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(None),
                Err(e) => return Err(e),
            }
            if self.header_read < 4 {
                return Ok(None);
            }
            let len = u32::from_be_bytes(self.header) as usize;
            if len == 0 || len > RECORD_MAX {
                return Err(invalid("relay record exceeds bound"));
            }
            self.body.resize(len, 0);
        }
        match reader.read(&mut self.body[self.body_read..]) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "relay truncated body",
                ))
            }
            Ok(n) => self.body_read += n,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(None),
            Err(e) => return Err(e),
        }
        if self.body_read != self.body.len() {
            return Ok(None);
        }
        self.header_read = 0;
        self.body_read = 0;
        self.started = None;
        Ok(Some(std::mem::take(&mut self.body)))
    }
    pub fn expired(&self, limit: Duration) -> bool {
        self.started.is_some_and(|t| t.elapsed() >= limit)
    }
}

/// Owns the entire TLS state and a single pending framed write.
pub(crate) struct TlsSocket {
    pub conn: rustls::Connection,
    pub tcp: TcpStream,
    pub framer: Framer,
    pending: Vec<u8>,
    written: usize,
    sending: Option<Instant>,
    pub last_io: Instant,
    pub credit: u64,
    pub credit_epoch: u64,
    pub transferred: u64,
    handshake_bytes: usize,
}
impl TlsSocket {
    pub fn new(conn: rustls::Connection, tcp: TcpStream) -> io::Result<Self> {
        tcp.set_nonblocking(true)?;
        tcp.set_nodelay(true)?;
        let mut result = Self {
            conn,
            tcp,
            framer: Framer::default(),
            pending: Vec::new(),
            written: 0,
            sending: None,
            last_io: Instant::now(),
            credit: 0,
            credit_epoch: 0,
            transferred: 0,
            handshake_bytes: 0,
        };
        result.conn.set_buffer_limit(Some(TLS_BUFFER));
        Ok(result)
    }
    pub fn idle_write(&self) -> bool {
        self.pending.is_empty() && !self.conn.wants_write()
    }
    pub fn queue(&mut self, record: &[u8]) -> io::Result<()> {
        if !self.idle_write() {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        if record.is_empty() || record.len() > RECORD_MAX {
            return Err(invalid("invalid outbound relay record"));
        }
        self.pending
            .extend_from_slice(&(record.len() as u32).to_be_bytes());
        self.pending.extend_from_slice(record);
        self.written = 0;
        self.sending = Some(Instant::now());
        Ok(())
    }
    pub fn pump(&mut self, quota: Option<&mut crate::quota::Quota>) -> io::Result<()> {
        if let Some(quota) = quota {
            match quota.credit(&mut self.credit, &mut self.credit_epoch) {
                Ok(()) => (),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(error) => return Err(error),
            }
        }
        let was_handshaking = self.conn.is_handshaking();
        let metered = self.credit != 0;
        let mut meter = Meter {
            tcp: &mut self.tcp,
            credit: &mut self.credit,
            transferred: &mut self.transferred,
            metered,
        };
        let before = *meter.transferred;
        // At most one bounded socket read and write per event-loop turn.
        if self.conn.wants_write() {
            match self.conn.write_tls(&mut meter) {
                Ok(_) => (),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => (),
                Err(e) => return Err(e),
            }
        }
        if self.conn.wants_read() {
            match self.conn.read_tls(&mut meter) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "relay socket closed",
                    ))
                }
                Ok(_) => {
                    self.conn
                        .process_new_packets()
                        .map_err(|_| invalid("TLS verification or protocol failed"))?;
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => (),
                Err(e) => return Err(e),
            }
        }
        let bytes = (*meter.transferred - before) as usize;
        if bytes > 0 {
            self.last_io = Instant::now();
        }
        if was_handshaking {
            self.handshake_bytes += bytes;
            if self.handshake_bytes > 128 * 1024 {
                return Err(invalid("TLS handshake exceeds bound"));
            }
        }
        if !self.conn.is_handshaking() {
            if self.conn.alpn_protocol() != Some(ALPN) {
                return Err(invalid("relay ALPN required"));
            }
            if self.written < self.pending.len() {
                match self.conn.writer().write(&self.pending[self.written..]) {
                    Ok(0) => (),
                    Ok(n) => self.written += n,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => (),
                    Err(e) => return Err(e),
                }
            }
            if self.written == self.pending.len() {
                self.pending.clear();
                self.written = 0;
            }
        }
        if self.idle_write() {
            self.sending = None;
        }
        Ok(())
    }
    pub fn read_record(&mut self) -> io::Result<Option<Vec<u8>>> {
        if self.conn.is_handshaking() {
            return Ok(None);
        }
        self.framer.read(&mut self.conn.reader())
    }
    pub fn check_deadlines(&self, stall: Duration, idle: Duration) -> io::Result<()> {
        if self.sending.is_some_and(|t| t.elapsed() >= stall) {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "TLS write deadline expired",
            ));
        }
        if self.framer.expired(stall) {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "TLS record deadline expired",
            ));
        }
        if self.last_io.elapsed() >= idle {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "TLS socket idle deadline expired",
            ));
        }
        Ok(())
    }
}
struct Meter<'a> {
    tcp: &'a mut TcpStream,
    credit: &'a mut u64,
    transferred: &'a mut u64,
    metered: bool,
}
impl Meter<'_> {
    fn limit(&self, requested: usize) -> usize {
        if self.metered {
            requested.min(*self.credit as usize).min(CHUNK)
        } else {
            requested.min(CHUNK)
        }
    }
    fn used(&mut self, n: usize) {
        *self.transferred += n as u64;
        if self.metered {
            *self.credit -= n as u64;
        }
    }
}
impl Read for Meter<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.limit(buf.len());
        if n == 0 {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        let n = self.tcp.read(&mut buf[..n])?;
        self.used(n);
        Ok(n)
    }
}
impl Write for Meter<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.limit(buf.len());
        if n == 0 {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        let n = self.tcp.write(&buf[..n])?;
        self.used(n);
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn records_reject_oversize_and_truncation() {
        let mut f = Framer::default();
        assert!(f
            .read(&mut &((RECORD_MAX as u32 + 1).to_be_bytes())[..])
            .is_err());
        let mut f = Framer::default();
        assert!(f.read(&mut &[0, 0, 0, 4, 1][..]).unwrap().is_none());
        assert_eq!(
            f.read(&mut &[][..]).unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
        let mut f = Framer::default();
        assert!(f.read(&mut &[0, 0][..]).unwrap().is_none());
        assert!(f.read(&mut &[][..]).is_err());
    }
    fn tls_pair(name: &'static str) -> io::Result<(TlsSocket, TlsSocket)> {
        use rustls::pki_types::{pem::PemObject, CertificateDer, PrivateKeyDer, ServerName};
        use std::{net::TcpListener, sync::Arc};
        crate::ensure_supported_cpu()?;
        let ca = CertificateDer::from_pem_slice(include_bytes!("../tests/certs/ca.pem")).unwrap();
        let cert =
            CertificateDer::from_pem_slice(include_bytes!("../tests/certs/server.pem")).unwrap();
        let key =
            PrivateKeyDer::from_pem_slice(include_bytes!("../tests/certs/server-key.pem")).unwrap();
        let mut roots = rustls::RootCertStore::empty();
        roots.add(ca).unwrap();
        let provider = Arc::new(rustls_graviola::default_provider());
        let mut client = rustls::ClientConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
        client.alpn_protocols = vec![ALPN.to_vec()];
        let mut server = rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![cert], key)
            .unwrap();
        server.alpn_protocols = vec![ALPN.to_vec()];
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let tcp = TcpStream::connect(listener.local_addr()?)?;
        let (peer, _) = listener.accept()?;
        let mut client = TlsSocket::new(
            rustls::ClientConnection::new(Arc::new(client), ServerName::try_from(name).unwrap())
                .unwrap()
                .into(),
            tcp,
        )?;
        let mut server = TlsSocket::new(
            rustls::ServerConnection::new(Arc::new(server))
                .unwrap()
                .into(),
            peer,
        )?;
        let deadline = Instant::now() + Duration::from_secs(2);
        while client.conn.is_handshaking()
            || server.conn.is_handshaking()
            || !client.idle_write()
            || !server.idle_write()
        {
            if Instant::now() >= deadline {
                return Err(timeout());
            }
            client.pump(None)?;
            server.pump(None)?;
        }
        Ok((client, server))
    }
    #[test]
    fn tls_hostname_verification_remains_enabled() {
        assert!(matches!(tls_pair("wrong.example"),Err(e) if e.kind()==io::ErrorKind::InvalidData));
    }
    #[test]
    fn partial_tls_writes_preserve_record_framing() {
        let (mut sender, mut receiver) = tls_pair("localhost").unwrap();
        socket2::SockRef::from(&sender.tcp)
            .set_send_buffer_size(1024)
            .unwrap();
        for value in 1..=4 {
            let record = vec![value; RECORD_MAX];
            sender.queue(&record).unwrap();
            assert_eq!(
                sender.queue(&record).unwrap_err().kind(),
                io::ErrorKind::WouldBlock
            );
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                assert!(Instant::now() < deadline);
                sender.pump(None).unwrap();
                receiver.pump(None).unwrap();
                if let Some(received) = receiver.read_record().unwrap() {
                    assert_eq!(received, record);
                    break;
                }
            }
            while !sender.idle_write() {
                sender.pump(None).unwrap();
                receiver.pump(None).unwrap();
            }
        }
    }
    #[test]
    fn stalled_and_closed_tls_peers_fail_on_bounded_deadlines() {
        let (mut sender, receiver) = tls_pair("localhost").unwrap();
        socket2::SockRef::from(&sender.tcp)
            .set_send_buffer_size(1024)
            .unwrap();
        socket2::SockRef::from(&receiver.tcp)
            .set_recv_buffer_size(1024)
            .unwrap();
        let started = Instant::now();
        loop {
            assert!(started.elapsed() < Duration::from_secs(2));
            if sender.idle_write() {
                sender.queue(&vec![7; RECORD_MAX]).unwrap();
            }
            sender.pump(None).unwrap();
            if let Err(e) =
                sender.check_deadlines(Duration::from_millis(25), Duration::from_secs(1))
            {
                assert_eq!(e.kind(), io::ErrorKind::TimedOut);
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        drop(receiver);
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            assert!(Instant::now() < deadline);
            if sender.pump(None).is_err() {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}
