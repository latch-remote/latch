//! Two outbound native TLS or HTTPS WebSocket lanes with end-to-end encryption.
//! See the crate README for integration and operator requirements.
#![forbid(unsafe_code)]

mod carrier;
mod client;
mod crypto;
pub mod daemon;
mod diagnostics;
mod proxy;
mod quota;
mod wire;

pub use carrier::WEBSOCKET_PATH;
pub use client::{
    create_grant, create_grant_with_info, ClientConfig, Endpoint, Event, Transport, TransportLimits,
};
pub use crypto::{ensure_supported_cpu, SessionKeys};
pub use diagnostics::{AttemptFailure, Carrier, ConnectError, ConnectionInfo, Failure, MssReason};
pub use proxy::{BasicCredentials, ProxyConfig};
pub use wire::{Attachment, Grant, Lane, Role};

use std::io;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

/// Cloneable cancellation signal. Cancellation is permanent for this attempt.
#[derive(Clone, Default, Debug)]
pub struct Cancel(Arc<AtomicBool>);
impl Cancel {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
    pub(crate) fn check(&self) -> io::Result<()> {
        if self.is_cancelled() {
            Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "relay cancelled",
            ))
        } else {
            Ok(())
        }
    }
}
fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
fn timeout() -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, "relay deadline expired")
}
fn denied() -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, "relay admission rejected")
}
