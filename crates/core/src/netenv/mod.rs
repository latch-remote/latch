//! Local network facts for future transport selection and troubleshooting.
//!
//! [`discover`] blocks only its caller, for at most [`Options::timeout`]
//! (clamped to 100 ms–15 s). Use [`NetworkEnvironment`] from a UI. It runs
//! discovery on workers and caches by a caller-supplied network generation.
//! Increment that generation on route/DNS/proxy changes, Tailscale status
//! changes, after wake, or for an explicit retry.
//! Results are observations, not guarantees that a later connection will work.

mod ip;
#[cfg(any(test, target_os = "linux"))]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
mod portal;
mod proxy;
mod tailscale;
#[cfg(target_os = "windows")]
mod windows;

pub use ip::{IpAvailability, Nat64Prefix};
pub use portal::{CaptivePortal, PortalUrl, ProbeTarget};
pub use proxy::{Credentials, Proxy, ProxyCandidate, ProxyDiscovery, ProxySource};
pub use tailscale::{TailscaleDiagnosis, TailscaleReason};

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

/// Inputs contain no implicit probe destinations. A caller may explicitly
/// choose a relay HTTP endpoint and an optional fallback (see [`ProbeTarget`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    pub destination_host: String,
    /// `direct` or an HTTP proxy URL. Wins even over system bypass rules.
    /// Parse with `Proxy::parse`, which rejects malformed values safely.
    pub proxy_override: Option<Proxy>,
    /// Probe in order, falling back only on unavailable/unexpected responses.
    /// Empty means use native portal signals only.
    pub probes: Vec<ProbeTarget>,
    /// Tailscale node ID or IP. `None` diagnoses the local daemon only.
    pub peer: Option<String>,
    pub timeout: Duration,
}

impl Options {
    pub fn new(destination_host: impl Into<String>) -> Self {
        Self {
            destination_host: destination_host.into(),
            proxy_override: None,
            probes: Vec::new(),
            peer: None,
            timeout: Duration::from_secs(6),
        }
    }
}

#[derive(Debug, Clone)]
pub struct NetworkDiagnosis {
    pub proxies: ProxyDiscovery,
    pub captive_portal: CaptivePortal,
    pub tailscale: TailscaleDiagnosis,
    pub ip: IpAvailability,
    /// Fixed messages only. Raw OS settings, CLI errors and credentials never
    /// enter diagnostics. An incomplete check stays unknown.
    pub warnings: Vec<String>,
}

impl Default for NetworkDiagnosis {
    fn default() -> Self {
        Self {
            proxies: ProxyDiscovery::default(),
            captive_portal: CaptivePortal::Unknown,
            tailscale: TailscaleDiagnosis::unknown(),
            ip: IpAvailability::default(),
            warnings: Vec::new(),
        }
    }
}

enum Observation {
    Proxy(ProxyDiscovery),
    Portal(CaptivePortal),
    Tailscale(TailscaleDiagnosis),
    Ip(IpAvailability),
}

// Resolver and synchronous OS APIs cannot always be cancelled. Bound both
// caller latency and process-wide outstanding work, including timed-out jobs.
static JOBS: AtomicUsize = AtomicUsize::new(0);
fn spawn_job(job: impl FnOnce() + Send + 'static) -> bool {
    if JOBS
        .try_update(Ordering::AcqRel, Ordering::Relaxed, |n| {
            (n < 8).then_some(n + 1)
        })
        .is_err()
    {
        return false;
    }
    struct Permit;
    impl Drop for Permit {
        fn drop(&mut self) {
            JOBS.fetch_sub(1, Ordering::AcqRel);
        }
    }
    let permit = Permit;
    std::thread::Builder::new()
        .name("latch-netenv".into())
        .spawn(move || {
            let _permit = permit;
            job();
        })
        .is_ok()
}

