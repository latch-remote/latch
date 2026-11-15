//! The portal's restore tokens, kept between sessions so that the owner
//! approves sharing once rather than at every connection.
//!
//! A token is good for one session: starting a session with it gives a new
//! one, which replaces it here. The file is the owner's alone (0600), in
//! `$XDG_STATE_HOME/latch` (`~/.local/state/latch`), or where
//! `$LATCH_WAYLAND_TOKEN_FILE` says; the window's setup and the service
//! share it.

use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Which portal interface a token restores. A remote-desktop session
/// (screen and input together) and a screencast-only one are approved,
/// and restored, separately.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    RemoteDesktop,
    ScreenCast,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tokens {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_desktop: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub screen_cast: Option<String>,
}

impl Tokens {
    pub fn get(&self, kind: Kind) -> Option<&str> {
        match kind {
            Kind::RemoteDesktop => self.remote_desktop.as_deref(),
            Kind::ScreenCast => self.screen_cast.as_deref(),
        }
        .filter(|t| valid(t))
    }

    pub fn set(&mut self, kind: Kind, token: Option<String>) {
        let token = token.filter(|t| valid(t));
        match kind {
            Kind::RemoteDesktop => self.remote_desktop = token,
            Kind::ScreenCast => self.screen_cast = token,
        }
    }
}

/// Portals hand out UUIDs; anything long or with control characters is
/// not one, and is not sent back.
fn valid(token: &str) -> bool {
    !token.is_empty() && token.len() <= 256 && token.bytes().all(|b| b.is_ascii_graphic())
}

/// Where the tokens are kept.
pub fn path() -> Option<PathBuf> {
    path_from(
        std::env::var_os("LATCH_WAYLAND_TOKEN_FILE").map(PathBuf::from),
        std::env::var_os("XDG_STATE_HOME").map(PathBuf::from),
        std::env::var_os("HOME").map(PathBuf::from),
    )
}

fn path_from(
    explicit: Option<PathBuf>,
    state: Option<PathBuf>,
    home: Option<PathBuf>,
) -> Option<PathBuf> {
    if let Some(file) = explicit.filter(|p| !p.as_os_str().is_empty()) {
        return Some(file);
    }
    // XDG: a relative $XDG_STATE_HOME is invalid and ignored.
    let state = state.filter(|p| p.is_absolute()).or_else(|| {
        home.filter(|h| h.is_absolute())
            .map(|h| h.join(".local/state"))
    })?;
    Some(state.join("latch").join("wayland-portal.json"))
}

/// What the file holds; nothing when it is missing or unreadable (the
/// owner is asked again, which is all a lost token costs).
pub fn load(path: &Path) -> Tokens {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
            tracing::warn!("ignoring {}: {e}", path.display());
            Tokens::default()
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Tokens::default(),
        Err(e) => {
            tracing::warn!("cannot read {}: {e}", path.display());
            Tokens::default()
        }
    }
}

/// Replaces the file whole: written beside it, readable only by its
/// owner, then renamed over it, so a reader never sees half of one.
pub fn save(path: &Path, tokens: &Tokens) -> anyhow::Result<()> {
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let staged = path.with_extension(format!("{:016x}.tmp", rand::random::<u64>()));
    let result = (|| {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&staged)?;
        std::io::Write::write_all(&mut file, &serde_json::to_vec_pretty(tokens)?)?;
        file.sync_all()?;
        std::fs::rename(&staged, path)?;
        anyhow::Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&staged);
    }
    result.with_context(|| format!("save {}", path.display()))
}

/// Serializes restoring and rotating tokens across the panel and service.
/// An approval already in progress returns a retry error immediately.
pub(super) fn lock(path: &Path) -> anyhow::Result<std::fs::File> {
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)?;
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path.with_extension("lock"))?;
    file.try_lock()
        .context("another Latch process is approving screen sharing; retry when it finishes")?;
    Ok(file)
}

