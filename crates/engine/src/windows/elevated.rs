//! The helper for what a signed-in Latch cannot do by itself: control
//! windows that run as administrator, and show and control the secure
//! desktop (the lock screen, the sign-in screen, administrator prompts).
//!
//! Windows drops input that a process sends while a window of higher
//! integrity has focus (UIPI), and SendInput still reports it as sent. The
//! secure desktop is out of reach entirely: only SYSTEM may open it, send
//! input to it or duplicate it, so a stream used to end at every
//! administrator prompt and every lock.
//!
//! Turning this on gives one Windows user's Latch the power to answer
//! administrator prompts: whatever runs as that user and can start or
//! replace their Latch can do the same. Setup copies latch-host.exe into
//! `%ProgramFiles%\Latch\Input`, a folder only administrators can change,
//! and records beside it the user who asked for setup (`user-sid.txt`,
//! the unelevated user, not the administrator who approved the prompt)
//! and that user's host path (`host-path.txt`). It registers the [`TASK`]
//! task that runs that copy with [`HELPER_ARG`] as LocalSystem; the task's
//! own security lets that user start it, nothing more. The copy then plays
//! one of three roles, by its token:
//!
//! - The **broker**: SYSTEM in session 0, where the task starts it. The task
//!   takes no arguments, so the broker finds the sessions to serve itself:
//!   those where the recorded user runs the recorded host executable. Into
//!   each it starts the agent, its own executable and nothing else, with a
//!   copy of its SYSTEM token moved to that session, on `winsta0\default`.
//!   It looks again every few seconds, and when a host rings its wake
//!   event, while any agent runs; then it exits. The wake only hurries it:
//!   when the event's name is taken, it runs without one.
//! - The **agent**: SYSTEM in the user's session. It serves the named pipe
//!   `LatchInput-{session}`, whose security admits only SYSTEM and the
//!   recorded user, and accepts a connection only from that user's host
//!   executable in this session. Each of its threads follows the desktop
//!   that has input (see `desktop`). It injects input there, calls SendSAS
//!   for Ctrl+Alt+Delete, and on a second kind of connection duplicates an
//!   output for the host (see `frames`).
//! - **Elevated as the user**: what an older setup's task ran. It serves
//!   input as before; it cannot reach the secure desktop and refuses frames.
//!
//! Before it sends anything, the host checks who serves the pipe: the
//! protected copy, running as SYSTEM in this session (or, for input only,
//! as this very user elevated). The helper keeps what it pressed and lets
//! go of it when the stream's end closes the pipe.
use super::frames::{self, Frames, Want};
use super::input::Local;
use super::pipe::{Deadline, Pipe};
use crate::input::{DisplayRect, Event};
use anyhow::{bail, ensure, Context, Result};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsStr,
    io::{Read, Write},
    os::windows::{
        io::{FromRawHandle, OwnedHandle},
        process::CommandExt,
    },
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc, Arc,
    },
    thread,
    time::{Duration, Instant},
};
use windows::{
    core::{s, HSTRING, PCWSTR, PWSTR},
    Win32::{
        Foundation::{
            CloseHandle, GetLastError, LocalFree, BOOL, ERROR_ALREADY_EXISTS, FILETIME, HANDLE,
            HLOCAL, HWND, WAIT_OBJECT_0, WAIT_TIMEOUT,
        },
        Security::{
            Authorization::{
                ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
                SDDL_REVISION_1,
            },
            DuplicateTokenEx, GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation,
            SecurityImpersonation, SetKernelObjectSecurity, SetTokenInformation,
            TokenIntegrityLevel, TokenPrimary, TokenSessionId, TokenUser,
            DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES,
            TOKEN_ACCESS_MASK, TOKEN_ALL_ACCESS, TOKEN_DUPLICATE, TOKEN_MANDATORY_LABEL,
            TOKEN_QUERY, TOKEN_USER,
        },
        Storage::FileSystem::{
            FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, PIPE_ACCESS_DUPLEX,
        },
        System::{
            Diagnostics::ToolHelp::{
                CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
                TH32CS_SNAPPROCESS,
            },
            LibraryLoader::{GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_SYSTEM32},
            Pipes::{
                CreateNamedPipeW, GetNamedPipeClientProcessId, GetNamedPipeClientSessionId,
                GetNamedPipeServerProcessId, GetNamedPipeServerSessionId, PIPE_READMODE_BYTE,
                PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT,
            },
            Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD},
            RemoteDesktop::ProcessIdToSessionId,
            SystemInformation::{GetSystemDirectoryW, GetSystemTimePreciseAsFileTime},
            Threading::{
                CreateEventW, CreateProcessAsUserW, GetCurrentProcess, GetProcessTimes, OpenEventW,
                OpenProcess, OpenProcessToken, QueryFullProcessImageNameW, SetEvent,
                WaitForSingleObject, CREATE_NO_WINDOW, EVENT_MODIFY_STATE, PROCESS_ACCESS_RIGHTS,
                PROCESS_DUP_HANDLE, PROCESS_INFORMATION, PROCESS_NAME_WIN32,
                PROCESS_QUERY_LIMITED_INFORMATION, STARTUPINFOW,
            },
        },
        UI::WindowsAndMessaging::GetWindowThreadProcessId,
    },
};

/// The task setup registers; the host only ever runs it.
pub const TASK: &str = "LatchInput";
/// The argument that makes latch-host.exe the helper.
pub const HELPER_ARG: &str = "--input-helper";
/// Where setup puts the helper, under `%ProgramFiles%`.
pub const DIR: &str = r"Latch\Input";
/// Beside the helper: the user setup approved, and that user's host.
pub const USER_RECORD: &str = "user-sid.txt";
pub const HOST_RECORD: &str = "host-path.txt";
/// A build compiled with `LATCH_HELPER_TEST_DIR` set uses its own task,
/// pipe and folder, so it can be tried beside an installed Latch. Release
/// builds never set it.
const TEST_DIR: Option<&str> = option_env!("LATCH_HELPER_TEST_DIR");
const TEST_NAME: &str = "LatchSecureTest";
/// Both ends say this first; a different build's helper is refused.
const HELLO: &[u8; 8] = b"LATCHIN1";
const HELLO2: &[u8; 8] = b"LATCHIN2";
/// A connection for frames rather than input. An older helper closes it.
const HELLO_FRAMES: &[u8; 8] = b"LATCHFR1";
/// A request: the display rectangle, then one event, at most a text event.
const MAX_REQUEST: usize = 16 + 2 + latch_proto::input::MAX_TEXT;
/// The helper exits once no stream has used it for this long.
const IDLE: Duration = Duration::from_secs(5);
/// How long a stream waits for the helper it started.
const START_WAIT: Duration = Duration::from_secs(10);
/// How long either end waits for the other's greeting (and, for frames,
/// the request and its answer) before it hangs up.
const HANDSHAKE: Duration = Duration::from_secs(3);
/// How long the host waits for the helper's frames once it asked: the
/// helper opens a capture of the output first.
const GRANT_WAIT: Duration = Duration::from_secs(8);
/// How long the host waits for the helper to answer one input event. The
/// longest is Ctrl+Alt+Delete, which waits up to [`SAS_CHECK`].
const REPLY_WAIT: Duration = Duration::from_secs(3);
/// How long the helper waits for the rest of an input request once its
/// first byte came, and for the host to take the reply. Between requests it
/// waits as long as the stream lasts.
const REQUEST_WAIT: Duration = Duration::from_secs(3);
/// How long the agent waits for the secure desktop after SendSAS.
const SAS_CHECK: Duration = Duration::from_secs(1);
/// How often the broker looks for sessions that need an agent.
const RECONCILE: Duration = Duration::from_secs(3);
/// How often an injecting thread checks that it is on the input desktop
/// (it also checks whenever Windows refuses an event).
const DESKTOP_CHECK: Duration = Duration::from_millis(250);
const MEDIUM: u32 = 0x2000;
const HIGH: u32 = 0x3000;
const SYSTEM_SID: &str = "S-1-5-18";
const VK_DELETE: u16 = 0x2e;

/// The helper's process: dispatched before the host parses its arguments.
/// Returns false when this process is not the helper.
pub fn run_helper() -> bool {
    if std::env::args_os().nth(1).as_deref() != Some(OsStr::new(HELPER_ARG)) {
        return false;
    }
    // A helper writes no log: the user's data folder is one they could
    // point elsewhere. A failure just leaves the host on its own.
    let result = (|| {
        let role = role(&own_token(TOKEN_QUERY)?)?;
        trace(|| format!("role {role:?}"));
        match role {
            Role::Broker => broker(),
            Role::Agent => serve(true),
            Role::User => serve(false),
        }
    })();
    trace(|| format!("exit: {result:?}"));
    true
}

