//! Explicit private-PKI configuration shared only by measurement examples.
use rustls::pki_types::{pem::PemObject, CertificateDer};

pub fn relay(args: &[String]) -> anyhow::Result<Option<latch_relay::ClientConfig>> {
    if args.iter().any(|arg| arg == "--no-relay") {
        return Ok(None);
    }
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
    // Keep environment/proxy selection in the product's configuration entry point.
    // The wiring PR must discover proxies for this endpoint and populate ClientConfig::proxy.
    let settings = latch_proto::control::RelaySettings {
        enabled: true,
        host: host.to_owned(),
        port: value("--relay-port").unwrap_or("443").parse()?,
    };
    let mut config = latch_stream::session::relay_config(&settings)
        .ok_or_else(|| anyhow::anyhow!("relay configuration unavailable"))?;
    if let Some(path) = value("--relay-ca") {
        let mut roots = rustls::RootCertStore::empty();
        for certificate in CertificateDer::pem_slice_iter(&std::fs::read(path)?) {
            roots.add(certificate?)?;
        }
        anyhow::ensure!(!roots.is_empty(), "relay-ca contains no certificates");
        config = config.with_tls_roots(roots)?;
    }
    Ok(Some(config))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_opt_out_has_no_relay_configuration() {
        assert!(relay(&["--no-relay".into()]).unwrap().is_none());
    }
}
