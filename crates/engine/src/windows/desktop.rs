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
    System::StationsAndDesktops::{
        CloseDesktop, GetUserObjectInformationW, OpenInputDesktop, SetThreadDesktop,
        DESKTOP_ACCESS_FLAGS, DESKTOP_CONTROL_FLAGS, HDESK, UOI_NAME,
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

struct Desktop {
    handle: HDESK,
    name: String,
}
impl Drop for Desktop {
    fn drop(&mut self) {
        // SAFETY: The handle is owned. Closing the thread's current desktop
        // fails harmlessly; it is replaced before it is dropped.
        unsafe {
            let _ = CloseDesktop(self.handle);
        }
    }
}

thread_local! {
    static CURRENT: RefCell<Option<Desktop>> = const { RefCell::new(None) };
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
    CURRENT.with(|current| {
        let mut current = current.borrow_mut();
        if current.as_ref().is_some_and(|c| c.name == desktop.name) {
            return Ok(false);
        }
        // SAFETY: The desktop handle is open; the previous one is closed
        // only after the thread has left it.
        unsafe { SetThreadDesktop(desktop.handle) }
            .with_context(|| format!("switch to the {} desktop", desktop.name))?;
        *current = Some(desktop);
        Ok(true)
    })
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
