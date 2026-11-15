//! Public release manifests and verified downloads for both apps' updaters.
//! Source lives separately in a private repository. Consumer requests carry
//! no credentials, and each receiving host resolves its own manifest.

use anyhow::{anyhow, bail, Context, Result};
use semver::Version;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::io::{BufReader, Read, Write};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

#[cfg(target_os = "linux")]
pub mod linux;

pub const REPO: &str = "latch-remote/latch-releases";
pub const LATEST_MANIFEST: &str =
    "https://github.com/latch-remote/latch-releases/releases/latest/download/release.json";
pub const MAC_ASSET: &str = "latch-macos-arm64.tar.gz";
pub const WINDOWS_ASSET: &str = "latch-windows-x64.zip";
/// The host executable inside [`WINDOWS_ASSET`].
pub const HOST_EXE: &str = "latch-host.exe";

const TIMEOUT: Duration = Duration::from_secs(30);
const MAX_HOPS: usize = 5;
/// Nothing Latch ships comes near this.
const MAX_DOWNLOAD: u64 = 512 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub version: Version,
    pub tag: String,
    pub prerelease: bool,
    pub assets: Vec<Asset>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Asset {
    pub name: String,
    /// An immutable, version-tagged HTTPS download URL.
    pub url: String,
    pub size: u64,
    /// The published SHA-256, lowercase hex. Required for consumer updates.
    pub sha256: Option<String>,
}

impl Release {
    pub fn asset(&self, name: &str) -> Option<&Asset> {
        self.assets.iter().find(|a| a.name == name)
    }

    /// Worth installing over `current`? A pre-release is offered only to a
    /// machine already on a pre-release.
    pub fn is_newer_than(&self, current: &Version) -> bool {
        self.version > *current && (!self.prerelease || !current.pre.is_empty())
    }
}

/// The version this binary was built as; every crate shares the workspace's.
pub fn current() -> Version {
    Version::parse(env!("CARGO_PKG_VERSION")).expect("the crate version is semver")
}

/// Oldest host that will accept a POST to [`crate::api::UPDATE_PATH`].
pub fn first_update() -> Version {
    Version::parse(crate::api::FIRST_UPDATE_VERSION).expect("FIRST_UPDATE_VERSION is semver")
}

/// Whether a running host can take `latch-host.exe` from the Mac.
pub fn host_can_receive_update(running: &Version) -> bool {
    *running >= first_update()
}

/// Whether a peer takes a pushed `latch-host.exe` over `/v1/update`:
/// Windows only. `os` is the peer's `Status.os` (Tailscale's string), or
/// empty from a host older than 4.0, which was always Windows.
pub fn takes_pushed_host(os: &str) -> bool {
    os.is_empty() || os.eq_ignore_ascii_case("windows")
}

/// Kept for callers built against the old API. Public distribution never
/// reads configured tokens, the environment, git credentials, or the keychain.
pub fn token(_configured: Option<&str>) -> Option<String> {
    None
}

/// The newest public release. The legacy token parameter is ignored.
pub fn latest(_token: Option<&str>) -> Result<Release> {
    manifest(LATEST_MANIFEST)
}

/// Resolve the claimed version independently over TLS, including on receivers.
pub fn for_version(version: &Version, _token: Option<&str>) -> Result<Release> {
    let url = format!("https://github.com/{REPO}/releases/download/v{version}/release.json");
    let release = manifest(&url)?;
    if release.version != *version {
        bail!("the manifest does not match claimed version {version}");
    }
    Ok(release)
}

fn manifest(url: &str) -> Result<Release> {
    let mut body = Vec::new();
    let status = fetch(url, None, "application/json", &mut body)?;
    if status != 200 {
        bail!("could not fetch the public release manifest (HTTP {status})");
    }
    parse_manifest(&body)
}

/// Authenticate uploaded bytes against the independently resolved public manifest.
pub fn verify_host(release: &Release, claimed: &Version, bytes: &[u8]) -> Result<()> {
    verify_host_asset(release, claimed, bytes, HOST_EXE)
}

