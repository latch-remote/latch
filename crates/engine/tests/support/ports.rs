use std::net::{Ipv4Addr, TcpListener, UdpSocket};

// Keep both sockets alive on success. Hyper-V excluded ranges can return
// PermissionDenied for a port the other protocol was free to take.
pub(super) fn retry_bind<T>(mut bind: impl FnMut(u16) -> anyhow::Result<T>) -> T {
    const ATTEMPTS: usize = 50;
    let mut last_error = None;
    for attempt in 0..ATTEMPTS {
        match bind(candidate_port(attempt)) {
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

/// The port to try first: any free TCP port, then random ones.
///
/// Windows hands out ephemeral ports in sequence, so retrying with port 0 can
/// keep landing in the same range the other protocol has excluded. A random
/// port across a wide range leaves it.
pub(super) fn candidate_port(attempt: usize) -> u16 {
    use std::hash::{BuildHasher, RandomState};
    if attempt == 0 {
        return 0;
    }
    20_000 + (RandomState::new().hash_one(attempt) % 40_000) as u16
}

/// A loopback UDP socket and TCP listener held on the same port number.
pub fn udp_and_tcp_on_one_port() -> (UdpSocket, TcpListener) {
    retry_bind(|port| {
        let tcp = TcpListener::bind((Ipv4Addr::LOCALHOST, port))?;
        let udp = UdpSocket::bind(tcp.local_addr()?)?;
        Ok((udp, tcp))
    })
}

#[test]
fn retry_reserved_ports_and_bound_attempts() {
    let mut attempts = 0;
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        retry_bind::<()>(|_| {
            attempts += 1;
            Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied).into())
        });
    }));
    assert!(result.is_err());
    assert_eq!(attempts, 50);
    let mut attempts = 0;
    retry_bind(|port| {
        attempts += 1;
        // The first try takes any free port; retries pick a fixed one.
        assert_eq!(port == 0, attempts == 1);
        assert!(port == 0 || (20_000..60_000).contains(&port));
        if attempts == 1 {
            Err(std::io::Error::from(std::io::ErrorKind::AddrInUse).into())
        } else {
            Ok(())
        }
    });
    assert_eq!(attempts, 2);
}
