//! Local network facts for transport selection and troubleshooting.
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
mod warning;
#[cfg(target_os = "windows")]
mod windows;

pub use ip::{IpAvailability, Nat64Prefix};
pub use portal::{CaptivePortal, PortalUrl, ProbeTarget};
pub use proxy::{Credentials, Proxy, ProxyCandidate, ProxyDiscovery, ProxySource};
pub use tailscale::{TailscaleDiagnosis, TailscaleReason};
pub use warning::{NetworkWarning, WarningSeverity};

use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
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
    pub warnings: Vec<NetworkWarning>,
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

#[derive(Clone)]
enum Observation {
    Proxy(ProxyDiscovery),
    Portal(CaptivePortal),
    Tailscale(TailscaleDiagnosis),
    Ip(IpAvailability),
}

// A stalled resolver or synchronous OS call occupies only its own kind.
// Identical inputs subscribe to the existing work. Different inputs remain
// pending, because another host's proxy or probe result must never be reused.
#[derive(Clone, Copy)]
enum CheckKind {
    Proxy,
    Portal,
    Tailscale,
    Ip,
}
impl CheckKind {
    fn same_inputs(self, left: &Options, right: &Options) -> bool {
        match self {
            Self::Proxy => {
                left.destination_host == right.destination_host
                    && left.proxy_override == right.proxy_override
            }
            Self::Portal => left.probes == right.probes,
            Self::Tailscale => left.peer == right.peer,
            Self::Ip => true,
        }
    }
}
struct Flight {
    options: Options,
    subscribers: Vec<(Instant, mpsc::Sender<Observation>)>,
}
#[derive(Default)]
struct JobSlot(Mutex<Option<Flight>>);
impl JobSlot {
    fn subscribe(
        self: &Arc<Self>,
        kind: CheckKind,
        options: &Options,
        tx: mpsc::Sender<Observation>,
        deadline: Instant,
        job: impl FnOnce() -> Observation + Send + 'static,
    ) -> bool {
        let mut flight = self.0.lock().unwrap();
        if let Some(running) = flight.as_mut() {
            if kind.same_inputs(&running.options, options) {
                running
                    .subscribers
                    .retain(|(until, _)| *until > Instant::now());
                running.subscribers.push((deadline, tx));
                return true;
            }
            return false;
        }
        *flight = Some(Flight {
            options: options.clone(),
            subscribers: vec![(deadline, tx)],
        });
        let slot = Arc::clone(self);
        // The guard also releases the slot if an OS wrapper panics.
        struct Completion {
            slot: Arc<JobSlot>,
            finished: bool,
        }
        impl Drop for Completion {
            fn drop(&mut self) {
                if !self.finished {
                    self.slot.0.lock().unwrap().take();
                }
            }
        }
        if std::thread::Builder::new()
            .name("latch-netenv".into())
            .spawn(move || {
                let mut completion = Completion {
                    slot,
                    finished: false,
                };
                let observation = job();
                if let Some(flight) = completion.slot.0.lock().unwrap().take() {
                    for (_, tx) in flight.subscribers {
                        let _ = tx.send(observation.clone());
                    }
                }
                completion.finished = true;
            })
            .is_err()
        {
            flight.take();
            return false;
        }
        true
    }
}
fn slots() -> &'static [Arc<JobSlot>; 4] {
    static SLOTS: std::sync::OnceLock<[Arc<JobSlot>; 4]> = std::sync::OnceLock::new();
    SLOTS.get_or_init(|| std::array::from_fn(|_| Arc::new(JobSlot::default())))
}

/// Blocking worker entry point. Partial results are returned at the deadline.
/// No connection is made to the HTTPS destination during proxy discovery,
/// apart from OS PAC/WPAD evaluation. See the module docs for probe privacy.
pub fn discover(options: &Options) -> NetworkDiagnosis {
    discover_progress(options, None)
}

