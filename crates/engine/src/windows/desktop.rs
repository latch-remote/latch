//! The desktop that has input. Windows shows the lock screen, the sign-in
//! screen and administrator prompts on the `Winlogon` desktop, which only
//! SYSTEM may open. SendInput and Desktop Duplication act on the calling
//! thread's desktop, so the SYSTEM helper moves each of its threads to the
//! input desktop whenever Windows switches it.
use anyhow::{Context, Result};
use std::{
    cell::RefCell,
    sync::atomic::{AtomicBool, Ordering},
};
use windows::Win32::{
    Foundation::{GENERIC_ALL, HANDLE},
    System::{
        StationsAndDesktops::{
            CloseDesktop, GetThreadDesktop, GetUserObjectInformationW, OpenInputDesktop,
            SetThreadDesktop, DESKTOP_ACCESS_FLAGS, DESKTOP_CONTROL_FLAGS, DESKTOP_READOBJECTS,
            HDESK, UOI_NAME,
        },
        Threading::GetCurrentThreadId,
    },
};

/// Set once in the SYSTEM helper; the host itself never changes desktops.
static FOLLOW: AtomicBool = AtomicBool::new(false);

pub(super) fn enable_following() {
    FOLLOW.store(true, Ordering::Relaxed);
}

pub(super) fn following() -> bool {
    FOLLOW.load(Ordering::Relaxed)
}

/// A desktop handle this thread opened. Dropped only when the thread is
/// not on it: Windows refuses to close a thread's current desktop, and the
/// handle would stay open for the life of the process.
struct Desktop {
    handle: HDESK,
    name: String,
}
impl Drop for Desktop {
    fn drop(&mut self) {
        // SAFETY: The handle is owned, and no thread is on it (see above).
        unsafe {
            let _ = CloseDesktop(self.handle);
        }
    }
}

/// The desktop this thread moved to, and the one it started on.
struct Followed {
    /// From GetThreadDesktop: not to be closed.
    original: HDESK,
    current: Option<Desktop>,
}
impl Drop for Followed {
    /// As the thread exits: back to the desktop it started on, and only
    /// then close the one it moved to. If it cannot move back, the handle
    /// stays open rather than being closed under the thread.
    fn drop(&mut self) {
        let Some(current) = self.current.take() else {
            return;
        };
        // SAFETY: `original` is the desktop this thread started on, which
        // stays open as long as the process's window station does.
        if unsafe { SetThreadDesktop(self.original) }.is_ok() {
            drop(current);
        } else {
            std::mem::forget(current);
        }
    }
}

thread_local! {
    static CURRENT: RefCell<Option<Followed>> = const { RefCell::new(None) };
}

fn name(desktop: HDESK) -> Result<String> {
    let mut buf = [0u16; 256];
    let mut needed = 0;
    // SAFETY: The buffer's byte length is passed with it.
    unsafe {
        GetUserObjectInformationW(
            HANDLE(desktop.0),
            UOI_NAME,
            Some(buf.as_mut_ptr().cast()),
            (buf.len() * 2) as u32,
            Some(&mut needed),
        )
    }
    .context("read the input desktop's name")?;
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Ok(String::from_utf16_lossy(&buf[..len]))
}

/// Moves this thread to the desktop that has input, if that is another
/// one. Returns whether it moved. Only a thread without windows or hooks
/// can move; the helper's threads have neither.
pub(super) fn follow() -> Result<bool> {
    if !following() {
        return Ok(false);
    }
    // SAFETY: The returned handle is owned by Desktop and closed on drop.
    let handle = unsafe {
        OpenInputDesktop(
            DESKTOP_CONTROL_FLAGS(0),
            false,
            DESKTOP_ACCESS_FLAGS(GENERIC_ALL.0),
        )
    }
    .context("open the input desktop")?;
    let desktop = Desktop {
        handle,
        name: name(handle)?,
    };
    CURRENT.with(|followed| {
        let mut slot = followed.borrow_mut();
        if slot.is_none() {
            // SAFETY: Reads this thread's desktop; the handle is not owned.
            let original = unsafe { GetThreadDesktop(GetCurrentThreadId()) }
                .context("read this thread's desktop")?;
            *slot = Some(Followed {
                original,
                current: None,
            });
        }
        let followed = slot.as_mut().expect("set above");
        if followed
            .current
            .as_ref()
            .is_some_and(|c| c.name == desktop.name)
        {
            // `desktop`, a second handle to the one the thread is on, closes.
            return Ok(false);
        }
        // SAFETY: The desktop handle is open.
        unsafe { SetThreadDesktop(desktop.handle) }
            .with_context(|| format!("switch to the {} desktop", desktop.name))?;
        // The previous desktop closes here, now that the thread left it.
        followed.current = Some(desktop);
        Ok(true)
    })
}

/// The name of the desktop that has input: `Winlogon` for the secure
/// desktop, `Default` for the user's.
pub(super) fn input_name() -> Result<String> {
    // SAFETY: The handle is owned by Desktop, which closes it; this thread
    // never moves to it.
    let handle = unsafe { OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, DESKTOP_READOBJECTS) }
        .context("open the input desktop")?;
    let desktop = Desktop {
        handle,
        name: String::new(),
    };
    name(desktop.handle)
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_host_thread_never_changes_desktops() {
        // Only the SYSTEM helper enables following; for the host (and these
        // tests) follow() leaves the thread where it is.
        assert!(!super::following());
        assert!(!super::follow().unwrap());
        assert!(super::CURRENT.with(|c| c.borrow().is_none()));
    }
}
