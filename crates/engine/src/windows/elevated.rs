//! Input into windows that run as administrator. Windows drops input that a
//! process sends while a window of higher integrity has focus (UIPI), and
//! SendInput still reports it as sent: a Latch running unelevated could not
//! move the pointer or type while an installer, Task Manager or a game
//! launcher that runs as administrator was in front.
//!
//! Setup copies latch-host.exe into `%ProgramFiles%\Latch\Input`, a folder
//! only administrators can change, and registers the [`TASK`] task that
//! runs that copy with [`HELPER_ARG`] as the signed-in user, elevated. A
//! stream starts the task and sends each event through a named pipe that
//! only this user's processes in this session can open, and only to a pipe
//! whose server is that protected copy. The helper injects with the same
//! code as the host, keeps what it pressed, and lets go of it when the
//! stream's end closes the pipe.
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
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc, Arc,
    },
    thread,
    time::{Duration, Instant},
};
use windows::{
    core::{HSTRING, PCWSTR, PWSTR},
    Win32::{
        Foundation::{CloseHandle, LocalFree, ERROR_PIPE_CONNECTED, HANDLE, HLOCAL, HWND},
        Security::{
            Authorization::{
                ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
                SDDL_REVISION_1,
            },
            GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, TokenIntegrityLevel,
            TokenSessionId, TokenUser, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES,
            TOKEN_MANDATORY_LABEL, TOKEN_QUERY, TOKEN_USER,
        },
        Storage::FileSystem::{
            FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_DUPLEX, SECURITY_IDENTIFICATION,
            SECURITY_SQOS_PRESENT,
        },
        System::{
            Pipes::{
                ConnectNamedPipe, CreateNamedPipeW, GetNamedPipeClientProcessId,
                GetNamedPipeClientSessionId, GetNamedPipeServerProcessId, PIPE_READMODE_BYTE,
                PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT,
            },
            SystemInformation::GetSystemDirectoryW,
            Threading::{
                GetCurrentProcess, OpenProcess, OpenProcessToken, QueryFullProcessImageNameW,
                PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
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
/// Both ends say this first; a different build's helper is refused.
const HELLO: &[u8; 8] = b"LATCHIN1";
const HELLO2: &[u8; 8] = b"LATCHIN2";
/// A request: the display rectangle, then one event, at most a text event.
const MAX_REQUEST: usize = 16 + 2 + latch_proto::input::MAX_TEXT;
/// The helper exits once no stream has used it for this long.
const IDLE: Duration = Duration::from_secs(5);
/// How long a stream waits for the helper it started.
const START_WAIT: Duration = Duration::from_secs(10);
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const MEDIUM: u32 = 0x2000;

/// The helper's process: dispatched before the host parses its arguments.
/// Returns false when this process is not the helper.
pub fn run_helper() -> bool {
    if std::env::args_os().nth(1).as_deref() != Some(OsStr::new(HELPER_ARG)) {
        return false;
    }
    // An elevated process writes no log: the user's data folder is one
    // they could point elsewhere. A failure just leaves the host on its own.
    let _ = serve();
    true
}

/// `%ProgramFiles%\Latch\Input\latch-host.exe`.
pub fn helper_exe() -> Option<PathBuf> {
    let pf = std::env::var_os("ProgramFiles").filter(|d| !d.is_empty())?;
    Some(PathBuf::from(pf).join(DIR).join("latch-host.exe"))
}

fn pipe_name(session: u32) -> String {
    format!(r"\\.\pipe\LatchInput-{session}")
}

/// The pipe's security: this user (not as an administrator) may read and
/// write it but not create instances of it; administrators and SYSTEM
/// fully. The medium label keeps sandboxed (low integrity) processes out.
fn pipe_sddl(user_sid: &str) -> String {
    format!("D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;0x0012019b;;;{user_sid})S:(ML;;NW;;;ME)")
}

struct Handle(HANDLE);
impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: The handle is owned and closed once.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

fn own_token() -> Result<Handle> {
    let mut token = HANDLE::default();
    // SAFETY: The pseudo-handle needs no closing; the token is owned.
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }
        .context("open own token")?;
    Ok(Handle(token))
}

fn process_token(pid: u32) -> Result<Handle> {
    // SAFETY: Both handles are owned and closed by Handle.
    unsafe {
        let process = Handle(OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid)?);
        let mut token = HANDLE::default();
        OpenProcessToken(process.0, TOKEN_QUERY, &mut token)?;
        Ok(Handle(token))
    }
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

fn process_image(pid: u32) -> Result<PathBuf> {
    let mut buf = vec![0u16; 1024];
    let mut len = buf.len() as u32;
    // SAFETY: The buffer's length is passed and the handle is owned.
    unsafe {
        let process = Handle(OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid)?);
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

fn same_file_name(a: &std::path::Path, b: &std::path::Path) -> bool {
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
    let level = integrity(&process_token(pid).ok()?).ok()?;
    let name = process_image(pid)
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_else(|| format!("process {pid}"));
    Some((name, level))
}

/// This process's integrity level (medium for a Latch started at sign-in).
pub(super) fn own_integrity() -> u32 {
    own_token().and_then(|t| integrity(&t)).unwrap_or(MEDIUM)
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

fn serve() -> Result<()> {
    let token = own_token()?;
    let session = session(&token)?;
    // Only setup writes this record in the administrators-only helper folder.
    let expected =
        std::fs::read_to_string(std::env::current_exe()?.with_file_name("host-path.txt"))?;
    let expected = PathBuf::from(expected.trim().trim_start_matches('\u{feff}'));
    let name = HSTRING::from(pipe_name(session));
    let sddl = HSTRING::from(pipe_sddl(&user_sid(&token)?));
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
                4,
                4096,
                4096,
                0,
                Some(&attributes),
            )
        };
        if pipe.is_invalid() {
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
            continue;
        }
        let mut pid = 0;
        // SAFETY: The pipe is connected and the output points to a local.
        if unsafe { GetNamedPipeClientProcessId(pipe, &mut pid) }.is_err()
            || !process_image(pid).is_ok_and(|image| same_file_name(&image, &expected))
        {
            continue;
        }
        active.fetch_add(1, Ordering::AcqRel);
        let (active, last) = (active.clone(), last.clone());
        thread::spawn(move || {
            let _ = serve_client(file);
            *last.lock() = Instant::now();
            active.fetch_sub(1, Ordering::AcqRel);
        });
    }
}

/// One stream's input until it closes the pipe. Dropping `local` lets go
/// of whatever this stream still holds.
fn serve_client(mut pipe: impl Read + Write) -> Result<()> {
    let mut hello = [0; HELLO.len()];
    pipe.read_exact(&mut hello)?;
    ensure!(
        &hello == HELLO || &hello == HELLO2,
        "a different Latch build asked for input"
    );
    pipe.write_all(&hello)?;
    let mut local = Local::default();
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
        let result = decode(&buf[..len]).and_then(|(event, display)| local.apply(&event, display));
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

/// The host's end of the pipe.
pub(super) struct Client {
    pipe: File,
    key_positions: bool,
    legacy_held: std::collections::BTreeSet<u16>,
}

impl Client {
    fn connect(session: u32, expected: &std::path::Path) -> Result<Self> {
        // An old helper closes an unrecognised handshake. Reopen its pipe
        // using v1, validating server identity on both connections.
        Self::connect_version(session, expected, HELLO2)
            .or_else(|_| Self::connect_version(session, expected, HELLO))
    }

    fn connect_version(
        session: u32,
        expected: &std::path::Path,
        version: &[u8; 8],
    ) -> Result<Self> {
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
        pipe.read_exact(&mut hello)?;
        ensure!(
            &hello == version,
            "the input helper is a different Latch build"
        );
        Ok(Self {
            pipe,
            key_positions: &hello == HELLO2,
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
    let session = own_token().and_then(|t| session(&t)).ok()?;
    let expected = helper_exe()?;
    if !expected.is_file() {
        if !NOT_SET_UP.swap(true, Ordering::Relaxed) {
            tracing::info!("windows that run as administrator ignore a viewer's mouse and keyboard until setup runs again (Set up as administrator)");
        }
        return None;
    }
    // A helper an earlier stream started may still be running.
    if let Ok(client) = Client::connect(session, &expected) {
        return Some(client);
    }
    let deadline = Instant::now() + START_WAIT;
    let mut started = None::<Instant>;
    let mut last = anyhow::anyhow!("not started");
    while Instant::now() < deadline {
        // The task ignores a start while an instance runs; one that is just
        // exiting would leave nothing, so ask again now and then.
        if started.is_none_or(|t| t.elapsed() >= Duration::from_secs(2)) {
            if let Err(e) = start_task() {
                if !NOT_SET_UP.swap(true, Ordering::Relaxed) {
                    tracing::warn!("could not start the input helper for windows that run as administrator ({e:#}); run setup again");
                }
                return None;
            }
            started = Some(Instant::now());
        }
        thread::sleep(Duration::from_millis(100));
        match Client::connect(session, &expected) {
            Ok(client) => return Some(client),
            Err(e) => last = e,
        }
    }
    tracing::warn!(
        "the input helper for windows that run as administrator did not answer: {last:#}"
    );
    None
}

fn start_task() -> Result<()> {
    let mut dir = vec![0u16; 260];
    // SAFETY: The buffer's length is passed with it.
    let len = unsafe { GetSystemDirectoryW(Some(&mut dir)) } as usize;
    ensure!(len > 0 && len < dir.len(), "system folder");
    let schtasks = PathBuf::from(String::from_utf16_lossy(&dir[..len])).join("schtasks.exe");
    let out = std::process::Command::new(schtasks)
        .args(["/Run", "/TN", TASK])
        .creation_flags(CREATE_NO_WINDOW)
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
    fn only_the_user_and_administrators_can_open_the_pipe() {
        let sddl = pipe_sddl("S-1-5-21-1-2-3-1001");
        assert!(sddl.starts_with("D:P"), "no inherited access");
        // Read and write, but not FILE_CREATE_PIPE_INSTANCE (0x4): the user
        // cannot put up a pipe instance of their own beside the helper's.
        assert!(sddl.contains("(A;;0x0012019b;;;S-1-5-21-1-2-3-1001)"));
        assert_eq!(0x0012019b & 0x4, 0);
        assert!(!sddl.contains(";WD)") && !sddl.contains(";AU)"));
        assert!(sddl.ends_with("S:(ML;;NW;;;ME)"));
        assert_eq!(pipe_name(1), r"\\.\pipe\LatchInput-1");
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

    #[test]
    fn this_process_reports_its_own_session_and_level() {
        let token = own_token().unwrap();
        assert!(session(&token).is_ok());
        assert!(user_sid(&token).unwrap().starts_with("S-1-5-"));
        assert!(own_integrity() >= MEDIUM);
    }
}
