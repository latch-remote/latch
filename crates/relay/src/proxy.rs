use crate::{invalid, Endpoint};
use base64::{engine::general_purpose::STANDARD, Engine};
use std::{fmt, io};
use zeroize::Zeroize;

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
    username: String,
    password: String,
}
impl BasicCredentials {
    pub fn new(username: impl Into<String>, password: impl Into<String>) -> io::Result<Self> {
        let username = username.into();
        let password = password.into();
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
    pub(crate) fn authorization(&self) -> String {
        let mut plaintext = format!("{}:{}", self.username, self.password);
        let encoded = STANDARD.encode(plaintext.as_bytes());
        plaintext.zeroize();
        encoded
    }
}
impl fmt::Debug for BasicCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("BasicCredentials([REDACTED])")
    }
}
impl Drop for BasicCredentials {
    fn drop(&mut self) {
        self.username.zeroize();
        self.password.zeroize();
    }
}
