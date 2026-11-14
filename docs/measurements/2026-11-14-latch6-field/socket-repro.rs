use std::{io::{self, Read, Write}, net::{TcpListener, TcpStream}, thread, time::Duration};
fn show<T: std::fmt::Debug>(label: &str, value: io::Result<T>) -> io::Result<T> {
    println!("{label} => {value:?}");
    value
}
fn main() -> io::Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let address = std::env::args().nth(1).map(|s| s.parse().unwrap()).unwrap_or(listener.local_addr()?);
    let socket = show("Socket::new", socket2::Socket::new(socket2::Domain::for_address(address), socket2::Type::STREAM, Some(socket2::Protocol::TCP)))?;
    show("set_nonblocking(true)", socket.set_nonblocking(true))?;
    let _ = show("connect", socket.connect(&address.into()));
    let tcp: TcpStream = socket.into();
    for _ in 0..10 {
        show("take_error", tcp.take_error())?;
        if show("peer_addr", tcp.peer_addr()).is_ok() { break; }
        thread::sleep(Duration::from_millis(5));
    }
    let mut tcp = tcp;
    let _ = show("first write", tcp.write(b"field socket probe"));
    let _ = show("first read", tcp.read(&mut [0u8; 1]));
    show("set_nodelay(true)", tcp.set_nodelay(true))?;
    show("set_nonblocking(true) after connect", tcp.set_nonblocking(true))?;
    Ok(())
}
