//! Restrictive-network discovery shared by the product and measurement examples.
//! Discovery never waits on a session/UI thread. Partial proxy results can be used
//! while PAC, portal and Tailscale checks continue under their own deadlines.
use latch_core::netenv::{self, CaptivePortal, NetworkDiagnosis, NetworkEnvironment, Proxy};
use latch_relay::{BasicCredentials, Carrier, ConnectionInfo, Failure, ProxyConfig};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::net::UdpSocket;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

const RETAINED_PROXY_MAX_AGE: Duration = Duration::from_secs(60);
const PORTAL_OPEN_CONFIRMATION: Duration = Duration::from_secs(1);

struct Entry {
    host: String,
    environment: NetworkEnvironment,
    latest: NetworkDiagnosis,
    checked: Instant,
    network_key: u64,
    generation: u64,
    refresh_at: Instant,
    portal_backoff: Duration,
    portal_open_since: Option<(u64, Instant)>,
    retained_proxy_since: Option<Instant>,
    retry_requested: bool,
    media_usable: bool,
}

impl Entry {
    fn should_refresh(&self, now: Instant, pending: bool) -> bool {
        refresh_due(now, self.refresh_at, pending)
            && (self.retry_requested
                || matches!(self.latest.captive_portal, CaptivePortal::Captive { .. }))
    }
    fn observe(&mut self, mut latest: NetworkDiagnosis, now: Instant) {
        let incomplete_proxy = proxy_observation_incomplete(&latest);
        let portal_unknown = latest.captive_portal == CaptivePortal::Unknown;
        merge_observations(&mut latest, &self.latest, self.media_usable);
        if incomplete_proxy {
            let since = self.retained_proxy_since.get_or_insert(now);
            if now.saturating_duration_since(*since) >= RETAINED_PROXY_MAX_AGE {
                latest.proxies.candidates.clear();
            }
        } else {
            self.retained_proxy_since = None;
        }
        if !self.media_usable
            && matches!(self.latest.captive_portal, CaptivePortal::Captive { .. })
            && latest.captive_portal == CaptivePortal::OpenInternet
        {
            let (generation, since) = *self.portal_open_since.get_or_insert_with(|| {
                self.refresh_at = self.refresh_at.min(now + PORTAL_OPEN_CONFIRMATION);
                (self.generation, now)
            });
            if self.generation == generation
                || now.saturating_duration_since(since) < PORTAL_OPEN_CONFIRMATION
            {
                latest.captive_portal = self.latest.captive_portal.clone();
            }
        } else if !portal_unknown {
            self.portal_open_since = None;
        }
        if matches!(latest.captive_portal, CaptivePortal::Captive { .. })
            && !matches!(self.latest.captive_portal, CaptivePortal::Captive { .. })
        {
            self.refresh_at = now + self.portal_backoff;
        }
        if !matches!(latest.captive_portal, CaptivePortal::Captive { .. }) {
            self.portal_backoff = Duration::from_secs(1);
        }
        self.latest = latest;
    }

    fn request_retry(&mut self, now: Instant, urgent: bool) {
        self.retry_requested = true;
        if urgent {
            self.portal_backoff = Duration::from_secs(1);
        }
        self.refresh_at = self.refresh_at.min(
            now + if urgent {
                Duration::ZERO
            } else {
                Duration::from_secs(2)
            },
        );
    }

    fn refresh(&mut self, now: Instant, pending: bool) {
        if !self.should_refresh(now, pending) {
            return;
        }
        self.generation = self.generation.wrapping_add(1);
        let interval = if matches!(self.latest.captive_portal, CaptivePortal::Captive { .. }) {
            let interval = self.portal_backoff;
            self.portal_backoff = (self.portal_backoff * 2).min(Duration::from_secs(4));
            interval
        } else {
            self.portal_backoff = Duration::from_secs(1);
            Duration::from_secs(2)
        };
        self.retry_requested = false;
        self.refresh_at = now + interval;
    }
}

static CACHE: OnceLock<Mutex<Vec<Entry>>> = OnceLock::new();

/// Request a fresh observation after a relay connection fails. Repeated failures
/// coalesce and respect the discovery backoff.
pub fn relay_failed(host: &str) {
    if let Some(cache) = CACHE.get() {
        if let Some(entry) = cache
            .lock()
            .unwrap()
            .iter_mut()
            .find(|entry| entry.host == host)
        {
            entry.request_retry(Instant::now(), false);
            entry.media_usable = false;
        }
    }
}

/// Recheck promptly after opening sign-in or returning to the app. Only a
/// confirmed portal is invalidated, so healthy sessions still remain idle.
pub fn recheck_portals() {
    if let Some(cache) = CACHE.get() {
        for entry in cache.lock().unwrap().iter_mut() {
            if matches!(entry.latest.captive_portal, CaptivePortal::Captive { .. }) {
                entry.request_retry(Instant::now(), true);
            }
        }
    }
}

/// Delivered media proves this local network can carry the session, even when
/// an HTTP filter keeps redirecting the probe. Stop portal polling on that key.
pub fn media_received(host: &str) {
    if let Some(cache) = CACHE.get() {
        if let Some(entry) = cache
            .lock()
            .unwrap()
            .iter_mut()
            .find(|entry| entry.host == host)
        {
            entry.media_usable = true;
            entry.latest.captive_portal = CaptivePortal::OpenInternet;
        }
    }
}

