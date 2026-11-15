//! Socket readiness and a pollable local notification, shared by every OS.
use crate::wire::TICK;
use latch_proto::transport::{wait_sockets, SocketInterest};
use std::{
    io,
    net::UdpSocket,
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex, Once,
    },
    time::{Duration, Instant},
};

/// Cumulative timing for a transport's readiness waits.
#[derive(Clone, Copy, Debug, Default)]
pub struct WaitStats {
    pub waits: u64,
    pub timeouts: u64,
    pub notifications: u64,
    /// First coalesced local notification to its drain, including ongoing work.
    pub max_notification_delay: Duration,
    /// Time beyond the requested deadline on a timed-out wait.
    pub max_timeout_lateness: Duration,
}
impl WaitStats {
    pub(crate) fn merge(&mut self, other: Self) {
        self.waits += other.waits;
        self.timeouts += other.timeouts;
        self.notifications += other.notifications;
        self.max_notification_delay = self
            .max_notification_delay
            .max(other.max_notification_delay);
        self.max_timeout_lateness = self.max_timeout_lateness.max(other.max_timeout_lateness);
    }
}
#[derive(Debug, Default)]
struct Timing {
    notified: Option<Instant>,
    stats: WaitStats,
}

/// A connected loopback datagram pair. A full receive buffer already represents
/// a pending wake, so notifications can coalesce without blocking the sender.
/// Socket failures fall back to the existing bounded polling cadence.
#[derive(Debug)]
pub(crate) struct Wake {
    sockets: Option<(UdpSocket, UdpSocket)>,
    failed: AtomicBool,
    /// Whether the latest wait returned with a ready socket.
    #[cfg(test)]
    pub(crate) last_wait_ready: AtomicBool,
    timing: Mutex<Timing>,
}
fn warn(error: &io::Error) {
    static WARNING: Once = Once::new();
    WARNING.call_once(|| {
        eprintln!(
            "relay local wake unavailable kind={:?}, using timed polling",
            error.kind()
        )
    });
}
impl Wake {
    pub fn new() -> Self {
        Self::from_sockets(Self::open_sockets())
    }
    fn open_sockets() -> io::Result<(UdpSocket, UdpSocket)> {
        let sender = UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0))?;
        let receiver = UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0))?;
        sender.connect(receiver.local_addr()?)?;
        receiver.connect(sender.local_addr()?)?;
        sender.set_nonblocking(true)?;
        receiver.set_nonblocking(true)?;
        Ok((sender, receiver))
    }
    fn from_sockets(sockets: io::Result<(UdpSocket, UdpSocket)>) -> Self {
        let sockets = sockets.map_err(|error| warn(&error)).ok();
        Self {
            sockets,
            failed: AtomicBool::new(false),
            #[cfg(test)]
            last_wait_ready: AtomicBool::new(false),
            timing: Mutex::new(Timing::default()),
        }
    }
    fn sockets(&self) -> Option<&(UdpSocket, UdpSocket)> {
        self.sockets
            .as_ref()
            .filter(|_| !self.failed.load(Ordering::Relaxed))
    }
    pub fn notify(&self) {
        if let Some((sender, _)) = self.sockets() {
            let mut timing = self.timing.lock().unwrap();
            match sender.send(&[1]) {
                Ok(_) => {
                    timing.notified.get_or_insert_with(Instant::now);
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => (),
                Err(error) => {
                    self.failed.store(true, Ordering::Relaxed);
                    warn(&error);
                }
            }
        }
    }
    pub fn interest(&self) -> Option<SocketInterest<'_>> {
        self.sockets().map(|(_, receiver)| SocketInterest {
            socket: socket2::SockRef::from(receiver),
            readable: true,
            writable: false,
        })
    }
    pub fn stats(&self) -> WaitStats {
        self.timing.lock().unwrap().stats
    }
    pub fn wait<'a>(
        &'a self,
        mut sockets: Vec<SocketInterest<'a>>,
        timeout: Duration,
    ) -> io::Result<()> {
        // poll reports hangups even with no requested directions. Exclude
        // paused sockets so backpressure or control pacing cannot spin on HUP.
        sockets.retain(|socket| socket.readable || socket.writable);
        let timeout = if let Some(interest) = self.interest() {
            sockets.push(interest);
            timeout
        } else {
            timeout.min(TICK)
        };
        let start = Instant::now();
        let ready = wait_sockets(&sockets, timeout)?;
        #[cfg(test)]
        self.last_wait_ready.store(ready != 0, Ordering::Relaxed);
        let elapsed = start.elapsed();
        let mut timing = self.timing.lock().unwrap();
        timing.stats.waits += 1;
        if ready == 0 && elapsed >= timeout {
            timing.stats.timeouts += 1;
            timing.stats.max_timeout_lateness =
                timing.stats.max_timeout_lateness.max(elapsed - timeout);
        }
        if ready == 0 {
            return Ok(());
        }
        if let Some((_, receiver)) = self.sockets() {
            let mut bytes = [0; 256];
            let mut notified = false;
            loop {
                match receiver.recv(&mut bytes) {
                    Ok(_) => notified = true,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                    Err(error) => {
                        self.failed.store(true, Ordering::Relaxed);
                        warn(&error);
                        break;
                    }
                }
            }
            if notified {
                timing.stats.notifications += 1;
                if let Some(sent) = timing.notified.take() {
                    timing.stats.max_notification_delay =
                        timing.stats.max_notification_delay.max(sent.elapsed());
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn queued_send_and_cancellation_wakes_survive_until_wait() {
        let wake = Arc::new(Wake::new());
        let cancel = crate::Cancel::new();
        cancel.register(&wake);
        wake.notify();
        assert_eq!(
            wait_sockets(&[wake.interest().unwrap()], Duration::from_secs(1)).unwrap(),
            1
        );
        wake.wait(Vec::new(), Duration::ZERO).unwrap();
        assert_eq!(
            wait_sockets(&[wake.interest().unwrap()], Duration::ZERO).unwrap(),
            0
        );
        cancel.cancel();
        assert_eq!(
            wait_sockets(&[wake.interest().unwrap()], Duration::from_secs(1)).unwrap(),
            1
        );
        let late = Arc::new(Wake::new());
        cancel.register(&late);
        assert_eq!(
            wait_sockets(&[late.interest().unwrap()], Duration::from_secs(1)).unwrap(),
            1
        );
    }

    #[test]
    fn unavailable_wake_sockets_fall_back_to_bounded_polling() {
        let wake = Arc::new(Wake::from_sockets(Err(
            io::ErrorKind::PermissionDenied.into()
        )));
        let cancel = crate::Cancel::new();
        cancel.register(&wake);
        assert!(wake.interest().is_none());
        wake.notify();
        let start = Instant::now();
        wake.wait(Vec::new(), Duration::from_secs(60)).unwrap();
        assert!(start.elapsed() >= TICK);
        assert!(start.elapsed() < Duration::from_secs(1));
        cancel.cancel();
        assert!(cancel.is_cancelled());
        assert_eq!(wake.stats().timeouts, 1);
    }
}
