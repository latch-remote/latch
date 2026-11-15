//! Reserve real media and control sockets together. UDP and TCP allocate
//! ports independently, so a free UDP port can already have a TCP listener.
use latch_engine::server::{Options, Server};
use std::net::{Ipv4Addr, TcpListener, UdpSocket};
use std::sync::Arc;

// Keep both sockets alive on success. Hyper-V excluded ranges can return
// PermissionDenied even when an ephemeral UDP bind selected the port.
fn retry_bind<T>(mut bind: impl FnMut() -> anyhow::Result<T>) -> T {
    const ATTEMPTS: usize = 50;
    let mut last_error = None;
    for _ in 0..ATTEMPTS {
        match bind() {
            Ok(value) => return value,
            Err(error)
                if error.downcast_ref::<std::io::Error>().is_some_and(|error| {
                    matches!(
                        error.kind(),
                        std::io::ErrorKind::AddrInUse | std::io::ErrorKind::PermissionDenied
                    )
                }) =>
            {
                last_error = Some(error);
            }
            Err(error) => panic!("cannot bind test media/control sockets: {error:#}"),
        }
    }
    panic!(
        "cannot bind test media/control sockets after {ATTEMPTS} attempts: {:#}",
        last_error.unwrap()
    );
}

#[allow(dead_code)]
pub fn proxy() -> (UdpSocket, TcpListener) {
    retry_bind(|| {
        let udp = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))?;
        let tcp = TcpListener::bind(udp.local_addr()?)?;
        Ok((udp, tcp))
    })
}

#[allow(dead_code)]
pub fn host() -> (Arc<Server>, TcpListener) {
    retry_bind(|| {
        // The actual server keeps its UDP socket, including on Windows where
        // Hyper-V can reserve blocks of UDP ports independently of TCP.
        let server = Server::bind(0, Options::default())?;
        let tcp = TcpListener::bind((Ipv4Addr::LOCALHOST, server.port()))?;
        Ok((server, tcp))
    })
}

#[test]
fn retry_reserved_ports_and_bound_attempts() {
    let mut attempts = 0;
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        retry_bind::<()>(|| {
            attempts += 1;
            Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied).into())
        });
    }));
    assert!(result.is_err());
    assert_eq!(attempts, 50);
    let mut attempts = 0;
    retry_bind(|| {
        attempts += 1;
        if attempts == 1 {
            Err(std::io::Error::from(std::io::ErrorKind::AddrInUse).into())
        } else {
            Ok(())
        }
    });
    assert_eq!(attempts, 2);
}
