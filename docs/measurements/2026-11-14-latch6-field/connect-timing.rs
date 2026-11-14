use std::{io, net::{TcpListener, TcpStream, ToSocketAddrs}, time::{Duration, Instant}};
fn main() -> io::Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    let dns_started = Instant::now();
    let addresses: Vec<_> = ("localhost", port).to_socket_addrs()?.collect();
    println!("localhost_resolution_ms={:.3}", dns_started.elapsed().as_secs_f64()*1000.0);
    for address in addresses {
        let started = Instant::now();
        let socket = socket2::Socket::new(socket2::Domain::for_address(address), socket2::Type::STREAM, Some(socket2::Protocol::TCP))?;
        socket.set_nonblocking(true)?;
        let connect = socket.connect(&address.into());
        println!("address={address} connect={connect:?} elapsed_ms={:.3}", started.elapsed().as_secs_f64()*1000.0);
        let tcp: TcpStream = socket.into();
        println!("early_peer={:?}", tcp.peer_addr());
        loop {
            match latch_proto::transport::tcp_connect_ready(&tcp, Duration::from_millis(20)) {
                Ok(false) if started.elapsed() < Duration::from_secs(4) => continue,
                result => {
                    println!("address={address} readiness={result:?} elapsed_ms={:.3}",started.elapsed().as_secs_f64()*1000.0);
                    break;
                }
            }
        }
    }
    Ok(())
}