fn merge_observations(latest: &mut NetworkDiagnosis, previous: &NetworkDiagnosis, usable: bool) {
    if usable {
        latest.captive_portal = CaptivePortal::OpenInternet;
    } else if latest.captive_portal == CaptivePortal::Unknown {
        latest.captive_portal = previous.captive_portal.clone();
    }
    if proxy_observation_incomplete(latest) {
        latest.proxies = previous.proxies.clone();
    }
}

fn proxy_observation_incomplete(latest: &NetworkDiagnosis) -> bool {
    // Only the proxy check's own warnings make its empty result incomplete.
    // Another check timing out says nothing about the proxy settings.
    !latest.proxy_observed
        || (latest.proxies.candidates.is_empty()
            && latest.proxies.warnings.iter().any(|warning| {
                !matches!(
                    warning,
                    netenv::NetworkWarning::WindowsServiceSettings
                        | netenv::NetworkWarning::MacStaticFallback
                )
            }))
}

/// Local route/environment changes invalidate immediately. Healthy sessions use
/// the completed snapshot until a change or relay failure. Confirmed portals
/// retry with exponential backoff. At most eight destinations are retained.
pub fn diagnosis(host: &str) -> NetworkDiagnosis {
    if latch_relay::Endpoint::new(host, 443).is_err() {
        return NetworkDiagnosis::default();
    }
    let mut cache = CACHE.get_or_init(Mutex::default).lock().unwrap();
    if let Some(entry) = cache.iter().find(|entry| entry.host == host) {
        if entry.checked.elapsed() < Duration::from_millis(250) {
            return entry.latest.clone();
        }
    }
    let mut options = netenv::Options::new(host);
    options.timeout = Duration::from_secs(6);
    if !cfg!(test) && std::env::var("LATCH_NETENV_PROBE").as_deref() != Ok("0") {
        if let Ok(probe) =
            netenv::ProbeTarget::new(&format!("http://{host}/netenv/probe"), 204, b"")
        {
            options.probes.push(probe);
        }
    }
    let mut fingerprint = DefaultHasher::new();
    for key in [
        "HTTPS_PROXY",
        "https_proxy",
        "HTTP_PROXY",
        "http_proxy",
        "ALL_PROXY",
        "all_proxy",
        "NO_PROXY",
        "no_proxy",
    ] {
        std::env::var_os(key).hash(&mut fingerprint);
    }
    for (bind, target) in [("0.0.0.0:0", "192.0.2.1:9"), ("[::]:0", "[2001:db8::1]:9")] {
        let source = UdpSocket::bind(bind).ok().and_then(|socket| {
            socket.connect(target).ok()?;
            socket.local_addr().ok().map(|address| address.ip())
        });
        source.hash(&mut fingerprint);
    }
    let network_key = fingerprint.finish();
    let now = Instant::now();
    if !cache.iter().any(|entry| entry.host == host) {
        if cache.len() == 8 {
            cache.remove(0);
        }
        cache.push(Entry {
            host: host.into(),
            environment: NetworkEnvironment::default(),
            latest: NetworkDiagnosis::default(),
            checked: now,
            network_key,
            generation: 0,
            refresh_at: now + Duration::from_secs(5),
            portal_backoff: Duration::from_secs(1),
            portal_open_since: None,
            retained_proxy_since: None,
            retry_requested: false,
            media_usable: false,
        });
    }
    let entry = cache.iter_mut().find(|entry| entry.host == host).unwrap();
    entry.checked = now;
    entry.environment.poll();
    if entry.network_key != network_key {
        entry.network_key = network_key;
        entry.generation = entry.generation.wrapping_add(1);
        entry.refresh_at = now + Duration::from_secs(5);
        entry.portal_backoff = Duration::from_secs(1);
        entry.portal_open_since = None;
        entry.retained_proxy_since = None;
        entry.retry_requested = false;
        entry.latest = NetworkDiagnosis::default();
        entry.media_usable = false;
    } else {
        if let Some(latest) = entry.environment.get(entry.generation, &options).cloned() {
            entry.observe(latest, now);
        }
        // Never discard a slow PAC result just because its refresh interval
        // elapsed. Consume the completed snapshot before starting a refresh.
        entry.refresh(now, entry.environment.is_pending());
    }
    entry.environment.request(entry.generation, options);
    entry.latest.clone()
}

fn refresh_due(now: Instant, refresh_at: Instant, pending: bool) -> bool {
    now >= refresh_at && !pending
}