/// Authenticate a platform artifact using the receiver's public manifest.
pub fn verify_host_asset(
    release: &Release,
    claimed: &Version,
    bytes: &[u8],
    name: &str,
) -> Result<()> {
    if release.tag != format!("v{claimed}")
        || release.version != *claimed
        || !release.is_newer_than(&current())
    {
        bail!("release tag must match the claimed version and be newer than this host");
    }
    let asset = release.asset(name).ok_or_else(|| {
        anyhow!(
            "release {} has no published {name}; update the installation once by hand",
            release.tag
        )
    })?;
    require_digest(asset)?;
    if asset.size != bytes.len() as u64
        || !asset
            .sha256
            .as_ref()
            .is_some_and(|h| h.eq_ignore_ascii_case(&sha256_hex(bytes)))
    {
        bail!("the artifact does not match the manifest's published {name} SHA-256");
    }
    Ok(())
}

#[derive(Deserialize)]
struct ReleaseJson {
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    draft: bool,
    tag_name: String,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    assets: Vec<AssetJson>,
}

#[derive(Deserialize)]
struct AssetJson {
    name: String,
    url: String,
    #[serde(default)]
    size: u64,
    #[serde(default)]
    digest: Option<String>,
}

pub fn parse_release(json: &[u8]) -> Result<Release> {
    let r: ReleaseJson = serde_json::from_slice(json).context("parse the release")?;
    release_from_json(r)
}

/// Static release.json uses the existing release/asset fields plus an explicit
/// `version`. Every asset binds its name, byte length and SHA-256 to that tag.
pub fn parse_manifest(json: &[u8]) -> Result<Release> {
    let r: ReleaseJson = serde_json::from_slice(json).context("parse the release manifest")?;
    let version = r.version.as_deref().context("manifest has no version")?;
    let parsed = Version::parse(version).context("manifest version is not semver")?;
    if parsed.to_string() != version || r.tag_name != format!("v{version}") {
        bail!("manifest version and tag must match exactly");
    }
    if r.prerelease != !parsed.pre.is_empty() || r.assets.is_empty() {
        bail!("manifest must describe a published release with matching prerelease status");
    }
    let release = release_from_json(r)?;
    let mut names = std::collections::BTreeSet::new();
    for asset in &release.assets {
        if !names.insert(&asset.name) {
            bail!("manifest contains duplicate asset {}", asset.name);
        }
        validate_published_asset(asset, &release.tag)?;
    }
    Ok(release)
}

fn validate_published_asset(asset: &Asset, tag: &str) -> Result<()> {
    let version = tag
        .strip_prefix('v')
        .context("asset URL has no version tag")?;
    if Version::parse(version)?.to_string() != version
        || !asset
            .name
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_alphanumeric())
        || !asset
            .name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
        || asset.url
            != format!(
                "https://github.com/{REPO}/releases/download/{tag}/{}",
                asset.name
            )
    {
        bail!(
            "{} must use its exact version-tagged public download URL",
            asset.name
        );
    }
    require_digest(asset)?;
    if asset.size == 0 || asset.size > MAX_DOWNLOAD {
        bail!("{} has an invalid published size", asset.name);
    }
    Ok(())
}

fn release_from_json(r: ReleaseJson) -> Result<Release> {
    if r.draft {
        bail!("draft releases are not published updates");
    }
    let version = Version::parse(r.tag_name.trim_start_matches('v'))
        .with_context(|| format!("tag {:?} is not a version", r.tag_name))?;
    Ok(Release {
        version,
        tag: r.tag_name,
        prerelease: r.prerelease,
        assets: r
            .assets
            .into_iter()
            .map(|a| Asset {
                name: a.name,
                url: a.url,
                size: a.size,
                sha256: a
                    .digest
                    .as_deref()
                    .and_then(|d| d.strip_prefix("sha256:"))
                    .map(|h| h.to_ascii_lowercase()),
            })
            .collect(),
    })
}

/// Download `asset` to `dest`, checking the manifest's digest and size. The file
/// is written under a temporary name and renamed only when it checks out, so
/// an existing `dest` is always a complete, verified copy.
pub fn download(asset: &Asset, _token: Option<&str>, dest: &Path) -> Result<()> {
    download_with(asset, dest, |url, sink| {
        fetch(url, None, "application/octet-stream", sink)
    })
}

