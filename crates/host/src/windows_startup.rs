use anyhow::{Context, Result};
use std::io::ErrorKind;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use std::process::Command;
use windows::core::HSTRING;
use windows::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND, HANDLE, WAIT_OBJECT_0,
};
use windows::Win32::Security::{
    GetTokenInformation, TokenElevation, TokenElevationType, TokenElevationTypeFull,
    TokenLinkedToken, TokenStatistics, TOKEN_ELEVATION, TOKEN_ELEVATION_TYPE,
    TOKEN_INFORMATION_CLASS, TOKEN_LINKED_TOKEN, TOKEN_QUERY, TOKEN_STATISTICS,
};
use windows::Win32::System::Threading::{
    CreateEventW, CreateMutexW, GetCurrentProcess, OpenEventW, OpenMutexW, OpenProcessToken,
    SetEvent, WaitForSingleObject, EVENT_MODIFY_STATE, SYNCHRONIZATION_SYNCHRONIZE,
};

const HANDOVER: &str = r"Local\LatchHostHandover";
/// A permanent contract between this version and every later one: an
/// updated exe is started with this variable naming an event, and must
/// open the update guard and set that event before its parent lets go.
/// A later version that drops it can't be installed by this one's update
/// (the update fails and rolls back; the host stays reachable).
const HANDOVER_READY: &str = "LATCH_HANDOVER_READY";
/// How long a new exe has to acknowledge, long enough for antivirus to scan
/// an executable it has never seen.
const HANDOVER_ACK_MS: u32 = 30_000;

fn own(handle: HANDLE) -> OwnedHandle {
    // SAFETY: the Win32 call returned a newly owned, valid handle.
    unsafe { OwnedHandle::from_raw_handle(handle.0) }
}

fn current_token() -> Result<OwnedHandle> {
    let mut token = HANDLE::default();
    // SAFETY: the current process pseudo-handle is valid and token is an out pointer.
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }?;
    Ok(own(token))
}

// SAFETY: T must be the Windows struct for this information class.
unsafe fn token_info<T: Default>(token: &OwnedHandle, class: TOKEN_INFORMATION_CLASS) -> Result<T> {
    let mut value = T::default();
    // SAFETY: the token stays open and value is writable for the given size.
    unsafe {
        GetTokenInformation(
            HANDLE(token.as_raw_handle()),
            class,
            Some((&mut value as *mut T).cast()),
            std::mem::size_of::<T>() as u32,
            &mut 0,
        )?;
    }
    Ok(value)
}

pub(crate) fn elevated() -> Result<bool> {
    // SAFETY: TokenElevation writes TOKEN_ELEVATION.
    let elevation: TOKEN_ELEVATION =
        unsafe { token_info(&current_token()?, TokenElevation) }.context("read token elevation")?;
    Ok(elevation.TokenIsElevated != 0)
}

fn logon_luid() -> Result<String> {
    let mut token = current_token()?;
    // SAFETY: TokenElevationType writes TOKEN_ELEVATION_TYPE.
    let elevation: TOKEN_ELEVATION_TYPE = unsafe { token_info(&token, TokenElevationType) }?;
    if elevation == TokenElevationTypeFull {
        // UAC gives the elevated and limited halves of a sign-in different
        // LUIDs. Use the limited half, which also runs the logon task.
        // SAFETY: TokenLinkedToken writes TOKEN_LINKED_TOKEN with an owned handle.
        let linked: TOKEN_LINKED_TOKEN = unsafe { token_info(&token, TokenLinkedToken) }
            .context("open the linked logon token")?;
        token = own(linked.LinkedToken);
    }
    // SAFETY: TokenStatistics writes TOKEN_STATISTICS.
    let stats: TOKEN_STATISTICS =
        unsafe { token_info(&token, TokenStatistics) }.context("read logon session")?;
    let id = stats.AuthenticationId;
    Ok(format!("{:08x}{:08x}", id.HighPart, id.LowPart))
}

fn stop_marker() -> Result<PathBuf> {
    Ok(latch_core::config::data_dir()?.join("stopped-logon"))
}

pub(crate) fn record_stop() -> Result<()> {
    std::fs::write(stop_marker()?, logon_luid()?).context("write the stop marker")
}

pub(crate) fn clear_stop() -> Result<()> {
    match std::fs::remove_file(stop_marker()?) {
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
        result => result.context("clear the stop marker"),
    }
}

pub(crate) fn stopped_this_logon() -> Result<bool> {
    marker_matches(&stop_marker()?, logon_luid()?)
}

fn marker_matches(marker: &Path, luid: String) -> Result<bool> {
    match std::fs::read(marker) {
        Ok(bytes) => Ok(bytes == luid.as_bytes()),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e).context("read the stop marker"),
    }
}

