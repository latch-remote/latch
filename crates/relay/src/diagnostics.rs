use rustls::{
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    pki_types::{CertificateDer, ServerName, UnixTime},
    DigitallySignedStruct, SignatureScheme,
};
use std::{
    fmt, io,
    sync::{Arc, Mutex},
};
use x509_cert::der::Decode;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Carrier {
    Native,
    WebSocket,
}

/// Safe to display. Contains no proxy credentials, capabilities or peer payloads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Failure {
    ConnectionBlocked,
    ClosedBeforeHandshake,
    Timeout,
    TlsInterception { issuer: String },
    CaptivePortal,
    ProxyAuthenticationRequired,
    ProxyRefused { status: u16 },
    AlpnRejected,
    UpgradeRejected { status: u16 },
    RelayRejected,
    TlsRejected,
    InvalidProtocol,
    Cancelled,
    Unsupported,
}
impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ConnectionBlocked => f.write_str("relay connection blocked or disconnected"),
            Self::ClosedBeforeHandshake => {
                f.write_str("connection closed before TLS (blocked or relay busy)")
            }
            Self::Timeout => f.write_str("relay connection timed out"),
            Self::TlsInterception { issuer } => {
                write!(
                    f,
                    "TLS certificate chain is untrusted or incomplete ({issuer})"
                )
            }
            Self::CaptivePortal => {
                f.write_str("probable captive portal or redirected relay connection")
            }
            Self::ProxyAuthenticationRequired => f.write_str("proxy authentication required"),
            Self::ProxyRefused { status } => write!(f, "proxy refused CONNECT (HTTP {status})"),
            Self::AlpnRejected => f.write_str("relay ALPN rejected"),
            Self::UpgradeRejected { status } => {
                write!(f, "WebSocket upgrade rejected (HTTP {status})")
            }
            Self::RelayRejected => f.write_str("relay rejected admission or transfer quota"),
            Self::TlsRejected => f.write_str("relay TLS verification or protocol failed"),
            Self::InvalidProtocol => f.write_str("invalid relay carrier response"),
            Self::Cancelled => f.write_str("relay connection cancelled"),
            Self::Unsupported => f.write_str("relay is unsupported on this CPU or platform"),
        }
    }
}
impl Failure {
    pub(crate) fn io_kind(&self) -> io::ErrorKind {
        match self {
            Self::Timeout => io::ErrorKind::TimedOut,
            Self::Cancelled => io::ErrorKind::Interrupted,
            Self::Unsupported => io::ErrorKind::Unsupported,
            Self::RelayRejected | Self::ProxyAuthenticationRequired | Self::ProxyRefused { .. } => {
                io::ErrorKind::PermissionDenied
            }
            Self::ConnectionBlocked | Self::ClosedBeforeHandshake => {
                io::ErrorKind::ConnectionAborted
            }
            _ => io::ErrorKind::InvalidData,
        }
    }
    pub(crate) fn error(self) -> io::Error {
        ConnectError {
            failure: self,
            attempts: Vec::new(),
        }
        .into_io()
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttemptFailure {
    pub carrier: Carrier,
    pub via_proxy: bool,
    pub failure: Failure,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MssReason {
    ExplicitStall,
    /// Both carrier TLS handshakes stalled after TCP connected.
    TlsHandshakeStall,
    WriteStall,
    RecordStall,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConnectionInfo {
    pub carrier: Carrier,
    pub via_proxy: bool,
    /// TCP_MAXSEG was applied before TLS on this socket.
    pub reduced_mss: bool,
    pub reduced_mss_reason: Option<MssReason>,
    pub reduced_mss_value: Option<u32>,
    pub earlier_failures: Vec<AttemptFailure>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConnectError {
    pub failure: Failure,
    pub attempts: Vec<AttemptFailure>,
}
impl fmt::Display for ConnectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.failure.fmt(f)
    }
}
impl std::error::Error for ConnectError {}
impl ConnectError {
    /// Recover typed connection diagnostics from the compatibility io::Result API.
    pub fn from_io(error: &io::Error) -> Self {
        if let Some(typed) = error.get_ref().and_then(|e| e.downcast_ref::<Self>()) {
            return typed.clone();
        }
        let failure = match error.kind() {
            io::ErrorKind::TimedOut => Failure::Timeout,
            io::ErrorKind::Interrupted => Failure::Cancelled,
            io::ErrorKind::Unsupported => Failure::Unsupported,
            io::ErrorKind::InvalidData | io::ErrorKind::InvalidInput => Failure::InvalidProtocol,
            _ => Failure::ConnectionBlocked,
        };
        Self {
            failure,
            attempts: Vec::new(),
        }
    }
    pub(crate) fn into_io(self) -> io::Error {
        io::Error::new(self.failure.io_kind(), self)
    }
}

/// Observe verification failures while retaining rustls' complete verification.
#[derive(Debug)]
pub(crate) struct Verifier {
    pub inner: Arc<dyn ServerCertVerifier>,
    pub failure: Arc<Mutex<Option<Failure>>>,
}
impl ServerCertVerifier for Verifier {
    fn verify_server_cert(
        &self,
        end: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        name: &ServerName<'_>,
        ocsp: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let result = self
            .inner
            .verify_server_cert(end, intermediates, name, ocsp, now);
        if let Err(error) = &result {
            use rustls::{CertificateError, Error};
            let failure = match error {
                Error::InvalidCertificate(CertificateError::UnknownIssuer) => {
                    let issuer = x509_cert::Certificate::from_der(end.as_ref())
                        .map(|c| c.tbs_certificate.issuer.to_string())
                        .unwrap_or_else(|_| "unavailable".into());
                    Failure::TlsInterception {
                        issuer: sanitize_issuer(&issuer),
                    }
                }
                Error::InvalidCertificate(
                    CertificateError::NotValidForName
                    | CertificateError::NotValidForNameContext { .. },
                ) => Failure::CaptivePortal,
                _ => Failure::TlsRejected,
            };
            *self.failure.lock().unwrap() = Some(failure);
        }
        result
    }
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls12_signature(message, cert, dss)
    }
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.inner.supported_verify_schemes()
    }
}

fn sanitize_issuer(issuer: &str) -> String {
    issuer
        .chars()
        .filter(|c| {
            !c.is_control()
                && !matches!(*c,
                    '\u{ad}' | '\u{600}'..='\u{605}' | '\u{61c}' | '\u{6dd}' | '\u{70f}'
                    | '\u{890}'..='\u{891}' | '\u{8e2}' | '\u{180e}' | '\u{200b}'..='\u{200f}'
                    | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{2064}' | '\u{2066}'..='\u{206f}'
                    | '\u{feff}' | '\u{fff9}'..='\u{fffb}' | '\u{110bd}' | '\u{110cd}'
                    | '\u{13430}'..='\u{1343f}' | '\u{1bca0}'..='\u{1bca3}'
                    | '\u{1d173}'..='\u{1d17a}' | '\u{e0001}' | '\u{e0020}'..='\u{e007f}'
                )
        })
        .take(512)
        .collect()
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn issuer_text_removes_controls_and_unicode_format_characters() {
        assert_eq!(
            sanitize_issuer("Example\n CA\u{202e}\u{2066}\u{200d}\u{feff}\u{e0001}"),
            "Example CA"
        );
        assert_eq!(sanitize_issuer(&"é".repeat(600)).chars().count(), 512);
        assert_eq!(sanitize_issuer("École CA"), "École CA");
        assert!(Failure::TlsInterception {
            issuer: "CA".into()
        }
        .to_string()
        .contains("untrusted or incomplete"));
        assert_eq!(Failure::AlpnRejected.to_string(), "relay ALPN rejected");
    }
}
