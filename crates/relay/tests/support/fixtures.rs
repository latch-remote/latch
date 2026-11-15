// Shared fixtures for the relay integration test binaries, included textually.
use latch_proto::video::InFrame;
use latch_relay::{
    create_grant,
    daemon::{Daemon, DaemonConfig, Metrics, PilotLimits},
    Cancel, ClientConfig, Endpoint, Event, SessionKeys, Transport,
};
use rustls::pki_types::{pem::PemObject, CertificateDer, PrivateKeyDer};
use std::{
    collections::VecDeque,
    io::{self, Read, Write},
    net::{TcpListener, TcpStream},
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc, Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};


static NEXT: AtomicU64 = AtomicU64::new(0);
struct Pilot {
    config: ClientConfig,
    cancel: Cancel,
    worker: Option<JoinHandle<io::Result<()>>>,
    quota: PathBuf,
    metrics: Arc<Metrics>,
}
fn test_config(port: u16) -> ClientConfig {
    let ca = CertificateDer::from_pem_slice(include_bytes!("../certs/ca.pem")).unwrap();
    let mut roots = rustls::RootCertStore::empty();
    roots.add(ca).unwrap();
    ClientConfig::with_roots(Endpoint::new("localhost", port).unwrap(), roots).unwrap()
}
impl Pilot {
    fn start() -> Self {
        Self::with_limits(PilotLimits::default())
    }
    fn with_limits(limits: PilotLimits) -> Self {
        let cert = CertificateDer::from_pem_slice(include_bytes!("../certs/server.pem")).unwrap();
        let key = PrivateKeyDer::from_pem_slice(include_bytes!("../certs/server-key.pem")).unwrap();
        let quota = std::env::temp_dir().join(format!(
            "latch-relay-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let mut daemon_config = DaemonConfig::new(
            "127.0.0.1:0".parse().unwrap(),
            vec![cert],
            key,
            quota.clone(),
            32 * 1024 * 1024,
        )
        .unwrap();
        daemon_config.initialize_quota = true;
        daemon_config.limits = limits;
        let daemon = Daemon::bind(daemon_config).unwrap();
        let port = daemon.local_addr().unwrap().port();
        let metrics = daemon.metrics();
        let config = test_config(port);
        let cancel = Cancel::new();
        let worker_cancel = cancel.clone();
        let worker = thread::spawn(move || daemon.run(&worker_cancel));
        Self {
            config,
            cancel,
            worker: Some(worker),
            quota,
            metrics,
        }
    }
    fn pair(&self) -> (Transport, Transport) {
        let grant = create_grant(&self.config, &Cancel::new()).unwrap();
        let keys = SessionKeys::generate().unwrap();
        let host = Transport::connect(self.config.clone(), grant.host, keys.clone()).unwrap();
        let viewer = Transport::connect(self.config.clone(), grant.viewer, keys).unwrap();
        ready(&host);
        ready(&viewer);
        (host, viewer)
    }
}
impl Drop for Pilot {
    fn drop(&mut self) {
        self.cancel.cancel();
        self.worker.take().unwrap().join().unwrap().unwrap();
        std::fs::remove_file(&self.quota).unwrap();
    }
}
fn ready(transport: &Transport) {
    let event = transport.recv_timeout(Duration::from_secs(5));
    assert!(
        matches!(event, Some(Event::Ready)),
        "expected Ready, got {event:?}"
    );
}
fn closed(transport: &Transport) -> io::ErrorKind {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match transport.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Some(Event::Closed { kind, .. }) => return kind,
            Some(_) => (),
            None => panic!("transport did not close"),
        }
    }
}
#[derive(Clone, Copy)]
enum GatewayMode {
    // Constructed only by the carrier throughput binary.
    #[allow(dead_code)]
    Delay(Duration),
    HttpAlpnOnly,
    StallNative,
    StallNativeSlow,
    CachedNativeThenStall,
    Connect {
        auth: bool,
        status: u16,
        intercept: bool,
        http_only: bool,
    },
    PlainHttp,
    TlsHttp {
        status: u16,
    },
}
/// A disposable loopback middlebox. All workers stop and join with the fixture.
struct Gateway {
    port: u16,
    recover_native: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    lateness: Arc<Mutex<Vec<Duration>>>,
    worker: Option<JoinHandle<()>>,
}
impl Gateway {
    fn start(destination: u16, mode: GatewayMode) -> Self {
        Self::start_for_endpoint(destination, mode, 443)
    }
    fn start_for_endpoint(destination: u16, mode: GatewayMode, endpoint_port: u16) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let recover_native = Arc::new(AtomicBool::new(false));
        let recovered = recover_native.clone();
        let lateness = Arc::new(Mutex::new(Vec::new()));
        let timings = lateness.clone();
        let worker = thread::spawn(move || {
            let mut workers = Vec::new();
            while !stopped.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((client, _)) => {
                        let stopped = stopped.clone();
                        let recovered = recovered.clone();
                        let timings = timings.clone();
                        workers.push(thread::spawn(move || {
                            let _ = run_gateway_connection(
                                client,
                                destination,
                                mode,
                                endpoint_port,
                                &stopped,
                                &recovered,
                                &timings,
                            );
                        }));
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(1))
                    }
                    Err(e) => panic!("{e}"),
                }
            }
            for worker in workers {
                worker.join().unwrap();
            }
        });
        Self {
            port,
            recover_native,
            stop,
            lateness,
            worker: Some(worker),
        }
    }
    #[allow(dead_code)]
    fn lateness(&self) -> (usize, Duration, Duration, Duration) {
        let mut samples = self.lateness.lock().unwrap().clone();
        samples.sort_unstable();
        if samples.is_empty() {
            return (0, Duration::ZERO, Duration::ZERO, Duration::ZERO);
        }
        let last = samples.len() - 1;
        (samples.len(), samples[last / 2], samples[last * 95 / 100], samples[last])
    }
}
impl Drop for Gateway {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.worker.take().unwrap().join().unwrap();
    }
}
fn run_gateway_connection(
    mut client: TcpStream,
    destination: u16,
    mode: GatewayMode,
    endpoint_port: u16,
    stopped: &AtomicBool,
    recovered: &AtomicBool,
    lateness: &Mutex<Vec<Duration>>,
) -> io::Result<()> {
    client.set_nonblocking(false)?;
    client.set_read_timeout(Some(Duration::from_secs(1)))?;
    client.set_write_timeout(Some(Duration::from_secs(1)))?;
    let changed = recovered.load(Ordering::Acquire);
    let recovered = changed && !matches!(mode, GatewayMode::CachedNativeThenStall);
    let mut http_only = matches!(
        mode,
        GatewayMode::HttpAlpnOnly | GatewayMode::StallNative | GatewayMode::StallNativeSlow
    ) && !recovered
        || matches!(mode, GatewayMode::CachedNativeThenStall) && changed;
    if let GatewayMode::Connect {
        auth,
        status,
        intercept,
        http_only: only,
    } = mode
    {
        let mut header = Vec::new();
        while !header.ends_with(b"\r\n\r\n") {
            let mut byte = [0];
            client.read_exact(&mut byte)?;
            header.extend(byte);
            assert!(header.len() <= 8192);
        }
        let header = String::from_utf8(header).unwrap();
        // The client must ask for the configured endpoint port even though
        // this fixture sends that tunnel to a separate local destination.
        let authority = header
            .lines()
            .next()
            .unwrap()
            .strip_prefix("CONNECT ")
            .unwrap()
            .strip_suffix(" HTTP/1.1")
            .unwrap();
        assert!(authority.ends_with(&format!(":{endpoint_port}")));
        assert!(header.contains(&format!("Host: {authority}\r\n")));
        if auth && !header.contains("Proxy-Authorization: Basic dXNlcjpwYXNz\r\n") {
            client.write_all(b"HTTP/1.1 407 Proxy Authentication Required\r\nProxy-Authenticate: Basic realm=\"test\"\r\nContent-Length: 0\r\n\r\n")?;
            return Ok(());
        }
        if status != 200 {
            client.write_all(
                format!("HTTP/1.1 {status} Refused\r\nContent-Type: text/html\r\nContent-Length: 0\r\n\r\n").as_bytes(),
            )?;
            return Ok(());
        }
        client.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")?;
        if intercept {
            let mut tls = rustls::StreamOwned::new(
                rustls::ServerConnection::new(test_server_tls()).unwrap(),
                client,
            );
            let _ = tls.write_all(b"intercepted");
            let _ = tls.flush();
            return Ok(());
        }
        http_only = only;
    }
    if let GatewayMode::TlsHttp { status } = mode {
        let mut config = (*test_server_tls()).clone();
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        let mut tls = rustls::StreamOwned::new(
            rustls::ServerConnection::new(Arc::new(config)).unwrap(),
            client,
        );
        let mut request = [0; 4096];
        if tls.read(&mut request)? == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        if status == 0 {
            tls.write_all(b"<html>Please sign in</html>")?;
            tls.flush()?;
            return Ok(());
        }
        let content_type = if status == 302 {
            "text/html"
        } else {
            "text/plain"
        };
        tls.write_all(format!("HTTP/1.1 {status} Response\r\nContent-Type: {content_type}\r\nContent-Length: 0\r\n\r\n").as_bytes())?;
        tls.flush()?;
        return Ok(());
    }
    if matches!(mode, GatewayMode::PlainHttp) {
        client.write_all(b"HTTP/1.1 302 Found\r\nLocation: http://login.example/\r\nContent-Type: text/html\r\nContent-Length: 0\r\n\r\n")?;
        // Read the ClientHello before closing to avoid an OS RST
        // obscuring the plaintext response on Windows.
        let mut discard = [0; 4096];
        let _ = client.read(&mut discard);
        return Ok(());
    }
    let mut hello = Vec::new();
    if http_only || recovered {
        let mut record = [0; 5];
        client.read_exact(&mut record)?;
        let len = u16::from_be_bytes(record[3..5].try_into().unwrap()) as usize;
        assert!(len <= 16384);
        hello.extend(record);
        hello.resize(5 + len, 0);
        client.read_exact(&mut hello[5..])?;
        let mut acceptor = rustls::server::Acceptor::default();
        acceptor.read_tls(&mut &hello[..])?;
        let accepted = acceptor.accept().unwrap().unwrap();
        let is_http = accepted
            .client_hello()
            .alpn()
            .is_some_and(|mut alpn| alpn.any(|a| a == b"http/1.1"));
        if recovered && is_http {
            let mut conn = rustls::ServerConnection::new(test_server_tls()).unwrap();
            conn.read_tls(&mut &hello[..])?;
            conn.process_new_packets().map_err(io::Error::other)?;
            let mut tls = rustls::StreamOwned::new(conn, client);
            let mut request = [0; 4096];
            if tls.read(&mut request)? == 0 {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            tls.write_all(
                b"HTTP/1.1 302 Found\r\nContent-Type: text/html\r\nContent-Length: 0\r\n\r\n",
            )?;
            tls.flush()?;
            return Ok(());
        }
        if !is_http && http_only {
            if matches!(
                mode,
                GatewayMode::StallNative
                    | GatewayMode::StallNativeSlow
                    | GatewayMode::CachedNativeThenStall
            ) {
                let until = Instant::now() + Duration::from_secs(1);
                while Instant::now() < until && !stopped.load(Ordering::Acquire) {
                    thread::sleep(Duration::from_millis(2));
                }
            }
            return Ok(());
        }
    }
    let mut server = TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, destination))?;
    let delay = match mode {
        GatewayMode::Delay(delay) => delay,
        GatewayMode::StallNativeSlow => Duration::from_millis(100),
        GatewayMode::CachedNativeThenStall if changed => Duration::from_millis(140),
        _ => Duration::ZERO,
    };
    // Loopback TCP connect itself has no network latency. Charge one simulated
    // TCP handshake RTT before forwarding TLS, plus the ClientHello's one-way
    // delay. Subsequent bursts are scheduled independently in each direction.
    thread::sleep(delay * 3);
    server.write_all(&hello)?;
    server.set_nonblocking(true)?;
    client.set_nonblocking(true)?;
    server.set_nodelay(true)?;
    client.set_nodelay(true)?;
    let mut to_server = DelayedDirection::default();
    let mut to_client = DelayedDirection::default();
    while !stopped.load(Ordering::Acquire) {
        to_server.forward(&client, &server, delay, lateness)?;
        to_client.forward(&server, &client, delay, lateness)?;
        let now = Instant::now();
        // Cancellation is checked at least every 10 ms. Otherwise wake on
        // incoming bytes, write capacity, or the earliest queued deadline.
        let timeout = [to_server.deadline(), to_client.deadline()]
            .into_iter().flatten()
            .map(|deadline| deadline.saturating_duration_since(now))
            .filter(|remaining| !remaining.is_zero())
            .min().unwrap_or(Duration::from_millis(10))
            .min(Duration::from_millis(10));
        let sockets = [
            latch_proto::transport::SocketInterest {
                socket: socket2::SockRef::from(&client),
                readable: to_server.buffered < 1 << 20 && !to_server.eof,
                writable: to_client.due(now),
            },
            latch_proto::transport::SocketInterest {
                socket: socket2::SockRef::from(&server),
                readable: to_client.buffered < 1 << 20 && !to_client.eof,
                writable: to_server.due(now),
            },
        ];
        let sockets: Vec<_> = sockets.into_iter()
            .filter(|socket| socket.readable || socket.writable).collect();
        if sockets.is_empty() {
            thread::sleep(timeout);
        } else {
            latch_proto::transport::wait_sockets(&sockets, timeout)?;
        }
    }
    Ok(())
}

