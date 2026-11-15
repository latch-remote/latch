//! Reserve the real host media socket and its control listener together.
use latch_engine::server::{Options, Server};
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
    ports::retry_bind(|| {
        let server = Server::bind(0, Options::default())?;
        let tcp = TcpListener::bind((Ipv4Addr::LOCALHOST, server.port()))?;
        Ok((server, tcp))
    })
}
