//! Anonymous HTTP probe and ACME webroot. No request or address logging.
use std::fs;
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const MAX_REQUEST: usize = 4096;
const MAX_BODY: u64 = 4096;
const TIMEOUT: Duration = Duration::from_secs(2);
const CONNECTIONS: usize = 8;
const ACCEPTS_PER_MINUTE: u32 = 120;

struct Request {
    stream: TcpStream,
    deadline: Instant,
    input: Vec<u8>,
    output: Vec<u8>,
    written: usize,
}
impl Request {
    fn step(&mut self, webroot: Option<&Path>, now: Instant) -> io::Result<bool> {
        if now >= self.deadline {
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
        })
    }
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.listener.local_addr()
    }
    pub fn poll(&mut self) -> io::Result<()> {
        self.poll_at(Instant::now())
    }
    fn poll_at(&mut self, now: Instant) -> io::Result<()> {
        if now.duration_since(self.window) >= Duration::from_secs(60) {
            self.window = now;
            self.accepts = 0;
        }
        // Bound work per relay tick, even under a continuous connection flood.
        for _ in 0..16 {
            let stream = match self.listener.accept() {
                Ok((stream, _)) => stream,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) => return Err(error),
            };
            if self.accepts >= ACCEPTS_PER_MINUTE || self.requests.len() >= CONNECTIONS {
                continue;
            }
            self.accepts += 1;
            stream.set_nonblocking(true)?;
            self.requests.push(Request {
                stream,
                deadline: now + TIMEOUT,
                input: Vec::new(),
                output: Vec::new(),
                written: 0,
            });
        }
        let webroot = self.webroot.as_deref();
        self.requests
            .retain_mut(|request| matches!(request.step(webroot, now), Ok(false)));
        Ok(())
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
            deadline: now + TIMEOUT,
            input: vec![],
            output: vec![],
            written: 0,
        };
        assert!(!request.step(None, now).unwrap());
        assert!(request.step(None, now + TIMEOUT).unwrap());
    }
    #[test]
    fn connection_and_rate_limits_recover_on_injected_clock() {
        let mut server = ProbeServer::bind("127.0.0.1:0".parse().unwrap(), None).unwrap();
        let now = Instant::now();
        let peers: Vec<_> = (0..CONNECTIONS + 1)
            .map(|_| TcpStream::connect(server.local_addr().unwrap()).unwrap())
            .collect();
        server.poll_at(now).unwrap();
        assert_eq!(server.requests.len(), CONNECTIONS);
        server.poll_at(now + TIMEOUT).unwrap();
        assert!(server.requests.is_empty());
        drop(peers);
        server.accepts = ACCEPTS_PER_MINUTE;
        let _peer = TcpStream::connect(server.local_addr().unwrap()).unwrap();
        server.poll_at(now + TIMEOUT).unwrap();
        assert!(server.requests.is_empty());
        let _peer = TcpStream::connect(server.local_addr().unwrap()).unwrap();
        server.poll_at(now + Duration::from_secs(61)).unwrap();
        assert_eq!(server.requests.len(), 1);
        assert_eq!(server.accepts, 1);
    }
}
