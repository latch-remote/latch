//! Frame deadlines need a timer without macOS's background sleep coalescing.
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::time::{Duration, Instant};

thread_local! {
    // One timer per media thread, closed when that thread exits.
    static TIMER: Option<OwnedFd> = {
        // SAFETY: kqueue takes no arguments and returns a new descriptor.
        let fd = unsafe { libc::kqueue() };
        if fd < 0 {
            tracing::debug!(
                error = %std::io::Error::last_os_error(),
                "cannot create critical media timer, using ordinary sleep for this thread"
            );
            None
        } else {
            // SAFETY: a successful kqueue descriptor is uniquely owned here.
            Some(unsafe { OwnedFd::from_raw_fd(fd) })
        }
    };
}

pub(crate) fn sleep(wait: Duration) {
    if wait.is_zero() {
        return;
    }
    let started = Instant::now();
    let completed = TIMER.with(|timer| {
        let Some(timer) = timer else { return false };
        let change = libc::kevent {
            ident: 1,
            filter: libc::EVFILT_TIMER,
            flags: libc::EV_ADD | libc::EV_ONESHOT,
            // Ordinary nanosleep/mach_wait_until may add ~150 ms even after
            // requesting interactive thread QoS. Critical timers explicitly
            // opt this frame deadline out of discretionary timer coalescing.
            fflags: libc::NOTE_NSECONDS | libc::NOTE_CRITICAL,
            data: wait.as_nanos().min(isize::MAX as u128) as isize,
            udata: std::ptr::null_mut(),
        };
        let mut event = change;
        // SAFETY: both event pointers cover one initialized kevent. The
        // thread-local descriptor cannot be used concurrently or outlive us.
        let result = unsafe {
            libc::kevent(
                timer.as_raw_fd(),
                &change,
                1,
                &mut event,
                1,
                std::ptr::null(),
            )
        };
        result == 1 && event.flags & libc::EV_ERROR == 0
    });
    if !completed {
        // A denied kqueue or interrupted wait retains the ordinary fallback.
        std::thread::sleep(wait.saturating_sub(started.elapsed()));
    }
}
