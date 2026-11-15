//! `cargo run -p latch-core --example netenv -j 4 -- [http://probe/path] [peer-id]`
//! Without a probe URL this makes no HTTP connectivity probe. Tailscale's
//! netcheck and the RFC 7050 system DNS lookup still run.
use latch_core::netenv::{discover, Options, ProbeTarget, Proxy};
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut options = Options::new("relay.latch.rune.foo");
    if let Some(value) = std::env::args().nth(1) {
        options.probes.push(ProbeTarget::new(&value, 204, &[])?);
    }
    options.peer = std::env::args().nth(2);
    if let Ok(value) = std::env::var("LATCH_PROXY") {
        options.proxy_override = Some(Proxy::parse(&value)?);
    }
    println!("{:#?}", discover(&options));
    Ok(())
}