/// Test builds only (see [`TEST_DIR`]): a line in the helper's log there.
/// Release builds write nothing, as the helper never logs.
fn trace(message: impl FnOnce() -> String) {
    if let Some(dir) = TEST_DIR {
        use std::io::Write as _;
        if let Ok(mut log) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(Path::new(dir).join(format!("helper-{}.log", std::process::id())))
        {
            let _ = writeln!(log, "{:?} {}", std::time::SystemTime::now(), message());
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Role {
    Broker,
    Agent,
    User,
}
fn role_of(sid: &str, session: u32) -> Role {
    match (sid == SYSTEM_SID, session) {
        (true, 0) => Role::Broker,
        (true, _) => Role::Agent,
        (false, _) => Role::User,
    }
}
fn role(token: &Handle) -> Result<Role> {
    Ok(role_of(&user_sid(token)?, session(token)?))
}

/// `%ProgramFiles%\Latch\Input\latch-host.exe`.
pub fn helper_exe() -> Option<PathBuf> {
    if let Some(dir) = TEST_DIR {
        return Some(PathBuf::from(dir).join("latch-host.exe"));
    }
    let pf = std::env::var_os("ProgramFiles").filter(|d| !d.is_empty())?;
    Some(PathBuf::from(pf).join(DIR).join("latch-host.exe"))
}

/// Whether setup installed the helper, so the host may ask it for frames.
pub(super) fn frames_installed() -> bool {
    helper_exe().is_some_and(|exe| exe.is_file())
}

fn task_name() -> &'static str {
    if TEST_DIR.is_some() {
        TEST_NAME
    } else {
        TASK
    }
}

fn pipe_name(session: u32) -> String {
    let base = if TEST_DIR.is_some() {
        TEST_NAME
    } else {
        "LatchInput"
    };
    format!(r"\\.\pipe\{base}-{session}")
}

/// The broker's wake event, in the global namespace. Any local process may
/// create an event there, so the broker uses only one it created itself.
fn wake_name() -> String {
    let base = if TEST_DIR.is_some() {
        TEST_NAME
    } else {
        "LatchInput"
    };
    format!(r"Global\{base}Wake")
}

/// Whether `sid` names a person's account: a local or domain account
/// (`S-1-5-21-…`) or a Microsoft Entra one (`S-1-12-1-…`). Never SYSTEM,
/// a service or a group.
pub fn is_user_sid(sid: &str) -> bool {
    let Some(rest) = sid
        .strip_prefix("S-1-5-21-")
        .or_else(|| sid.strip_prefix("S-1-12-1-"))
    else {
        return false;
    };
    let parts: Vec<&str> = rest.split('-').collect();
    parts.len() == 4
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.len() <= 10 && p.bytes().all(|b| b.is_ascii_digit()))
        && parts.iter().all(|p| p.parse::<u32>().is_ok())
}

/// The pipe's security. The user (not as an administrator) may read and
/// write it but not create instances of it; its creator fully: SYSTEM for
/// the agent, administrators for a helper elevated as the user. The medium
/// label keeps sandboxed (low integrity) processes out.
fn pipe_sddl(user_sid: &str, system: bool) -> String {
    let creator = if system { "" } else { "(A;;GA;;;BA)" };
    format!("D:P(A;;GA;;;SY){creator}(A;;0x0012019b;;;{user_sid})S:(ML;;NW;;;ME)")
}

pub(super) struct Handle(pub(super) HANDLE);
impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: The handle is owned and closed once.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}
// SAFETY: Kernel handles may be used and closed from any thread.
unsafe impl Send for Handle {}

fn own_token(access: TOKEN_ACCESS_MASK) -> Result<Handle> {
    let mut token = HANDLE::default();
    // SAFETY: The pseudo-handle needs no closing; the token is owned.
    unsafe { OpenProcessToken(GetCurrentProcess(), access, &mut token) }
        .context("open own token")?;
    Ok(Handle(token))
}

fn open_process(pid: u32, access: PROCESS_ACCESS_RIGHTS) -> Result<Handle> {
    // SAFETY: The handle is owned and closed by Handle.
    Ok(Handle(unsafe { OpenProcess(access, false, pid)? }))
}

fn process_token(process: &Handle) -> Result<Handle> {
    let mut token = HANDLE::default();
    // SAFETY: The token is owned and closed by Handle.
    unsafe { OpenProcessToken(process.0, TOKEN_QUERY, &mut token)? };
    Ok(Handle(token))
}

/// A token's variable-length information, in a buffer aligned for it.
fn token_info(
    token: &Handle,
    class: windows::Win32::Security::TOKEN_INFORMATION_CLASS,
) -> Result<Vec<u64>> {
    let mut len = 0;
    // SAFETY: The first call only reports the size; the second writes at
    // most `len` bytes into a buffer of at least that size.
    unsafe {
        let _ = GetTokenInformation(token.0, class, None, 0, &mut len);
        ensure!(len > 0, "empty token information");
        let mut buf = vec![0u64; (len as usize).div_ceil(8)];
        GetTokenInformation(token.0, class, Some(buf.as_mut_ptr().cast()), len, &mut len)?;
        Ok(buf)
    }
}

fn integrity(token: &Handle) -> Result<u32> {
    let buf = token_info(token, TokenIntegrityLevel)?;
    // SAFETY: The buffer holds a TOKEN_MANDATORY_LABEL whose SID points
    // into the same buffer, which outlives these reads.
    unsafe {
        let label = &*buf.as_ptr().cast::<TOKEN_MANDATORY_LABEL>();
        let sid = label.Label.Sid;
        let count = *GetSidSubAuthorityCount(sid);
        ensure!(count > 0, "integrity SID without a level");
        Ok(*GetSidSubAuthority(sid, u32::from(count) - 1))
    }
}

fn session(token: &Handle) -> Result<u32> {
    let buf = token_info(token, TokenSessionId)?;
    Ok(buf[0] as u32)
}

fn user_sid(token: &Handle) -> Result<String> {
    let buf = token_info(token, TokenUser)?;
    // SAFETY: The buffer holds a TOKEN_USER whose SID points into it; the
    // string the conversion allocates is freed with LocalFree.
    unsafe {
        let user = &*buf.as_ptr().cast::<TOKEN_USER>();
        let mut text = PWSTR::null();
        ConvertSidToStringSidW(user.User.Sid, &mut text)?;
        let sid = text.to_string();
        LocalFree(HLOCAL(text.0.cast()));
        Ok(sid?)
    }
}

/// The user this process runs as: what the unelevated panel passes to the
/// elevated setup as the user to approve.
pub fn current_user_sid() -> Result<String> {
    user_sid(&own_token(TOKEN_QUERY)?)
}

fn image(process: &Handle) -> Result<PathBuf> {
    let mut buf = vec![0u16; 1024];
    let mut len = buf.len() as u32;
    // SAFETY: The buffer's length is passed and the handle is open.
    unsafe {
        QueryFullProcessImageNameW(
            process.0,
            PROCESS_NAME_WIN32,
            PWSTR(buf.as_mut_ptr()),
            &mut len,
        )?;
    }
    Ok(PathBuf::from(String::from_utf16_lossy(
        &buf[..len as usize],
    )))
}

fn same_file_name(a: &Path, b: &Path) -> bool {
    a.as_os_str()
        .to_string_lossy()
        .eq_ignore_ascii_case(&b.as_os_str().to_string_lossy())
}

fn filetime(t: FILETIME) -> u64 {
    u64::from(t.dwHighDateTime) << 32 | u64::from(t.dwLowDateTime)
}

/// Now, as process creation times are kept. Precise, so it is never
/// behind a creation time Windows took from its coarser clock in the same
/// tick.
fn now() -> u64 {
    // SAFETY: Only returns the time.
    filetime(unsafe { GetSystemTimePreciseAsFileTime() })
}

/// When the process behind `process` started.
fn created(process: &Handle) -> Result<u64> {
    let [mut created, mut exited, mut kernel, mut user] = [FILETIME::default(); 4];
    // SAFETY: Writes four locals; the handle is open.
    unsafe { GetProcessTimes(process.0, &mut created, &mut exited, &mut kernel, &mut user)? };
    Ok(filetime(created))
}

/// A process id names whichever process has it now. The process a pipe
/// names must already have run when the pipe was opened: one started
/// since then got a reused id, and is not the pipe's other end.
fn existed_by(created: u64, at: u64) -> bool {
    created <= at
}

/// Who a process runs as, from its token.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Identity {
    sid: String,
    session: u32,
    integrity: u32,
}
fn identity(process: &Handle) -> Result<Identity> {
    let token = process_token(process)?;
    Ok(Identity {
        sid: user_sid(&token)?,
        session: session(&token)?,
        integrity: integrity(&token)?,
    })
}

/// Who serves the input pipe, as far as the host trusts it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Server {
    /// The SYSTEM agent: input, Ctrl+Alt+Delete and frames.
    System,
    /// An older setup's helper, elevated as this very user: input only.
    ElevatedUser,
}

/// Whether the host may talk to a pipe server running as `server`: SYSTEM
/// in the host's own session, or this same user elevated. Anything else (a
/// copy of the helper that a user started, another user's process, a
/// process in another session) is refused before it receives anything.
fn judge_server(server: &Identity, own_sid: &str, own_session: u32) -> Result<Server> {
    ensure!(
        server.session == own_session,
        "the input pipe's server runs in session {}, not this one ({own_session})",
        server.session
    );
    if server.sid == SYSTEM_SID {
        return Ok(Server::System);
    }
    ensure!(
        server.sid == own_sid,
        "the input pipe's server runs as {}, neither SYSTEM nor this user",
        server.sid
    );
    ensure!(
        server.integrity >= HIGH,
        "the input pipe's server runs as this user but not elevated"
    );
    Ok(Server::ElevatedUser)
}

/// Whether the helper serves a client running as `client`: the approved
/// user in the helper's own session.
fn judge_client(client: &Identity, approved: &str, session: u32) -> Result<()> {
    ensure!(
        client.session == session,
        "client in session {}",
        client.session
    );
    ensure!(
        client.sid == approved,
        "client runs as {}, not the approved user",
        client.sid
    );
    Ok(())
}

/// The program that owns `window` and its integrity level, when this
/// process can read them.
pub(super) fn window_owner(window: HWND) -> Option<(String, u32)> {
    let mut pid = 0;
    // SAFETY: Reads the window's process id into a local.
    unsafe { GetWindowThreadProcessId(window, Some(&mut pid)) };
    if pid == 0 {
        return None;
    }
    let process = open_process(pid, PROCESS_QUERY_LIMITED_INFORMATION).ok()?;
    let level = integrity(&process_token(&process).ok()?).ok()?;
    let name = image(&process)
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_else(|| format!("process {pid}"));
    Some((name, level))
}

/// This process's integrity level (medium for a Latch started at sign-in).
pub(super) fn own_integrity() -> u32 {
    own_token(TOKEN_QUERY)
        .and_then(|t| integrity(&t))
        .unwrap_or(MEDIUM)
}

