use super::Options;
use percent_encoding::percent_decode_str;
#[cfg(any(test, target_os = "linux", target_os = "macos"))]
use std::collections::BTreeMap;
use std::fmt;
use std::net::IpAddr;
use std::time::Instant;
use url::Url;

#[derive(Clone, PartialEq, Eq)]
pub struct Credentials {
    username: String,
    password: String,
}
impl Credentials {
    pub fn new(username: String, password: String) -> Self {
        Self { username, password }
    }
    /// Explicit secret access for the carrier's authentication header.
    pub fn username(&self) -> &str {
        &self.username
    }
    pub fn password(&self) -> &str {
        &self.password
    }
}
impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[redacted]")
    }
}
impl fmt::Display for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Proxy {
    Direct,
    Http {
        host: String,
        port: u16,
        credentials: Option<Credentials>,
    },
}
impl Proxy {
    /// Accept `direct`, `host:port`, or `http://[user:password@]host:port`.
    /// Errors deliberately omit the input, which may contain a password.
    pub fn parse(value: &str) -> Result<Self, &'static str> {
        let value = value.trim();
        if value.eq_ignore_ascii_case("direct") {
            return Ok(Self::Direct);
        }
        let url = Url::parse(&if value.contains("://") {
            value.to_owned()
        } else {
            format!("http://{value}")
        })
        .map_err(|_| "Invalid proxy URL.")?;
        if url.scheme() != "http" {
            return Err("Only HTTP CONNECT proxies are supported.");
        }
        if url.path() != "/" || url.query().is_some() || url.fragment().is_some() {
            return Err("Proxy URL must contain only a host, port and optional credentials.");
        }
        let host = url
            .host_str()
            .ok_or("Proxy hostname is missing.")?
            .trim_matches(['[', ']'])
            .to_owned();
        let port = url
            .port_or_known_default()
            .filter(|port| *port != 0)
            .ok_or("Invalid proxy port.")?;
        let credentials = if !url.username().is_empty() || url.password().is_some() {
            let decode = |value: &str| {
                percent_decode_str(value)
                    .decode_utf8()
                    .map(|s| s.into_owned())
                    .map_err(|_| "Invalid proxy credentials.")
            };
            Some(Credentials::new(
                decode(url.username())?,
                decode(url.password().unwrap_or(""))?,
            ))
        } else {
            None
        };
        Ok(Self::Http {
            host,
            port,
            credentials,
        })
    }
}
impl fmt::Display for Proxy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Direct => f.write_str("direct"),
            Self::Http {
                host,
                port,
                credentials,
            } => {
                let auth = if credentials.is_some() {
                    "[redacted]@"
                } else {
                    ""
                };
                if host.contains(':') {
                    write!(f, "http://{auth}[{host}]:{port}")
                } else {
                    write!(f, "http://{auth}{host}:{port}")
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProxySource {
    Override,
    Environment,
    MacSystem,
    MacPac,
    MacWpad,
    WindowsUser,
    WindowsMachine,
    WindowsPac,
    WindowsWpad,
    Gnome,
    Kde,
    Unconfigured,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyCandidate {
    pub proxy: Proxy,
    pub source: ProxySource,
}
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProxyDiscovery {
    /// Ordered, including DIRECT only where settings/PAC explicitly allow it,
    /// or where no supported configuration was found.
    pub candidates: Vec<ProxyCandidate>,
    pub warnings: Vec<String>,
}
impl ProxyDiscovery {
    pub(super) fn one(proxy: Proxy, source: ProxySource) -> Self {
        Self {
            candidates: vec![ProxyCandidate { proxy, source }],
            warnings: Vec::new(),
        }
    }
    pub(super) fn add(&mut self, value: &str, source: ProxySource) {
        match Proxy::parse(value) {
            Ok(proxy) => self.candidates.push(ProxyCandidate { proxy, source }),
            Err(_) => self
                .warnings
                .push("A configured proxy is malformed or uses an unsupported protocol.".into()),
        }
    }
}

pub(super) fn destination_url(host: &str) -> Option<Url> {
    // Host only, never a URL/userinfo/path supplied to the OS evaluator.
    let mut url = Url::parse("https://placeholder.invalid/").ok()?;
    if host.contains(['/', '@', '?', '#', '\\']) {
        return None;
    }
    let host = if host.parse::<std::net::Ipv6Addr>().is_ok() {
        format!("[{host}]")
    } else {
        host.to_owned()
    };
    url.set_host(Some(&host)).ok()?;
    Some(url)
}

pub(super) fn discover(options: &Options, deadline: Instant) -> ProxyDiscovery {
    if let Some(proxy) = &options.proxy_override {
        return ProxyDiscovery::one(proxy.clone(), ProxySource::Override);
    }
    if destination_url(&options.destination_host).is_none() {
        return ProxyDiscovery {
            warnings: vec!["The HTTPS destination hostname is invalid.".into()],
            ..Default::default()
        };
    }
    #[cfg(target_os = "linux")]
    {
        super::linux::proxies(&options.destination_host, deadline)
    }
    #[cfg(target_os = "macos")]
    {
        super::macos::proxies(&options.destination_host, deadline)
    }
    #[cfg(target_os = "windows")]
    {
        super::windows::proxies(&options.destination_host, deadline)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        let _ = deadline;
        ProxyDiscovery::one(Proxy::Direct, ProxySource::Unconfigured)
    }
}

/// curl-style NO_PROXY. DNS names match at label boundaries, optional ports
/// restrict matching, IP literals and CIDR never cause DNS lookups. Leading
/// dots and `*.` match the domain and its descendants. `*` bypasses everything.
/// `<local>` and general globs are opt-in for OS bypass lists, not NO_PROXY.
pub(super) fn bypass(list: &str, host: &str, port: u16, os_patterns: bool) -> bool {
    let host = host
        .trim_matches(['[', ']'])
        .trim_end_matches('.')
        .to_ascii_lowercase();
    list.split([',', ';', ' '])
        .filter(|s| !s.is_empty())
        .any(|rule| {
            let rule = rule.trim().trim_end_matches('.').to_ascii_lowercase();
            if rule == "*" {
                return true;
            }
            if os_patterns && rule == "<local>" {
                return !host.contains('.') && host.parse::<IpAddr>().is_err();
            }
            if let Some((network, bits)) = rule.split_once('/') {
                return cidr_matches(network, bits, &host);
            }
            let (name, required_port) = split_host_port(&rule);
            if required_port.is_some_and(|p| p != port) {
                return false;
            }
            let name = name
                .trim_matches(['[', ']'])
                .trim_start_matches('.')
                .trim_start_matches("*.");
            if os_patterns && name.contains('*') {
                return glob(name.as_bytes(), host.as_bytes());
            }
            if let Ok(ip) = name.parse::<IpAddr>() {
                return host.parse::<IpAddr>().ok() == Some(ip);
            }
            host == name
                || host
                    .strip_suffix(name)
                    .is_some_and(|prefix| prefix.ends_with('.'))
        })
}
fn split_host_port(value: &str) -> (&str, Option<u16>) {
    if let Some(rest) = value.strip_prefix('[') {
        if let Some((host, suffix)) = rest.split_once(']') {
            return (host, suffix.strip_prefix(':').and_then(|s| s.parse().ok()));
        }
    }
    if value.matches(':').count() == 1 {
        if let Some((host, port)) = value.rsplit_once(':') {
            if let Ok(port) = port.parse() {
                return (host, Some(port));
            }
        }
    }
    (value, None)
}
fn cidr_matches(network: &str, bits: &str, host: &str) -> bool {
    match (
        network.parse::<IpAddr>(),
        host.parse::<IpAddr>(),
        bits.parse::<u32>(),
    ) {
        (Ok(IpAddr::V4(net)), Ok(IpAddr::V4(ip)), Ok(bits @ 0..=32)) => {
            (u32::from(net) ^ u32::from(ip))
                .checked_shr(32 - bits)
                .unwrap_or(0)
                == 0
        }
        (Ok(IpAddr::V6(net)), Ok(IpAddr::V6(ip)), Ok(bits @ 0..=128)) => {
            (u128::from(net) ^ u128::from(ip))
                .checked_shr(128 - bits)
                .unwrap_or(0)
                == 0
        }
        _ => false,
    }
}
fn glob(pattern: &[u8], value: &[u8]) -> bool {
    // Iterative wildcard matcher, bounded by the small OS bypass strings.
    let (mut p, mut v, mut star, mut retry) = (0, 0, None, 0);
    while v < value.len() {
        if p < pattern.len() && pattern[p] == value[v] {
            p += 1;
            v += 1;
        } else if p < pattern.len() && pattern[p] == b'*' {
            star = Some(p);
            p += 1;
            retry = v;
        } else if let Some(s) = star {
            retry += 1;
            v = retry;
            p = s + 1;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == b'*' {
        p += 1;
    }
    p == pattern.len()
}

#[cfg(any(test, target_os = "linux"))]
pub(super) fn environment(env: &BTreeMap<String, String>, host: &str) -> Option<ProxyDiscovery> {
    let get = |upper: &str, lower: &str| {
        env.get(upper)
            .filter(|s| !s.trim().is_empty())
            .or_else(|| env.get(lower).filter(|s| !s.trim().is_empty()))
    };
    let value = get("HTTPS_PROXY", "https_proxy").or_else(|| get("ALL_PROXY", "all_proxy"))?;
    if get("NO_PROXY", "no_proxy").is_some_and(|list| bypass(list, host, 443, false)) {
        return Some(ProxyDiscovery::one(Proxy::Direct, ProxySource::Environment));
    }
    let mut result = ProxyDiscovery::default();
    result.add(value, ProxySource::Environment);
    Some(result)
}

/// Ordered PAC return syntax. Unsupported SOCKS/HTTPS entries are diagnosed
/// and never silently converted into an HTTP proxy.
#[cfg(any(test, target_os = "windows"))]
pub(super) fn pac_result(value: &str, source: ProxySource) -> ProxyDiscovery {
    let mut result = ProxyDiscovery::default();
    for item in value.split(';').map(str::trim).filter(|s| !s.is_empty()) {
        if item.eq_ignore_ascii_case("DIRECT") {
            result.add("direct", source);
        } else if let Some((kind, address)) = item.split_once(char::is_whitespace) {
            if kind.eq_ignore_ascii_case("PROXY") || kind.eq_ignore_ascii_case("HTTP") {
                result.add(address.trim(), source);
            } else {
                result
                    .warnings
                    .push("The PAC result includes an unsupported proxy protocol.".into());
            }
        } else {
            result
                .warnings
                .push("The PAC result includes a malformed candidate.".into());
        }
    }
    result
}

// Plain snapshots keep platform parsers testable on every CI OS.
#[cfg(any(test, target_os = "windows"))]
pub(super) fn winhttp(
    proxy: &str,
    bypass_list: &str,
    host: &str,
    source: ProxySource,
) -> ProxyDiscovery {
    if bypass(bypass_list, host, 443, true) {
        return ProxyDiscovery::one(Proxy::Direct, source);
    }
    // Accept PAC return syntax alongside WinHTTP's scheme-specific list.
    let first = proxy.trim().split([' ', ';']).next().unwrap_or("");
    if [
        "DIRECT", "PROXY", "HTTP", "HTTPS", "SOCKS", "SOCKS4", "SOCKS5",
    ]
    .iter()
    .any(|kind| first.eq_ignore_ascii_case(kind))
    {
        return pac_result(proxy, source);
    }
    let entries: Vec<_> = proxy.split([';', ' ']).filter(|s| !s.is_empty()).collect();
    let https: Vec<_> = entries
        .iter()
        .filter_map(|entry| {
            let (scheme, value) = entry.split_once('=')?;
            scheme.eq_ignore_ascii_case("https").then_some(value)
        })
        .collect();
    let entries: Vec<_> = if https.is_empty() {
        entries
            .into_iter()
            .filter(|entry| {
                entry.contains("://")
                    || !entry.split_once('=').is_some_and(|(key, _)| {
                        ["http", "https", "ftp", "socks"]
                            .iter()
                            .any(|scheme| key.eq_ignore_ascii_case(scheme))
                    })
            })
            .collect()
    } else {
        https
    };
    let mut result = ProxyDiscovery::default();
    for value in entries {
        result.add(value, source);
    }
    if result.candidates.is_empty() && result.warnings.is_empty() {
        result = ProxyDiscovery::one(Proxy::Direct, source);
    }
    result
}

#[cfg(any(test, target_os = "macos"))]
pub(super) fn scutil(text: &str, host: &str) -> ProxyDiscovery {
    let fields: BTreeMap<_, _> = text
        .lines()
        .filter_map(|line| line.trim().split_once(" : "))
        .map(|(k, v)| (k.trim(), v.trim()))
        .collect();
    let mut result = ProxyDiscovery::default();
    let exceptions = fields
        .iter()
        .filter(|(k, _)| k.parse::<u32>().is_ok())
        .map(|(_, v)| *v)
        .collect::<Vec<_>>()
        .join(",");
    if bypass(&exceptions, host, 443, true)
        || (fields.get("ExcludeSimpleHostnames") == Some(&"1") && !host.contains('.'))
    {
        return ProxyDiscovery::one(Proxy::Direct, ProxySource::MacSystem);
    }
    if fields.get("ProxyAutoConfigEnable") == Some(&"1")
        || fields.get("ProxyAutoDiscoveryEnable") == Some(&"1")
    {
        result
            .warnings
            .push("Automatic proxy settings require the macOS PAC evaluator.".into());
    }
    if fields.get("HTTPSEnable") == Some(&"1") {
        if let (Some(host), Some(port)) = (fields.get("HTTPSProxy"), fields.get("HTTPSPort")) {
            result.add(&format!("{host}:{port}"), ProxySource::MacSystem);
        } else {
            result
                .warnings
                .push("The system HTTPS proxy is incomplete.".into());
        }
    }
    if result.candidates.is_empty() && result.warnings.is_empty() {
        result = ProxyDiscovery::one(Proxy::Direct, ProxySource::MacSystem);
    }
    result
}
