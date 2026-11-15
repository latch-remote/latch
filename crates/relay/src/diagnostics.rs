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
            Self::Timeout => f.write_str("relay connection timed out"),
            Self::TlsInterception { issuer } => {
                write!(f, "TLS certificate has an untrusted issuer ({issuer})")
            }
            Self::CaptivePortal => {
                f.write_str("probable captive portal or redirected relay connection")
            }
            Self::ProxyAuthenticationRequired => f.write_str("proxy authentication required"),
            Self::ProxyRefused { status } => write!(f, "proxy refused CONNECT (HTTP {status})"),
            Self::AlpnRejected => f.write_str("native relay ALPN rejected"),
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
            Self::ConnectionBlocked => io::ErrorKind::ConnectionAborted,
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
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConnectionInfo {
    pub carrier: Carrier,
    pub via_proxy: bool,
    /// TCP_MAXSEG was applied before TLS on this socket.
    pub reduced_mss: bool,
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
                        issuer: issuer
                            .chars()
                            .filter(|c| !c.is_control())
                            .take(512)
                            .collect(),
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
