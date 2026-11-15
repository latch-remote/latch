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

struct Entry {
    host: String,
    environment: NetworkEnvironment,
    latest: NetworkDiagnosis,
    checked: Instant,
    network_key: u64,
    generation: u64,
    refresh_at: Instant,
    portal_backoff: Duration,
    retry_requested: bool,
    media_usable: bool,
}

impl Entry {
    fn should_refresh(&self, now: Instant, pending: bool) -> bool {
        refresh_due(now, self.refresh_at, pending)
            && (self.retry_requested
                || matches!(self.latest.captive_portal, CaptivePortal::Captive { .. }))
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
            entry.retry_requested = true;
            entry.media_usable = false;
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
    if !latest.proxy_observed {
        latest.proxies = previous.proxies.clone();
    }
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
            portal_backoff: Duration::from_secs(2),
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
        entry.portal_backoff = Duration::from_secs(2);
        entry.retry_requested = false;
        entry.latest = NetworkDiagnosis::default();
        entry.media_usable = false;
    } else {
        if let Some(mut latest) = entry.environment.get(entry.generation, &options).cloned() {
            merge_observations(&mut latest, &entry.latest, entry.media_usable);
            if matches!(latest.captive_portal, CaptivePortal::Captive { .. })
                && !matches!(entry.latest.captive_portal, CaptivePortal::Captive { .. })
            {
                entry.refresh_at = now + entry.portal_backoff;
            }
            entry.latest = latest;
        }
        // Never discard a slow PAC result just because its refresh interval
        // elapsed. Consume the completed snapshot before starting a refresh.
        if entry.should_refresh(now, entry.environment.is_pending()) {
            entry.generation = entry.generation.wrapping_add(1);
            entry.retry_requested = false;
            entry.portal_backoff = (entry.portal_backoff * 2).min(Duration::from_secs(60));
            entry.refresh_at = now + entry.portal_backoff;
        }
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
    pub(crate) fn sign_in_timed_out(&self, elapsed: Duration) -> bool {
        self.needs_sign_in && elapsed >= Duration::from_secs(180)
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
            Some(Failure::CaptivePortal) if result.messages.is_empty() => {
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
        assert!(!status.sign_in_timed_out(Duration::from_secs(179)));
        assert!(status.sign_in_timed_out(Duration::from_secs(180)));
        status.media_arrived(true);
        assert!(!status.sign_in_timed_out(Duration::from_secs(180)));
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
            portal_backoff: Duration::from_secs(2),
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