/// Applies `change` to what is on disk now (another process may have
/// replaced a token since this one read the file) and saves the result.
pub fn update(path: &Path, change: impl FnOnce(&mut Tokens)) -> anyhow::Result<()> {
    let _lock = lock(path)?;
    let mut tokens = load(path);
    let before = tokens.clone();
    change(&mut tokens);
    if tokens == before {
        return Ok(());
    }
    save(path, &tokens)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch() -> PathBuf {
        std::env::temp_dir()
            .join(format!("latch-token-{:016x}", rand::random::<u64>()))
            .join("latch")
            .join("wayland-portal.json")
    }

    #[test]
    fn the_file_follows_xdg_state_home_or_the_override() {
        let root = std::env::temp_dir();
        let p = |s: &str| Some(root.join(s));
        assert_eq!(
            path_from(None, p("state"), p("home/a")),
            p("state/latch/wayland-portal.json")
        );
        assert_eq!(
            path_from(None, None, p("home/a")),
            p("home/a/.local/state/latch/wayland-portal.json")
        );
        assert_eq!(
            path_from(None, Some("relative".into()), p("home/a")),
            p("home/a/.local/state/latch/wayland-portal.json"),
            "a relative XDG_STATE_HOME is ignored"
        );
        assert_eq!(path_from(p("t.json"), p("state"), None), p("t.json"));
        assert_eq!(path_from(None, None, None), None);
        assert_eq!(
            path_from(Some(PathBuf::new()), None, Some("relative".into())),
            None
        );
    }

    #[test]
    fn tokens_survive_a_round_trip_and_each_kind_is_its_own() {
        let path = scratch();
        assert_eq!(load(&path), Tokens::default(), "no file: no tokens");
        update(&path, |t| t.set(Kind::RemoteDesktop, Some("aaaa-1".into()))).unwrap();
        update(&path, |t| t.set(Kind::ScreenCast, Some("bbbb-2".into()))).unwrap();
        let tokens = load(&path);
        assert_eq!(tokens.get(Kind::RemoteDesktop), Some("aaaa-1"));
        assert_eq!(tokens.get(Kind::ScreenCast), Some("bbbb-2"));
        // A session that started with a token gets a new one in its place.
        update(&path, |t| t.set(Kind::RemoteDesktop, Some("aaaa-3".into()))).unwrap();
        assert_eq!(load(&path).get(Kind::RemoteDesktop), Some("aaaa-3"));
        assert_eq!(load(&path).get(Kind::ScreenCast), Some("bbbb-2"));
        // One the portal no longer gives is forgotten.
        update(&path, |t| t.set(Kind::ScreenCast, None)).unwrap();
        assert_eq!(load(&path).get(Kind::ScreenCast), None);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "only the owner reads it");
        }
        let dir = path.parent().unwrap().parent().unwrap();
        let leftovers: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .flatten()
            .filter(|e| e.path() != path && e.path() != path.with_extension("lock"))
            .collect();
        assert!(leftovers.is_empty(), "no staged files left: {leftovers:?}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_damaged_file_or_token_is_treated_as_none() {
        let path = scratch();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"{not json").unwrap();
        assert_eq!(load(&path), Tokens::default());
        std::fs::write(&path, br#"{"remote_desktop":"has space","screen_cast":""}"#).unwrap();
        let tokens = load(&path);
        assert_eq!(tokens.get(Kind::RemoteDesktop), None);
        assert_eq!(tokens.get(Kind::ScreenCast), None);
        let mut t = Tokens::default();
        t.set(Kind::ScreenCast, Some("x".repeat(300)));
        assert_eq!(t, Tokens::default(), "an implausible token is not kept");
        std::fs::remove_dir_all(path.parent().unwrap().parent().unwrap()).unwrap();
    }

    #[test]
    fn restoration_is_serialized_and_the_lock_is_released_on_failure() {
        let path = scratch();
        let guard = lock(&path).unwrap();
        assert!(lock(&path).is_err());
        assert!(update(&path, |t| t.set(Kind::ScreenCast, Some("a".into()))).is_err());
        assert_eq!(load(&path), Tokens::default());
        drop(guard);
        update(&path, |t| t.set(Kind::ScreenCast, Some("a".into()))).unwrap();
        assert_eq!(load(&path).get(Kind::ScreenCast), Some("a"));
        std::fs::remove_dir_all(path.parent().unwrap().parent().unwrap()).unwrap();
    }
}