/// A record setup wrote beside this executable, in the
/// administrators-only folder.
fn record(name: &str) -> Result<String> {
    let path = std::env::current_exe()?.with_file_name(name);
    let text = std::fs::read_to_string(path).with_context(|| format!("read {name}"))?;
    Ok(text.trim().trim_start_matches('\u{feff}').trim().to_owned())
}

/// The host executable setup recorded.
fn recorded_host() -> Result<PathBuf> {
    let path = PathBuf::from(record(HOST_RECORD)?);
    ensure!(path.is_absolute(), "{HOST_RECORD} names no full path");
    Ok(path)
}

/// The one user setup approved: whoever asked for it, unelevated.
fn recorded_user() -> Result<String> {
    let sid = record(USER_RECORD)?;
    ensure!(is_user_sid(&sid), "{USER_RECORD} names no user");
    Ok(sid)
}

/// A running copy of the recorded host: its session and user.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Host {
    session: u32,
    sid: String,
}

/// Every process running exactly `expected`.
fn hosts(expected: &Path) -> Vec<Host> {
    let Some(file) = expected
        .file_name()
        .map(|f| f.to_string_lossy().into_owned())
    else {
        return Vec::new();
    };
    let mut found = Vec::new();
    // SAFETY: The snapshot is owned; each entry is sized before the walk.
    unsafe {
        let Ok(snapshot) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return found;
        };
        let snapshot = Handle(snapshot);
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut more = Process32FirstW(snapshot.0, &mut entry).is_ok();
        while more {
            let len = entry
                .szExeFile
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(entry.szExeFile.len());
            let name = String::from_utf16_lossy(&entry.szExeFile[..len]);
            if name.eq_ignore_ascii_case(&file) {
                let pid = entry.th32ProcessID;
                let host = (|| {
                    let process = open_process(pid, PROCESS_QUERY_LIMITED_INFORMATION)?;
                    ensure!(same_file_name(&image(&process)?, expected), "another copy");
                    let mut session = 0;
                    ProcessIdToSessionId(pid, &mut session)?;
                    let sid = user_sid(&process_token(&process)?)?;
                    Ok::<_, anyhow::Error>(Host { session, sid })
                })();
                if let Ok(host) = host {
                    if !found.contains(&host) {
                        found.push(host);
                    }
                }
            }
            more = Process32NextW(snapshot.0, &mut entry).is_ok();
        }
    }
    found
}

/// The users running `exe` (elevated setup: whether the user it was asked
/// to approve really runs the Latch that asked).
pub fn users_running(exe: &Path) -> Vec<String> {
    let mut users: Vec<String> = hosts(exe).into_iter().map(|h| h.sid).collect();
    users.sort();
    users.dedup();
    users
}

/// The sessions the broker starts an agent in: those where the approved
/// user runs the host. Another account's copy of the same path (a shared
/// PC, a folder every user can write to) never gets one.
fn approved_sessions(hosts: &[Host], approved: &str) -> Vec<u32> {
    let mut sessions: Vec<u32> = hosts
        .iter()
        .filter(|h| h.sid == approved && h.session != 0)
        .map(|h| h.session)
        .collect();
    sessions.sort_unstable();
    sessions.dedup();
    sessions
}

/// What the broker knows about its agents, apart from their handles.
#[derive(Debug, Default)]
struct Roster {
    /// Sessions with a running agent.
    running: BTreeSet<u32>,
    /// Sessions whose agent ended (idle, or it could not start): left alone
    /// until a host rings the wake event, so an agent that went idle is
    /// not started again and again while another session streams.
    quiet: BTreeSet<u32>,
    /// No wake event: a host could not ask again, so an ended agent is
    /// started again on the next pass instead of being left quiet.
    unwakeable: bool,
}
impl Roster {
    /// The approved sessions to start an agent in now: each at most once.
    fn to_start(&self, approved: &[u32]) -> Vec<u32> {
        approved
            .iter()
            .copied()
            .filter(|s| !self.running.contains(s) && !self.quiet.contains(s))
            .collect()
    }
    fn started(&mut self, session: u32) {
        self.running.insert(session);
    }
    fn ended(&mut self, session: u32) {
        self.running.remove(&session);
        if !self.unwakeable {
            self.quiet.insert(session);
        }
    }
    /// A host asked for its agent.
    fn woken(&mut self) {
        self.quiet.clear();
    }
    /// The broker's work is done once no agent runs.
    fn done(&self) -> bool {
        self.running.is_empty()
    }
}

/// The wake event's security: SYSTEM fully, the approved user may only
/// set it.
fn wake_sddl(user_sid: &str) -> String {
    format!("D:P(A;;GA;;;SY)(A;;0x00100002;;;{user_sid})")
}

fn security(sddl: &str) -> Result<(SECURITY_ATTRIBUTES, Descriptor)> {
    let sddl = HSTRING::from(sddl);
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    // SAFETY: Allocated by the call; freed by Descriptor.
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(sddl.as_ptr()),
            SDDL_REVISION_1,
            &mut descriptor,
            None,
        )?;
    }
    Ok((
        SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor.0,
            bInheritHandle: false.into(),
        },
        Descriptor(descriptor),
    ))
}

/// A security descriptor ConvertStringSecurityDescriptorToSecurityDescriptorW
/// allocated.
struct Descriptor(PSECURITY_DESCRIPTOR);
impl Drop for Descriptor {
    fn drop(&mut self) {
        // SAFETY: Allocated with LocalAlloc by the conversion; freed once.
        unsafe {
            LocalFree(HLOCAL(self.0 .0));
        }
    }
}

/// The wake event, created by this broker with [`wake_sddl`]. `None` when
/// the name is taken: another process's event, whose security is its own,
/// is never waited on.
fn wake_event(approved: &str) -> Option<Handle> {
    let created = (|| {
        let (attributes, _descriptor) = security(&wake_sddl(approved))?;
        let name = HSTRING::from(wake_name());
        // SAFETY: The attributes outlive the call; the event is owned.
        let event = Handle(unsafe { CreateEventW(Some(&attributes), false, false, &name)? });
        // SAFETY: Reads this thread's last error, which CreateEventW set.
        let existed = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
        ensure!(!existed, "another process holds {}", wake_name());
        anyhow::Ok(event)
    })();
    created
        .map_err(|e| trace(|| format!("no wake event, looking every few seconds: {e:#}")))
        .ok()
}

/// The task's process: starts an agent in each session where the approved
/// user runs the recorded host, and keeps doing so while any agent runs.
/// The task ignores a start while the broker runs, so a host whose agent
/// went idle rings the wake event instead.
fn broker() -> Result<()> {
    let expected = recorded_host()?;
    let approved = recorded_user()?;
    let exe = std::env::current_exe()?;
    let wake = wake_event(&approved);
    let mut agents: BTreeMap<u32, Handle> = BTreeMap::new();
    let mut roster = Roster {
        unwakeable: wake.is_none(),
        ..Default::default()
    };
    loop {
        agents.retain(|&session, agent| {
            // SAFETY: The process handle is owned.
            let alive = unsafe { WaitForSingleObject(agent.0, 0) } == WAIT_TIMEOUT;
            if !alive {
                roster.ended(session);
            }
            alive
        });
        let found = hosts(&expected);
        for session in roster.to_start(&approved_sessions(&found, &approved)) {
            match start_agent(&exe, session, &approved) {
                Ok(agent) => {
                    agents.insert(session, agent);
                    roster.started(session);
                }
                Err(e) => {
                    trace(|| format!("agent for session {session}: {e:#}"));
                    roster.ended(session);
                }
            }
        }
        if roster.done() {
            return Ok(());
        }
        let Some(wake) = &wake else {
            thread::sleep(RECONCILE);
            continue;
        };
        // SAFETY: The event is owned.
        if unsafe { WaitForSingleObject(wake.0, RECONCILE.as_millis() as u32) } == WAIT_OBJECT_0 {
            trace(|| "woken by a host".into());
            roster.woken();
        }
    }
}

/// The agent process's security. A SYSTEM token's default would let no
/// one else open it, and the host must read its image path to trust the
/// pipe: so SYSTEM fully, the user only that and waiting on it.
/// Administrators may also find and end it (setup stops it to replace the
/// file it runs from).
fn agent_process_sddl(user_sid: &str) -> String {
    format!("D:P(A;;GA;;;SY)(A;;0x00101001;;;BA)(A;;0x00101000;;;{user_sid})")
}

/// The agent's token's security: the user's host may read who the agent
/// runs as (TOKEN_QUERY), which is how it tells this agent from a copy of
/// the helper anyone else started. Nothing that could use the token.
fn agent_token_sddl(user_sid: &str) -> String {
    format!("D:P(A;;GA;;;SY)(A;;0x00020008;;;BA)(A;;0x00000008;;;{user_sid})")
}

/// Lets `user_sid` read this process's token (see [`agent_token_sddl`]).
fn share_identity(user_sid: &str) -> Result<()> {
    // WRITE_DAC on the token.
    let token = own_token(TOKEN_ACCESS_MASK(0x0004_0000))?;
    let (_, descriptor) = security(&agent_token_sddl(user_sid))?;
    // SAFETY: The token and the descriptor are open for the call.
    unsafe { SetKernelObjectSecurity(token.0, DACL_SECURITY_INFORMATION, descriptor.0) }
        .context("let the host read the agent's token")?;
    Ok(())
}

/// Starts `exe` (this very executable) as SYSTEM in `session`, on the
/// interactive window station's default desktop.
fn start_agent(exe: &Path, session: u32, user_sid: &str) -> Result<Handle> {
    let own = own_token(TOKEN_DUPLICATE | TOKEN_QUERY)?;
    let (attributes, _descriptor) = security(&agent_process_sddl(user_sid))?;
    start_agent_as(&own, exe, session, &attributes)
}

