use std::net::{Ipv4Addr, TcpListener, UdpSocket};

/// A loopback UDP socket and a TCP listener on the same port number.
///
/// The OS hands out a free UDP port, but nothing reserves the same number
/// for TCP: another test or process can already hold it, and on Windows CI
/// it can sit inside a range reserved against TCP. Try again on a fresh
/// port rather than failing the test with AddrInUse.
pub fn udp_and_tcp_on_one_port() -> (UdpSocket, TcpListener) {
    let mut last = None;
    for _ in 0..50 {
        let udp = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = udp.local_addr().unwrap().port();
        match TcpListener::bind((Ipv4Addr::LOCALHOST, port)) {
            Ok(listener) => return (udp, listener),
            Err(e) => last = Some(e),
        }
    }
    panic!("no port free for both UDP and TCP after 50 tries: {last:?}");
}
