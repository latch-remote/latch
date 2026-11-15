//! Which display server a Linux host shares: X11 or Wayland.
//!
//! `$LATCH_LINUX_BACKEND` chooses: `auto` (the default), `x11` or
//! `wayland`. Under `auto` a session that does not look like Wayland
//! (`$WAYLAND_DISPLAY` empty, `$XDG_SESSION_TYPE` not `wayland`) is X11,
//! exactly as before. A Wayland session is shared through the portal
//! unless `$DISPLAY` reaches an X server that is not rootless Xwayland
//! (a separate Xorg, such as the headless display of `deploy/native`),
//! which keeps the X11 path it always had.

use std::ffi::OsStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    X11,
    Wayland,
}

impl Backend {
    pub fn name(self) -> &'static str {
        match self {
            Backend::X11 => "X11",
            Backend::Wayland => "Wayland",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Preference {
    Auto,
    Only(Backend),
}

/// Reads `$LATCH_LINUX_BACKEND`; anything unrecognised is `auto`.
pub fn preference(value: Option<&str>) -> Preference {
    match value.map(str::trim) {
        Some(v) if v.eq_ignore_ascii_case("x11") || v.eq_ignore_ascii_case("xorg") => {
            Preference::Only(Backend::X11)
        }
        Some(v) if v.eq_ignore_ascii_case("wayland") || v.eq_ignore_ascii_case("portal") => {
            Preference::Only(Backend::Wayland)
        }
        _ => Preference::Auto,
    }
}

/// Whether `$WAYLAND_DISPLAY` and `$XDG_SESSION_TYPE` describe a Wayland
/// session. Only a hint: a separate Xorg server on `$DISPLAY` still works.
pub fn wayland_session(display: Option<&OsStr>, session_type: Option<&OsStr>) -> bool {
    display.is_some_and(|d| !d.is_empty())
        || session_type.is_some_and(|t| t.eq_ignore_ascii_case("wayland"))
}

/// What `$DISPLAY` reaches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum XServer {
    /// An X server with a desktop on its root window.
    Desktop,
    /// Rootless Xwayland: its root window holds no desktop.
    Xwayland,
    /// No X server answers.
    Unavailable,
}

/// The backend for this session. `x11` is asked only for a Wayland
/// session under `auto`, so an X11 desktop costs no extra connection.
pub fn choose(
    preference: Preference,
    wayland_session: bool,
    x11: impl FnOnce() -> XServer,
) -> Backend {
    match preference {
        Preference::Only(backend) => backend,
        Preference::Auto if !wayland_session => Backend::X11,
        Preference::Auto => match x11() {
            XServer::Desktop => Backend::X11,
            XServer::Xwayland | XServer::Unavailable => Backend::Wayland,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_variable_names_a_backend_or_leaves_it_automatic() {
        assert_eq!(preference(None), Preference::Auto);
        assert_eq!(preference(Some("auto")), Preference::Auto);
        assert_eq!(preference(Some("")), Preference::Auto);
        assert_eq!(preference(Some("nonsense")), Preference::Auto);
        assert_eq!(preference(Some("X11")), Preference::Only(Backend::X11));
        assert_eq!(preference(Some(" xorg ")), Preference::Only(Backend::X11));
        assert_eq!(
            preference(Some("Wayland")),
            Preference::Only(Backend::Wayland)
        );
        assert_eq!(
            preference(Some("portal")),
            Preference::Only(Backend::Wayland)
        );
    }

    #[test]
    fn recognises_wayland_sessions() {
        let os = |s| Some(OsStr::new(s));
        assert!(wayland_session(os("wayland-0"), None));
        assert!(wayland_session(None, os("wayland")));
        assert!(wayland_session(os(""), os("Wayland")));
        assert!(!wayland_session(None, None));
        assert!(!wayland_session(os(""), os("x11")));
        assert!(!wayland_session(None, os("tty")));
    }

    #[test]
    fn an_x11_session_stays_x11_without_probing() {
        let backend = choose(Preference::Auto, false, || {
            panic!("an X11 session needs no probe")
        });
        assert_eq!(backend, Backend::X11);
    }

    #[test]
    fn a_wayland_session_keeps_a_separate_xorg_and_shares_wayland_otherwise() {
        assert_eq!(
            choose(Preference::Auto, true, || XServer::Desktop),
            Backend::X11
        );
        assert_eq!(
            choose(Preference::Auto, true, || XServer::Xwayland),
            Backend::Wayland
        );
        assert_eq!(
            choose(Preference::Auto, true, || XServer::Unavailable),
            Backend::Wayland
        );
    }

    #[test]
    fn an_override_wins_without_probing() {
        let never = || panic!("an override needs no probe");
        assert_eq!(
            choose(Preference::Only(Backend::X11), true, never),
            Backend::X11
        );
        assert_eq!(
            choose(Preference::Only(Backend::Wayland), false, never),
            Backend::Wayland
        );
    }
}
