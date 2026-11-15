use super::proxy::{bypass, environment, Proxy, ProxyDiscovery, ProxySource};
use std::collections::BTreeMap;
#[cfg(target_os = "linux")]
use std::time::Instant;

/// gsettings' GVariant string/array representation, including escaped quotes.
fn strings(value: &str) -> Vec<String> {
    let mut result = Vec::new();
    let mut quote = None;
    let mut escaped = false;
    let mut current = String::new();
    for c in value.chars() {
        if escaped {
            current.push(match c {
                'n' => '\n',
                'r' => '\r',
                't' => '\t',
                other => other,
            });
            escaped = false;
        } else if quote.is_some() && c == '\\' {
            escaped = true;
        } else if quote == Some(c) {
            result.push(std::mem::take(&mut current));
            quote = None;
        } else if quote.is_some() {
            current.push(c);
        } else if c == '\'' || c == '"' {
            quote = Some(c);
        }
    }
    result
}

pub(super) fn gsettings(text: &str, host: &str) -> Option<ProxyDiscovery> {
    let mut fields = BTreeMap::new();
    for line in text.lines() {
        let mut parts = line.splitn(3, ' ');
        if let (Some(schema), Some(key), Some(value)) = (parts.next(), parts.next(), parts.next()) {
            fields.insert(format!("{schema}.{key}"), value.trim());
        }
    }
    let get = |key: &str| fields.get(key).copied().unwrap_or("");
    let mode = strings(get("org.gnome.system.proxy.mode")).pop()?;
    if mode == "none" {
        return Some(ProxyDiscovery::one(Proxy::Direct, ProxySource::Gnome));
    }
    let ignore = strings(get("org.gnome.system.proxy.ignore-hosts")).join(",");
    if bypass(&ignore, host, 443, true) {
        return Some(ProxyDiscovery::one(Proxy::Direct, ProxySource::Gnome));
    }
    if mode == "auto" {
        return Some(pac_unavailable());
    }
    if mode != "manual" {
        return None;
    }
    let mut proxy_host = strings(get("org.gnome.system.proxy.https.host"))
        .pop()
        .unwrap_or_default();
    let mut port = get("org.gnome.system.proxy.https.port");
    if get("org.gnome.system.proxy.use-same-proxy") == "true" {
        proxy_host = strings(get("org.gnome.system.proxy.http.host"))
            .pop()
            .unwrap_or_default();
        port = get("org.gnome.system.proxy.http.port");
    }
    let mut result = ProxyDiscovery::default();
    if proxy_host.is_empty() {
        return Some(ProxyDiscovery::one(Proxy::Direct, ProxySource::Gnome));
    }
    result.add(
        &format!("{}:{port}", bracket_host(&proxy_host)),
        ProxySource::Gnome,
    );
    if get("org.gnome.system.proxy.http.use-authentication") == "true" {
        let username = strings(get("org.gnome.system.proxy.http.authentication-user"))
            .pop()
            .unwrap_or_default();
        let password = strings(get("org.gnome.system.proxy.http.authentication-password"))
            .pop()
            .unwrap_or_default();
        for candidate in &mut result.candidates {
            if let Proxy::Http { credentials, .. } = &mut candidate.proxy {
                *credentials = Some(super::Credentials::new(username.clone(), password.clone()));
            }
        }
    }
    Some(result)
}
fn bracket_host(host: &str) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.into()
    }
}
fn pac_unavailable() -> ProxyDiscovery {
    ProxyDiscovery { candidates: Vec::new(), warnings: vec!["A PAC file or automatic proxy discovery is configured. Linux has no system PAC evaluator. Set an explicit HTTP proxy or direct connection in Latch.".into()] }
}