fn start_agent_as(
    own: &Handle,
    exe: &Path,
    session: u32,
    process_security: &SECURITY_ATTRIBUTES,
) -> Result<Handle> {
    let mut token = HANDLE::default();
    // SAFETY: Each handle is owned; the session id is a local u32; the
    // strings outlive the call; the thread handle is closed at once.
    unsafe {
        DuplicateTokenEx(
            own.0,
            TOKEN_ALL_ACCESS,
            None,
            SecurityImpersonation,
            TokenPrimary,
            &mut token,
        )
        .context("duplicate the SYSTEM token")?;
        let token = Handle(token);
        SetTokenInformation(
            token.0,
            TokenSessionId,
            std::ptr::from_ref(&session).cast(),
            std::mem::size_of::<u32>() as u32,
        )
        .context("move the token to the user's session")?;
        let mut desktop: Vec<u16> = "winsta0\\default\0".encode_utf16().collect();
        let startup = STARTUPINFOW {
            cb: std::mem::size_of::<STARTUPINFOW>() as u32,
            lpDesktop: PWSTR(desktop.as_mut_ptr()),
            ..Default::default()
        };
        let mut line: Vec<u16> = format!("\"{}\" {HELPER_ARG}\0", exe.display())
            .encode_utf16()
            .collect();
        let dir = HSTRING::from(exe.parent().context("helper folder")?.as_os_str());
        let mut process = PROCESS_INFORMATION::default();
        CreateProcessAsUserW(
            token.0,
            &HSTRING::from(exe.as_os_str()),
            PWSTR(line.as_mut_ptr()),
            Some(process_security),
            None,
            false,
            CREATE_NO_WINDOW,
            None,
            &dir,
            &startup,
            &mut process,
        )
        .context("start the agent in the user's session")?;
        let _ = CloseHandle(process.hThread);
        Ok(Handle(process.hProcess))
    }
}

/// The agent (`system`) or a helper elevated as the user: serves the
/// session's pipe until no stream has used it for [`IDLE`].
fn serve(system: bool) -> Result<()> {
    let token = own_token(TOKEN_QUERY)?;
    let session = session(&token)?;
    // Only setup writes these records in the administrators-only folder.
    let expected = recorded_host()?;
    let approved = recorded_user();
    let user = if system {
        let user = approved?;
        ensure!(
            approved_sessions(&hosts(&expected), &user).contains(&session),
            "the approved user's host does not run in this session"
        );
        share_identity(&user)?;
        super::desktop::enable_following();
        user
    } else {
        // An elevated helper's user is its own, and must be the approved
        // one when setup recorded one.
        let own = user_sid(&token)?;
        if let Ok(approved) = approved {
            ensure!(own == approved, "this helper runs as another user");
        }
        own
    };
    trace(|| format!("serving {} for {user}", pipe_name(session)));
    let name = HSTRING::from(pipe_name(session));
    // Kept for the process's life: every pipe instance is created with it.
    let (attributes, _descriptor) = security(&pipe_sddl(&user, system))?;
    let active = Arc::new(AtomicUsize::new(0));
    let last = Arc::new(parking_lot::Mutex::new(Instant::now()));
    {
        let (active, last) = (active.clone(), last.clone());
        thread::spawn(move || loop {
            thread::sleep(Duration::from_secs(5));
            if active.load(Ordering::Acquire) == 0 && last.lock().elapsed() >= IDLE {
                std::process::exit(0);
            }
        });
    }
    let user = Arc::new(user);
    let mut first = true;
    loop {
        // SAFETY: The name and attributes outlive the call. The first
        // instance fails if another process already owns this name.
        let handle = unsafe {
            CreateNamedPipeW(
                PCWSTR(name.as_ptr()),
                PIPE_ACCESS_DUPLEX
                    | FILE_FLAG_OVERLAPPED
                    | if first {
                        FILE_FLAG_FIRST_PIPE_INSTANCE
                    } else {
                        Default::default()
                    },
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                8,
                4096,
                4096,
                0,
                Some(&attributes),
            )
        };
        if handle.is_invalid() {
            trace(|| format!("create pipe: {}", windows::core::Error::from_win32()));
            if first {
                bail!(
                    "create the input pipe: {}",
                    windows::core::Error::from_win32()
                );
            }
            thread::sleep(Duration::from_millis(500));
            continue;
        }
        first = false;
        // SAFETY: The pipe handle is owned by the Pipe from here on.
        let mut pipe = match Pipe::from_handle(unsafe { OwnedHandle::from_raw_handle(handle.0) }) {
            Ok(pipe) => pipe,
            Err(_) => continue,
        };
        if pipe.accept().is_err() {
            continue;
        }
        let connected_at = now();
        let client = match accept_client(&pipe, session, &expected, &user, system, connected_at) {
            Ok(client) => client,
            Err(e) => {
                trace(|| format!("refused a client: {e:#}"));
                continue;
            }
        };
        active.fetch_add(1, Ordering::AcqRel);
        let (active, last, user) = (active.clone(), last.clone(), user.clone());
        thread::spawn(move || {
            let result = serve_connection(pipe, &client, &user, system);
            trace(|| format!("client done: {result:?}"));
            *last.lock() = Instant::now();
            active.fetch_sub(1, Ordering::AcqRel);
        });
    }
}

/// Checks the client that just connected (at `connected_at`): the
/// approved user's recorded host, in this session. Returns its process,
/// opened for the frames to be duplicated into.
fn accept_client(
    pipe: &Pipe,
    session: u32,
    expected: &Path,
    user: &str,
    system: bool,
    connected_at: u64,
) -> Result<Handle> {
    let mut client_session = u32::MAX;
    // SAFETY: Reads the connected client's session into a local.
    unsafe { GetNamedPipeClientSessionId(pipe.raw(), &mut client_session) }?;
    ensure!(
        client_session == session,
        "client in session {client_session}"
    );
    let mut pid = 0;
    // SAFETY: The pipe is connected and the output points to a local.
    unsafe { GetNamedPipeClientProcessId(pipe.raw(), &mut pid) }?;
    // The client's identity is read through the very handle the frames
    // are later duplicated into.
    let access = if system {
        PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_DUP_HANDLE
    } else {
        PROCESS_QUERY_LIMITED_INFORMATION
    };
    let client = open_process(pid, access).with_context(|| format!("open client {pid}"))?;
    // A process that started after it connected only reused its id.
    ensure!(
        existed_by(created(&client)?, connected_at),
        "client {pid} started after it connected"
    );
    let client_image = image(&client)?;
    ensure!(
        same_file_name(&client_image, expected),
        "client {pid} runs {}",
        client_image.display()
    );
    judge_client(&identity(&client)?, user, session)?;
    Ok(client)
}

enum Hello {
    Input,
    Frames,
}

/// Answers the client's greeting with the same one when this helper
/// speaks it; anything else ends the connection.
fn handshake(pipe: &mut (impl Read + Write), frames: bool) -> Result<Hello> {
    let mut hello = [0; HELLO.len()];
    pipe.read_exact(&mut hello)?;
    let kind = if &hello == HELLO || &hello == HELLO2 {
        Hello::Input
    } else if frames && &hello == HELLO_FRAMES {
        Hello::Frames
    } else {
        bail!("a different Latch build asked the helper")
    };
    pipe.write_all(&hello)?;
    Ok(kind)
}

fn serve_connection(mut pipe: Pipe, client: &Handle, user: &str, system: bool) -> Result<()> {
    // A client that connects and says nothing is not a stream, and must
    // not keep the helper from going idle.
    pipe.set_deadline(Some(Instant::now() + HANDSHAKE));
    match handshake(&mut pipe, system)? {
        Hello::Input => serve_input(pipe, system),
        Hello::Frames => frames::serve(pipe, client.0, user, HANDSHAKE),
    }
}

/// One stream's input until it closes the pipe (tests: with its greeting).
#[cfg(test)]
fn serve_client(mut pipe: impl Read + Write + Deadline) -> Result<()> {
    handshake(&mut pipe, false)?;
    serve_input(pipe, false)
}

/// Ctrl+Alt+Delete from the viewer. Windows ignores the chord when it is
/// injected; the agent asks for the secure attention sequence instead.
#[derive(Default)]
struct Sas {
    ctrl: bool,
    alt: bool,
    /// The Delete the chord pressed: its release is not injected either.
    swallowed: bool,
}
impl Sas {
    /// Whether `event` completes the chord. Notes the modifiers it sees,
    /// and says to swallow the chord's Delete press and release.
    fn sees(&mut self, event: &Event) -> (bool, bool) {
        use latch_proto::input::{MOD_ALT, MOD_CTRL};
        let Event::Key {
            vk,
            down,
            modifiers,
        } = event
        else {
            if matches!(event, Event::ReleaseAll) {
                *self = Self::default();
            }
            return (false, false);
        };
        match vk {
            0x11 | 0xa2 | 0xa3 => self.ctrl = *down,
            0x12 | 0xa4 | 0xa5 => self.alt = *down,
            &VK_DELETE if *down => {
                let chord = (self.ctrl || modifiers & MOD_CTRL != 0)
                    && (self.alt || modifiers & MOD_ALT != 0);
                if chord {
                    self.swallowed = true;
                    return (true, true);
                }
            }
            &VK_DELETE if std::mem::take(&mut self.swallowed) => return (false, true),
            _ => {}
        }
        (false, false)
    }
}

/// Windows' SoftwareSASGeneration policy: 1 lets services (SYSTEM) ask
/// for the secure attention sequence, 3 services and accessibility tools.
fn software_sas_allowed() -> bool {
    let mut value = 0u32;
    let mut size = std::mem::size_of::<u32>() as u32;
    // SAFETY: The output is a local u32 with its size.
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            windows::core::w!(r"SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System"),
            windows::core::w!("SoftwareSASGeneration"),
            RRF_RT_REG_DWORD,
            None,
            Some(std::ptr::from_mut(&mut value).cast()),
            Some(&mut size),
        )
    };
    status.is_ok() && matches!(value, 1 | 3)
}

fn is_secure_desktop(name: &str) -> bool {
    name.eq_ignore_ascii_case("Winlogon")
}