fn download_with(
    asset: &Asset,
    dest: &Path,
    fetch: impl FnOnce(&str, &mut dyn Write) -> Result<u16>,
) -> Result<()> {
    let prefix = format!("https://github.com/{REPO}/releases/download/");
    let tag = asset
        .url
        .strip_prefix(&prefix)
        .and_then(|s| s.split_once('/'))
        .map(|(tag, _)| tag)
        .context("asset has no pinned public download URL")?;
    validate_published_asset(asset, tag)?;
    if let Some(dir) = dest.parent() {
        std::fs::create_dir_all(dir)?;
    }
    static NEXT_DOWNLOAD: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let serial = NEXT_DOWNLOAD.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = dest.with_extension(format!("part-{}-{serial}", std::process::id()));
    let result = (|| -> Result<()> {
        let mut options = std::fs::File::options();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&tmp)?;
        let mut sink = Hashing {
            inner: &mut file,
            sha: Sha256::new(),
            written: 0,
        };
        let status = fetch(&asset.url, &mut sink)?;
        if status != 200 {
            bail!("GitHub answered HTTP {status} for {}", asset.name);
        }
        let got = hex(&sink.sha.finalize());
        let written = sink.written;
        if let Some(want) = &asset.sha256 {
            if !got.eq_ignore_ascii_case(want) {
                bail!("{} does not match its published SHA-256", asset.name);
            }
        }
        if written != asset.size {
            bail!(
                "{} is {written} bytes, the release says {}",
                asset.name,
                asset.size
            );
        }
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp, dest).with_context(|| dest.display().to_string())
    })();
    if let Err(e) = result {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

/// Check that `path` holds exactly the bytes the release describes. A cached
/// download is only as trustworthy as this check: the file may have been
/// truncated or replaced since it was written.
pub fn verify_asset(asset: &Asset, path: &Path) -> Result<()> {
    require_digest(asset)?;
    let mut file = std::fs::File::open(path).with_context(|| path.display().to_string())?;
    let mut sink = Hashing {
        inner: &mut std::io::sink(),
        sha: Sha256::new(),
        written: 0,
    };
    std::io::copy(&mut file, &mut sink)?;
    if asset.size == 0 || sink.written != asset.size {
        bail!(
            "{} is {} bytes, the release says {}",
            asset.name,
            sink.written,
            asset.size
        );
    }
    if let Some(want) = &asset.sha256 {
        if !hex(&sink.sha.finalize()).eq_ignore_ascii_case(want) {
            bail!("{} does not match its published SHA-256", asset.name);
        }
    }
    Ok(())
}

struct Hashing<'a, W: Write> {
    inner: &'a mut W,
    sha: Sha256,
    written: u64,
}

impl<W: Write> Write for Hashing<'_, W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.sha.update(&buf[..n]);
        self.written += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

