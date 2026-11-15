//! Anonymous HTTP probe and ACME webroot. No request or address logging.
use std::collections::HashMap;
use std::fs;
use std::io::{self, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const MAX_REQUEST: usize = 4096;
const MAX_BODY: u64 = 4096;
const TIMEOUT: Duration = Duration::from_secs(2);
const HEADER_TIMEOUT: Duration = Duration::from_millis(250);
const CONNECTIONS: usize = 8;
const ACCEPTS_PER_MINUTE: u32 = 120;
const PER_SOURCE: u32 = 30;
const ACME_RESERVE: u32 = 16;
const RESERVED_CONNECTIONS: usize = 2;

struct Request {
    stream: TcpStream,
    source: IpAddr,
    acme_only: bool,
    deadline: Instant,
    header_deadline: Instant,
    input: Vec<u8>,
    output: Vec<u8>,
    written: usize,
}
impl Request {
    fn step(
        &mut self,
        webroot: Option<&Path>,
        now: Instant,
        acme_credit: &mut u32,
    ) -> io::Result<bool> {
        if now >= self.deadline || (self.output.is_empty() && now >= self.header_deadline) {
            return Ok(true);
        }
        if self.output.is_empty() {
            let mut bytes = [0; 1024];
            match self.stream.read(&mut bytes) {
                Ok(0) => return Ok(true),
                Ok(size) => self.input.extend_from_slice(&bytes[..size]),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(false),
                Err(error) => return Err(error),
            }
            if self.input.len() > MAX_REQUEST {
                return Ok(true);
            }
            if !self.input.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                return Ok(false);
            }
            self.output = response(&self.input, webroot);
            if self.acme_only {
                // Only an existing, validated challenge can spend reserve credit.
                // A missing/invalid token receives ordinary admission or closes.
                if !self.output.starts_with(b"HTTP/1.1 200 ") || *acme_credit >= ACME_RESERVE {
                    return Ok(true);
                }
                *acme_credit += 1;
            }
        }
        match self.stream.write(&self.output[self.written..]) {
            Ok(0) => Ok(true),
            Ok(size) => {
                self.written += size;
                Ok(self.written == self.output.len())
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(false),
            Err(error) => Err(error),
        }
    }
}

pub(crate) struct ProbeServer {
    listener: TcpListener,
    webroot: Option<PathBuf>,
    requests: Vec<Request>,
    window: Instant,
    accepts: u32,
    sources: HashMap<IpAddr, u32>,
    acme_accepts: u32,
    accept_retry_at: Option<Instant>,
    accept_error_logged: bool,
}
impl ProbeServer {
    pub fn bind(address: SocketAddr, webroot: Option<&Path>) -> io::Result<Self> {
        let socket = socket2::Socket::new(
            socket2::Domain::for_address(address),
            socket2::Type::STREAM,
            Some(socket2::Protocol::TCP),
        )?;
        #[cfg(unix)]
        socket.set_reuse_address(true)?;
        if address.is_ipv6() {
            socket.set_only_v6(false)?;
        }
        socket.bind(&address.into())?;
        socket.listen(16)?;
        let listener: TcpListener = socket.into();
        listener.set_nonblocking(true)?;
        let webroot = webroot.map(fs::canonicalize).transpose()?;
        if webroot.as_ref().is_some_and(|path| !path.is_dir()) {
            return Err(crate::invalid("ACME webroot must be a directory"));
        }
        Ok(Self {
            listener,
            webroot,
            requests: Vec::new(),
            window: Instant::now(),
            accepts: 0,
            sources: HashMap::new(),
            acme_accepts: 0,
            accept_retry_at: None,
            accept_error_logged: false,
        })
    }
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.listener.local_addr()
    }
    pub fn poll(&mut self) {
        self.poll_at(Instant::now())
    }
    fn poll_at(&mut self, now: Instant) {
        if now.duration_since(self.window) >= Duration::from_secs(60) {
            self.window = now;
            self.accepts = 0;
            self.sources.clear();
            self.acme_accepts = 0;
        }
        // Pending requests from one source cannot occupy every slot. The last
        // two slots and separate ACME credits remain available after probe caps.
        if self.accept_retry_at.is_none_or(|at| now >= at) {
            for _ in 0..16 {
                let (stream, address) = match self.listener.accept() {
                    Ok(pair) => pair,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                    Err(error) => {
                        self.accept_error(&error, now);
                        break;
                    }
                };
                self.accept_error_logged = false;
                self.accept_stream(stream, address.ip(), now);
            }
        }
        let webroot = self.webroot.as_deref();
        self.requests.retain_mut(|request| {
            matches!(
                request.step(webroot, now, &mut self.acme_accepts),
                Ok(false)
            )
        });
    }
    fn accept_stream(&mut self, stream: TcpStream, source: IpAddr, now: Instant) {
        let source = crate::daemon::normalize_ip(source);
        if self.requests.len() >= CONNECTIONS
            || self.requests.iter().filter(|r| r.source == source).count() >= 2
            || stream.set_nonblocking(true).is_err()
        {
            return;
        }
        let reserved_slot = self.requests.len() >= CONNECTIONS - RESERVED_CONNECTIONS;
        let acme_only = !self.admit_probe(source) || reserved_slot;
        let mut request = Request {
            stream,
            source,
            acme_only,
            deadline: now + TIMEOUT,
            header_deadline: now + HEADER_TIMEOUT,
            input: Vec::new(),
            output: Vec::new(),
            written: 0,
        };
        let pending = matches!(
            request.step(self.webroot.as_deref(), now, &mut self.acme_accepts),
            Ok(false)
        );
        if pending
            && !(request.output.is_empty()
                && (reserved_slot
                    || self
                        .requests
                        .iter()
                        .any(|r| r.source == source && r.output.is_empty())))
        {
            self.requests.push(request);
        }
    }
    fn admit_probe(&mut self, source: IpAddr) -> bool {
        if self.accepts >= ACCEPTS_PER_MINUTE
            || self.sources.get(&source).copied().unwrap_or(0) >= PER_SOURCE
        {
            return false;
        }
        // At most 120 different normal admissions fit in a window.
        *self.sources.entry(source).or_default() += 1;
        self.accepts += 1;
        true
    }
    fn accept_error(&mut self, error: &io::Error, now: Instant) {
        if !self.accept_error_logged {
            eprintln!("HTTP probe accept failed ({:?}), retrying", error.kind());
            self.accept_error_logged = true;
        }
        self.accept_retry_at = Some(now + Duration::from_secs(1));
    }
}

