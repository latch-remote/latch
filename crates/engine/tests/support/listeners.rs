//! Reserve the real host media socket and its control listener together.
use latch_engine::server::{Options, Server};

#[path = "options.rs"]
pub mod options;
use std::net::{Ipv4Addr, TcpListener, UdpSocket};
use std::sync::Arc;

#[path = "ports.rs"]
mod ports;

#[allow(dead_code)]
pub fn proxy() -> (UdpSocket, TcpListener) {
    ports::udp_and_tcp_on_one_port()
}

#[allow(dead_code)]
pub fn host() -> (Arc<Server>, TcpListener) {
    ports::retry_bind(|port| {
        let tcp = TcpListener::bind((Ipv4Addr::LOCALHOST, port))?;
        let server = Server::bind(tcp.local_addr()?.port(), options())?;
        Ok((server, tcp))
    })
}

#[allow(dead_code)]
pub fn options() -> Options {
    std::env::set_var("LATCH_NETENV_PROBE", "0");
    options::options()
}
