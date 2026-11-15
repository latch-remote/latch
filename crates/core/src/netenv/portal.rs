use crate::http::{header, read_response_body, read_response_head, DeadlineStream};
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Instant;
use url::Url;

/// An anonymous, cleartext HTTP check is necessary to observe interception.
/// No default third-party request is made. Configure a project-controlled
/// endpoint first, then an explicitly chosen fallback if desired. No cookies,
/// credentials, redirects, device IDs or identifying headers are sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeTarget {
    url: Url,
    expected_status: u16,
    expected_body: Vec<u8>,
}
impl ProbeTarget {
    /// Endpoint must return this exact status and body (at most 4 KiB).
    /// HTTP 204 with an empty body is recommended. URLs with credentials,
    /// queries or fragments are rejected to keep requests anonymous.
    pub fn new(
        url: &str,
        expected_status: u16,
        expected_body: &[u8],
    ) -> Result<Self, &'static str> {
        let url = Url::parse(url).map_err(|_| "Invalid probe URL.")?;
        if url.scheme() != "http"
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || expected_body.len() > 4096
            || !matches!(expected_status, 200 | 204)
            || (expected_status == 204 && !expected_body.is_empty())
        {
            return Err("Probe requires an anonymous HTTP URL and an expected 200 body or empty 204 response.");
        }
        Ok(Self {
            url,
            expected_status,
            expected_body: expected_body.into(),
        })
    }
}
/// Sign-in destination. Paths, queries and fragments can contain session tokens.
/// Only `as_str` exposes them. Formatting always removes them.
#[derive(Clone, PartialEq, Eq)]
pub struct PortalUrl(Url);
impl PortalUrl {
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}
impl std::fmt::Display for PortalUrl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut safe = self.0.clone();
        safe.set_path("/");
        safe.set_query(None);
        safe.set_fragment(None);
        write!(f, "{safe}")
    }
}
impl std::fmt::Debug for PortalUrl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self, f)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptivePortal {
    /// The configured probe matched, or the OS reports internet access.
    OpenInternet,
    Captive {
        /// Sign-in URL with userinfo removed and redacted formatting.
        portal_url: Option<PortalUrl>,
    },
    /// All configured direct HTTP endpoints were unreachable. This is scoped
    /// to those endpoints. Proxy-only networks can produce the same result.
    NoConnectivity,
    /// A responding filter/proxy rejected the check or changed its content.
    Restricted,
    Unknown,
}

#[cfg(test)]
pub(super) fn classify(target: &ProbeTarget, response: &[u8]) -> CaptivePortal {
    read_probe(target, &mut std::io::Cursor::new(response)).unwrap_or(CaptivePortal::Restricted)
}

fn read_probe(target: &ProbeTarget, reader: &mut impl BufRead) -> Option<CaptivePortal> {
    let (status, headers) = read_response_head(reader).ok()?;
    if matches!(status, 301 | 302 | 303 | 307 | 308 | 511) {
        let portal_url = header(&headers, "location")
            .and_then(|value| target.url.join(value).ok())
            .filter(|url| {
                matches!(url.scheme(), "http" | "https")
                    && match url.host() {
                        Some(url::Host::Ipv4(ip)) => !ip.is_loopback(),
                        Some(url::Host::Ipv6(ip)) => {
                            !ip.is_loopback()
                                && !ip.to_ipv4_mapped().is_some_and(|ip| ip.is_loopback())
                        }
                        Some(url::Host::Domain(host)) => {
                            !host.trim_end_matches('.').eq_ignore_ascii_case("localhost")
                        }
                        None => false,
                    }
            })
            .map(|mut url| {
                let _ = url.set_username("");
                let _ = url.set_password(None);
                PortalUrl(url)
            });
        return Some(CaptivePortal::Captive { portal_url });
    }
    let mut body = Vec::new();
    if status != 204 && read_response_body(reader, &headers, &mut body, 4096, false).is_err() {
        return Some(CaptivePortal::Restricted);
    }
    if status == target.expected_status && body == target.expected_body {
        return Some(CaptivePortal::OpenInternet);
    }
    if status == 200
        && header(&headers, "content-type")
            .is_some_and(|value| value.to_ascii_lowercase().contains("text/html"))
    {
        return Some(CaptivePortal::Captive { portal_url: None });
    }
    Some(CaptivePortal::Restricted)
}

