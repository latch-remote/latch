use latch_relay::{
    daemon::{Daemon, DaemonConfig, PilotLimits},
    Cancel,
};
use rustls::pki_types::{pem::PemObject, CertificateDer, PrivateKeyDer};
use std::{
    collections::HashMap,
    io,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::mpsc,
    thread,
    time::Duration,
};

const HELP: &str = "latch-relay --cert certificate-chain.pem --key private-key.pem --quota-file persistent-ledger\n\
  [--listen 127.0.0.1:8443] [--monthly-bytes 10737418240]\n\
  [--limits-json pilot-limits.json] [--initialize-quota]\n\
  [--http-listen [::]:80] [--acme-webroot /var/www/latch-relay]\n\n\
Explicitly select --listen [::]:443 only after TLS, persistent budget storage\n\
and operator limits are configured. Initialize the ledger once. Later starts\n\
require the same retained file. Counters are emitted to stdout every 30 seconds.";
fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}
fn bounded_read(path: &Path, max: u64) -> io::Result<Vec<u8>> {
    use std::io::Read;
    let file = std::fs::File::open(path)?;
    let mut bytes = Vec::new();
    file.take(max + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max {
        return Err(invalid("operator configuration file exceeds bound"));
    }
    Ok(bytes)
}
fn run() -> io::Result<()> {
    let mut args = std::env::args().skip(1);
    let mut values = HashMap::new();
    let mut initialize = false;
    while let Some(arg) = args.next() {
        if arg == "--help" || arg == "-h" {
            println!("{HELP}");
            return Ok(());
        }
        if arg == "--initialize-quota" {
            initialize = true;
            continue;
        }
        if !matches!(
            arg.as_str(),
            "--cert"
                | "--key"
                | "--quota-file"
                | "--listen"
                | "--monthly-bytes"
                | "--limits-json"
                | "--http-listen"
                | "--acme-webroot"
        ) {
            return Err(invalid("unknown relay option, use --help"));
        }
        let value = args
            .next()
            .ok_or_else(|| invalid("missing relay option value"))?;
        if values.insert(arg, value).is_some() {
            return Err(invalid("duplicate relay option"));
        }
    }
    let required = |name: &str| {
        values
            .get(name)
            .ok_or_else(|| invalid("--cert, --key and --quota-file are required"))
    };
    let cert = bounded_read(Path::new(required("--cert")?), 128 * 1024)?;
    let key = bounded_read(Path::new(required("--key")?), 16 * 1024)?;
    let certificates = CertificateDer::pem_slice_iter(&cert)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| invalid("invalid PEM certificate chain"))?;
    let key =
        PrivateKeyDer::from_pem_slice(&key).map_err(|_| invalid("invalid PEM private key"))?;
    let listen: SocketAddr = values
        .get("--listen")
        .map(String::as_str)
        .unwrap_or("127.0.0.1:8443")
        .parse()
        .map_err(|_| invalid("invalid listen address"))?;
    let monthly = values
        .get("--monthly-bytes")
        .map(String::as_str)
        .unwrap_or("10737418240")
        .parse()
        .map_err(|_| invalid("invalid monthly byte budget"))?;
    let mut config = DaemonConfig::new(
        listen,
        certificates,
        key,
        PathBuf::from(required("--quota-file")?),
        monthly,
    )?;
    config.initialize_quota = initialize;
    config.http_listen = values
        .get("--http-listen")
        .map(|value| {
            value
                .parse()
                .map_err(|_| invalid("invalid HTTP listener address"))
        })
        .transpose()?;
    config.acme_webroot = values.get("--acme-webroot").map(PathBuf::from);
    if config.acme_webroot.is_some() && config.http_listen.is_none() {
        return Err(invalid("--acme-webroot requires --http-listen"));
    }
    if let Some(path) = values.get("--limits-json") {
        let limits: PilotLimits =
            serde_json::from_slice(&bounded_read(Path::new(path), 16 * 1024)?)
                .map_err(|_| invalid("invalid pilot limits JSON"))?;
        config.limits = limits;
    }
    let daemon = Daemon::bind(config)?;
    let metrics = daemon.metrics();
    if let Some(address) = daemon.http_addr()? {
        eprintln!("latch-relay HTTP probe on {address}");
    }
    eprintln!("latch-relay listening on {}", daemon.local_addr()?);
    let (stop, stopped) = mpsc::channel();
    let logger = thread::spawn(move || {
        while stopped.recv_timeout(Duration::from_secs(30)).is_err() {
            println!("{}", metrics.snapshot());
        }
    });
    let result = daemon.run(&Cancel::new());
    let _ = stop.send(());
    let _ = logger.join();
    result
}
fn main() {
    if let Err(error) = run() {
        eprintln!("latch-relay stopped: {error}");
        std::process::exit(1);
    }
}