pub fn proxy_selection(diagnosis: &NetworkDiagnosis) -> (Option<ProxyConfig>, bool) {
    let direct = diagnosis.proxies.candidates.is_empty()
        || diagnosis
            .proxies
            .candidates
            .iter()
            .any(|c| c.proxy == Proxy::Direct);
    let proxy = diagnosis.proxies.candidates.iter().find_map(|candidate| {
        let Proxy::Http {
            host,
            port,
            credentials,
        } = &candidate.proxy
        else {
            return None;
        };
        let mut proxy = ProxyConfig::new(host, *port).ok()?;
        proxy.credentials = credentials.as_ref().and_then(|credentials| {
            BasicCredentials::new(credentials.username(), credentials.password()).ok()
        });
        Some(proxy)
    });
    (proxy, direct)
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NetworkStatus {
    pub messages: Vec<String>,
    pub needs_sign_in: bool,
    pub proxy_available: bool,
    pub portal_url: Option<netenv::PortalUrl>,
}

impl NetworkStatus {
    pub(crate) fn recovered_from(&self, previous: &Self) -> bool {
        (previous.needs_sign_in && !self.needs_sign_in)
            || (!previous.proxy_available && self.proxy_available)
    }

    pub(crate) fn sign_in_timed_out(&self, since: &mut Option<Instant>, now: Instant) -> bool {
        if !self.needs_sign_in {
            *since = None;
            return false;
        }
        now.saturating_duration_since(*since.get_or_insert(now)) >= Duration::from_secs(180)
    }

    pub(crate) fn media_arrived(&mut self, arrived: bool) {
        if arrived {
            self.needs_sign_in = false;
            self.portal_url = None;
            self.messages
                .retain(|message| !message.starts_with("This network needs a sign-in."));
        }
    }
}

/// Only positive evidence produces a message. Missing/404 HTTP probes from older
/// relays and unavailable OS checks are silent. A successful carrier clears old
/// relay failures, even when UDP is still being retried in the background.
pub fn status(
    diagnosis: &NetworkDiagnosis,
    failure: Option<&Failure>,
    info: Option<&ConnectionInfo>,
) -> NetworkStatus {
    let mut result = NetworkStatus {
        proxy_available: proxy_selection(diagnosis).0.is_some(),
        ..Default::default()
    };
    if let CaptivePortal::Captive { portal_url } = &diagnosis.captive_portal {
        result.needs_sign_in = true;
        result.portal_url = portal_url.clone();
        result.messages.push(match portal_url {
            Some(url) => format!("This network needs a sign-in. Open {url}"),
            None => "This network needs a sign-in. Open this network's sign-in page.".into(),
        });
    }
    if info.is_none() {
        match failure {
            Some(Failure::TlsInterception { issuer }) => result.messages.push(format!("This network inspects encrypted traffic. Latch refuses the substituted certificate (issuer {issuer:.128}).")),
            Some(Failure::TlsRejected) => result.messages.push("Latch could not verify the relay's encrypted connection. Check this network's certificate policy.".into()),
            Some(Failure::CaptivePortal) if result.messages.is_empty() && diagnosis.captive_portal != CaptivePortal::OpenInternet => {
                result.needs_sign_in = true;
                result.messages.push("This network needs a sign-in. Open this network's sign-in page.".into());
            },
            Some(Failure::ProxyAuthenticationRequired) => result.messages.push("Proxy authentication required. This network's CONNECT proxy needs usable credentials in the system or environment proxy settings.".into()),
            Some(Failure::ProxyRefused { .. }) => result.messages.push("Proxy required. This network's proxy refused the CONNECT tunnel to the relay.".into()),
            Some(Failure::ConnectionBlocked | Failure::ClosedBeforeHandshake | Failure::Timeout)
                if proxy_selection(diagnosis).0.is_some() => result.messages.push("Proxy required. This network's CONNECT proxy did not respond.".into()),
            Some(Failure::ConnectionBlocked | Failure::ClosedBeforeHandshake | Failure::Timeout)
                if diagnosis.proxies.warnings.iter().any(|warning| matches!(warning.code(), "linux_pac_unavailable" | "pac_failed" | "wpad_failed" | "pac_evaluator_unavailable" | "incomplete_system_proxy")) => result.messages.push("Proxy required. Automatic proxy settings could not be resolved. Check this network's proxy settings.".into()),
            _ => {}
        }
    }
    if let Some(info) = info {
        if info.carrier == Carrier::WebSocket {
            result.messages.push("Using the WebSocket carrier because this network blocked native relay TLS (HTTPS-only network).".into());
        }
        if info.via_proxy {
            result
                .messages
                .push("Proxy required. Using a CONNECT tunnel via proxy.".into());
        }
    }
    use netenv::TailscaleReason;
    match diagnosis.tailscale.reason {
        TailscaleReason::ControlUnreachable | TailscaleReason::DerpUnreachable => {
            result.messages.push(
                "Tailscale is blocked on this network. Check this network's access policy.".into(),
            )
        }
        TailscaleReason::DerpOnly => result.messages.push(
            "Tailscale is using DERP only. This network blocks direct Tailscale connections."
                .into(),
        ),
        _ => {}
    }
    result
}

/// Version-1 Notice text for the remote viewer. Host-local URLs, issuer names
/// and credentials stay in the host log. Existing viewers display plain text.
pub fn host_notice(
    status: &NetworkStatus,
    failure: Option<&Failure>,
) -> Option<latch_proto::control::Notice> {
    let message = match failure {
        Some(Failure::TlsInterception { .. }) => "The host's network is intercepting the relay's encrypted connection. Ask the host's network administrator to allow the relay's genuine certificate.",
        Some(Failure::ProxyRefused { .. }) => "The host's network proxy refused the relay connection. Ask the host's network administrator to allow CONNECT to the relay on port 443.",
        _ if status.needs_sign_in => "The host's network needs a sign-in. Complete network sign-in on the host, then reconnect.",
        _ => return None,
    };
    Some(latch_proto::control::Notice {
        kind: "network".into(),
        message: message.into(),
    })
}

/// Track the wire notice per control session, so a later viewer starts fresh.
/// Clearing a condition rearms it without adding a frame older peers must handle.
pub fn changed_host_notice(
    previous: &mut Option<latch_proto::control::Notice>,
    status: &NetworkStatus,
    failure: Option<&Failure>,
) -> Option<latch_proto::control::Notice> {
    let current = host_notice(status, failure);
    if *previous == current {
        return None;
    }
    *previous = current.clone();
    current
}

pub fn carrier_label(info: &ConnectionInfo) -> String {
    let carrier = match info.carrier {
        Carrier::Native => "native",
        Carrier::WebSocket => "WebSocket",
    };
    format!(
        "Latch {carrier}{}",
        if info.via_proxy { " via proxy" } else { "" }
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use latch_core::netenv::{ProxyCandidate, ProxySource};
    struct LocalRelay {
        config: latch_relay::ClientConfig,
        cancel: latch_relay::Cancel,
        worker: Option<std::thread::JoinHandle<()>>,
        quota: std::path::PathBuf,
    }
    impl LocalRelay {
        fn new() -> Self {
            use latch_relay::daemon::{Daemon, DaemonConfig};
            use rustls::pki_types::{pem::PemObject, CertificateDer, PrivateKeyDer};
            let quota = std::env::temp_dir().join(format!(
                "latch-discovery-{}-{}",
                std::process::id(),
                rand::random::<u64>()
            ));
            let mut daemon_config = DaemonConfig::new(
                "127.0.0.1:0".parse().unwrap(),
                vec![CertificateDer::from_pem_slice(include_bytes!(
                    "../../relay/tests/certs/server.pem"
                ))
                .unwrap()],
                PrivateKeyDer::from_pem_slice(include_bytes!(
                    "../../relay/tests/certs/server-key.pem"
                ))
                .unwrap(),
                quota.clone(),
                32 * 1024 * 1024,
            )
            .unwrap();
            daemon_config.initialize_quota = true;
            let daemon = Daemon::bind(daemon_config).unwrap();
            let endpoint =
                latch_relay::Endpoint::new("localhost", daemon.local_addr().unwrap().port())
                    .unwrap();
            let mut roots = rustls::RootCertStore::empty();
            roots
                .add(
                    CertificateDer::from_pem_slice(include_bytes!(
                        "../../relay/tests/certs/ca.pem"
                    ))
                    .unwrap(),
                )
                .unwrap();
            let config = latch_relay::ClientConfig::with_roots(endpoint, roots).unwrap();
            let cancel = latch_relay::Cancel::new();
            let run_cancel = cancel.clone();
            let worker = std::thread::spawn(move || daemon.run(&run_cancel).unwrap());
            Self {
                config,
                cancel,
                worker: Some(worker),
                quota,
            }
        }
    }
    impl Drop for LocalRelay {
        fn drop(&mut self) {
            self.cancel.cancel();
            self.worker.take().unwrap().join().unwrap();
            std::fs::remove_file(&self.quota).unwrap();
        }
    }

    fn entry_at(now: Instant) -> Entry {
        Entry {
            host: "relay.test".into(),
            environment: NetworkEnvironment::default(),
            latest: NetworkDiagnosis::default(),
            checked: now,
            network_key: 0,
            generation: 0,
            refresh_at: now,
            portal_backoff: Duration::from_secs(1),
            portal_open_since: None,
            retained_proxy_since: None,
            retry_requested: false,
            media_usable: false,
        }
    }

    #[test]
    fn retained_proxy_expires_allows_direct_and_later_discovery_wins() {
        let relay = LocalRelay::new();
        let now = Instant::now();
        let mut entry = entry_at(now);
        let mut discovered = NetworkDiagnosis {
            proxy_observed: true,
            ..Default::default()
        };
        discovered.proxies.candidates.push(ProxyCandidate {
            proxy: Proxy::parse("http://proxy.test:3128").unwrap(),
            source: ProxySource::MacPac,
        });
        entry.observe(discovered.clone(), now);
        let mut failed = NetworkDiagnosis {
            proxy_observed: true,
            ..Default::default()
        };
        failed
            .proxies
            .warnings
            .push(netenv::NetworkWarning::PacFailed);
        entry.observe(failed.clone(), now);
        entry.observe(
            failed.clone(),
            now + RETAINED_PROXY_MAX_AGE - Duration::from_millis(1),
        );
        assert!(proxy_selection(&entry.latest).0.is_some());
        assert!(!proxy_selection(&entry.latest).1);
        entry.observe(failed.clone(), now + RETAINED_PROXY_MAX_AGE);
        assert!(proxy_selection(&entry.latest).0.is_none());
        assert!(proxy_selection(&entry.latest).1);
        // Exercise the normal relay entry point after expiry with a real TLS relay.
        let mut config = relay.config.clone();
        let selection = proxy_selection(&entry.latest);
        config.set_proxy_discovery(move || selection.clone());
        let (_, info) = latch_relay::create_grant_with_info(&config, &relay.cancel).unwrap();
        assert!(!info.via_proxy);
        assert!(info.earlier_failures.is_empty());
        entry.observe(failed, now + RETAINED_PROXY_MAX_AGE * 2);
        assert!(proxy_selection(&entry.latest).0.is_none());
        entry.observe(discovered.clone(), now + RETAINED_PROXY_MAX_AGE * 2);
        assert!(proxy_selection(&entry.latest).0.is_some());
        assert!(entry.retained_proxy_since.is_none());
        // A successful empty discovery also overrides the retained proxy immediately.
        discovered.proxies.candidates.clear();
        entry.observe(discovered, now + RETAINED_PROXY_MAX_AGE * 2);
        assert!(proxy_selection(&entry.latest).0.is_none());
    }

    #[test]
    fn alternating_portal_probes_preserve_banner_and_backoff_on_injected_clock() {
        let start = Instant::now();
        let mut entry = entry_at(start);
        let captive = NetworkDiagnosis {
            captive_portal: CaptivePortal::Captive { portal_url: None },
            ..Default::default()
        };
        let open = NetworkDiagnosis {
            captive_portal: CaptivePortal::OpenInternet,
            ..Default::default()
        };
        entry.observe(captive.clone(), start);
        let mut now = entry.refresh_at;
        let mut previous = status(&entry.latest, None, None);
        for tick in 0..12 {
            entry.refresh(now, false);
            entry.observe(
                if tick % 2 == 0 {
                    open.clone()
                } else {
                    captive.clone()
                },
                now,
            );
            let current = status(&entry.latest, None, None);
            assert_eq!(current, previous, "flap {tick} changed the banner");
            assert!(!current.recovered_from(&previous));
            assert_eq!(
                entry.portal_backoff,
                if tick == 0 {
                    Duration::from_secs(2)
                } else {
                    Duration::from_secs(4)
                }
            );
            previous = current;
            now = entry.refresh_at;
        }
        entry.refresh(now, false);
        entry.observe(open.clone(), now);
        // Re-reading the same cached result cannot confirm recovery.
        entry.observe(open.clone(), now + PORTAL_OPEN_CONFIRMATION);
        assert!(status(&entry.latest, None, None).needs_sign_in);
        now += PORTAL_OPEN_CONFIRMATION;
        entry.refresh(now, false);
        // A new round can publish partial proxy/Tailscale results first.
        entry.observe(NetworkDiagnosis::default(), now);
        entry.observe(open, now);
        let recovered = status(&entry.latest, None, None);
        assert!(recovered.recovered_from(&previous));
        assert!(!recovered.needs_sign_in);
        assert_eq!(entry.portal_backoff, Duration::from_secs(1));
        // Actual media remains an immediate override for an HTTP filter.
        entry.observe(captive, now);
        entry.media_usable = true;
        entry.observe(NetworkDiagnosis::default(), now);
        assert!(!status(&entry.latest, None, None).needs_sign_in);
    }

    #[test]
    fn host_notice_changes_are_per_viewer_and_keep_the_6_0_0_wire_shape() {
        let mut previous = None;
        let mut status = NetworkStatus {
            needs_sign_in: true,
            ..Default::default()
        };
        let first = changed_host_notice(&mut previous, &status, None).unwrap();
        // Local details and unrelated network changes leave the wire notice alone.
        status.proxy_available = true;
        status.messages.push("Tailscale is using DERP only.".into());
        assert!(changed_host_notice(&mut previous, &status, None).is_none());
        let failure = Failure::ProxyRefused { status: 403 };
        let changed = changed_host_notice(&mut previous, &status, Some(&failure)).unwrap();
        assert_ne!(first, changed);
        assert!(changed_host_notice(&mut previous, &status, Some(&failure)).is_none());
        // Each new viewer owns an empty history and receives the active notice once.
        let mut later_viewer = None;
        assert_eq!(
            changed_host_notice(&mut later_viewer, &status, Some(&failure)),
            Some(changed.clone())
        );
        assert!(changed_host_notice(&mut later_viewer, &status, Some(&failure)).is_none());
        // 6.0.0 reads a length-prefixed type-4 frame with just kind and message.
        for notice in [first, changed] {
            let mut bytes = Vec::new();
            latch_proto::control::write_message(
                &mut bytes,
                &latch_proto::control::Message::Notice(notice.clone()),
            )
            .unwrap();
            let length = u32::from_le_bytes(bytes[..4].try_into().unwrap()) as usize;
            assert_eq!(length, bytes.len() - 4);
            assert_eq!(bytes[4], 4);
            let legacy: serde_json::Value = serde_json::from_slice(&bytes[5..]).unwrap();
            assert_eq!(
                legacy,
                serde_json::json!({"kind": "network", "message": notice.message})
            );
        }
        status.needs_sign_in = false;
        assert!(changed_host_notice(&mut previous, &status, None).is_none());
        assert!(previous.is_none());
        status.needs_sign_in = true;
        assert!(changed_host_notice(&mut previous, &status, None).is_some());
    }

    #[test]
    fn proxy_order_and_supplied_credentials() {
        let mut diagnosis = NetworkDiagnosis::default();
        assert!(proxy_selection(&diagnosis).1);
        diagnosis.proxies.candidates.push(ProxyCandidate {
            proxy: Proxy::parse("http://user:password@localhost:3128").unwrap(),
            source: ProxySource::Environment,
        });
        let (proxy, direct) = proxy_selection(&diagnosis);
        assert!(!direct);
        assert!(proxy.unwrap().credentials.is_some());
        diagnosis.proxies.candidates.push(ProxyCandidate {
            proxy: Proxy::Direct,
            source: ProxySource::MacPac,
        });
        assert!(proxy_selection(&diagnosis).1);
        assert!(Proxy::parse("https://localhost:3128").is_err());
    }
    #[test]
    fn messages_require_evidence_and_clear_after_recovery() {
        let mut diagnosis = NetworkDiagnosis::default();
        for portal in [
            CaptivePortal::Unknown,
            CaptivePortal::NoConnectivity,
            CaptivePortal::Restricted,
            CaptivePortal::OpenInternet,
        ] {
            diagnosis.captive_portal = portal;
            assert!(status(&diagnosis, None, None).messages.is_empty());
        }
        let failure = Failure::TlsInterception {
            issuer: "Campus CA".into(),
        };
        let message = status(&diagnosis, Some(&failure), None).messages.join(" ");
        assert!(
            message.contains("refuses the substituted certificate")
                && message.contains("Campus CA")
        );
        assert!(status(
            &diagnosis,
            Some(&Failure::ProxyAuthenticationRequired),
            None
        )
        .messages[0]
            .contains("Proxy authentication required"));
        let info = ConnectionInfo {
            carrier: Carrier::Native,
            via_proxy: false,
            reduced_mss: false,
            reduced_mss_reason: None,
            reduced_mss_value: None,
            earlier_failures: vec![],
        };
        assert!(status(&diagnosis, Some(&failure), Some(&info))
            .messages
            .is_empty());
        diagnosis.captive_portal = CaptivePortal::Captive { portal_url: None };
        assert!(status(&diagnosis, None, None).messages[0].contains("sign-in"));
        diagnosis.captive_portal = CaptivePortal::OpenInternet;
        assert!(status(&diagnosis, None, None).messages.is_empty());
    }
    #[test]
    fn portal_wait_is_bounded_and_media_clears_it() {
        let mut status = NetworkStatus {
            needs_sign_in: true,
            ..Default::default()
        };
        let started = Instant::now();
        let detected = started + Duration::from_secs(200);
        let mut since = None;
        assert!(!status.sign_in_timed_out(&mut since, detected));
        assert_eq!(since, Some(detected));
        assert!(!status.sign_in_timed_out(&mut since, detected + Duration::from_secs(179)));
        assert!(status.sign_in_timed_out(&mut since, detected + Duration::from_secs(180)));
        status.media_arrived(true);
        assert!(!status.sign_in_timed_out(&mut since, detected + Duration::from_secs(180)));
        assert!(since.is_none());
        status.needs_sign_in = true;
        assert!(!status.sign_in_timed_out(&mut since, detected + Duration::from_secs(400)));
    }
    #[test]
    fn relay_portal_failure_waits_and_dead_proxy_does_not_claim_progress() {
        let mut diagnosis = NetworkDiagnosis::default();
        assert!(status(&diagnosis, Some(&Failure::CaptivePortal), None).needs_sign_in);
        diagnosis.proxies.candidates.push(ProxyCandidate {
            proxy: Proxy::parse("http://proxy.test:3128").unwrap(),
            source: ProxySource::Environment,
        });
        let status = status(&diagnosis, Some(&Failure::Timeout), None);
        assert!(status.messages[0].contains("did not respond"));
        assert!(!status.messages[0].contains("Connecting through"));
    }
    #[test]
    fn slow_discovery_survives_refresh_interval_with_injected_clock() {
        let now = Instant::now();
        let refresh_at = now + Duration::from_secs(5);
        assert!(!refresh_due(
            now + Duration::from_secs(4),
            refresh_at,
            false
        ));
        assert!(!refresh_due(now + Duration::from_secs(6), refresh_at, true));
        assert!(refresh_due(now + Duration::from_secs(6), refresh_at, false));
    }
    #[test]
    fn healthy_idle_session_does_not_refresh_over_a_minute() {
        let now = Instant::now();
        let mut entry = Entry {
            host: "relay.test".into(),
            environment: NetworkEnvironment::default(),
            latest: NetworkDiagnosis::default(),
            checked: now,
            network_key: 0,
            generation: 1,
            refresh_at: now,
            portal_backoff: Duration::from_secs(1),
            portal_open_since: None,
            retained_proxy_since: None,
            retry_requested: false,
            media_usable: false,
        };
        for second in 0..=60 {
            assert!(!entry.should_refresh(now + Duration::from_secs(second), false));
        }
        entry.retry_requested = true;
        assert!(entry.should_refresh(now, false));
        assert!(!entry.should_refresh(now, true));
        entry.retry_requested = false;
        entry.latest.captive_portal = CaptivePortal::Captive { portal_url: None };
        assert!(entry.should_refresh(now, false));
    }
    #[test]
    fn complete_empty_proxy_observation_expires_discovered_proxy() {
        let mut previous = NetworkDiagnosis::default();
        previous.proxies.candidates.push(ProxyCandidate {
            proxy: Proxy::parse("http://proxy.test:3128").unwrap(),
            source: ProxySource::Environment,
        });
        let mut partial = NetworkDiagnosis::default();
        merge_observations(&mut partial, &previous, false);
        assert_eq!(partial.proxies.candidates.len(), 1);
        let mut complete = NetworkDiagnosis {
            proxy_observed: true,
            ..Default::default()
        };
        merge_observations(&mut complete, &previous, false);
        assert!(complete.proxies.candidates.is_empty());
        assert!(proxy_selection(&complete).1);
    }
    #[test]
    fn failed_pac_and_incomplete_observations_keep_the_previous_proxy() {
        let mut previous = NetworkDiagnosis::default();
        previous.proxies.candidates.push(ProxyCandidate {
            proxy: Proxy::parse("http://proxy.test:3128").unwrap(),
            source: ProxySource::MacPac,
        });
        for warning in [
            netenv::NetworkWarning::PacFailed,
            netenv::NetworkWarning::WpadFailed,
            netenv::NetworkWarning::ChecksPending,
            netenv::NetworkWarning::ChecksTimedOut,
            netenv::NetworkWarning::LinuxPacUnavailable,
            netenv::NetworkWarning::IncompleteSystemProxy,
            netenv::NetworkWarning::PacEvaluatorUnavailable,
            netenv::NetworkWarning::MacSettingsUnavailable,
        ] {
            let mut latest = NetworkDiagnosis {
                proxy_observed: true,
                ..Default::default()
            };
            latest.proxies.warnings.push(warning);
            merge_observations(&mut latest, &previous, false);
            assert!(proxy_selection(&latest).0.is_some(), "{warning:?}");
            assert!(!proxy_selection(&latest).1);
        }
        let mut latest = NetworkDiagnosis {
            proxy_observed: true,
            ..Default::default()
        };
        // The proxy check finished empty. Another check timing out does not
        // keep the old proxy.
        latest.warnings.push(netenv::NetworkWarning::ChecksTimedOut);
        merge_observations(&mut latest, &previous, false);
        assert!(proxy_selection(&latest).0.is_none());
    }

    #[test]
    fn relay_failure_refresh_stays_short_and_portal_return_is_immediate() {
        let now = Instant::now();
        let mut entry = Entry {
            host: "relay.test".into(),
            environment: NetworkEnvironment::default(),
            latest: NetworkDiagnosis::default(),
            checked: now,
            network_key: 0,
            generation: 0,
            refresh_at: now + Duration::from_secs(60),
            portal_backoff: Duration::from_secs(60),
            portal_open_since: None,
            retained_proxy_since: None,
            retry_requested: false,
            media_usable: false,
        };
        entry.request_retry(now, false);
        assert!(!entry.should_refresh(now + Duration::from_secs(1), false));
        for second in (2..20).step_by(2) {
            let at = now + Duration::from_secs(second);
            entry.refresh(at, false);
            assert_eq!(entry.portal_backoff, Duration::from_secs(1));
            assert_eq!(entry.refresh_at, at + Duration::from_secs(2));
            entry.request_retry(at, false);
        }
        entry.latest.captive_portal = CaptivePortal::Captive { portal_url: None };
        entry.retry_requested = false;
        entry.refresh(entry.refresh_at, false);
        assert_eq!(entry.portal_backoff, Duration::from_secs(2));
        entry.request_retry(now + Duration::from_secs(21), true);
        assert!(entry.should_refresh(now + Duration::from_secs(21), false));
        entry.refresh(now + Duration::from_secs(21), false);
        assert_eq!(entry.portal_backoff, Duration::from_secs(2));
    }

    #[test]
    fn an_open_portal_probe_clears_an_earlier_relay_portal_failure() {
        let diagnosis = NetworkDiagnosis {
            captive_portal: CaptivePortal::OpenInternet,
            ..Default::default()
        };
        assert!(!status(&diagnosis, Some(&Failure::CaptivePortal), None).needs_sign_in);
    }

    #[test]
    fn host_notices_are_labelled_and_have_only_fixed_text() {
        let status = NetworkStatus {
            needs_sign_in: true,
            ..Default::default()
        };
        for failure in [
            None,
            Some(Failure::ProxyRefused { status: 403 }),
            Some(Failure::TlsInterception {
                issuer: "http://private.portal/token".into(),
            }),
        ] {
            let notice = host_notice(&status, failure.as_ref()).unwrap();
            assert_eq!(notice.kind, "network");
            assert!(notice.message.starts_with("The host's network"));
            assert!(!notice.message.contains("http"));
            let mut bytes = Vec::new();
            latch_proto::control::write_message(
                &mut bytes,
                &latch_proto::control::Message::Notice(notice.clone()),
            )
            .unwrap();
            assert!(
                matches!(latch_proto::control::read_message(&mut bytes.as_slice()).unwrap(), latch_proto::control::Message::Notice(decoded) if decoded == notice)
            );
        }
        assert!(host_notice(&NetworkStatus::default(), None).is_none());
    }

    #[test]
    fn pac_failure_reconnects_through_the_retained_proxy() {
        use std::io::{Read, Write};
        use std::net::{Shutdown, TcpListener, TcpStream};
        let relay = LocalRelay::new();
        let relay_address = ("127.0.0.1", relay.config.endpoint.port());
        let cancel = relay.cancel.clone();
        let proxy = TcpListener::bind("127.0.0.1:0").unwrap();
        let proxy_port = proxy.local_addr().unwrap().port();
        proxy.set_nonblocking(true).unwrap();
        let tunnel_cancel = cancel.clone();
        let tunnel = std::thread::spawn(move || {
            let (mut client, _) = loop {
                match proxy.accept() {
                    Ok(pair) => break pair,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        if tunnel_cancel.is_cancelled() {
                            return false;
                        }
                        std::thread::yield_now();
                    }
                    Err(error) => panic!("{error}"),
                }
            };
            client.set_nonblocking(false).unwrap();
            client
                .set_read_timeout(Some(Duration::from_secs(4)))
                .unwrap();
            let mut header = Vec::new();
            while !header.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                client.read_exact(&mut byte).unwrap();
                header.extend(byte);
                assert!(header.len() <= 4096);
            }
            assert!(header.starts_with(b"CONNECT localhost:443 HTTP/1.1\r\n"));
            let mut upstream = TcpStream::connect(relay_address).unwrap();
            client
                .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                .unwrap();
            let mut upload_client = client.try_clone().unwrap();
            let mut upload_server = upstream.try_clone().unwrap();
            let upload = std::thread::spawn(move || {
                let _ = std::io::copy(&mut upload_client, &mut upload_server);
                let _ = upload_server.shutdown(Shutdown::Write);
            });
            let _ = std::io::copy(&mut upstream, &mut client);
            let _ = client.shutdown(Shutdown::Both);
            upload.join().unwrap();
            true
        });
        let mut previous = NetworkDiagnosis::default();
        previous.proxies.candidates.push(ProxyCandidate {
            proxy: Proxy::parse(&format!("http://127.0.0.1:{proxy_port}")).unwrap(),
            source: ProxySource::MacPac,
        });
        let mut latest = NetworkDiagnosis {
            proxy_observed: true,
            ..Default::default()
        };
        latest
            .proxies
            .warnings
            .push(netenv::NetworkWarning::PacFailed);
        let now = Instant::now();
        let mut entry = entry_at(now);
        entry.latest = previous;
        entry.observe(latest.clone(), now);
        entry.observe(
            latest,
            now + RETAINED_PROXY_MAX_AGE - Duration::from_millis(1),
        );
        let latest = entry.latest;
        let mut config = relay.config.clone();
        config.endpoint = latch_relay::Endpoint::new("localhost", 443).unwrap();
        let selection = proxy_selection(&latest);
        config.set_proxy_discovery(move || selection.clone());
        let result = latch_relay::create_grant_with_info(&config, &cancel);
        cancel.cancel();
        assert!(
            tunnel.join().unwrap(),
            "reconnect never reached the retained proxy"
        );
        let (_, info) = result.unwrap();
        assert!(info.via_proxy);
        assert!(info.earlier_failures.is_empty());
    }

    #[test]
    fn portal_opens_and_session_connects_within_the_bound_on_injected_clock() {
        let relay = LocalRelay::new();
        let start = Instant::now();
        let mut entry = Entry {
            host: "localhost".into(),
            environment: NetworkEnvironment::default(),
            latest: NetworkDiagnosis::default(),
            checked: start,
            network_key: 0,
            generation: 0,
            refresh_at: start,
            portal_backoff: Duration::from_secs(1),
            portal_open_since: None,
            retained_proxy_since: None,
            retry_requested: false,
            media_usable: false,
        };
        entry.observe(
            NetworkDiagnosis {
                captive_portal: CaptivePortal::Captive { portal_url: None },
                ..Default::default()
            },
            start,
        );
        let failure = Failure::CaptivePortal;
        let mut previous = status(&entry.latest, Some(&failure), None);
        assert!(previous.needs_sign_in);
        let mut connected_at = None;
        for tick in 1..=56 {
            let now = start + Duration::from_millis(tick * 250);
            if entry.should_refresh(now, false) {
                entry.refresh(now, false);
                let portal = if now.duration_since(start) >= Duration::from_secs(10) {
                    CaptivePortal::OpenInternet
                } else {
                    CaptivePortal::Captive { portal_url: None }
                };
                entry.observe(
                    NetworkDiagnosis {
                        captive_portal: portal,
                        ..Default::default()
                    },
                    now,
                );
            }
            let current = status(&entry.latest, Some(&failure), None);
            if current.recovered_from(&previous) {
                let connecting = Instant::now();
                let grant = latch_relay::create_grant(&relay.config, &relay.cancel).unwrap();
                let keys = latch_relay::SessionKeys::generate().unwrap();
                let host =
                    latch_relay::Transport::connect(relay.config.clone(), grant.host, keys.clone())
                        .unwrap();
                let viewer =
                    latch_relay::Transport::connect(relay.config.clone(), grant.viewer, keys)
                        .unwrap();
                for peer in [&host, &viewer] {
                    assert!(matches!(
                        peer.recv_timeout(Duration::from_secs(2)),
                        Some(latch_relay::Event::Ready)
                    ));
                }
                connected_at = Some(now.duration_since(start) + connecting.elapsed());
                host.shutdown();
                viewer.shutdown();
                break;
            }
            previous = current;
        }
        assert!(
            connected_at
                .is_some_and(|elapsed| elapsed >= Duration::from_secs(10)
                    && elapsed <= Duration::from_secs(14)),
            "session did not connect within 14 seconds: {connected_at:?}"
        );
    }

    #[test]
    fn invalid_relay_host_cannot_supply_a_probe_path_or_credentials() {
        for host in [
            "relay.test/private",
            "user:password@relay.test",
            "relay.test?secret",
            "",
        ] {
            let result = diagnosis(host);
            assert!(result.proxies.candidates.is_empty());
            assert_eq!(result.captive_portal, CaptivePortal::Unknown);
        }
    }
}
