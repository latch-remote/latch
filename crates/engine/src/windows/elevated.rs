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
//! Setup copies latch-host.exe into `%ProgramFiles%\Latch\Input`, a folder
//! only administrators can change, records the signed-in user's host path
//! beside it (`host-path.txt`), and registers the [`TASK`] task that runs
//! that copy with [`HELPER_ARG`] as LocalSystem. The task's own security
//! lets that user start it, nothing more. The copy then plays one of three
//! roles, by its token:
//!
//! - The **broker**: SYSTEM in session 0, where the task starts it. The task
//!   takes no arguments, so the broker finds the sessions to serve itself:
//!   those running the recorded host executable. Into each it starts the
//!   agent, its own executable and nothing else, with a copy of its SYSTEM
//!   token moved to that session, on `winsta0\default`; it waits for them
//!   and exits.
//! - The **agent**: SYSTEM in the user's session. It serves the named pipe
//!   `LatchInput-{session}`, whose security admits only SYSTEM and the user
//!   whose recorded host runs in this session, and accepts a connection
//!   only from that host executable in this session. Each of its threads
//!   follows the desktop that has input (see `desktop`). It injects input
//!   there, calls SendSAS for Ctrl+Alt+Delete, and on a second kind of
//!   connection duplicates an output for the host (see `frames`).
//! - **Elevated as the user**: what an older setup's task ran. It serves
//!   input as before; it cannot reach the secure desktop and refuses frames.
//!
//! The host checks that the pipe's server is that protected copy before it
//! sends anything. The helper keeps what it pressed and lets go of it when
//! the stream's end closes the pipe.
use super::frames::{self, Frames, Want};
use super::input::Local;
use crate::input::{DisplayRect, Event};
use anyhow::{bail, ensure, Context, Result};
use std::{
    ffi::OsStr,
    fs::File,
    io::{Read, Write},
    os::windows::{
        fs::OpenOptionsExt,
        io::{AsRawHandle, FromRawHandle},
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
        Foundation::{CloseHandle, LocalFree, BOOL, ERROR_PIPE_CONNECTED, HANDLE, HLOCAL, HWND},
        Security::{
            Authorization::{
                ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
                SDDL_REVISION_1,
            },
            DuplicateTokenEx, GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation,
            SecurityImpersonation, SetTokenInformation, TokenIntegrityLevel, TokenPrimary,
            TokenSessionId, TokenUser, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES,
            TOKEN_ACCESS_MASK, TOKEN_ALL_ACCESS, TOKEN_DUPLICATE, TOKEN_MANDATORY_LABEL,
            TOKEN_QUERY, TOKEN_USER,
        },
        Storage::FileSystem::{
            FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_DUPLEX, SECURITY_IDENTIFICATION,
            SECURITY_SQOS_PRESENT,
        },
        System::{
            Diagnostics::ToolHelp::{
                CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
                TH32CS_SNAPPROCESS,
            },
            LibraryLoader::{GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_SYSTEM32},
            Pipes::{
                ConnectNamedPipe, CreateNamedPipeW, GetNamedPipeClientProcessId,
                GetNamedPipeClientSessionId, GetNamedPipeServerProcessId, PIPE_READMODE_BYTE,
                PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT,
            },
            Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD},
            RemoteDesktop::ProcessIdToSessionId,
            SystemInformation::GetSystemDirectoryW,
            Threading::{
                CreateProcessAsUserW, GetCurrentProcess, OpenProcess, OpenProcessToken,
                QueryFullProcessImageNameW, WaitForSingleObject, CREATE_NO_WINDOW, INFINITE,
                PROCESS_ACCESS_RIGHTS, PROCESS_DUP_HANDLE, PROCESS_INFORMATION, PROCESS_NAME_WIN32,
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
/// How often an injecting thread checks that it is on the input desktop
/// (it also checks whenever Windows refuses an event).
const DESKTOP_CHECK: Duration = Duration::from_millis(250);
const MEDIUM: u32 = 0x2000;
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

fn process_image(pid: u32) -> Result<PathBuf> {
    image(&open_process(pid, PROCESS_QUERY_LIMITED_INFORMATION)?)
}

fn same_file_name(a: &Path, b: &Path) -> bool {
    a.as_os_str()
        .to_string_lossy()
        .eq_ignore_ascii_case(&b.as_os_str().to_string_lossy())
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

/// The host executable setup recorded, from beside this executable in the
/// administrators-only folder.
fn recorded_host() -> Result<PathBuf> {
    let record = std::env::current_exe()?.with_file_name("host-path.txt");
    let text = std::fs::read_to_string(record).context("read host-path.txt")?;
    let path = PathBuf::from(text.trim().trim_start_matches('\u{feff}'));
    ensure!(path.is_absolute(), "host-path.txt names no full path");
    Ok(path)
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

/// The sessions the broker starts an agent in: those running the host.
fn agent_sessions(hosts: &[Host]) -> Vec<u32> {
    let mut sessions: Vec<u32> = hosts
        .iter()
        .map(|h| h.session)
        .filter(|&s| s != 0)
        .collect();
    sessions.sort_unstable();
    sessions.dedup();
    sessions
}

/// The one user whose host runs in `session`; none when there is no host
/// or several users' (which a per-user install cannot produce).
fn session_user(hosts: &[Host], session: u32) -> Option<String> {
    let mut users = hosts
        .iter()
        .filter(|h| h.session == session)
        .map(|h| &h.sid);
    let first = users.next()?;
    users.all(|sid| sid == first).then(|| first.clone())
}

/// The task's process: starts an agent in each session running the
/// recorded host, and lives as long as they do.
fn broker() -> Result<()> {
    let expected = recorded_host()?;
    let exe = std::env::current_exe()?;
    let mut agents = Vec::new();
    let found = hosts(&expected);
    trace(|| format!("hosts of {}: {found:?}", expected.display()));
    for session in agent_sessions(&found) {
        let Some(user) = session_user(&found, session) else {
            continue;
        };
        match start_agent(&exe, session, &user) {
            Ok(agent) => agents.push(agent),
            Err(e) => trace(|| format!("agent for session {session}: {e:#}")),
        }
    }
    for agent in agents {
        // SAFETY: The process handle is owned.
        unsafe { WaitForSingleObject(agent.0, INFINITE) };
    }
    Ok(())
}

/// The agent process's security. A SYSTEM token's default would let no
/// one else open it, and the host must read its image path to trust the
/// pipe: so SYSTEM fully, the user only that and waiting on it.
fn agent_process_sddl(user_sid: &str) -> String {
    format!("D:P(A;;GA;;;SY)(A;;0x00101000;;;{user_sid})")
}

/// Starts `exe` (this very executable) as SYSTEM in `session`, on the
/// interactive window station's default desktop.
fn start_agent(exe: &Path, session: u32, user_sid: &str) -> Result<Handle> {
    let own = own_token(TOKEN_DUPLICATE | TOKEN_QUERY)?;
    let sddl = HSTRING::from(agent_process_sddl(user_sid));
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    // SAFETY: Allocated by the call, freed below after process creation.
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(sddl.as_ptr()),
            SDDL_REVISION_1,
            &mut descriptor,
            None,
        )?;
    }
    let attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: false.into(),
    };
    let started = start_agent_as(&own, exe, session, &attributes);
    // SAFETY: Allocated by ConvertStringSecurityDescriptorToSecurityDescriptorW.
    unsafe {
        LocalFree(HLOCAL(descriptor.0));
    }
    started
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
    // Only setup writes this record in the administrators-only helper folder.
    let expected = recorded_host()?;
    let user = if system {
        super::desktop::enable_following();
        session_user(&hosts(&expected), session).context("no host runs in this session")?
    } else {
        // (An elevated helper's user is its own.)
        user_sid(&token)?
    };
    trace(|| format!("serving {} for {user}", pipe_name(session)));
    let name = HSTRING::from(pipe_name(session));
    let sddl = HSTRING::from(pipe_sddl(&user, system));
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    // SAFETY: The descriptor is allocated by the call and kept for the
    // process's life, since every pipe instance is created with it.
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(sddl.as_ptr()),
            SDDL_REVISION_1,
            &mut descriptor,
            None,
        )?;
    }
    let attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: false.into(),
    };
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
        let pipe = unsafe {
            CreateNamedPipeW(
                PCWSTR(name.as_ptr()),
                PIPE_ACCESS_DUPLEX
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
        if pipe.is_invalid() {
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
        // SAFETY: The pipe handle is owned by the File from here on.
        let file = unsafe { File::from_raw_handle(pipe.0) };
        // SAFETY: Blocks until a client opens this instance.
        match unsafe { ConnectNamedPipe(pipe, None) } {
            Ok(()) => {}
            Err(e) if e.code() == ERROR_PIPE_CONNECTED.to_hresult() => {}
            Err(_) => continue,
        }
        let mut client_session = u32::MAX;
        // SAFETY: Reads the connected client's session into a local.
        if unsafe { GetNamedPipeClientSessionId(pipe, &mut client_session) }.is_err()
            || client_session != session
        {
            trace(|| format!("refused a client in session {client_session}"));
            continue;
        }
        let mut pid = 0;
        // SAFETY: The pipe is connected and the output points to a local.
        if unsafe { GetNamedPipeClientProcessId(pipe, &mut pid) }.is_err() {
            trace(|| "client process id unreadable".into());
            continue;
        }
        // The client's identity is read through the very handle the frames
        // are later duplicated into, so a reused process id cannot slip in.
        let access = if system {
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_DUP_HANDLE
        } else {
            PROCESS_QUERY_LIMITED_INFORMATION
        };
        let client = match open_process(pid, access) {
            Ok(client) => client,
            Err(e) => {
                trace(|| format!("open client {pid}: {e:#}"));
                continue;
            }
        };
        let client_image = image(&client);
        if !client_image
            .as_ref()
            .is_ok_and(|image| same_file_name(image, &expected))
        {
            trace(|| format!("refused client {pid}: {client_image:?}"));
            continue;
        }
        active.fetch_add(1, Ordering::AcqRel);
        let (active, last, user) = (active.clone(), last.clone(), user.clone());
        thread::spawn(move || {
            let result = serve_connection(file, &client, &user, system);
            trace(|| format!("client {pid} done: {result:?}"));
            *last.lock() = Instant::now();
            active.fetch_sub(1, Ordering::AcqRel);
        });
    }
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

fn serve_connection(mut pipe: File, client: &Handle, user: &str, system: bool) -> Result<()> {
    match handshake(&mut pipe, system)? {
        Hello::Input => serve_input(pipe, system),
        Hello::Frames => frames::serve(pipe, client.0, user),
    }
}

/// One stream's input until it closes the pipe (tests: with its greeting).
#[cfg(test)]
fn serve_client(mut pipe: impl Read + Write) -> Result<()> {
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

fn send_sas() -> Result<()> {
    ensure!(
        software_sas_allowed(),
        "Ctrl+Alt+Delete from a viewer needs Windows' SoftwareSASGeneration policy; run Set up as administrator again"
    );
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
    Ok(())
}

/// One stream's input until it closes the pipe. Dropping `local` lets go
/// of whatever this stream still holds. The agent (`system`) injects on
/// whichever desktop has input.
fn serve_input(mut pipe: impl Read + Write, system: bool) -> Result<()> {
    let mut local = Local::default();
    let mut sas = Sas::default();
    let mut checked = Instant::now() - DESKTOP_CHECK;
    let mut buf = [0u8; MAX_REQUEST];
    loop {
        let mut len = [0; 2];
        if pipe.read_exact(&mut len).is_err() {
            return Ok(());
        }
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

/// Opens the session's pipe and checks that its server is the protected
/// helper copy, then greets it with `version`, which it must echo.
fn open_pipe(session: u32, expected: &Path, version: &[u8; 8]) -> Result<File> {
    let mut pipe = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .security_qos_flags(SECURITY_SQOS_PRESENT.0 | SECURITY_IDENTIFICATION.0)
        .open(pipe_name(session))?;
    let mut pid = 0;
    // SAFETY: The handle is the open pipe's; the id is written to a local.
    unsafe { GetNamedPipeServerProcessId(HANDLE(pipe.as_raw_handle()), &mut pid)? };
    let image = process_image(pid)?;
    ensure!(
        same_file_name(&image, expected),
        "the input pipe belongs to {}, not {}",
        image.display(),
        expected.display()
    );
    pipe.write_all(version)?;
    let mut hello = [0; HELLO.len()];
    pipe.read_exact(&mut hello)
        .context("the helper refused; it may be from an older setup")?;
    ensure!(
        &hello == version,
        "the input helper is a different Latch build"
    );
    Ok(pipe)
}

/// The host's end of the pipe.
pub(super) struct Client {
    pipe: File,
    key_positions: bool,
    legacy_held: std::collections::BTreeSet<u16>,
}

impl Client {
    fn connect(session: u32, expected: &Path) -> Result<Self> {
        // An old helper closes an unrecognised handshake. Reopen its pipe
        // using v1, validating server identity on both connections.
        Self::connect_version(session, expected, HELLO2)
            .or_else(|_| Self::connect_version(session, expected, HELLO))
    }

    fn connect_version(session: u32, expected: &Path, version: &[u8; 8]) -> Result<Self> {
        let pipe = open_pipe(session, expected, version)?;
        Ok(Self {
            pipe,
            key_positions: version == HELLO2,
            legacy_held: Default::default(),
        })
    }

    /// The outer error is the pipe's (the helper is gone); the inner one
    /// is the helper's (it could not inject).
    pub(super) fn apply(
        &mut self,
        event: &Event,
        display: DisplayRect,
    ) -> std::io::Result<Result<()>> {
        let Some(event) = helper_event(event, self.key_positions, &mut self.legacy_held) else {
            return Ok(Ok(()));
        };
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
    let session = own_token(TOKEN_QUERY).and_then(|t| session(&t)).ok()?;
    let expected = helper_exe()?;
    if !expected.is_file() {
        if !NOT_SET_UP.swap(true, Ordering::Relaxed) {
            tracing::info!("windows that run as administrator, the lock screen and administrator prompts ignore a viewer until setup runs again (Set up as administrator)");
        }
        return None;
    }
    match with_helper(|| Client::connect(session, &expected), |_| true) {
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
/// running, for up to [`START_WAIT`]. `retry` says whether an attempt's
/// error may mean it is still starting.
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

/// Frames of the output `want` from the helper, starting it if needed.
/// Blocks for up to [`START_WAIT`].
pub(super) fn connect_frames(want: Want) -> Result<Frames> {
    let session = own_token(TOKEN_QUERY).and_then(|t| session(&t))?;
    let expected = helper_exe().context("no Program Files folder")?;
    ensure!(expected.is_file(), "the helper is not set up");
    with_helper(
        || {
            let mut pipe = open_pipe(session, &expected, HELLO_FRAMES)?;
            pipe.write_all(&frames::encode_want(&want))?;
            let granted = frames::read_granted(&mut pipe)?;
            Frames::open(pipe, granted)
        },
        absent,
    )
}

/// [`connect_frames`] on a thread of its own, so capture never waits.
pub(super) fn request_frames(want: Want) -> mpsc::Receiver<Result<Frames>> {
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

fn start_task() -> Result<()> {
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
        assert_eq!(sddl.matches("(A;").count(), 2);
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
    fn agents_go_to_the_sessions_running_the_host_and_serve_its_user() {
        let host = |session, sid: &str| Host {
            session,
            sid: sid.into(),
        };
        let a = "S-1-5-21-1-2-3-1001";
        let b = "S-1-5-21-1-2-3-1002";
        let hosts = [host(2, a), host(1, a), host(2, a), host(0, a)];
        // Never session 0, each session once.
        assert_eq!(agent_sessions(&hosts), vec![1, 2]);
        assert_eq!(agent_sessions(&[]), Vec::<u32>::new());
        assert_eq!(session_user(&hosts, 1).as_deref(), Some(a));
        assert_eq!(session_user(&hosts, 3), None, "no host, no agent");
        // Two users' hosts in one session: whose would the pipe admit? Neither.
        assert_eq!(session_user(&[host(1, a), host(1, b)], 1), None);
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
        let actual = process_image(std::process::id()).unwrap();
        assert!(same_file_name(&actual, &std::env::current_exe().unwrap()));
    }

    /// Needs the helper set up (a build with LATCH_HELPER_TEST_DIR, its
    /// folder and SYSTEM task, `host-path.txt` naming this test binary).
    #[test]
    #[ignore]
    fn live_system_agent_serves_this_session() -> Result<()> {
        let started = Instant::now();
        let own = session(&own_token(TOKEN_QUERY)?)?;
        let expected = helper_exe().context("no helper path")?;
        let mut client = with_helper(|| Client::connect(own, &expected), |_| true)?;
        eprintln!("input helper answered after {:?}", started.elapsed());
        client
            .apply(&Event::ReleaseAll, DisplayRect::default())
            .context("pipe")??;
        let mut pid = 0;
        // SAFETY: The handle is the open pipe's; the id is written to a local.
        unsafe { GetNamedPipeServerProcessId(HANDLE(client.pipe.as_raw_handle()), &mut pid)? };
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