/// Blocking worker entry point. Partial results are returned at the deadline.
/// No connection is made to the HTTPS destination during proxy discovery,
/// apart from OS PAC/WPAD evaluation. See the module docs for probe privacy.
pub fn discover(options: &Options) -> NetworkDiagnosis {
    let deadline = Instant::now()
        + options
            .timeout
            .clamp(Duration::from_millis(100), Duration::from_secs(15));
    let (tx, rx) = mpsc::channel();
    let mut started = 0;
    for kind in 0..4 {
        let tx = tx.clone();
        let options = options.clone();
        if spawn_job(move || {
            let observation = match kind {
                0 => Observation::Proxy(proxy::discover(&options, deadline)),
                1 => Observation::Portal(portal::detect(&options.probes, deadline)),
                2 => Observation::Tailscale(tailscale::discover(options.peer.as_deref(), deadline)),
                _ => Observation::Ip(ip::discover()),
            };
            let _ = tx.send(observation);
        }) {
            started += 1;
        }
    }
    drop(tx);
    collect(rx, started, deadline)
}

fn collect(rx: Receiver<Observation>, started: usize, deadline: Instant) -> NetworkDiagnosis {
    let mut result = NetworkDiagnosis::default();
    let mut received = 0;
    while let Ok(observation) = rx.recv_timeout(deadline.saturating_duration_since(Instant::now()))
    {
        received += 1;
        match observation {
            Observation::Proxy(value) => result.proxies = value,
            Observation::Portal(value) => result.captive_portal = value,
            Observation::Tailscale(value) => result.tailscale = value,
            Observation::Ip(value) => result.ip = value,
        }
    }
    if received != 4 {
        result.warnings.push(
            if started < 4 {
                "Network discovery workers are busy. Try again shortly."
            } else {
                "Some network checks timed out. Try again after connecting to the network."
            }
            .into(),
        );
    }
    result
}

/// UI-safe cache. Call `request` and `poll` from the UI, or `discover` directly
/// on an existing worker. Only one refresh runs per cache. Changed inputs also
/// invalidate the cache. Call `request` again after a pending refresh finishes
/// if the network changed during it. Dropping this cache never waits.
#[derive(Default)]
pub struct NetworkEnvironment {
    cached: Option<(u64, Options, NetworkDiagnosis)>,
    pending: Option<(u64, Options, Receiver<NetworkDiagnosis>)>,
}

impl NetworkEnvironment {
    /// Starts a refresh if needed. Returns false while a refresh is running
    /// or a worker could not be started. A cache hit is true.
    pub fn request(&mut self, network_generation: u64, options: Options) -> bool {
        self.poll();
        if self
            .cached
            .as_ref()
            .is_some_and(|(key, old, _)| *key == network_generation && *old == options)
        {
            return true;
        }
        if self.pending.is_some() {
            return false;
        }
        let (tx, rx) = mpsc::channel();
        let worker_options = options.clone();
        // The coordinator has a deadline and does not consume an OS-job slot.
        if std::thread::Builder::new()
            .name("latch-network-diagnosis".into())
            .spawn(move || {
                let _ = tx.send(discover(&worker_options));
            })
            .is_err()
        {
            return false;
        }
        self.pending = Some((network_generation, options, rx));
        true
    }

    pub fn poll(&mut self) {
        let Some((_, _, rx)) = &self.pending else {
            return;
        };
        match rx.try_recv() {
            Ok(result) => {
                if let Some((key, options, _)) = self.pending.take() {
                    self.cached = Some((key, options, result));
                }
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                self.pending = None;
            }
            Err(mpsc::TryRecvError::Empty) => {}
        }
    }

    /// Only returns a result for these exact inputs, never a stale network.
    pub fn get(&self, network_generation: u64, options: &Options) -> Option<&NetworkDiagnosis> {
        self.cached
            .as_ref()
            .filter(|(key, old, _)| *key == network_generation && old == options)
            .map(|(_, _, result)| result)
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn command(program: &str, args: &[&str], deadline: Instant) -> Option<String> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return None;
    }
    crate::tailscale::run_limited(std::process::Command::new(program).args(args), remaining).ok()
}

#[cfg(test)]
mod tests;
