use crate::{invalid, Endpoint};
use base64::{engine::general_purpose::STANDARD, Engine};
use std::{fmt, io};
use zeroize::Zeroizing;

/// Explicit HTTP CONNECT proxy. Discovery belongs to the caller.
/// Proxy addresses may be private or loopback. Relay TLS still verifies the relay hostname.
#[derive(Clone, Debug)]
pub struct ProxyConfig {
    pub(crate) host: String,
    pub(crate) port: u16,
    pub credentials: Option<BasicCredentials>,
}
impl ProxyConfig {
    pub fn host(&self) -> &str {
        &self.host
    }
    pub fn port(&self) -> u16 {
        self.port
    }
    pub(crate) fn connect_request(&self, host: &str, port: u16) -> Zeroizing<Vec<u8>> {
        let prefix = format!("CONNECT {host}:{port} HTTP/1.1\r\nHost: {host}:{port}\r\n");
        let authorization = b"Proxy-Authorization: Basic ";
        let plaintext = self.credentials.as_ref().map(|c| {
            let mut bytes =
                Zeroizing::new(Vec::with_capacity(c.username.len() + 1 + c.password.len()));
            bytes.extend_from_slice(c.username.as_bytes());
            bytes.push(b':');
            bytes.extend_from_slice(c.password.as_bytes());
            bytes
        });
        let encoded_len = plaintext
            .as_ref()
            .map_or(0, |p| base64::encoded_len(p.len(), true).unwrap());
        let capacity = prefix.len()
            + 2
            + plaintext
                .as_ref()
                .map_or(0, |_| authorization.len() + encoded_len + 2);
        // Allocate once before any secret is copied. Encode directly into its final location.
        let mut request = Zeroizing::new(Vec::with_capacity(capacity));
        request.extend_from_slice(prefix.as_bytes());
        if let Some(plaintext) = plaintext {
            request.extend_from_slice(authorization);
            let start = request.len();
            request.resize(start + encoded_len, 0);
            STANDARD
                .encode_slice(&*plaintext, &mut request[start..])
                .unwrap();
            request.extend_from_slice(b"\r\n");
        }
        request.extend_from_slice(b"\r\n");
        debug_assert_eq!(request.len(), capacity);
        request
    }
    pub fn new(host: impl Into<String>, port: u16) -> io::Result<Self> {
        let host = host.into();
        if host.parse::<std::net::IpAddr>().is_err() {
            Endpoint::new(host.clone(), port)?;
        }
        if port == 0 {
            return Err(invalid("proxy port must be nonzero"));
        }
        Ok(Self {
            host,
            port,
            credentials: None,
        })
    }
}
/// Basic credentials are sent only to the explicitly configured proxy.
#[derive(Clone)]
pub struct BasicCredentials {
    username: Zeroizing<String>,
    password: Zeroizing<String>,
}
impl BasicCredentials {
    pub fn new(username: impl Into<String>, password: impl Into<String>) -> io::Result<Self> {
        let username = Zeroizing::new(username.into());
        let password = Zeroizing::new(password.into());
        if username.contains(':')
            || username.len() + password.len() > 2048
            || username
                .chars()
                .chain(password.chars())
                .any(char::is_control)
        {
            return Err(invalid("invalid proxy Basic credentials"));
        }
        Ok(Self { username, password })
    }
}

impl fmt::Debug for BasicCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("BasicCredentials([REDACTED])")
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn connect_credentials_are_encoded_in_one_presized_zeroizing_buffer() {
        for size in [0, 1, 2, 3, 1023] {
            let mut proxy = ProxyConfig::new("localhost", 3128).unwrap();
            proxy.credentials =
                Some(BasicCredentials::new("u".repeat(size), "p".repeat(size)).unwrap());
            let request: Zeroizing<Vec<u8>> = proxy.connect_request("relay.example", 443);
            assert_eq!(request.len(), request.capacity());
            let expected = STANDARD.encode(format!("{}:{}", "u".repeat(size), "p".repeat(size)));
            assert_eq!(&*request, format!("CONNECT relay.example:443 HTTP/1.1\r\nHost: relay.example:443\r\nProxy-Authorization: Basic {expected}\r\n\r\n").as_bytes());
            // Consuming every derived buffer through Zeroizing guarantees wiping on every exit.
            let credentials = proxy.credentials.as_ref().unwrap();
            let _: &Zeroizing<String> = &credentials.username;
            let _: &Zeroizing<String> = &credentials.password;
        }
        assert!(BasicCredentials::new("bad:user", "secret").is_err());
        assert!(BasicCredentials::new("user", "bad\r\nsecret").is_err());
    }
}