fn discover_progress(
    options: &Options,
    progress: Option<&mpsc::Sender<NetworkDiagnosis>>,
) -> NetworkDiagnosis {
    let deadline = Instant::now()
        + options
            .timeout
            .clamp(Duration::from_millis(100), Duration::from_secs(15));
    let (tx, rx) = mpsc::channel();
    let mut started = 0;
    for (slot, kind) in slots().iter().zip([
        CheckKind::Proxy,
        CheckKind::Portal,
        CheckKind::Tailscale,
        CheckKind::Ip,
    ]) {
        let worker_options = options.clone();
        if slot.subscribe(kind, options, tx.clone(), deadline, move || match kind {
            CheckKind::Proxy => Observation::Proxy(proxy::discover(&worker_options, deadline)),
            CheckKind::Portal => {
                Observation::Portal(portal::detect(&worker_options.probes, deadline))
            }
            CheckKind::Tailscale => Observation::Tailscale(tailscale::discover(
                worker_options.peer.as_deref(),
                deadline,
            )),
            CheckKind::Ip => Observation::Ip(ip::discover()),
        }) {
            started += 1;
        }
    }
    drop(tx);
    collect_progress(rx, started, deadline, progress)
}

#[cfg(test)]
fn collect(rx: Receiver<Observation>, started: usize, deadline: Instant) -> NetworkDiagnosis {
    collect_progress(rx, started, deadline, None)
}

fn collect_progress(
    rx: Receiver<Observation>,
    started: usize,
    deadline: Instant,
    progress: Option<&mpsc::Sender<NetworkDiagnosis>>,
) -> NetworkDiagnosis {
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
        if let Some(progress) = progress {
            let _ = progress.send(result.clone());
        }
    }
    if received != 4 {
        result.warnings.push(if started < 4 {
            NetworkWarning::ChecksPending
        } else {
            NetworkWarning::ChecksTimedOut
        });
    }
    result
}

/// UI-safe cache. Call `request` and `poll` from the UI, or `discover` directly
/// on an existing worker. Only one refresh runs per cache. Changed inputs also
/// invalidate the cache. The latest request received during a refresh starts
/// automatically from `poll` after that refresh finishes. Dropping never waits.
#[derive(Default)]
pub struct NetworkEnvironment {
    cached: Option<(u64, Options, NetworkDiagnosis)>,
    pending: Option<(u64, Options, Receiver<NetworkDiagnosis>)>,
    queued: Option<(u64, Options)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestStatus {
    Cached,
    Started,
    Pending,
    Unavailable,
}

impl NetworkEnvironment {
    /// Coalesces refreshes and remembers the latest generation and inputs.
    pub fn request(&mut self, network_generation: u64, options: Options) -> RequestStatus {
        // Remember the new request before poll can start a queued generation.
        if self.pending.is_some() {
            self.queued = Some((network_generation, options.clone()));
        }
        self.poll();
        if self
            .cached
            .as_ref()
            .is_some_and(|(key, old, _)| *key == network_generation && *old == options)
        {
            return RequestStatus::Cached;
        }
        if self.pending.is_some() {
            return RequestStatus::Pending;
        }
        self.start(network_generation, options)
    }

    fn start(&mut self, network_generation: u64, options: Options) -> RequestStatus {
        let (tx, rx) = mpsc::channel();
        let worker_options = options.clone();
        // The coordinator has a deadline and does not consume an OS-job slot.
        if std::thread::Builder::new()
            .name("latch-network-diagnosis".into())
            .spawn(move || {
                let result = discover_progress(&worker_options, Some(&tx));
                let _ = tx.send(result);
            })
            .is_err()
        {
            return RequestStatus::Unavailable;
        }
        self.pending = Some((network_generation, options, rx));
        RequestStatus::Started
    }

    pub fn poll(&mut self) {
        if let Some((key, options, rx)) = &self.pending {
            loop {
                match rx.try_recv() {
                    Ok(result) => self.cached = Some((*key, options.clone(), result)),
                    Err(mpsc::TryRecvError::Disconnected) => break,
                    Err(mpsc::TryRecvError::Empty) => return,
                }
            }
            self.pending = None;
        }
        if let Some((key, options)) = self.queued.take() {
            if self.get(key, &options).is_none()
                && self.start(key, options.clone()) == RequestStatus::Unavailable
            {
                self.queued = Some((key, options));
            }
        }
    }

    /// Partial observations remain usable while the remaining checks run.
    pub fn is_pending(&self) -> bool {
        self.pending.is_some() || self.queued.is_some()
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
    crate::tailscale::run_limited(
        std::process::Command::new(command_program(program)).args(args),
        remaining,
    )
    .ok()
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn command_program(program: &str) -> std::path::PathBuf {
    // Prefer the system executable over desktop/session PATH entries.
    let system = std::path::Path::new("/usr/bin").join(program);
    if system.is_file() {
        system
    } else {
        program.into()
    }
}

#[cfg(test)]
mod tests;
