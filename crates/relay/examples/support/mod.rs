//! Explicit private-PKI configuration shared only by measurement examples.
use rustls::pki_types::{pem::PemObject, CertificateDer};

pub fn relay(args: &[String]) -> anyhow::Result<Option<latch_relay::ClientConfig>> {
    let value = |name: &str| {
        args.windows(2)
            .find(|pair| pair[0] == name)
            .map(|pair| pair[1].as_str())
    };
    let Some(host) = value("--relay-host") else {
        anyhow::ensure!(
            value("--relay-ca").is_none(),
            "relay-ca requires relay-host"
        );
        return Ok(latch_stream::session::relay_config(
            &latch_proto::control::RelaySettings::default(),
        ));
    };
    let endpoint =
        latch_relay::Endpoint::new(host, value("--relay-port").unwrap_or("443").parse()?)?;
    let config = if let Some(path) = value("--relay-ca") {
        let mut roots = rustls::RootCertStore::empty();
        for certificate in CertificateDer::pem_slice_iter(&std::fs::read(path)?) {
            roots.add(certificate?)?;
        }
        anyhow::ensure!(!roots.is_empty(), "relay-ca contains no certificates");
        latch_relay::ClientConfig::with_roots(endpoint, roots)?
    } else {
        latch_relay::ClientConfig::production(endpoint)?
    };
    Ok(Some(config))
}
