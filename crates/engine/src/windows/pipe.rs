//! The helper's named pipe, with deadlines. A plain ReadFile on a pipe
//! waits forever when the other end stops answering mid-message, so both
//! ends open their pipe for overlapped I/O, and every read and write gives
//! up (and cancels itself) at the pipe's deadline.
use std::{
    io::{self, Read, Write},
    os::windows::{
        fs::OpenOptionsExt,
        io::{AsRawHandle, FromRawHandle, IntoRawHandle, OwnedHandle},
    },
    time::Instant,
};
use windows::{
    core::PCWSTR,
    Win32::{
        Foundation::{
            ERROR_BROKEN_PIPE, ERROR_IO_PENDING, ERROR_PIPE_CONNECTED, ERROR_PIPE_NOT_CONNECTED,
            HANDLE, WAIT_OBJECT_0,
        },
        Storage::FileSystem::{
            ReadFile, WriteFile, FILE_FLAG_OVERLAPPED, FILE_GENERIC_READ, FILE_WRITE_ATTRIBUTES,
            FILE_WRITE_DATA, SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT,
        },
        System::{
            Pipes::ConnectNamedPipe,
            Threading::{CreateEventW, WaitForSingleObject, INFINITE},
            IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED},
        },
    },
};

/// What a client asks for when it opens the pipe: read, and write data,
/// and nothing it does not use. Not FILE_APPEND_DATA, which on a pipe is
/// FILE_CREATE_PIPE_INSTANCE, a right the pipe's security grants only its
/// creator. (GENERIC_WRITE, what `OpenOptions::write` asks for, includes
/// it; Windows' pipe driver does not hold that against a client opening
/// an existing pipe, but there is no reason to ask.)
pub(super) const CLIENT_ACCESS: u32 =
    FILE_GENERIC_READ.0 | FILE_WRITE_DATA.0 | FILE_WRITE_ATTRIBUTES.0;

/// One end of a pipe opened for overlapped I/O. Reads and writes block
/// until the deadline, if one is set, and fail with `TimedOut` after it.
pub(super) struct Pipe {
    handle: OwnedHandle,
    /// Manual-reset; each operation's OVERLAPPED signals it.
    event: OwnedHandle,
    deadline: Option<Instant>,
}

fn io_error(e: windows::core::Error) -> io::Error {
    // Win32 errors arrive as HRESULTs of FACILITY_WIN32: the low word.
    io::Error::from_raw_os_error(e.code().0 & 0xffff)
}

fn is(e: &windows::core::Error, code: windows::Win32::Foundation::WIN32_ERROR) -> bool {
    e.code() == code.to_hresult()
}

impl Pipe {
    /// Takes over a pipe handle opened with FILE_FLAG_OVERLAPPED.
    pub(super) fn from_handle(handle: OwnedHandle) -> io::Result<Self> {
        // SAFETY: An unnamed event with default security; owned below.
        let event = unsafe { CreateEventW(None, true, false, PCWSTR::null()) }.map_err(io_error)?;
        Ok(Self {
            handle,
            // SAFETY: The event was just created and is owned only here.
            event: unsafe { OwnedHandle::from_raw_handle(event.0) },
            deadline: None,
        })
    }

    /// Opens the client end of `name`, with [`CLIENT_ACCESS`] and only
    /// identification rights for the server over this process.
    pub(super) fn open(name: &str) -> io::Result<Self> {
        let file = std::fs::OpenOptions::new()
            .access_mode(CLIENT_ACCESS)
            .custom_flags(FILE_FLAG_OVERLAPPED.0)
            .security_qos_flags(SECURITY_SQOS_PRESENT.0 | SECURITY_IDENTIFICATION.0)
            .open(name)?;
        // SAFETY: The handle comes straight from the file, which gives it up.
        Self::from_handle(unsafe { OwnedHandle::from_raw_handle(file.into_raw_handle()) })
    }

    pub(super) fn raw(&self) -> HANDLE {
        HANDLE(self.handle.as_raw_handle())
    }

    /// Every read and write from now on ends by `deadline`; `None` waits as
    /// long as it takes.
    pub(super) fn set_deadline(&mut self, deadline: Option<Instant>) {
        self.deadline = deadline;
    }

    /// The server's end: waits for a client to open this instance. No
    /// deadline: the helper waits for its host as long as it runs.
    pub(super) fn accept(&mut self) -> io::Result<()> {
        let mut overlapped = OVERLAPPED {
            hEvent: HANDLE(self.event.as_raw_handle()),
            ..Default::default()
        };
        // SAFETY: The OVERLAPPED outlives the operation: it is waited for
        // below before this frame returns.
        match unsafe { ConnectNamedPipe(self.raw(), Some(&mut overlapped)) } {
            Ok(()) => Ok(()),
            Err(e) if is(&e, ERROR_PIPE_CONNECTED) => Ok(()),
            Err(e) if is(&e, ERROR_IO_PENDING) => {
                let mut ignored = 0;
                // SAFETY: Waits for the pending operation to finish.
                unsafe { GetOverlappedResult(self.raw(), &overlapped, &mut ignored, true) }
                    .map_err(io_error)
            }
            Err(e) => Err(io_error(e)),
        }
    }