pub(super) fn kioslaverc(
    text: &str,
    host: &str,
    env: &BTreeMap<String, String>,
) -> Option<ProxyDiscovery> {
    let mut in_section = false;
    let mut fields = BTreeMap::new();
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            in_section = line == "[Proxy Settings]";
        } else if in_section && !line.starts_with(['#', ';']) {
            if let Some((key, value)) = line.split_once('=') {
                fields.insert(key.trim(), value.trim());
            }
        }
    }
    let mode = *fields.get("ProxyType")?;
    if mode == "0" {
        return Some(ProxyDiscovery::one(Proxy::Direct, ProxySource::Kde));
    }
    let matched = bypass(
        fields.get("NoProxyFor").copied().unwrap_or(""),
        host,
        443,
        true,
    );
    let reversed = fields
        .get("ReversedException")
        .is_some_and(|s| *s == "true");
    if matched != reversed {
        return Some(ProxyDiscovery::one(Proxy::Direct, ProxySource::Kde));
    }
    if mode == "2" || mode == "3" {
        return Some(pac_unavailable());
    }
    if mode == "4" {
        // KDE can name the environment variables to read in this mode.
        let proxy_key = fields.get("httpsProxy").copied().unwrap_or("https_proxy");
        let bypass_key = fields.get("NoProxyFor").copied().unwrap_or("no_proxy");
        let mut selected = BTreeMap::new();
        if let Some(value) = env.get(proxy_key) {
            selected.insert("HTTPS_PROXY".into(), value.clone());
        }
        if let Some(value) = env.get(bypass_key) {
            selected.insert("NO_PROXY".into(), value.clone());
        }
        return environment(&selected, host);
    }
    if mode != "1" {
        return None;
    }
    let value = fields.get("httpsProxy").copied().unwrap_or("");
    if value.is_empty() {
        return Some(ProxyDiscovery::one(Proxy::Direct, ProxySource::Kde));
    }
    // KDE also stores `http://hostname port`.
    let normalized = value
        .rsplit_once(' ')
        .map(|(host, port)| format!("{host}:{port}"))
        .unwrap_or_else(|| value.into());
    let mut result = ProxyDiscovery::default();
    result.add(&normalized, ProxySource::Kde);
    Some(result)
}

#[cfg(target_os = "linux")]
pub(super) fn proxies(host: &str, deadline: Instant) -> ProxyDiscovery {
    use std::io::Read;
    let env: BTreeMap<_, _> = std::env::vars_os()
        .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
        .collect();
    if let Some(result) = environment(&env, host) {
        return result;
    }
    let config = env
        .get("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            env.get("HOME")
                .map(|home| std::path::PathBuf::from(home).join(".config"))
        });
    let mut config_dirs: Vec<_> = env
        .get("XDG_CONFIG_DIRS")
        .map(|value| std::env::split_paths(value).collect())
        .unwrap_or_else(|| vec![std::path::PathBuf::from("/etc/xdg")]);
    config_dirs.reverse();
    config_dirs.extend(config);
    let mut text = String::new();
    for dir in config_dirs
        .into_iter()
        .filter(|dir| dir.is_absolute())
        .take(32)
    {
        if Instant::now() >= deadline {
            break;
        }
        if let Ok(file) = std::fs::File::open(dir.join("kioslaverc")) {
            text.push('\n');
            let _ = file.take(65536).read_to_string(&mut text);
        }
    }
    let kde = kioslaverc(&text, host, &env);
    let desktop = env
        .get("XDG_CURRENT_DESKTOP")
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_default();
    if desktop.contains("kde") {
        if let Some(result) = kde {
            return result;
        }
    }
    let gnome = super::command(
        "gsettings",
        &["list-recursively", "org.gnome.system.proxy"],
        deadline,
    )
    .and_then(|s| gsettings(&s, host));
    if desktop.contains("gnome") || desktop.contains("unity") {
        if let Some(result) = gnome {
            return result;
        }
    } else if let Some(result) = gnome.filter(|r| {
        r.candidates
            .first()
            .is_none_or(|c| c.proxy != Proxy::Direct)
    }) {
        return result;
    }
    kde.unwrap_or_else(|| ProxyDiscovery::one(Proxy::Direct, ProxySource::Unconfigured))
}