/// Asks Windows for the secure attention sequence, as Ctrl+Alt+Delete.
///
/// SendSAS is documented for services and for `uiAccess` applications
/// that are signed and installed in a secure folder. The agent is
/// neither: it runs as LocalSystem from a scheduled task, which Windows
/// accepts as a service under SoftwareSASGeneration 1 today. SendSAS
/// returns nothing, so if a Windows version stops accepting that, the call
/// silently does nothing; the agent checks that the secure desktop came up
/// and otherwise reports the failure to the host, which logs it.
fn send_sas() -> Result<()> {
    ensure!(
        software_sas_allowed(),
        "Ctrl+Alt+Delete from a viewer needs Windows' SoftwareSASGeneration policy; run Set up as administrator again"
    );
    let before = super::desktop::input_name().ok();
    // SAFETY: sas.dll is loaded from System32 only and stays loaded; the
    // export has the documented `VOID SendSAS(BOOL AsUser)` signature.
    unsafe {
        let library = LoadLibraryExW(
            windows::core::w!("sas.dll"),
            None,
            LOAD_LIBRARY_SEARCH_SYSTEM32,
        )
        .context("load sas.dll")?;
        let send = GetProcAddress(library, s!("SendSAS")).context("SendSAS is missing")?;
        let send: unsafe extern "system" fn(BOOL) = std::mem::transmute(send);
        send(false.into());
    }
    // Already on the secure desktop (the lock screen): nothing to see.
    if before.as_deref().is_none_or(is_secure_desktop) {
        return Ok(());
    }
    let deadline = Instant::now() + SAS_CHECK;
    while Instant::now() < deadline {
        thread::sleep(Duration::from_millis(50));
        if super::desktop::input_name().is_ok_and(|n| is_secure_desktop(&n)) {
            return Ok(());
        }
    }
    bail!("Windows ignored Ctrl+Alt+Delete from the helper: SendSAS did not bring up the secure desktop")
}

/// One stream's input until it closes the pipe. Dropping `local` lets go
/// of whatever this stream still holds. The agent (`system`) injects on
/// whichever desktop has input.
///
/// A stream may sit idle between requests for as long as it lasts, but a
/// request, once begun, and its reply each end within [`REQUEST_WAIT`]: a
/// client that stalls mid-message must not hold a pipe instance (shared
/// with the screen's connections) forever.
fn serve_input(mut pipe: impl Read + Write + Deadline, system: bool) -> Result<()> {
    let mut local = Local::default();
    let mut sas = Sas::default();
    let mut checked = Instant::now() - DESKTOP_CHECK;
    let mut buf = [0u8; MAX_REQUEST];
    loop {
        let mut len = [0; 2];
        pipe.set_deadline(None);
        if pipe.read_exact(&mut len[..1]).is_err() {
            return Ok(());
        }
        pipe.set_deadline(Some(Instant::now() + REQUEST_WAIT));
        pipe.read_exact(&mut len[1..])?;
        let len = usize::from(u16::from_le_bytes(len));
        ensure!(len <= MAX_REQUEST, "input request too long");
        pipe.read_exact(&mut buf[..len])?;
        // Framing is length-delimited. An unsupported message gets an error
        // reply while later requests can still release keys and inject input.
        let result = decode(&buf[..len]).and_then(|(event, display)| {
            if !system {
                return local.apply(&event, display);
            }
            if checked.elapsed() >= DESKTOP_CHECK {
                checked = Instant::now();
                let _ = super::desktop::follow();
            }
            match sas.sees(&event) {
                (true, _) => return send_sas(),
                (false, true) => return Ok(()),
                _ => {}
            }
            local.apply(&event, display).or_else(|e| {
                // Windows refuses input sent to a desktop that lost it.
                checked = Instant::now();
                if super::desktop::follow().unwrap_or(false) {
                    local.apply(&event, display)
                } else {
                    Err(e)
                }
            })
        });
        let reply = match result {
            Ok(()) => vec![0],
            Err(e) => {
                let mut message = format!("{e:#}").into_bytes();
                message.truncate(1024);
                let mut reply = vec![1];
                reply.extend((message.len() as u16).to_le_bytes());
                reply.extend(message);
                reply
            }
        };
        pipe.set_deadline(Some(Instant::now() + REQUEST_WAIT));
        pipe.write_all(&reply)?;
    }
}

fn helper_event(
    event: &Event,
    key_positions: bool,
    held: &mut std::collections::BTreeSet<u16>,
) -> Option<Event> {
    match event {
        Event::Key {
            vk,
            down,
            modifiers,
        } if !key_positions => {
            let target = latch_proto::keyboard::helper_legacy(*vk)?;
            if *down {
                held.insert(*vk);
            } else {
                held.remove(vk);
                if held
                    .iter()
                    .any(|&source| latch_proto::keyboard::helper_legacy(source) == Some(target))
                {
                    return None;
                }
            }
            Some(Event::Key {
                vk: target,
                down: *down,
                modifiers: *modifiers,
            })
        }
        Event::UnknownPosition { .. } => None,
        Event::ReleaseAll => {
            held.clear();
            Some(Event::ReleaseAll)
        }
        _ => Some(event.clone()),
    }
}

fn encode(event: &Event, display: DisplayRect) -> Vec<u8> {
    let mut out = vec![0, 0];
    out.extend(display.x.to_le_bytes());
    out.extend(display.y.to_le_bytes());
    out.extend(display.width.to_le_bytes());
    out.extend(display.height.to_le_bytes());
    event.write(&mut out);
    let len = (out.len() - 2) as u16;
    out[..2].copy_from_slice(&len.to_le_bytes());
    out
}

fn decode(bytes: &[u8]) -> Result<(Event, DisplayRect)> {
    ensure!(bytes.len() > 16, "short input request");
    let n = |i: usize| [bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]];
    let display = DisplayRect {
        x: i32::from_le_bytes(n(0)),
        y: i32::from_le_bytes(n(4)),
        width: u32::from_le_bytes(n(8)),
        height: u32::from_le_bytes(n(12)),
    };
    let (event, len) = Event::read(&bytes[16..]).context("malformed input event")?;
    ensure!(
        16 + len == bytes.len(),
        "trailing bytes after an input event"
    );
    Ok((event, display))
}

/// A verified helper closed the pipe instead of answering the greeting:
/// it does not speak this version (or this kind of connection).
#[derive(Debug)]
struct Refused;
impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the helper refused; it may be from an older setup")
    }
}

/// This process: the session whose pipe it opens, and the user it is.
struct Me {
    session: u32,
    sid: String,
}
impl Me {
    fn current() -> Result<Self> {
        let token = own_token(TOKEN_QUERY)?;
        Ok(Self {
            session: session(&token)?,
            sid: user_sid(&token)?,
        })
    }
}

/// Opens the session's pipe and checks who serves it (see
/// [`judge_server`]): the protected helper copy, running as SYSTEM in this
/// session or as this user elevated, and already running when the pipe
/// was opened. Then greets it with `version`, which it must echo in time.
fn open_pipe(me: &Me, expected: &Path, version: &[u8; 8]) -> Result<(Pipe, Server)> {
    let opened_at = now();
    let mut pipe = Pipe::open(&pipe_name(me.session))?;
    let mut server_session = u32::MAX;
    // SAFETY: The handle is the open pipe's; the id is written to a local.
    unsafe { GetNamedPipeServerSessionId(pipe.raw(), &mut server_session)? };
    ensure!(
        server_session == me.session,
        "the input pipe's server runs in session {server_session}"
    );
    let mut pid = 0;
    // SAFETY: As above.
    unsafe { GetNamedPipeServerProcessId(pipe.raw(), &mut pid)? };
    let process = open_process(pid, PROCESS_QUERY_LIMITED_INFORMATION)
        .context("open the input pipe's server")?;
    ensure!(
        existed_by(created(&process)?, opened_at),
        "the input pipe's server process started after the pipe was opened"
    );
    let image = image(&process)?;
    ensure!(
        same_file_name(&image, expected),
        "the input pipe belongs to {}, not {}",
        image.display(),
        expected.display()
    );
    let server = identity(&process).context("read who the input pipe's server runs as")?;
    let server = judge_server(&server, &me.sid, me.session)?;
    // Frame handles come only from the SYSTEM agent: an older helper,
    // elevated as this user, is not even asked.
    ensure!(
        version != HELLO_FRAMES || server == Server::System,
        "only the SYSTEM helper may share the screen"
    );
    pipe.set_deadline(Some(Instant::now() + HANDSHAKE));
    pipe.write_all(version)?;
    let mut hello = [0; HELLO.len()];
    pipe.read_exact(&mut hello).context(Refused)?;
    ensure!(
        &hello == version,
        "the input helper is a different Latch build"
    );
    pipe.set_deadline(None);
    Ok((pipe, server))
}

/// The host's end of the pipe.
pub(super) struct Client {
    pipe: Pipe,
    key_positions: bool,
    legacy_held: std::collections::BTreeSet<u16>,
}

impl Client {
    fn connect(me: &Me, expected: &Path) -> Result<Self> {
        // An old helper closes an unrecognised handshake. Reopen its pipe
        // using v1, validating server identity on both connections. Any
        // other failure (a server that is not the helper) stands as it is.
        match Self::connect_version(me, expected, HELLO2) {
            Err(e) if e.downcast_ref::<Refused>().is_some() => {
                Self::connect_version(me, expected, HELLO)
            }
            result => result,
        }
    }

    fn connect_version(me: &Me, expected: &Path, version: &[u8; 8]) -> Result<Self> {
        let (pipe, server) = open_pipe(me, expected, version)?;
        if server == Server::ElevatedUser {
            tracing::info!("an older setup's input helper serves this stream; the lock screen and administrator prompts need Set up as administrator again");
        }
        Ok(Self {
            pipe,
            key_positions: version == HELLO2,
            legacy_held: Default::default(),
        })
    }