    /// One ReadFile or WriteFile, finished or cancelled before it returns,
    /// so the buffer is never touched afterwards.
    fn transfer(&mut self, read: Option<&mut [u8]>, write: Option<&[u8]>) -> io::Result<usize> {
        let handle = self.raw();
        let mut overlapped = OVERLAPPED {
            hEvent: HANDLE(self.event.as_raw_handle()),
            ..Default::default()
        };
        // SAFETY: The buffer and the OVERLAPPED outlive the operation, which
        // completes, or is cancelled and waited for, before this returns.
        // ReadFile and WriteFile reset the event as they start.
        let started = unsafe {
            match (read, write) {
                (Some(buf), _) => ReadFile(handle, Some(buf), None, Some(&mut overlapped)),
                (None, Some(buf)) => WriteFile(handle, Some(buf), None, Some(&mut overlapped)),
                (None, None) => return Ok(0),
            }
        };
        if let Err(e) = started {
            if !is(&e, ERROR_IO_PENDING) {
                return Err(io_error(e));
            }
            let wait = match self.deadline {
                None => INFINITE,
                Some(deadline) => deadline
                    .saturating_duration_since(Instant::now())
                    .as_millis()
                    .min(u128::from(INFINITE - 1)) as u32,
            };
            // SAFETY: The event is owned by this pipe.
            if unsafe { WaitForSingleObject(HANDLE(self.event.as_raw_handle()), wait) }
                != WAIT_OBJECT_0
            {
                let mut done = 0;
                // SAFETY: Cancels this very operation, then waits until
                // Windows is done with the buffer.
                unsafe {
                    let _ = CancelIoEx(handle, Some(&overlapped));
                    if GetOverlappedResult(handle, &overlapped, &mut done, true).is_ok() {
                        // It finished just before the cancellation.
                        return Ok(done as usize);
                    }
                }
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "the other end of the pipe did not answer in time",
                ));
            }
        }
        let mut done = 0;
        // SAFETY: The operation has completed; this only reads its result.
        unsafe { GetOverlappedResult(handle, &overlapped, &mut done, false) }.map_err(io_error)?;
        Ok(done as usize)
    }
}

/// Something whose reads and writes can be made to end by a deadline.
pub(super) trait Deadline {
    fn set_deadline(&mut self, deadline: Option<Instant>);
}
impl Deadline for Pipe {
    fn set_deadline(&mut self, deadline: Option<Instant>) {
        Pipe::set_deadline(self, deadline);
    }
}
impl<T: Deadline> Deadline for &mut T {
    fn set_deadline(&mut self, deadline: Option<Instant>) {
        (**self).set_deadline(deadline);
    }
}

impl Read for Pipe {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        match self.transfer(Some(buf), None) {
            // The other end closed the pipe: the end of the stream.
            Err(e)
                if e.raw_os_error() == Some(ERROR_BROKEN_PIPE.0 as i32)
                    || e.raw_os_error() == Some(ERROR_PIPE_NOT_CONNECTED.0 as i32) =>
            {
                Ok(0)
            }
            result => result,
        }
    }
}

impl Write for Pipe {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        self.transfer(None, Some(buf))
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use windows::{
        core::HSTRING,
        Win32::{
            Storage::FileSystem::{FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_DUPLEX},
            System::Pipes::{
                CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE,
                PIPE_WAIT,
            },
        },
    };

    #[test]
    fn clients_never_ask_to_create_pipe_instances() {
        // FILE_CREATE_PIPE_INSTANCE is FILE_APPEND_DATA (0x4): not asked for.
        assert_eq!(CLIENT_ACCESS & 0x4, 0);
        // Everything asked for is within what the pipe grants the user.
        assert_eq!(CLIENT_ACCESS & !0x0012_019b, 0, "{CLIENT_ACCESS:#x}");
        // Read and write data, and SYNCHRONIZE for waiting on the handle.
        for needed in [0x1, 0x2, 0x0010_0000] {
            assert_ne!(CLIENT_ACCESS & needed, 0, "{needed:#x}");
        }
    }

    fn server(name: &str) -> Pipe {
        let wide = HSTRING::from(name);
        // SAFETY: Default security; the handle is owned by the pipe below.
        let handle = unsafe {
            CreateNamedPipeW(
                &wide,
                PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE | FILE_FLAG_OVERLAPPED,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                1,
                4096,
                4096,
                0,
                None,
            )
        };
        assert!(!handle.is_invalid());
        // SAFETY: Just created, owned only here.
        Pipe::from_handle(unsafe { OwnedHandle::from_raw_handle(handle.0) }).unwrap()
    }

    #[test]
    fn a_stalled_peer_times_out_and_a_closed_one_ends_the_stream() {
        let name = format!(r"\\.\pipe\latch-pipe-test-{}", std::process::id());
        let mut server = server(&name);
        let mut client = Pipe::open(&name).unwrap();
        server.accept().unwrap();
        // Data flows both ways.
        client.write_all(b"hello").unwrap();
        let mut got = [0; 5];
        server.read_exact(&mut got).unwrap();
        assert_eq!(&got, b"hello");
        // A server that never answers: the read gives up at the deadline.
        client.set_deadline(Some(Instant::now() + Duration::from_millis(200)));
        let started = Instant::now();
        let err = client.read_exact(&mut got).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(5));
        // The cancelled read took nothing: the next bytes still arrive whole.
        server.write_all(b"again").unwrap();
        client.set_deadline(Some(Instant::now() + Duration::from_secs(5)));
        client.read_exact(&mut got).unwrap();
        assert_eq!(&got, b"again");
        // The server going away is the end of the stream, not a hang.
        drop(server);
        assert_eq!(client.read(&mut got).unwrap(), 0);
    }
}