fn response(request: &[u8], webroot: Option<&Path>) -> Vec<u8> {
    let first = request
        .split(|byte| *byte == b'\n')
        .next()
        .unwrap_or_default();
    let line = std::str::from_utf8(first)
        .unwrap_or_default()
        .trim_end_matches('\r');
    let parts: Vec<_> = line.split(' ').collect();
    let mut status = "404 Not Found";
    let mut body = Vec::new();
    if parts.len() == 3 && parts[0] == "GET" && matches!(parts[2], "HTTP/1.1" | "HTTP/1.0") {
        if parts[1] == "/netenv/probe" {
            status = "204 No Content";
        } else if let Some(bytes) = webroot.and_then(|root| challenge(root, parts[1])) {
            status = "200 OK";
            body = bytes;
        }
    }
    let mut output = format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n", body.len()).into_bytes();
    output.extend(body);
    output
}
fn challenge(root: &Path, target: &str) -> Option<Vec<u8>> {
    let token = target.strip_prefix("/.well-known/acme-challenge/")?;
    if token.is_empty()
        || token.len() > 128
        || !token
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return None;
    }
    let path = root.join(".well-known/acme-challenge").join(token);
    if !fs::symlink_metadata(&path).ok()?.file_type().is_file()
        || !path.canonicalize().ok()?.starts_with(root)
    {
        return None;
    }
    let file = fs::File::open(path).ok()?;
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(MAX_BODY + 1).read_to_end(&mut bytes).ok()?;
    (bytes.len() as u64 <= MAX_BODY).then_some(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn probe_is_empty_anonymous_and_other_paths_are_closed() {
        let response = response(
            b"GET /netenv/probe HTTP/1.1\r\nHost: localhost\r\n\r\n",
            None,
        );
        assert_eq!(response, b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n");
        for path in [
            "/",
            "/netenv/probe?token=x",
            "/.well-known/acme-challenge/../secret",
            "/.well-known/acme-challenge/%2e%2e",
            "/relay",
        ] {
            assert!(
                super::response(format!("GET {path} HTTP/1.1\r\n\r\n").as_bytes(), None)
                    .starts_with(b"HTTP/1.1 404")
            );
        }
    }
    #[test]
    fn challenge_is_bounded_to_regular_files_in_webroot() {
        let root = std::env::temp_dir().join(format!("latch-probe-{}", std::process::id()));
        fs::create_dir_all(root.join(".well-known/acme-challenge")).unwrap();
        let root = root.canonicalize().unwrap();
        fs::write(root.join(".well-known/acme-challenge/token_1"), b"proof").unwrap();
        assert_eq!(
            challenge(&root, "/.well-known/acme-challenge/token_1"),
            Some(b"proof".to_vec())
        );
        fs::write(root.join(".well-known/acme-challenge/large"), vec![0; 4097]).unwrap();
        assert!(challenge(&root, "/.well-known/acme-challenge/large").is_none());
        assert!(challenge(&root, "/.well-known/acme-challenge/../secret").is_none());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn slow_requests_expire_with_injected_time() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let _peer = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (stream, _) = listener.accept().unwrap();
        stream.set_nonblocking(true).unwrap();
        let now = Instant::now();
        let mut request = Request {
            stream,
            source: "127.0.0.1".parse().unwrap(),
            acme_only: false,
            deadline: now + TIMEOUT,
            header_deadline: now + HEADER_TIMEOUT,
            input: vec![],
            output: vec![],
            written: 0,
        };
        assert!(!request.step(None, now, &mut 0).unwrap());
        assert!(request.step(None, now + HEADER_TIMEOUT, &mut 0).unwrap());
        assert!(request.step(None, now + TIMEOUT, &mut 0).unwrap());
    }
    fn exchange(server: &mut ProbeServer, path: &str, now: Instant) -> String {
        let mut peer = TcpStream::connect(server.local_addr().unwrap()).unwrap();
        peer.write_all(format!("GET {path} HTTP/1.1\r\n\r\n").as_bytes())
            .unwrap();
        peer.set_nonblocking(true).unwrap();
        let deadline = Instant::now() + TIMEOUT;
        let mut bytes = Vec::new();
        loop {
            server.poll_at(now);
            let mut chunk = [0; 1024];
            match peer.read(&mut chunk) {
                Ok(0) => break,
                Ok(size) => bytes.extend_from_slice(&chunk[..size]),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => panic!("{error}"),
            }
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        String::from_utf8(bytes).unwrap()
    }
    #[test]
    fn flooding_source_preserves_other_sources_and_acme_reserve() {
        let root = std::env::temp_dir().join(format!("latch-probe-flood-{}", std::process::id()));
        fs::create_dir_all(root.join(".well-known/acme-challenge")).unwrap();
        fs::write(root.join(".well-known/acme-challenge/token"), b"proof").unwrap();
        let mut server = ProbeServer::bind("127.0.0.1:0".parse().unwrap(), Some(&root)).unwrap();
        let flood = "192.0.2.1".parse().unwrap();
        for _ in 0..PER_SOURCE {
            assert!(server.admit_probe(flood));
        }
        for _ in 0..200 {
            assert!(!server.admit_probe(flood));
        }
        assert_eq!(server.accepts, PER_SOURCE);
        let now = Instant::now();
        assert!(exchange(&mut server, "/netenv/probe", now).starts_with("HTTP/1.1 204"));
        assert_eq!(server.accepts, PER_SOURCE + 1);
        // Even a global probe flood cannot spend the challenge reserve.
        server.accepts = ACCEPTS_PER_MINUTE;
        assert!(exchange(&mut server, "/netenv/probe", now).is_empty());
        assert_eq!(server.acme_accepts, 0);
        for _ in 0..ACME_RESERVE * 2 {
            assert!(exchange(&mut server, "/.well-known/acme-challenge/missing", now).is_empty());
        }
        assert_eq!(server.acme_accepts, 0);
        let reply = exchange(&mut server, "/.well-known/acme-challenge/token", now);
        assert!(reply.starts_with("HTTP/1.1 200"));
        assert!(reply.ends_with("proof"));
        assert_eq!(server.acme_accepts, 1);
        server.poll_at(now + Duration::from_secs(61));
        assert_eq!(server.accepts, 0);
        assert!(server.sources.is_empty());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn connection_and_rate_limits_recover_on_injected_clock() {
        let mut server = ProbeServer::bind("127.0.0.1:0".parse().unwrap(), None).unwrap();
        let now = Instant::now();
        let peers: Vec<_> = (0..CONNECTIONS + 1)
            .map(|_| TcpStream::connect(server.local_addr().unwrap()).unwrap())
            .collect();
        server.poll_at(now);
        assert_eq!(server.requests.len(), 1, "one source has one idle slot");
        server.poll_at(now + TIMEOUT);
        assert!(server.requests.is_empty());
        drop(peers);
        server.accepts = ACCEPTS_PER_MINUTE;
        let mut rejected = TcpStream::connect(server.local_addr().unwrap()).unwrap();
        rejected
            .write_all(b"GET /netenv/probe HTTP/1.1\r\n\r\n")
            .unwrap();
        rejected.set_nonblocking(true).unwrap();
        let deadline = Instant::now() + TIMEOUT;
        loop {
            server.poll_at(now + TIMEOUT);
            if matches!(rejected.read(&mut [0]), Ok(0)) {
                break;
            }
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        assert!(server.requests.is_empty());
        assert_eq!(server.acme_accepts, 0);
        let _peer = TcpStream::connect(server.local_addr().unwrap()).unwrap();
        let deadline = Instant::now() + TIMEOUT;
        while server.requests.is_empty() && Instant::now() < deadline {
            server.poll_at(now + Duration::from_secs(61));
            std::thread::yield_now();
        }
        assert_eq!(server.requests.len(), 1);
        assert_eq!(server.accepts, 1);
    }
    fn socket_pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let peer = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (stream, _) = listener.accept().unwrap();
        (stream, peer)
    }
    #[test]
    fn distinct_sources_cannot_fill_reserved_slots_with_idle_headers() {
        let root =
            std::env::temp_dir().join(format!("latch-reserved-slots-{}", std::process::id()));
        fs::create_dir_all(root.join(".well-known/acme-challenge")).unwrap();
        fs::write(root.join(".well-known/acme-challenge/token"), b"proof").unwrap();
        let mut server = ProbeServer::bind("127.0.0.1:0".parse().unwrap(), Some(&root)).unwrap();
        let now = Instant::now();
        let mut peers = Vec::new();
        for source in 1..=CONNECTIONS {
            let (stream, peer) = socket_pair();
            peers.push(peer);
            server.accept_stream(stream, IpAddr::from([192, 0, 2, source as u8]), now);
        }
        assert_eq!(server.requests.len(), CONNECTIONS - RESERVED_CONNECTIONS);
        let (stream, mut peer) = socket_pair();
        peer.write_all(b"GET /.well-known/acme-challenge/token HTTP/1.1\r\n\r\n")
            .unwrap();
        // Inject the accepted source to exercise the real admission path using
        // distinct IPv4 and IPv6 keys, independent of loopback routing support.
        stream.set_read_timeout(Some(TIMEOUT)).unwrap();
        assert_eq!(stream.peek(&mut [0]).unwrap(), 1);
        server.accept_stream(stream, "2001:db8:9::1".parse().unwrap(), now);
        let mut reply = String::new();
        peer.set_read_timeout(Some(TIMEOUT)).unwrap();
        peer.read_to_string(&mut reply).unwrap();
        assert!(reply.starts_with("HTTP/1.1 200"));
        assert!(reply.ends_with("proof"));
        assert_eq!(server.acme_accepts, 1);
        server.poll_at(now + HEADER_TIMEOUT);
        assert!(server.requests.is_empty());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn global_cap_includes_active_reserved_responses_from_distinct_sources() {
        let root = std::env::temp_dir().join(format!("latch-global-slots-{}", std::process::id()));
        fs::create_dir_all(root.join(".well-known/acme-challenge")).unwrap();
        fs::write(root.join(".well-known/acme-challenge/token"), b"proof").unwrap();
        let mut server = ProbeServer::bind("127.0.0.1:0".parse().unwrap(), Some(&root)).unwrap();
        let now = Instant::now();
        let mut peers = Vec::new();
        for source in 1..=CONNECTIONS {
            let (stream, peer) = socket_pair();
            peers.push(peer);
            // Model admitted responses whose nonblocking writes have not drained.
            // Their buffers and sockets stay within the production bounds.
            server.requests.push(Request {
                stream,
                source: IpAddr::from([192, 0, 2, source as u8]),
                acme_only: source > CONNECTIONS - RESERVED_CONNECTIONS,
                deadline: now + TIMEOUT,
                header_deadline: now + HEADER_TIMEOUT,
                input: vec![],
                output: b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n".to_vec(),
                written: 0,
            });
        }
        let (stream, mut peer) = socket_pair();
        peer.write_all(b"GET /.well-known/acme-challenge/token HTTP/1.1\r\n\r\n")
            .unwrap();
        stream.set_read_timeout(Some(TIMEOUT)).unwrap();
        assert_eq!(stream.peek(&mut [0]).unwrap(), 1);
        server.accept_stream(stream, "2001:db8:10::1".parse().unwrap(), now);
        assert_eq!(server.requests.len(), CONNECTIONS);
        assert_eq!(
            server
                .requests
                .iter()
                .filter(|request| request.acme_only)
                .count(),
            RESERVED_CONNECTIONS
        );
        peer.set_read_timeout(Some(TIMEOUT)).unwrap();
        match peer.read(&mut [0]) {
            Ok(0) => {}
            Err(error) if error.kind() == io::ErrorKind::ConnectionReset => {}
            result => panic!("global cap must reject a ready challenge: {result:?}"),
        }
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn accept_error_backs_off_without_stopping_service() {
        let mut server = ProbeServer::bind("127.0.0.1:0".parse().unwrap(), None).unwrap();
        let now = Instant::now();
        server.accept_error(&io::Error::from(io::ErrorKind::Other), now);
        assert_eq!(server.accept_retry_at, Some(now + Duration::from_secs(1)));
        assert!(server.accept_error_logged);
        let _peer = TcpStream::connect(server.local_addr().unwrap()).unwrap();
        server.poll_at(now);
        assert!(server.requests.is_empty());
        let deadline = Instant::now() + TIMEOUT;
        while server.requests.is_empty() && Instant::now() < deadline {
            server.poll_at(now + Duration::from_secs(1));
            std::thread::yield_now();
        }
        assert_eq!(server.requests.len(), 1);
    }
}