    /// The outer error is the pipe's (the helper is gone, or did not
    /// answer in time); the inner one is the helper's (it could not inject).
    pub(super) fn apply(
        &mut self,
        event: &Event,
        display: DisplayRect,
    ) -> std::io::Result<Result<()>> {
        let Some(event) = helper_event(event, self.key_positions, &mut self.legacy_held) else {
            return Ok(Ok(()));
        };
        self.pipe.set_deadline(Some(Instant::now() + REPLY_WAIT));
        self.pipe.write_all(&encode(&event, display))?;
        let mut status = [0];
        self.pipe.read_exact(&mut status)?;
        match status[0] {
            0 => Ok(Ok(())),
            1 => {
                let mut len = [0; 2];
                self.pipe.read_exact(&mut len)?;
                let mut message = vec![0; usize::from(u16::from_le_bytes(len))];
                self.pipe.read_exact(&mut message)?;
                Ok(Err(anyhow::anyhow!(
                    "{}",
                    String::from_utf8_lossy(&message)
                )))
            }
            _ => Err(std::io::Error::other(
                "unexpected reply from the input helper",
            )),
        }
    }
}

/// Said once per process, not per stream.
static NOT_SET_UP: AtomicBool = AtomicBool::new(false);

/// Starts the helper and connects to it in the background, so a stream
/// never waits for it: until it answers, the host injects by itself.
pub(super) fn connect() -> mpsc::Receiver<Client> {
    let (tx, rx) = mpsc::channel();
    let _ = thread::Builder::new()
        .name("latch-input-helper".into())
        .spawn(move || {
            if let Some(client) = connect_now() {
                let _ = tx.send(client);
            }
        });
    rx
}

fn connect_now() -> Option<Client> {
    let me = Me::current().ok()?;
    let expected = helper_exe()?;
    if !expected.is_file() {
        if !NOT_SET_UP.swap(true, Ordering::Relaxed) {
            tracing::info!("windows that run as administrator, the lock screen and administrator prompts ignore a viewer until setup runs again (Set up as administrator)");
        }
        return None;
    }
    match with_helper(|| Client::connect(&me, &expected), |_| true) {
        Ok(client) => Some(client),
        Err(e) => {
            if !NOT_SET_UP.swap(true, Ordering::Relaxed) {
                tracing::warn!(
                    "the input helper for windows that run as administrator did not answer: {e:#}"
                );
            }
            None
        }
    }
}

/// Runs `attempt` against the helper, starting its task when it is not
/// running, for up to [`START_WAIT`] (plus one attempt, whose own waits
/// are bounded by [`HANDSHAKE`]). `retry` says whether an attempt's error
/// may mean it is still starting.
fn with_helper<T>(
    attempt: impl Fn() -> Result<T>,
    retry: impl Fn(&anyhow::Error) -> bool,
) -> Result<T> {
    // A helper an earlier stream started may still be running.
    let mut last = match attempt() {
        Ok(v) => return Ok(v),
        Err(e) if !retry(&e) => return Err(e),
        Err(e) => e,
    };
    let deadline = Instant::now() + START_WAIT;
    let mut started = None::<Instant>;
    while Instant::now() < deadline {
        // The task ignores a start while an instance runs; one that is just
        // exiting would leave nothing, so ask again now and then.
        if started.is_none_or(|t| t.elapsed() >= Duration::from_secs(2)) {
            start_task().context("start the helper's task; run setup again")?;
            started = Some(Instant::now());
        }
        thread::sleep(Duration::from_millis(100));
        match attempt() {
            Ok(v) => return Ok(v),
            Err(e) if !retry(&e) => return Err(e),
            Err(e) => last = e,
        }
    }
    Err(last.context("the helper did not answer"))
}

/// Whether connecting failed because no helper serves the pipe (yet),
/// rather than because the one serving it refused.
fn absent(error: &anyhow::Error) -> bool {
    error.downcast_ref::<std::io::Error>().is_some_and(|e| {
        e.kind() == std::io::ErrorKind::NotFound || e.raw_os_error() == Some(231)
        // ERROR_PIPE_BUSY
    })
}

/// Frames of the output `want` from the helper, starting it if needed,
/// and when the helper granted them. Blocks for up to [`START_WAIT`], a
/// handshake and [`GRANT_WAIT`].
pub(super) fn connect_frames(want: Want) -> Result<(Frames, Instant)> {
    let me = Me::current()?;
    let expected = helper_exe().context("no Program Files folder")?;
    ensure!(expected.is_file(), "the helper is not set up");
    with_helper(
        || {
            let (mut pipe, _) = open_pipe(&me, &expected, HELLO_FRAMES)?;
            pipe.set_deadline(Some(Instant::now() + GRANT_WAIT));
            pipe.write_all(&frames::encode_want(&want))?;
            let granted = frames::read_granted(&mut pipe)?;
            let at = Instant::now();
            pipe.set_deadline(None);
            Ok((Frames::open(pipe, granted)?, at))
        },
        absent,
    )
}

/// [`connect_frames`] on a thread of its own, so capture never waits.
pub(super) fn request_frames(want: Want) -> mpsc::Receiver<Result<(Frames, Instant)>> {
    let (tx, rx) = mpsc::channel();
    let spawned = thread::Builder::new()
        .name("latch-screen-helper".into())
        .spawn({
            let tx = tx.clone();
            move || {
                let _ = tx.send(connect_frames(want));
            }
        });
    if let Err(e) = spawned {
        let _ = tx.send(Err(e.into()));
    }
    rx
}