// Each direction has a bounded FIFO. Monotonic read-time-plus-delay deadlines
// preserve ordering and independently model the forward and return path.
#[derive(Default)]
struct DelayedDirection {
    pending: VecDeque<(Instant, Vec<u8>, usize)>,
    buffered: usize,
    eof: bool,
}
impl DelayedDirection {
    fn deadline(&self) -> Option<Instant> {
        self.pending.front().map(|(deadline, _, _)| *deadline)
    }
    fn due(&self, now: Instant) -> bool {
        self.deadline().is_some_and(|deadline| deadline <= now)
    }
    fn forward(
        &mut self,
        mut source: &TcpStream,
        mut destination: &TcpStream,
        delay: Duration,
        lateness: &Mutex<Vec<Duration>>,
    ) -> io::Result<()> {
        while !self.eof && self.buffered < 1 << 20 {
            let mut bytes = vec![0; (64 * 1024).min((1 << 20) - self.buffered)];
            match source.read(&mut bytes) {
                Ok(0) => self.eof = true,
                Ok(n) => {
                    let deadline = Instant::now() + delay;
                    bytes.truncate(n);
                    self.buffered += n;
                    self.pending.push_back((deadline, bytes, 0));
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => return Err(e),
            }
        }
        while let Some((deadline, bytes, offset)) = self.pending.front_mut() {
            if Instant::now() < *deadline {
                break;
            }
            match destination.write(&bytes[*offset..]) {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(n) => {
                    *offset += n;
                    self.buffered -= n;
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => return Err(e),
            }
            if *offset == bytes.len() {
                lateness.lock().unwrap().push(deadline.elapsed());
                self.pending.pop_front();
            }
        }
        if self.eof && self.pending.is_empty() {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        Ok(())
    }
}
fn test_server_tls() -> Arc<rustls::ServerConfig> {
    let cert = CertificateDer::from_pem_slice(include_bytes!("../certs/server.pem")).unwrap();
    let key = PrivateKeyDer::from_pem_slice(include_bytes!("../certs/server-key.pem")).unwrap();
    let mut config =
        rustls::ServerConfig::builder_with_provider(Arc::new(rustls_graviola::default_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![cert], key)
            .unwrap();
    config.alpn_protocols = vec![b"latch-relay/1".to_vec(), b"http/1.1".to_vec()];
    Arc::new(config)
}