pub(super) fn probe(target: &ProbeTarget, deadline: Instant) -> Option<CaptivePortal> {
    let host = target.url.host_str()?.trim_matches(['[', ']']);
    let port = target.url.port_or_known_default()?;
    let remaining = || deadline.saturating_duration_since(Instant::now());
    for address in (host, port).to_socket_addrs().ok()?.take(8) {
        if remaining().is_zero() {
            break;
        }
        let Ok(mut stream) = TcpStream::connect_timeout(&address, remaining()) else {
            continue;
        };
        let host_header = if host.contains(':') {
            format!("[{host}]:{port}")
        } else {
            format!("{host}:{port}")
        };
        stream
            .set_write_timeout(Some(remaining().max(std::time::Duration::from_millis(1))))
            .ok()?;
        let request = format!("GET {} HTTP/1.1\r\nHost: {host_header}\r\nConnection: close\r\nCache-Control: no-cache\r\n\r\n", target.url.path());
        stream.write_all(request.as_bytes()).ok()?;
        let reader = DeadlineStream::new(&mut stream, deadline);
        return read_probe(target, &mut BufReader::new(reader));
    }
    None
}

fn native(deadline: Instant) -> CaptivePortal {
    #[cfg(target_os = "windows")]
    {
        let _ = deadline;
        super::windows::portal()
    }
    #[cfg(target_os = "linux")]
    {
        match super::command("nmcli", &["-t", "networking", "connectivity"], deadline)
            .as_deref()
            .map(str::trim)
        {
            Some("portal") => CaptivePortal::Captive { portal_url: None },
            Some("full") => CaptivePortal::OpenInternet,
            Some("none") => CaptivePortal::NoConnectivity,
            _ => CaptivePortal::Unknown,
        }
    }
    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    {
        let _ = deadline;
        CaptivePortal::Unknown
    }
}

pub(super) fn detect(targets: &[ProbeTarget], deadline: Instant) -> CaptivePortal {
    if targets.is_empty() {
        return native(deadline);
    }
    detect_with_native(targets, deadline, || native(deadline))
}

pub(super) fn detect_with_native(
    targets: &[ProbeTarget],
    deadline: Instant,
    native: impl FnOnce() -> CaptivePortal,
) -> CaptivePortal {
    let mut restricted = false;
    for (index, target) in targets.iter().enumerate() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        // Reserve time for the fallback even if the primary silently drops.
        let probe_deadline =
            Instant::now() + remaining / (targets.len() - index).min(u32::MAX as usize) as u32;
        match probe(target, probe_deadline) {
            Some(state @ (CaptivePortal::OpenInternet | CaptivePortal::Captive { .. })) => {
                return state
            }
            Some(_) => restricted = true,
            None => {}
        }
    }
    if restricted {
        return CaptivePortal::Restricted;
    }
    match native() {
        state @ (CaptivePortal::OpenInternet | CaptivePortal::Captive { .. }) => state,
        _ => CaptivePortal::NoConnectivity,
    }
}

#[cfg(test)]
mod url_tests {
    use super::*;
    #[test]
    fn portal_button_rejects_loopback_and_keeps_private_hosts() {
        let target = ProbeTarget::new("http://relay.test/netenv/probe", 204, b"").unwrap();
        for host in [
            "127.0.0.1",
            "127.42.1.2",
            "[::1]",
            "[::ffff:127.0.0.1]",
            "localhost",
            "LOCALHOST.",
        ] {
            let result = classify(
                &target,
                format!("HTTP/1.1 302 Found\r\nLocation: http://{host}:8000/action\r\n\r\n")
                    .as_bytes(),
            );
            assert_eq!(result, CaptivePortal::Captive { portal_url: None });
        }
        for host in ["192.168.1.1", "10.0.0.1", "172.16.0.1", "[fd00::1]"] {
            assert!(matches!(
                classify(
                    &target,
                    format!("HTTP/1.1 302 Found\r\nLocation: http://{host}/signin\r\n\r\n")
                        .as_bytes()
                ),
                CaptivePortal::Captive {
                    portal_url: Some(_)
                }
            ));
        }
    }
}