/// Asks for the helper: rings a running broker's wake event, and starts
/// the task, which Windows ignores while the broker runs.
fn start_task() -> Result<()> {
    let name = HSTRING::from(wake_name());
    // SAFETY: The handle is owned and closed by Handle.
    if let Ok(wake) = unsafe { OpenEventW(EVENT_MODIFY_STATE, false, &name) } {
        let wake = Handle(wake);
        // SAFETY: The event is open for EVENT_MODIFY_STATE.
        let _ = unsafe { SetEvent(wake.0) };
    }
    let mut dir = vec![0u16; 260];
    // SAFETY: The buffer's length is passed with it.
    let len = unsafe { GetSystemDirectoryW(Some(&mut dir)) } as usize;
    ensure!(len > 0 && len < dir.len(), "system folder");
    let schtasks = PathBuf::from(String::from_utf16_lossy(&dir[..len])).join("schtasks.exe");
    let out = std::process::Command::new(schtasks)
        .args(["/Run", "/TN", task_name()])
        .creation_flags(CREATE_NO_WINDOW.0)
        .output()
        .context("run schtasks")?;
    ensure!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr).trim()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v1_aliases_release_only_after_both_sources_and_unknowns_stay_local() {
        let mut held = Default::default();
        let key = |vk, down| Event::Key {
            vk,
            down,
            modifiers: 0,
        };
        assert!(helper_event(&key(0x0d, true), false, &mut held).is_some());
        assert!(helper_event(
            &key(latch_proto::keyboard::KEYPAD_ENTER, true),
            false,
            &mut held
        )
        .is_some());
        assert!(helper_event(&key(0x0d, false), false, &mut held).is_none());
        assert_eq!(
            helper_event(
                &key(latch_proto::keyboard::KEYPAD_ENTER, false),
                false,
                &mut held
            ),
            Some(key(0x0d, false))
        );
        assert!(held.is_empty());
        assert!(helper_event(
            &Event::UnknownPosition {
                usage: 0xffff,
                down: true,
                modifiers: 0
            },
            false,
            &mut held
        )
        .is_none());
    }

    #[test]
    fn v1_helper_receives_only_legacy_keys_and_v2_advertises_positions() {
        let display = DisplayRect::default();
        for case in latch_proto::keyboard::CASES {
            let event = Event::Key {
                vk: case.vk,
                down: true,
                modifiers: case.mask,
            };
            let old = encode(
                &helper_event(&event, false, &mut Default::default()).unwrap(),
                display,
            );
            assert_eq!(old[18], 4);
            assert_eq!(
                decode(&old[2..]).unwrap().0,
                helper_event(&event, false, &mut Default::default()).unwrap()
            );
            let new = encode(
                &helper_event(&event, true, &mut Default::default()).unwrap(),
                display,
            );
            assert_eq!(
                new[18],
                if latch_proto::keyboard::position(case.vk).is_some() {
                    8
                } else {
                    4
                }
            );
        }
    }

    #[test]
    fn both_handshakes_work_and_unknown_requests_leave_pipe_usable() {
        struct Pipe {
            input: std::io::Cursor<Vec<u8>>,
            output: Vec<u8>,
        }
        impl Read for Pipe {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                self.input.read(buf)
            }
        }
        impl Write for Pipe {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.output.extend(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        impl Deadline for Pipe {
            fn set_deadline(&mut self, _: Option<Instant>) {}
        }
        for hello in [HELLO, HELLO2] {
            let mut bytes = hello.to_vec();
            bytes.extend(17u16.to_le_bytes());
            bytes.extend([0; 16]);
            bytes.push(99);
            bytes.extend(encode(&Event::ReleaseAll, DisplayRect::default()));
            let mut pipe = Pipe {
                input: std::io::Cursor::new(bytes),
                output: Vec::new(),
            };
            serve_client(&mut pipe).unwrap();
            assert_eq!(&pipe.output[..8], hello);
            assert_eq!(pipe.output[8], 1);
            let error_len = u16::from_le_bytes([pipe.output[9], pipe.output[10]]) as usize;
            assert_eq!(pipe.output[11 + error_len], 0);
        }
    }

    #[test]
    fn a_request_once_begun_and_its_reply_end_in_time() {
        /// A client that sends what it has and then stalls: a read with a
        /// deadline times out, one without waits (here: the end).
        struct Stalling {
            input: std::io::Cursor<Vec<u8>>,
            output: Vec<u8>,
            deadline: Option<Instant>,
            log: Vec<(&'static str, usize, bool)>,
        }
        impl Read for Stalling {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                self.log.push(("read", buf.len(), self.deadline.is_some()));
                if self.input.position() as usize == self.input.get_ref().len()
                    && self.deadline.is_some()
                {
                    return Err(std::io::ErrorKind::TimedOut.into());
                }
                self.input.read(buf)
            }
        }
        impl Write for Stalling {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.log.push(("write", buf.len(), self.deadline.is_some()));
                self.output.extend(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        impl Deadline for Stalling {
            fn set_deadline(&mut self, deadline: Option<Instant>) {
                self.deadline = deadline;
            }
        }
        let request = encode(&Event::ReleaseAll, DisplayRect::default());
        let body = request.len() - 2;
        let stalled = |tail: &[u8]| {
            let mut bytes = HELLO2.to_vec();
            bytes.extend(&request);
            bytes.extend(tail);
            let mut pipe = Stalling {
                input: std::io::Cursor::new(bytes),
                output: Vec::new(),
                deadline: None,
                log: Vec::new(),
            };
            let result = serve_client(&mut pipe);
            (result, pipe)
        };
        // Idle after a whole request: the stream simply waits (and ends).
        let (result, pipe) = stalled(&[]);
        result.unwrap();
        assert_eq!(&pipe.output[8..], &[0]);
        assert_eq!(
            pipe.log[1..],
            [
                ("write", 8, false),
                ("read", 1, false),
                ("read", 1, true),
                ("read", body, true),
                ("write", 1, true),
                ("read", 1, false),
            ]
        );
        // Stalled after a length's first byte, or partway through the body:
        // the request times out instead of holding the instance.
        for tail in [&request[..1], &request[..5]] {
            let (result, _) = stalled(tail);
            let error = result.unwrap_err();
            assert_eq!(
                error.downcast_ref::<std::io::Error>().unwrap().kind(),
                std::io::ErrorKind::TimedOut,
                "{error:#}"
            );
        }
    }

    #[test]
    fn requests_round_trip_and_reject_extra_bytes() {
        let display = DisplayRect {
            x: -1920,
            y: 0,
            width: 1920,
            height: 1080,
        };
        for event in [
            Event::MouseMove { dx: -5, dy: 7 },
            Event::MousePosition {
                x: 1,
                y: 2,
                width: 3,
                height: 4,
            },
            Event::Text("é😀".into()),
            Event::ReleaseAll,
        ] {
            let bytes = encode(&event, display);
            assert_eq!(
                usize::from(u16::from_le_bytes([bytes[0], bytes[1]])),
                bytes.len() - 2
            );
            assert!(bytes.len() - 2 <= MAX_REQUEST);
            assert_eq!(decode(&bytes[2..]).unwrap(), (event, display));
            let mut longer = bytes[2..].to_vec();
            longer.push(0);
            assert!(decode(&longer).is_err());
        }
        assert!(decode(&[0; 16]).is_err());
        let mut unknown = vec![0; 16];
        unknown.push(99);
        assert!(decode(&unknown).is_err());
    }

    #[test]
    fn only_the_user_and_the_creator_can_open_the_pipe() {
        for system in [true, false] {
            let sddl = pipe_sddl("S-1-5-21-1-2-3-1001", system);
            assert!(sddl.starts_with("D:P(A;;GA;;;SY)"), "no inherited access");
            // Read and write, but not FILE_CREATE_PIPE_INSTANCE (0x4): the user
            // cannot put up a pipe instance of their own beside the helper's.
            assert!(sddl.contains("(A;;0x0012019b;;;S-1-5-21-1-2-3-1001)"));
            assert_eq!(0x0012019b & 0x4, 0);
            assert!(!sddl.contains(";WD)") && !sddl.contains(";AU)") && !sddl.contains(";IU)"));
            assert!(sddl.ends_with("S:(ML;;NW;;;ME)"));
            // The SYSTEM agent admits SYSTEM and the user, no one else; a
            // helper elevated as the user also needs administrators, whose
            // group owns the instances it creates.
            assert_eq!(sddl.contains("(A;;GA;;;BA)"), !system);
            assert_eq!(sddl.matches("(A;").count(), if system { 2 } else { 3 });
        }
        if TEST_DIR.is_none() {
            assert_eq!(pipe_name(1), r"\\.\pipe\LatchInput-1");
            assert_eq!(task_name(), TASK);
        }
    }

    #[test]
    fn only_the_user_may_look_the_agent_process_up() {
        let sddl = agent_process_sddl("S-1-5-21-1-2-3-1001");
        assert!(sddl.starts_with("D:P(A;;GA;;;SY)"));
        // PROCESS_QUERY_LIMITED_INFORMATION and SYNCHRONIZE, nothing that
        // could read its memory, inject into it or end it.
        assert!(sddl.ends_with("(A;;0x00101000;;;S-1-5-21-1-2-3-1001)"));
        // Administrators may also end it, as setup does to replace its file.
        assert!(sddl.contains("(A;;0x00101001;;;BA)"));
        assert_eq!(sddl.matches("(A;").count(), 3);
        for denied in [
            0x0001,
            0x0002,
            0x0008,
            0x0010,
            0x0020,
            0x0040,
            0x0400,
            0x0004_0000,
        ] {
            assert_eq!(0x0010_1000 & denied, 0, "{denied:#x}");
        }
        for denied in [0x0002, 0x0008, 0x0010, 0x0020, 0x0040, 0x0400, 0x0004_0000] {
            assert_eq!(0x0010_1001 & denied, 0, "{denied:#x}");
        }
    }

    #[test]
    fn the_user_may_only_read_who_the_agent_runs_as() {
        let sddl = agent_token_sddl("S-1-5-21-1-2-3-1001");
        assert!(sddl.starts_with("D:P(A;;GA;;;SY)"));
        // TOKEN_QUERY alone: not duplicate, impersonate, assign or adjust.
        assert!(sddl.ends_with("(A;;0x00000008;;;S-1-5-21-1-2-3-1001)"));
        assert!(sddl.contains("(A;;0x00020008;;;BA)"));
        assert_eq!(sddl.matches("(A;").count(), 3);
        // The wake event: the user may set it and nothing else.
        assert_eq!(
            wake_sddl("S-1-5-21-1-2-3-1001"),
            "D:P(A;;GA;;;SY)(A;;0x00100002;;;S-1-5-21-1-2-3-1001)"
        );
        if TEST_DIR.is_none() {
            assert_eq!(wake_name(), r"Global\LatchInputWake");
        }
    }

    #[test]
    fn the_token_decides_the_role() {
        assert_eq!(role_of("S-1-5-18", 0), Role::Broker);
        assert_eq!(role_of("S-1-5-18", 1), Role::Agent);
        assert_eq!(role_of("S-1-5-18", 7), Role::Agent);
        // The user (an older setup's task, or anyone starting the copy by
        // hand) never gets the SYSTEM roles.
        assert_eq!(role_of("S-1-5-21-1-2-3-1001", 0), Role::User);
        assert_eq!(role_of("S-1-5-21-1-2-3-1001", 1), Role::User);
        assert_eq!(role_of("S-1-5-32-544", 1), Role::User);
    }

    #[test]
    fn agents_go_only_to_the_approved_users_sessions() {
        let host = |session, sid: &str| Host {
            session,
            sid: sid.into(),
        };
        let a = "S-1-5-21-1-2-3-1001";
        let b = "S-1-5-21-1-2-3-1002";
        let hosts = [host(2, a), host(1, a), host(2, a), host(0, a), host(3, b)];
        // Never session 0, each session once, never another account's.
        assert_eq!(approved_sessions(&hosts, a), vec![1, 2]);
        assert_eq!(approved_sessions(&hosts, b), vec![3]);
        assert_eq!(approved_sessions(&[], a), Vec::<u32>::new());
        // Another user's copy of the same path in a session of their own,
        // or beside the approved user's: only the approved user's counts.
        assert_eq!(approved_sessions(&[host(1, b)], a), Vec::<u32>::new());
        assert_eq!(approved_sessions(&[host(1, a), host(1, b)], a), vec![1]);
    }

    #[test]
    fn the_broker_starts_each_session_once_and_again_only_when_woken() {
        let mut roster = Roster::default();
        assert_eq!(roster.to_start(&[1, 2]), vec![1, 2]);
        roster.started(1);
        roster.started(2);
        // Already running: no duplicate.
        assert_eq!(roster.to_start(&[1, 2]), Vec::<u32>::new());
        // A session that appears later gets one on the next pass.
        assert_eq!(roster.to_start(&[1, 2, 5]), vec![5]);
        roster.started(5);
        // An agent that went idle is not restarted by itself while the
        // others still run...
        roster.ended(2);
        assert!(!roster.done());
        assert_eq!(roster.to_start(&[1, 2, 5]), Vec::<u32>::new());
        // ...but when its host rings the wake event.
        roster.woken();
        assert_eq!(roster.to_start(&[1, 2, 5]), vec![2]);
        roster.ended(1);
        roster.ended(5);
        assert!(roster.done(), "no agent left: the broker exits");
    }

    #[test]
    fn without_a_wake_event_an_ended_agent_starts_again_on_the_next_pass() {
        // Another process holds the wake event's name: no host can ring it,
        // so the broker keeps every approved session served by itself.
        let mut roster = Roster {
            unwakeable: true,
            ..Default::default()
        };
        assert_eq!(roster.to_start(&[1, 2]), vec![1, 2]);
        roster.started(1);
        roster.started(2);
        roster.ended(2);
        assert!(!roster.done());
        assert_eq!(roster.to_start(&[1, 2]), vec![2]);
        roster.ended(1);
        assert!(roster.done(), "it still exits once no agent runs");
    }

    #[test]
    fn the_host_talks_only_to_system_in_its_session_or_itself_elevated() {
        let me = "S-1-5-21-1-2-3-1001";
        let id = |sid: &str, session, integrity| Identity {
            sid: sid.into(),
            session,
            integrity,
        };
        assert_eq!(
            judge_server(&id(SYSTEM_SID, 1, 0x4000), me, 1).unwrap(),
            Server::System
        );
        // The legacy helper: this very user, elevated, input only.
        assert_eq!(
            judge_server(&id(me, 1, HIGH), me, 1).unwrap(),
            Server::ElevatedUser
        );
        // A copy of the helper the user started without elevation.
        assert!(judge_server(&id(me, 1, MEDIUM), me, 1).is_err());
        // Another user, elevated or not.
        assert!(judge_server(&id("S-1-5-21-1-2-3-1002", 1, HIGH), me, 1).is_err());
        // Administrators or a service account are not SYSTEM.
        assert!(judge_server(&id("S-1-5-32-544", 1, 0x4000), me, 1).is_err());
        assert!(judge_server(&id("S-1-5-19", 1, 0x4000), me, 1).is_err());
        // SYSTEM, but serving from another session.
        assert!(judge_server(&id(SYSTEM_SID, 0, 0x4000), me, 1).is_err());
        assert!(judge_server(&id(SYSTEM_SID, 2, 0x4000), me, 1).is_err());
    }

    #[test]
    fn the_helper_serves_only_the_approved_user_in_its_session() {
        let approved = "S-1-5-21-1-2-3-1001";
        let id = |sid: &str, session| Identity {
            sid: sid.into(),
            session,
            integrity: MEDIUM,
        };
        assert!(judge_client(&id(approved, 1), approved, 1).is_ok());
        assert!(judge_client(&id("S-1-5-21-1-2-3-1002", 1), approved, 1).is_err());
        assert!(judge_client(&id(approved, 2), approved, 1).is_err());
        assert!(judge_client(&id(SYSTEM_SID, 1), approved, 1).is_err());
    }

    #[test]
    fn only_a_refused_greeting_falls_back_to_the_old_handshake() {
        let refused: Result<()> =
            Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof)).context(Refused);
        assert!(refused.unwrap_err().downcast_ref::<Refused>().is_some());
        // A server that failed the identity check is not retried as an
        // older helper, so its reason is what the host reports.
        let impostor = judge_server(
            &Identity {
                sid: "S-1-5-21-1-2-3-1001".into(),
                session: 1,
                integrity: MEDIUM,
            },
            "S-1-5-21-1-2-3-1001",
            1,
        )
        .unwrap_err();
        assert!(impostor.downcast_ref::<Refused>().is_none());
    }

    #[test]
    fn a_process_that_started_later_only_reused_the_id() {
        assert!(existed_by(10, 10));
        assert!(existed_by(9, 10));
        assert!(!existed_by(11, 10));
        let me = open_process(std::process::id(), PROCESS_QUERY_LIMITED_INFORMATION).unwrap();
        let started = created(&me).unwrap();
        assert!(started > 0 && existed_by(started, now()));
        let id = identity(&me).unwrap();
        let token = own_token(TOKEN_QUERY).unwrap();
        assert_eq!(id.sid, user_sid(&token).unwrap());
        assert_eq!(id.session, session(&token).unwrap());
    }

    #[test]
    fn only_person_accounts_can_be_approved() {
        assert!(is_user_sid("S-1-5-21-3623811015-3361044348-30300820-1013"));
        assert!(is_user_sid(
            "S-1-12-1-1234567890-1234567890-1234567890-123456789"
        ));
        for sid in [
            "S-1-5-18",
            "S-1-5-19",
            "S-1-5-32-544",
            "S-1-1-0",
            "S-1-5-21-1-2-3",
            "S-1-5-21-1-2-3-4-5",
            "S-1-5-21-1-2-3-x",
            "S-1-5-21-1-2-3-99999999999",
            "S-1-5-21-1-2-3-1001)(A;;GA;;;WD",
            "S-1-5-21-1-2-3-1001 ",
            "",
        ] {
            assert!(!is_user_sid(sid), "{sid}");
        }
        let me = current_user_sid().unwrap();
        assert!(is_user_sid(&me) || me == SYSTEM_SID, "{me}");
    }

    #[test]
    fn ctrl_alt_delete_becomes_the_secure_attention_sequence() {
        use latch_proto::input::{MOD_ALT, MOD_CTRL};
        let key = |vk, down, modifiers| Event::Key {
            vk,
            down,
            modifiers,
        };
        // The toolbar's chord: Ctrl, Alt, Delete, then the releases.
        let mut sas = Sas::default();
        assert_eq!(sas.sees(&key(0x11, true, MOD_CTRL)), (false, false));
        assert_eq!(
            sas.sees(&key(0x12, true, MOD_CTRL | MOD_ALT)),
            (false, false)
        );
        assert_eq!(
            sas.sees(&key(VK_DELETE, true, MOD_CTRL | MOD_ALT)),
            (true, true)
        );
        assert_eq!(
            sas.sees(&key(VK_DELETE, false, MOD_CTRL | MOD_ALT)),
            (false, true)
        );
        assert_eq!(sas.sees(&key(0x12, false, MOD_CTRL)), (false, false));
        assert_eq!(sas.sees(&key(0x11, false, 0)), (false, false));
        // Sided modifiers without a mask still make the chord.
        assert_eq!(sas.sees(&key(0xa3, true, 0)), (false, false));
        assert_eq!(sas.sees(&key(0xa4, true, 0)), (false, false));
        assert_eq!(sas.sees(&key(VK_DELETE, true, 0)), (true, true));
        // Delete alone, or with only Ctrl, is just Delete.
        let mut sas = Sas::default();
        assert_eq!(sas.sees(&key(VK_DELETE, true, 0)), (false, false));
        assert_eq!(sas.sees(&key(VK_DELETE, false, 0)), (false, false));
        assert_eq!(sas.sees(&key(VK_DELETE, true, MOD_CTRL)), (false, false));
        // A release-all forgets held modifiers.
        sas.sees(&key(0x11, true, 0));
        sas.sees(&key(0x12, true, 0));
        sas.sees(&Event::ReleaseAll);
        assert_eq!(sas.sees(&key(VK_DELETE, true, 0)), (false, false));
    }

    #[test]
    fn only_the_system_agent_accepts_a_frames_connection() {
        struct Pipe(std::io::Cursor<Vec<u8>>, Vec<u8>);
        impl Read for Pipe {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                self.0.read(buf)
            }
        }
        impl Write for Pipe {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.1.extend(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let pipe = |hello: &[u8]| Pipe(std::io::Cursor::new(hello.to_vec()), Vec::new());
        let mut p = pipe(HELLO_FRAMES);
        assert!(matches!(handshake(&mut p, true), Ok(Hello::Frames)));
        assert_eq!(p.1, HELLO_FRAMES);
        // A helper elevated as the user cannot reach the secure desktop:
        // it closes without answering, as an older build does.
        let mut p = pipe(HELLO_FRAMES);
        assert!(handshake(&mut p, false).is_err());
        assert!(p.1.is_empty());
        for hello in [HELLO, HELLO2] {
            for system in [true, false] {
                let mut p = pipe(hello);
                assert!(matches!(handshake(&mut p, system), Ok(Hello::Input)));
            }
        }
        assert!(handshake(&mut pipe(b"LATCHXX9"), true).is_err());
    }

    #[test]
    fn pipe_client_must_match_the_recorded_install_path() {
        use std::path::Path;
        let installed = Path::new(r"C:\Users\User\AppData\Local\Latch\latch-host.exe");
        assert!(same_file_name(
            installed,
            Path::new(r"c:\users\user\appdata\local\latch\LATCH-HOST.EXE")
        ));
        assert!(!same_file_name(
            installed,
            Path::new(r"C:\Temp\latch-host.exe")
        ));
        assert!(!same_file_name(
            installed,
            Path::new(r"C:\Users\User\AppData\Local\Latch\other.exe")
        ));
        let actual =
            image(&open_process(std::process::id(), PROCESS_QUERY_LIMITED_INFORMATION).unwrap())
                .unwrap();
        assert!(same_file_name(&actual, &std::env::current_exe().unwrap()));
    }

    /// Needs the helper set up (a build with LATCH_HELPER_TEST_DIR, its
    /// folder and SYSTEM task, `host-path.txt` naming this test binary).
    #[test]
    #[ignore]
    fn live_system_agent_serves_this_session() -> Result<()> {
        let started = Instant::now();
        let me = Me::current()?;
        let own = me.session;
        let expected = helper_exe().context("no helper path")?;
        let mut client = with_helper(|| Client::connect(&me, &expected), |_| true)?;
        eprintln!("input helper answered after {:?}", started.elapsed());
        client
            .apply(&Event::ReleaseAll, DisplayRect::default())
            .context("pipe")??;
        let mut pid = 0;
        // SAFETY: The handle is the open pipe's; the id is written to a local.
        unsafe { GetNamedPipeServerProcessId(client.pipe.raw(), &mut pid)? };
        let process = open_process(pid, PROCESS_QUERY_LIMITED_INFORMATION)?;
        let mut server_session = 0;
        // SAFETY: Writes the session id into a local.
        unsafe { ProcessIdToSessionId(pid, &mut server_session)? };
        eprintln!(
            "pipe server: pid {pid}, {}, session {server_session} (this test: session {own})",
            image(&process)?.display()
        );
        ensure!(server_session == own, "the agent runs in another session");
        match process_token(&process).and_then(|t| user_sid(&t)) {
            Ok(sid) => ensure!(sid == SYSTEM_SID, "the agent runs as {sid}"),
            Err(e) => eprintln!("agent token unreadable from here: {e:#}"),
        }
        Ok(())
    }

    #[test]
    fn this_process_reports_its_own_session_and_level() {
        let token = own_token(TOKEN_QUERY).unwrap();
        assert!(session(&token).is_ok());
        assert!(user_sid(&token).unwrap().starts_with("S-1-5-"));
        assert!(own_integrity() >= MEDIUM);
    }
}
