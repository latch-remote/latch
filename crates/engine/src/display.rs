//! The host's displays: which ones a viewer can choose to share
//! ([`list`]), and the desktop's size, for a host whose desktop follows its
//! viewer (a headless Linux X server, Windows' virtual display): set it to
//! the viewer's size for a session and put it back afterwards.

pub use latch_proto::control::HostDisplay;

/// What [`restore`] needs.
#[derive(Debug, Clone, PartialEq)]
pub struct Previous {
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
    #[cfg(windows)]
    pub(crate) generation: u64,
}

/// The displays this host can share, each with the id a stream request
/// names it by. At most one is the `default`, which a request naming no
/// display (or one that has gone) shares; on Linux none is, and such a
/// request shares the whole X screen.
pub fn list() -> anyhow::Result<Vec<HostDisplay>> {
    #[cfg(target_os = "linux")]
    return crate::linux::displays();
    #[cfg(windows)]
    return crate::windows::displays();
    #[cfg(target_os = "macos")]
    return Ok(crate::mac::displays());
}

/// Sets the captured display to `width × height` at `fps` Hz or the
/// nearest rate it offers. `display` is the one the viewer chose, as
/// [`list`] names it. `Ok(None)` when this host does not resize (a Mac, a
/// PC with a real monitor, a chosen display that is not the one it would
/// resize) or already has that size. On Windows, a display change denied
/// by the secure desktop can be deferred. Use the returned `Previous` as a
/// [`DesktopPreparation`](crate::server::DesktopPreparation) so the stream
/// starts the retry after capture opens and cancels it at cleanup.
pub fn prepare(
    width: u32,
    height: u32,
    fps: u32,
    display: Option<&str>,
) -> anyhow::Result<Option<Previous>> {
    #[cfg(target_os = "linux")]
    return crate::linux::prepare_display(width, height, fps, display);
    #[cfg(windows)]
    return crate::windows::prepare_display(width, height, fps, display);
    #[cfg(target_os = "macos")]
    {
        let _ = (width, height, fps, display);
        Ok(None)
    }
}

/// Restore the previous mode and cancel a deferred preparation. Windows
/// defers the restore if the secure desktop is active and ignores it after
/// another preparation starts.
pub fn restore(previous: &Previous) -> anyhow::Result<()> {
    #[cfg(target_os = "linux")]
    return crate::linux::restore_display(previous);
    #[cfg(windows)]
    return crate::windows::restore_display(previous);
    #[cfg(target_os = "macos")]
    {
        let _ = previous;
        Ok(())
    }
}

/// Supersede any Windows restore before running a custom desktop prep command.
#[cfg(windows)]
pub fn begin_custom_prepare() {
    Previous::begin_custom_prepare();
}

#[cfg(test)]
mod tests {
    /// Whatever this machine has (a CI runner may have none it can read),
    /// the list is well formed: unique ids, at most one default.
    #[test]
    fn the_list_names_each_display_once_and_has_at_most_one_default() {
        let Ok(displays) = super::list() else {
            return;
        };
        eprintln!("{displays:#?}");
        let mut ids: Vec<_> = displays.iter().map(|d| d.id.as_str()).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), displays.len(), "ids are unique");
        assert!(displays.iter().filter(|d| d.default).count() <= 1);
        assert!(displays
            .iter()
            .all(|d| !d.id.is_empty() && !d.name.is_empty()));
    }
}
