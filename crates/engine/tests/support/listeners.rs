//! Reserve real media and control sockets together. UDP and TCP allocate
//! ports independently, so a free UDP port can already have a TCP listener.
use latch_engine::server::{Options, Server};
use std::net::{Ipv4Addr, TcpListener, UdpSocket};
use std::sync::Arc;

#[allow(dead_code)]
pub fn proxy() -> (UdpSocket, TcpListener) {
    loop {
        let udp = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        match TcpListener::bind(udp.local_addr().unwrap()) {
            Ok(tcp) => return (udp, tcp),
            Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => continue,
            Err(error) => panic!("cannot bind test control listener: {error}"),
        }
    }
}

#[allow(dead_code)]
pub fn host() -> (Arc<Server>, TcpListener) {
    loop {
        // The actual server keeps its UDP socket, including on Windows where
        // Hyper-V can reserve blocks of UDP ports independently of TCP.
        let server = Server::bind(0, Options::default()).unwrap();
        match TcpListener::bind((Ipv4Addr::LOCALHOST, server.port())) {
            Ok(tcp) => return (server, tcp),
            Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => continue,
            Err(error) => panic!("cannot bind test control listener: {error}"),
        }
    }
}