pub(crate) fn scheduled_should_exit(
    handover: impl FnOnce() -> Result<bool>,
    service_alive: impl FnOnce() -> bool,
    stopped: impl FnOnce() -> Result<bool>,
) -> Result<bool> {
    if handover()? || service_alive() {
        return Ok(true);
    }
    stopped()
}

/// Hold a reference to the mutex for its lifetime. No thread takes ownership
/// of the mutex itself, so the service and both children can hold it together.
pub(crate) fn hold_handover() -> Result<OwnedHandle> {
    acquire_handover(HANDOVER)
}

fn acquire_handover(name: &str) -> Result<OwnedHandle> {
    // SAFETY: the name lives through the call; the handle is not inheritable.
    unsafe { CreateMutexW(None, false, &HSTRING::from(name)) }
        .map(own)
        .context("hold the update handover guard")
}

pub(crate) fn handover_active() -> Result<bool> {
    handover_exists(HANDOVER)
}

fn handover_exists(name: &str) -> Result<bool> {
    // SAFETY: the name lives through the call; any returned handle is owned.
    match unsafe { OpenMutexW(SYNCHRONIZATION_SYNCHRONIZE, false, &HSTRING::from(name)) } {
        Ok(handle) => {
            drop(own(handle));
            Ok(true)
        }
        Err(e) if e.code() == ERROR_FILE_NOT_FOUND.to_hresult() => Ok(false),
        // An unelevated launch may see a guard created by an elevated host.
        Err(e) if e.code() == ERROR_ACCESS_DENIED.to_hresult() => Ok(true),
        Err(e) => Err(e).context("check the update handover guard"),
    }
}

/// A child signals only after acquiring the same guard. The parent keeps
/// its own handle until then, including when handing back a rolled-back exe.
pub(crate) fn hold_child_handover() -> Result<OwnedHandle> {
    let guard = hold_handover()?;
    if let Some(name) = std::env::var_os(HANDOVER_READY) {
        // SAFETY: the name lives through the call and the event handle is owned.
        let event = own(unsafe { OpenEventW(EVENT_MODIFY_STATE, false, &HSTRING::from(name)) }?);
        // SAFETY: the event handle is owned and open.
        unsafe { SetEvent(HANDLE(event.as_raw_handle())) }?;
    }
    Ok(guard)
}

pub(crate) fn spawn_handover(command: &mut Command) -> Result<std::process::Child> {
    let name = format!(
        r"Local\LatchHostHandoverReady-{:032x}",
        rand::random::<u128>()
    );
    // SAFETY: the name lives through the call and the event handle is owned.
    let event = own(unsafe { CreateEventW(None, true, false, &HSTRING::from(&name)) }?);
    let mut child = command.env(HANDOVER_READY, name).spawn()?;
    // SAFETY: the process owns the event for the entire wait.
    if unsafe { WaitForSingleObject(HANDLE(event.as_raw_handle()), HANDOVER_ACK_MS) }
        == WAIT_OBJECT_0
    {
        return Ok(child);
    }
    let _ = child.kill();
    let _ = child.wait();
    anyhow::bail!("the handover child did not acquire the update guard");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handover_guard_stays_up_until_the_last_holder_leaves() {
        let name = format!(
            r"Local\LatchThrowawayHandover-{:032x}",
            rand::random::<u128>()
        );
        assert!(!handover_exists(&name).unwrap());
        let service = acquire_handover(&name).unwrap();
        let watcher = acquire_handover(&name).unwrap();
        drop(service);
        assert!(handover_exists(&name).unwrap());
        let replacement = acquire_handover(&name).unwrap();
        drop(watcher);
        assert!(handover_exists(&name).unwrap());
        drop(replacement);
        assert!(!handover_exists(&name).unwrap());
    }

    #[test]
    fn stop_marker_only_matches_its_logon_session() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("stopped-logon");
        let ours = logon_luid().unwrap();
        assert!(!marker_matches(&marker, ours.clone()).unwrap());
        std::fs::write(&marker, &ours).unwrap();
        assert!(marker_matches(&marker, ours).unwrap());
        assert!(!marker_matches(&marker, "different logon".into()).unwrap());
    }

    #[test]
    fn scheduled_start_skips_startup_work_when_a_service_answers() {
        assert!(scheduled_should_exit(
            || Ok(false),
            || true,
            || panic!("already serving, no need to read the stop marker")
        )
        .unwrap());
        assert!(scheduled_should_exit(
            || Ok(true),
            || panic!("handover protects the port"),
            || panic!("handover already blocked startup")
        )
        .unwrap());
    }
}
