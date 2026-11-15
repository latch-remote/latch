use latch_relay::{
    daemon::{Daemon, DaemonConfig},
    Cancel,
};
use rustls::pki_types::{pem::PemObject, CertificateDer, PrivateKeyDer};
use std::{
    fs,
    io::{self, Read, Write},
    net::{Ipv4Addr, SocketAddr, TcpStream},
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

static NEXT: AtomicU64 = AtomicU64::new(0);

struct HttpDaemon {
    address: SocketAddr,
    root: PathBuf,
    cancel: Cancel,
    worker: Option<JoinHandle<io::Result<()>>>,
}
impl HttpDaemon {
    fn start() -> Self {
        let root = std::env::temp_dir().join(format!(
            "latch-http-readiness-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(root.join(".well-known/acme-challenge")).unwrap();
        fs::write(
            root.join(".well-known/acme-challenge/token"),
            b"challenge-proof",
        )
        .unwrap();
        let mut config = DaemonConfig::new(
            (Ipv4Addr::LOCALHOST, 0).into(),
            vec![CertificateDer::from_pem_slice(include_bytes!("certs/server.pem")).unwrap()],
            PrivateKeyDer::from_pem_slice(include_bytes!("certs/server-key.pem")).unwrap(),
            root.join("quota"),
            16 * 1024 * 1024,
        )
        .unwrap();
        config.initialize_quota = true;
        config.http_listen = Some((Ipv4Addr::LOCALHOST, 0).into());
        config.acme_webroot = Some(root.clone());
        let daemon = Daemon::bind(config).unwrap();
        let address = daemon.http_addr().unwrap().unwrap();
        let cancel = Cancel::new();
        let worker_cancel = cancel.clone();
        let worker = thread::spawn(move || daemon.run(&worker_cancel));
        // Let the relay enter its idle readiness wait before any HTTP traffic.
        thread::sleep(Duration::from_millis(20));
        Self {
            address,
            root,
            cancel,
            worker: Some(worker),
        }
    }
    fn connect(&self) -> TcpStream {
        let peer = TcpStream::connect(self.address).unwrap();
        peer.set_nodelay(true).unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        peer.set_write_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        peer
    }
}
impl Drop for HttpDaemon {
    fn drop(&mut self) {
        self.cancel.cancel();
        self.worker.take().unwrap().join().unwrap().unwrap();
        fs::remove_dir_all(&self.root).unwrap();
    }
}

#[test]
fn idle_daemon_serves_probe_after_accept_and_partial_header_waits() {
    let daemon = HttpDaemon::start();
    let start = Instant::now();
    let mut peer = daemon.connect();
    peer.write_all(b"GET /netenv/probe HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .unwrap();
    let mut response = String::new();
    peer.read_to_string(&mut response).unwrap();
    let accepted = start.elapsed();
    assert!(response.starts_with("HTTP/1.1 204 No Content\r\n"));

    let mut peer = daemon.connect();
    peer.write_all(b"GET /netenv/probe HTTP/1.1\r\n").unwrap();
    thread::sleep(Duration::from_millis(20));
    let start = Instant::now();
    peer.write_all(b"Host: localhost\r\n\r\n").unwrap();
    response.clear();
    peer.read_to_string(&mut response).unwrap();
    let readied = start.elapsed();
    eprintln!(
        "idle HTTP probe accept-to-response={accepted:?}, header-ready-to-response={readied:?}"
    );
    assert!(response.starts_with("HTTP/1.1 204 No Content\r\n"));
    // Allow hosted-runner scheduling, with a bound below the header deadline
    // and far below the empty relay's otherwise 60-second wait.
    assert!(accepted < Duration::from_millis(100), "{accepted:?}");
    assert!(readied < Duration::from_millis(100), "{readied:?}");
}

#[test]
fn idle_daemon_expires_silent_http_headers_without_other_traffic() {
    let daemon = HttpDaemon::start();
    let start = Instant::now();
    let mut peer = daemon.connect();
    match peer.read(&mut [0]) {
        Ok(0) => (),
        Err(error) if error.kind() == io::ErrorKind::ConnectionReset => (),
        result => panic!("silent HTTP request did not close: {result:?}"),
    }
    let elapsed = start.elapsed();
    eprintln!("idle HTTP header expiry={elapsed:?}");
    let header_deadline = Duration::from_millis(1500);
    assert!(elapsed >= header_deadline, "{elapsed:?}");
    // Preserve the original 250-ms scheduling tolerance above the deadline.
    let overdue = elapsed - header_deadline;
    assert!(overdue < Duration::from_millis(250), "{elapsed:?}");
}

#[test]
fn delayed_loopback_headers_model_600_ms_rtt_and_a_lost_segment() {
    let daemon = HttpDaemon::start();
    // The request takes 1.3 seconds of real sleeps against the 1.5-second
    // header deadline. A loaded machine can oversleep past the deadline, and
    // the daemon then rightly closes the connection. Such an attempt did not
    // exercise the case, so it is repeated. An attempt whose headers were
    // complete in time must be answered, and a connection that fails before
    // the deadline is a failure.
    let on_time = Duration::from_millis(1400);
    for attempt in 1..=5 {
        // Take the start before connecting, so the daemon cannot have
        // accepted the connection any earlier than this.
        let start = Instant::now();
        let mut peer = daemon.connect();
        // Inject one-way travel after acceptance and a one-second initial RTO.
        thread::sleep(Duration::from_millis(300));
        let first = peer.write_all(b"GET /netenv/probe HTTP/1.1\r\n");
        thread::sleep(Duration::from_secs(1));
        let ready = Instant::now();
        let mut response = String::new();
        let result = first
            .and_then(|()| peer.write_all(b"Host: localhost\r\n\r\n"))
            .and_then(|()| {
                let sent = start.elapsed();
                peer.read_to_string(&mut response).map(|_| sent)
            });
        let readied = ready.elapsed();
        let ended = start.elapsed();
        eprintln!(
            "delayed HTTP request attempt {attempt} ended after {ended:?}, \
             header-ready-to-response={readied:?}, result={result:?}"
        );
        let answered = response.starts_with("HTTP/1.1 204 No Content\r\n");
        // Headers completed, or a failure seen, only after the allowance.
        let late = *result.as_ref().unwrap_or(&ended) > on_time;
        if late && !answered {
            continue;
        }
        result.unwrap();
        assert!(answered, "{response:?}");
        assert!(readied < Duration::from_millis(100), "{readied:?}");
        return;
    }
    panic!("every attempt overslept the header deadline");
}

#[test]
fn idle_daemon_serves_acme_challenge() {
    let daemon = HttpDaemon::start();
    let start = Instant::now();
    let mut peer = daemon.connect();
    peer.write_all(b"GET /.well-known/acme-challenge/token HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .unwrap();
    let mut response = String::new();
    peer.read_to_string(&mut response).unwrap();
    let elapsed = start.elapsed();
    eprintln!("idle ACME response={elapsed:?}");
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"));
    assert!(response.ends_with("challenge-proof"));
    assert!(elapsed < Duration::from_millis(100), "{elapsed:?}");
}
