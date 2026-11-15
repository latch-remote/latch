//! Socket readiness and a pollable local notification, shared by every OS.
use latch_proto::transport::{wait_sockets, SocketInterest};
use std::{io, net::UdpSocket, time::Duration};

/// A connected loopback datagram pair. A full receive buffer already represents
/// a pending wake, so notifications can coalesce without blocking the sender.
#[derive(Debug)]
pub(crate) struct Wake {
    sender: UdpSocket,
    receiver: UdpSocket,
}
impl Wake {
    pub fn new() -> io::Result<Self> {
        let sender = UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0))?;
        let receiver = UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0))?;
        sender.connect(receiver.local_addr()?)?;
        receiver.connect(sender.local_addr()?)?;
        sender.set_nonblocking(true)?;
        receiver.set_nonblocking(true)?;
        Ok(Self { sender, receiver })
    }
    pub fn notify(&self) {
        let _ = self.sender.send(&[1]);
    }
    pub fn interest(&self) -> SocketInterest<'_> {
        SocketInterest {
            socket: socket2::SockRef::from(&self.receiver),
            readable: true,
            writable: false,
        }
    }
    pub fn wait<'a>(
        &'a self,
        mut sockets: Vec<SocketInterest<'a>>,
        timeout: Duration,
    ) -> io::Result<()> {
        // poll reports hangups even with no requested directions. Exclude
        // paused sockets so backpressure or control pacing cannot spin on HUP.
        sockets.retain(|socket| socket.readable || socket.writable);
        sockets.push(self.interest());
        if wait_sockets(&sockets, timeout)? == 0 {
            return Ok(());
        }
        let mut bytes = [0; 256];
        loop {
            match self.receiver.recv(&mut bytes) {
                Ok(_) => (),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(error) => return Err(error),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn queued_send_and_cancellation_wakes_survive_until_wait() {
        let wake = Arc::new(Wake::new().unwrap());
        let cancel = crate::Cancel::new();
        cancel.register(&wake);
        wake.notify();
        assert_eq!(
            wait_sockets(&[wake.interest()], Duration::from_secs(1)).unwrap(),
            1
        );
        wake.wait(Vec::new(), Duration::ZERO).unwrap();
        assert_eq!(wait_sockets(&[wake.interest()], Duration::ZERO).unwrap(), 0);
        cancel.cancel();
        assert_eq!(
            wait_sockets(&[wake.interest()], Duration::from_secs(1)).unwrap(),
            1
        );
        let late = Arc::new(Wake::new().unwrap());
        cancel.register(&late);
        assert_eq!(
            wait_sockets(&[late.interest()], Duration::from_secs(1)).unwrap(),
            1
        );
    }
}
