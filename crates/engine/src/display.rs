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
/// resize) or already has that size.
pub fn prepare(
    width: u32,
    height: u32,
    fps: u32,
    display: Option<&str>,
) -> anyhow::Result<Option<Previous>> {
    #[cfg(target_os = "linux")]
    return crate::linux::prepare_display(width, height, fps, display);
    #[cfg(windows)]
    {
        finish_pending_restore()?;
        crate::windows::prepare_display(width, height, fps, display)
    }
    #[cfg(target_os = "macos")]
    {
        let _ = (width, height, fps, display);
        Ok(None)
    }
}

/// Restore the previous mode. Windows defers the restore if the secure desktop is active.
pub fn restore(previous: &Previous) -> anyhow::Result<()> {
    #[cfg(target_os = "linux")]
    return crate::linux::restore_display(previous);
    #[cfg(windows)]
    return deferred::restore(previous);
    #[cfg(target_os = "macos")]
    {
        let _ = previous;
        Ok(())
    }
}

/// Finish an older Windows restore before running a custom desktop prep command.
#[cfg(windows)]
pub fn finish_pending_restore() -> anyhow::Result<()> {
    deferred::finish()
}

#[cfg(windows)]
mod deferred {
    use super::Previous;
    use crate::{video::DesktopUnavailable, windows::restore_display};
    use anyhow::{Context, Result};
    use parking_lot::Mutex;
    use std::{sync::Arc, thread, time::Duration};

    static PENDING: Mutex<Option<Arc<Previous>>> = Mutex::new(None);
    const INTERVAL: Duration = Duration::from_secs(5);
    const ATTEMPTS: usize = 60;

    pub(super) fn restore(previous: &Previous) -> Result<()> {
        let mut pending = PENDING.lock();
        if pending.is_some() {
            return Ok(());
        }
        match restore_display(previous) {
            Err(e) if DesktopUnavailable::is(&e) => {
                tracing::warn!("desktop is inaccessible; deferring display restore: {e:#}");
                let restore = Arc::new(previous.clone());
                *pending = Some(restore.clone());
                thread::Builder::new()
                    .name("display-restore".into())
                    .spawn(move || retry(restore))
                    .context("start desktop restore worker")?;
                Ok(())
            }
            result => result,
        }
    }

    pub(super) fn finish() -> Result<()> {
        attempt(&mut PENDING.lock(), restore_display)
            .context("restore the previous session's desktop before starting a new session")
    }

    fn attempt(
        pending: &mut Option<Arc<Previous>>,
        restore_display: impl FnOnce(&Previous) -> Result<()>,
    ) -> Result<()> {
        let Some(previous) = pending else {
            return Ok(());
        };
        let result = restore_display(previous);
        if !result.as_ref().is_err_and(DesktopUnavailable::is) {
            *pending = None;
        }
        result
    }

    fn retry(restore: Arc<Previous>) {
        for _ in 0..ATTEMPTS {
            thread::sleep(INTERVAL);
            let mut pending = PENDING.lock();
            // A new stream may have already finished this restore and installed another.
            if !pending.as_ref().is_some_and(|p| Arc::ptr_eq(p, &restore)) {
                return;
            }
            match attempt(&mut pending, restore_display) {
                Ok(()) => {
                    tracing::info!("restored the desktop after it became accessible");
                    return;
                }
                Err(e) if DesktopUnavailable::is(&e) => {}
                Err(e) => {
                    tracing::warn!("could not restore the desktop: {e:#}");
                    return;
                }
            }
        }
        tracing::debug!("desktop restore retry window ended; the next stream will try again");
    }

    #[cfg(test)]
    #[test]
    fn a_pending_restore_keeps_the_original_mode_until_access_returns() {
        let original = Arc::new(Previous {
            width: 1920,
            height: 1080,
            refresh_hz: 60,
        });
        let mut pending = Some(original.clone());
        assert!(attempt(&mut pending, |_| Err(DesktopUnavailable.into())).is_err());
        assert!(Arc::ptr_eq(pending.as_ref().unwrap(), &original));
        attempt(&mut pending, |previous| {
            assert_eq!(previous, original.as_ref());
            Ok(())
        })
        .unwrap();
        assert!(pending.is_none());
        attempt(&mut pending, |_| panic!("the restore already finished")).unwrap();

        pending = Some(original);
        assert!(attempt(&mut pending, |_| anyhow::bail!("display was removed")).is_err());
        assert!(pending.is_none());
    }
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