/// The manifest's digest is the integrity check besides TLS. An asset without
/// one is refused: a same-size cache swap would otherwise become an update.
pub fn require_digest(asset: &Asset) -> Result<()> {
    match &asset.sha256 {
        Some(h) if h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit()) => Ok(()),
        _ => bail!(
            "{} has no valid SHA-256 in the release manifest; refusing to install it",
            asset.name
        ),
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// GET an approved public HTTPS URL. Redirects are checked at every hop and
/// legacy token arguments are ignored, including on the first request.
pub fn fetch(url: &str, token: Option<&str>, accept: &str, sink: &mut dyn Write) -> Result<u16> {
    fetch_with(url, token, accept, sink, connect)
}

fn fetch_with<S: Read + Write>(
    url: &str,
    _token: Option<&str>,
    accept: &str,
    sink: &mut dyn Write,
    mut connect: impl FnMut(&str) -> Result<S>,
) -> Result<u16> {
    crate::http::validate_header("Accept", accept)?;
    let mut url = url.to_string();
    for _ in 0..MAX_HOPS {
        let (host, path) = split_url(&url)?;
        if !matches!(
            host.as_str(),
            "github.com"
                | "release-assets.githubusercontent.com"
                | "objects.githubusercontent.com"
                | "github-releases.githubusercontent.com"
        ) {
            bail!("release download origin is not approved");
        }
        let mut tls = connect(&host)?;
        let head = format!(
            "GET {path} HTTP/1.1\r\nHost: {host}\r\nUser-Agent: latch/{}\r\nAccept: {accept}\r\nConnection: close\r\n",
            env!("CARGO_PKG_VERSION")
        ) + "\r\n";
        tls.write_all(head.as_bytes())
            .map_err(crate::http::io_err)?;
        tls.flush().map_err(crate::http::io_err)?;
        let mut reader = BufReader::new(tls);
        let (status, headers) = crate::http::read_response_head(&mut reader)?;
        if matches!(status, 301 | 302 | 303 | 307 | 308) {
            let loc = crate::http::header(&headers, "location")
                .ok_or_else(|| anyhow!("redirect without a location"))?;
            url = if loc.starts_with("https://") {
                loc.to_string()
            } else if loc.starts_with('/') && !loc.starts_with("//") {
                format!("https://{host}{loc}")
            } else {
                bail!("redirect to {loc:?} is not https");
            };
            continue;
        }
        // TLS peers report a truncated close as an error after the data;
        // the bytes read so far are the reply.
        let limit = if accept == "application/json" {
            1024 * 1024
        } else {
            MAX_DOWNLOAD
        };
        crate::http::read_response_body(&mut reader, &headers, sink, limit, true)?;
        return Ok(status);
    }
    bail!("too many redirects")
}

/// `https://host/path?query` into `(host, path-with-query)`.
fn split_url(url: &str) -> Result<(String, String)> {
    let rest = url
        .strip_prefix("https://")
        .ok_or_else(|| anyhow!("{url:?} is not an https URL"))?;
    let (host, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    if host.is_empty()
        || !host
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"-.".contains(&b))
        || path
            .bytes()
            .any(|b| !b.is_ascii_graphic() || b"\\#".contains(&b))
    {
        bail!("invalid HTTPS release URL");
    }
    Ok((host.to_string(), path.to_string()))
}

fn connect(host: &str) -> Result<impl Read + Write> {
    connect_with(host, |tcp| tcp)
}

fn connect_with<S: Read + Write>(
    host: &str,
    wrap: impl FnOnce(std::net::TcpStream) -> S,
) -> Result<rustls::StreamOwned<rustls::ClientConnection, S>> {
    let config = tls_config()?;
    let tcp = crate::http::connect((host, 443u16), TIMEOUT)
        .with_context(|| format!("connect to {host}"))?;
    tcp.set_read_timeout(Some(TIMEOUT))
        .map_err(crate::http::io_err)?;
    tcp.set_write_timeout(Some(TIMEOUT))
        .map_err(crate::http::io_err)?;
    let name = rustls::pki_types::ServerName::try_from(host.to_string())?;
    let conn = rustls::ClientConnection::new(config, name)?;
    Ok(rustls::StreamOwned::new(conn, wrap(tcp)))
}

// Matches Graviola 0.4.2's mandatory features before it can assert/abort.
fn tls_cpu_supported() -> bool {
    #[cfg(target_arch = "x86_64")]
    {
        std::arch::is_x86_feature_detected!("aes")
            && std::arch::is_x86_feature_detected!("pclmulqdq")
            && std::arch::is_x86_feature_detected!("bmi1")
            && std::arch::is_x86_feature_detected!("adx")
            && std::arch::is_x86_feature_detected!("avx")
            && std::arch::is_x86_feature_detected!("avx2")
    }
    #[cfg(target_arch = "aarch64")]
    {
        std::arch::is_aarch64_feature_detected!("neon")
            && std::arch::is_aarch64_feature_detected!("aes")
            && std::arch::is_aarch64_feature_detected!("pmull")
            && std::arch::is_aarch64_feature_detected!("sha2")
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    {
        false
    }
}

fn tls_config() -> Result<Arc<rustls::ClientConfig>> {
    tls_config_checked(tls_cpu_supported())
}

fn tls_config_checked(cpu_supported: bool) -> Result<Arc<rustls::ClientConfig>> {
    if !cpu_supported {
        bail!(
            "HTTPS updates require CPU features AES/PCLMULQDQ/BMI1/ADX/AVX/AVX2 on x86-64 or NEON/AES/PMULL/SHA2 on ARM64. Enable the required guest CPU features in your VM settings, or download and install the release manually from https://github.com/{REPO}/releases/latest"
        );
    }
    static CONFIG: std::sync::OnceLock<Arc<rustls::ClientConfig>> = std::sync::OnceLock::new();
    if let Some(c) = CONFIG.get() {
        return Ok(c.clone());
    }
    let roots = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    // Pure-Rust crypto provider using s2n-bignum verified assembly via Rust inline
    // assembly (core::arch::asm!), compiled directly by rustc without cc or nasm.
    let provider = rustls_graviola::default_provider();
    let cfg = rustls::ClientConfig::builder_with_provider(Arc::new(provider))
        .with_safe_default_protocol_versions()?
        .with_root_certificates(roots)
        .with_no_client_auth();
    let cfg = Arc::new(cfg);
    Ok(CONFIG.get_or_init(|| cfg).clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufRead;

    #[test]
    fn unsupported_crypto_cpus_return_an_actionable_error_before_provider_entry() {
        let error = tls_config_checked(false).unwrap_err().to_string();
        assert!(error.contains("CPU features"));
        assert!(error.contains("VM settings"));
        assert!(error.contains("/latch-releases/releases/latest"));
        if tls_cpu_supported() {
            tls_config().unwrap();
            // Even a previously initialized provider cannot bypass the guard.
            assert!(tls_config_checked(false).is_err());
        } else {
            assert!(tls_config().is_err());
        }
    }

    fn public_manifest() -> serde_json::Value {
        let version = Version::new(current().major + 1, 0, 0);
        let assets = [MAC_ASSET, HOST_EXE].map(|name| {
            serde_json::json!({
                "name": name,
                "url": format!("https://github.com/{REPO}/releases/download/v{version}/{name}"),
                "size": 3, "digest": format!("sha256:{}", sha256_hex(b"abc"))
            })
        });
        serde_json::json!({
            "version": version.to_string(), "tag_name": format!("v{version}"),
            "prerelease": false,
            "assets": assets
        })
    }

    fn parsed_manifest(value: &serde_json::Value) -> Result<Release> {
        parse_manifest(&serde_json::to_vec(value).unwrap())
    }

    #[test]
    fn manifests_bind_versions_names_urls_sizes_and_individual_hashes() {
        let valid = public_manifest();
        let release = parsed_manifest(&valid).unwrap();
        assert_eq!(release.asset(MAC_ASSET).unwrap().size, 3);
        for (field, value) in [
            ("version", serde_json::json!("6.0.0/../latest")),
            ("version", serde_json::json!("0.0.1")),
            ("tag_name", serde_json::json!("v999.0.0")),
            ("draft", serde_json::json!(true)),
            ("prerelease", serde_json::json!(true)),
        ] {
            let mut bad = valid.clone();
            bad[field] = value;
            assert!(parsed_manifest(&bad).is_err(), "{field}");
        }
        let mut bad = valid.clone();
        bad.as_object_mut().unwrap().remove("version");
        assert!(parsed_manifest(&bad).is_err());
        for url in [
            "http://github.com/latch-remote/latch-releases/releases/download/v6.0.0/x",
            "https://github.com.evil.example/x",
            "https://github.com@evil.example/x",
            "https://evil.example/x",
            "https://github.com/other/repo/releases/download/v6.0.0/x",
            "https://github.com/latch-remote/latch-releases/releases/latest/download/x",
            "https://github.com/latch-remote/latch-releases/releases/download/v0.0.1/x",
        ] {
            let mut bad = valid.clone();
            bad["assets"][0]["url"] = url.into();
            assert!(parsed_manifest(&bad).is_err(), "{url}");
        }
        for suffix in [
            "?secret=1",
            "#fragment",
            "/../other",
            "%2fother",
            "\r\nInjected: 1",
        ] {
            let mut bad = valid.clone();
            bad["assets"][0]["url"] = format!("{}{suffix}", release.assets[0].url).into();
            assert!(parsed_manifest(&bad).is_err());
        }
        for (field, value) in [
            ("name", serde_json::json!("../latch-macos-arm64.tar.gz")),
            ("size", serde_json::json!(0)),
            ("size", serde_json::json!(MAX_DOWNLOAD + 1)),
            ("size", serde_json::json!(-1)),
            ("size", serde_json::json!(1.5)),
            ("digest", serde_json::json!(null)),
            ("digest", serde_json::json!("sha256:1234")),
            (
                "digest",
                serde_json::json!(format!("sha256:{}", "g".repeat(64))),
            ),
            (
                "digest",
                serde_json::json!("md5:00000000000000000000000000000000"),
            ),
        ] {
            let mut bad = valid.clone();
            bad["assets"][0][field] = value;
            assert!(parsed_manifest(&bad).is_err(), "{field}");
        }
        // The next asset has a valid digest. It cannot supply this one's hash.
        let mut bad = valid.clone();
        bad["assets"][0].as_object_mut().unwrap().remove("digest");
        assert!(parsed_manifest(&bad).is_err());
        let mut bad = valid.clone();
        bad["assets"][1] = bad["assets"][0].clone();
        assert!(parsed_manifest(&bad).is_err());
    }

    #[test]
    fn failed_and_overlapping_downloads_preserve_verified_destination_bytes() {
        let dir =
            std::env::temp_dir().join(format!("latch-download-atomic-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dest = dir.join(HOST_EXE);
        let release = parsed_manifest(&public_manifest()).unwrap();
        let asset = release.asset(HOST_EXE).unwrap();
        std::fs::write(&dest, b"previous version").unwrap();
        for body in [b"ab".as_slice(), b"abd", b"abcd"] {
            assert!(download_with(asset, &dest, |_, sink| {
                sink.write_all(body)?;
                Ok(200)
            })
            .is_err());
            assert_eq!(std::fs::read(&dest).unwrap(), b"previous version");
            assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        }
        let mut wrong_size = asset.clone();
        wrong_size.size += 1;
        assert!(download_with(&wrong_size, &dest, |_, sink| {
            sink.write_all(b"abc")?;
            Ok(200)
        })
        .is_err());
        assert_eq!(std::fs::read(&dest).unwrap(), b"previous version");
        // Interleave two callers after the first has already opened its file.
        download_with(asset, &dest, |_, sink| {
            sink.write_all(b"a")?;
            download_with(asset, &dest, |_, other| {
                other.write_all(b"abc")?;
                Ok(200)
            })?;
            sink.write_all(b"bc")?;
            Ok(200)
        })
        .unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"abc");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    struct Reply {
        response: std::io::Cursor<Vec<u8>>,
        request: std::rc::Rc<std::cell::RefCell<Vec<u8>>>,
    }

    impl Read for Reply {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.response.read(buf)
        }
    }

    impl Write for Reply {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.request.borrow_mut().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn public_redirect_requests_never_carry_legacy_credentials() {
        let mut hosts = Vec::new();
        let mut requests = Vec::new();
        let mut responses = [
            b"HTTP/1.1 302 Found\r\nLocation: https://release-assets.githubusercontent.com/file?signature=public\r\n\r\n".to_vec(),
            b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\nabc".to_vec(),
        ].into_iter();
        let mut body = Vec::new();
        let status = fetch_with(
            LATEST_MANIFEST,
            Some("ghp_private_secret"),
            "application/json",
            &mut body,
            |host| {
                hosts.push(host.to_string());
                let request = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
                requests.push(request.clone());
                Ok(Reply {
                    response: std::io::Cursor::new(responses.next().unwrap()),
                    request,
                })
            },
        )
        .unwrap();
        assert_eq!(status, 200);
        assert_eq!(body, b"abc");
        assert_eq!(
            hosts,
            ["github.com", "release-assets.githubusercontent.com"]
        );
        for request in requests {
            let bytes = request.borrow();
            let text = std::str::from_utf8(&bytes).unwrap();
            assert!(!text.contains("Authorization"));
            assert!(!text.contains("ghp_private_secret"));
        }
        assert_eq!(token(Some("ghp_private_secret")), None);
    }

    #[test]
    fn redirects_cannot_escape_approved_https_origins() {
        for location in [
            "http://github.com/file",
            "https://evil.example/file",
            "//evil.example/file",
            "https://github.com@evil.example/file",
            "https://github.com:443/file",
            "https://github.com.evil.example/file",
            "https://github.com/\\evil",
        ] {
            let mut connections = 0;
            let result = fetch_with(
                LATEST_MANIFEST,
                Some("secret"),
                "application/json",
                &mut Vec::new(),
                |_| {
                    connections += 1;
                    assert_eq!(
                        connections, 1,
                        "must reject before connecting to {location}"
                    );
                    Ok(Reply {
                        response: std::io::Cursor::new(
                            format!("HTTP/1.1 302 Found\r\nLocation: {location}\r\n\r\n")
                                .into_bytes(),
                        ),
                        request: Default::default(),
                    })
                },
            );
            assert!(result.is_err(), "{location}");
        }
    }

    const RELEASE: &str = r#"{
      "tag_name": "v3.1.0", "prerelease": false,
      "assets": [
        {"name": "latch-macos-arm64.tar.gz", "url": "https://api.github.com/repos/x/y/releases/assets/1",
         "size": 5243666, "digest": "sha256:226CE7321E5185D10F6F83CD38916D25485D1734794119B279A72A9AF3CC4AB7"},
        {"name": "latch-windows-x64.zip", "url": "https://api.github.com/repos/x/y/releases/assets/2", "size": 28514670}
      ]
    }"#;

    #[test]
    fn host_authentication_requires_published_digest_and_matching_new_tag() {
        let v = Version::new(current().major + 1, 0, 0);
        let bytes = b"published executable";
        let json = serde_json::json!({"tag_name": format!("v{v}"), "assets": [{
            "name": HOST_EXE, "url": "https://api.github.com/unused", "size": bytes.len(),
            "digest": format!("sha256:{}", sha256_hex(bytes))
        }]});
        let rel = parse_release(&serde_json::to_vec(&json).unwrap()).unwrap();
        verify_host(&rel, &v, bytes).unwrap();
        assert!(verify_host(&rel, &v, b"malicious executable").is_err());
        assert!(verify_host(&rel, &current(), bytes).is_err());
        let mut bad = rel.clone();
        bad.assets[0].sha256 = None;
        assert!(verify_host(&bad, &v, bytes).is_err());
        bad.assets.clear();
        assert!(verify_host(&bad, &v, bytes)
            .unwrap_err()
            .to_string()
            .contains("by hand"));
        bad = rel.clone();
        bad.tag = "v999.0.0".into();
        assert!(verify_host(&bad, &v, bytes).is_err());
        bad = rel;
        bad.version = current();
        bad.tag = format!("v{}", current());
        assert!(verify_host(&bad, &current(), bytes).is_err());
    }

    #[test]
    fn release_json_is_parsed_and_compared() {
        let r = parse_release(RELEASE.as_bytes()).unwrap();
        assert_eq!(r.version, Version::new(3, 1, 0));
        assert_eq!(r.tag, "v3.1.0");
        let mac = r.asset(MAC_ASSET).unwrap();
        assert_eq!(mac.size, 5_243_666);
        assert_eq!(
            mac.sha256.as_deref(),
            Some("226ce7321e5185d10f6f83cd38916d25485d1734794119b279a72a9af3cc4ab7")
        );
        assert_eq!(r.asset(WINDOWS_ASSET).unwrap().sha256, None);
        require_digest(r.asset(MAC_ASSET).unwrap()).unwrap();
        assert!(require_digest(r.asset(WINDOWS_ASSET).unwrap()).is_err());
        assert!(r.asset("nope").is_none());
        assert!(r.is_newer_than(&Version::new(3, 0, 0)));
        assert!(!r.is_newer_than(&Version::new(3, 1, 0)));
        assert!(!r.is_newer_than(&Version::new(4, 0, 0)));
        // A pre-release goes only to machines already on one.
        let mut pre = r.clone();
        pre.prerelease = true;
        pre.version = Version::parse("3.2.0-beta.1").unwrap();
        assert!(!pre.is_newer_than(&Version::new(3, 1, 0)));
        assert!(pre.is_newer_than(&Version::parse("3.2.0-alpha.1").unwrap()));
        assert!(parse_release(br#"{"tag_name":"latest"}"#).is_err());
    }

    #[test]
    fn is_newer_than_offers_3_2_0_over_3_1_2_rc_5_and_not_equals() {
        let release = Release {
            version: Version::new(3, 2, 0),
            tag: "v3.2.0".into(),
            prerelease: false,
            assets: vec![],
        };
        assert!(release.is_newer_than(&Version::parse("3.1.2-rc.5").unwrap()));
        assert!(!release.is_newer_than(&Version::new(3, 2, 0)));
        assert_eq!(
            current(),
            Version::parse(env!("CARGO_PKG_VERSION")).unwrap()
        );
    }

    #[test]
    fn urls_split_and_headers_are_case_insensitive() {
        assert_eq!(
            split_url("https://api.github.com/repos/a/b?x=1").unwrap(),
            ("api.github.com".into(), "/repos/a/b?x=1".into())
        );
        assert_eq!(split_url("https://h").unwrap(), ("h".into(), "/".into()));
        assert!(split_url("http://h/").is_err());
        assert!(split_url("https:///x").is_err());
        let h = vec![("location".to_string(), "https://o/x".to_string())];
        assert_eq!(crate::http::header(&h, "Location"), Some("https://o/x"));
    }

    fn read_head<R: BufRead>(r: &mut R) -> Result<(u16, Vec<(String, String)>)> {
        crate::http::read_response_head(r)
    }

    fn read_body<R: BufRead>(
        r: &mut R,
        headers: &[(String, String)],
        sink: &mut dyn Write,
    ) -> Result<()> {
        crate::http::read_response_body(r, headers, sink, MAX_DOWNLOAD, true)
    }

    #[test]
    fn chunked_sized_and_unsized_bodies_are_read() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nTransfer-Encoding: chunked\r\n\r\n5;ext=1\r\nhello\r\n7\r\n, world\r\n0\r\nX-Trailer: 1\r\n\r\n";
        let mut r = BufReader::new(&raw[..]);
        let (status, headers) = read_head(&mut r).unwrap();
        assert_eq!(status, 200);
        assert_eq!(
            crate::http::header(&headers, "content-type"),
            Some("text/plain")
        );
        let mut out = Vec::new();
        read_body(&mut r, &headers, &mut out).unwrap();
        assert_eq!(out, b"hello, world");

        let raw =
            b"HTTP/1.1 302 Found\r\nLocation: https://x/y\r\nContent-Length: 3\r\n\r\nabcEXTRA";
        let mut r = BufReader::new(&raw[..]);
        let (status, headers) = read_head(&mut r).unwrap();
        assert_eq!(status, 302);
        let mut out = Vec::new();
        read_body(&mut r, &headers, &mut out).unwrap();
        assert_eq!(out, b"abc");

        let raw = b"HTTP/1.1 200 OK\r\n\r\nto the end";
        let mut r = BufReader::new(&raw[..]);
        let (_, headers) = read_head(&mut r).unwrap();
        let mut out = Vec::new();
        read_body(&mut r, &headers, &mut out).unwrap();
        assert_eq!(out, b"to the end");

        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 9\r\n\r\nshort";
        let mut r = BufReader::new(&raw[..]);
        let (_, headers) = read_head(&mut r).unwrap();
        assert!(read_body(&mut r, &headers, &mut Vec::new()).is_err());
    }

    #[test]
    fn only_windows_takes_a_pushed_host() {
        assert!(takes_pushed_host("windows"));
        assert!(takes_pushed_host("Windows"));
        assert!(
            takes_pushed_host(""),
            "a pre-4.0 host reports no OS and is Windows"
        );
        assert!(!takes_pushed_host("macOS"));
        assert!(!takes_pushed_host("linux"));
    }

    #[test]
    fn digests_are_lowercase_hex() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            current(),
            Version::parse(env!("CARGO_PKG_VERSION")).unwrap()
        );
        assert_eq!(first_update(), Version::new(3, 1, 0));
        assert!(!host_can_receive_update(&Version::new(3, 0, 0)));
        assert!(!host_can_receive_update(&Version::new(3, 0, 1)));
        assert!(host_can_receive_update(&Version::new(3, 1, 0)));
        assert!(host_can_receive_update(&Version::new(3, 2, 0)));
    }

    #[test]
    fn a_cached_asset_is_rechecked_before_use() {
        let dir = std::env::temp_dir().join(format!("latch-verify-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("x.bin");
        std::fs::write(&path, b"abc").unwrap();
        let mut asset = Asset {
            name: "x.bin".into(),
            url: String::new(),
            size: 3,
            sha256: Some(sha256_hex(b"abc").to_uppercase()),
        };
        assert!(verify_asset(&asset, &path).is_ok());
        assert!(verify_asset(&asset, &dir.join("missing")).is_err());
        std::fs::write(&path, b"abd").unwrap();
        assert!(verify_asset(&asset, &path).is_err());
        asset.sha256 = None;
        assert!(verify_asset(&asset, &path).is_err());
        asset.sha256 = Some(sha256_hex(b"abcd"));
        std::fs::write(&path, b"abcd").unwrap();
        assert!(verify_asset(&asset, &path).is_err());
        asset.size = 0;
        assert!(verify_asset(&asset, &path).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_bad_download_leaves_nothing_behind() {
        let dir = std::env::temp_dir().join(format!("latch-dl-{}", std::process::id()));
        let dest = dir.join("x.bin");
        let asset = Asset {
            name: "x.bin".into(),
            url: "https://127.0.0.1/nothing-listens-here".into(),
            size: 1,
            sha256: None,
        };
        assert!(download(&asset, None, &dest).is_err());
        assert!(!dest.exists());
        assert!(!dest.with_extension("part").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[ignore = "requires a published public release manifest over the network"]
    fn real_github_latest_release_fetch() {
        let rel = latest(None).expect("fetching the public release manifest should succeed");
        assert!(!rel.tag.is_empty(), "release tag must not be empty");
        assert!(!rel.assets.is_empty(), "release assets must not be empty");
    }
}
