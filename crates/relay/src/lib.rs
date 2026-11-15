//! Two outbound native TLS or HTTPS WebSocket lanes with end-to-end encryption.
//! See the crate README for integration and operator requirements.
#![forbid(unsafe_code)]

mod carrier;
mod client;
mod crypto;
pub mod daemon;
mod diagnostics;
mod probe;
mod proxy;
mod quota;
mod readiness;
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
    Arc, Mutex, Weak,
};

/// Cloneable cancellation signal. Cancellation is permanent for this attempt.
#[derive(Clone, Default, Debug)]
pub struct Cancel(Arc<Cancellation>);
#[derive(Default, Debug)]
struct Cancellation {
    cancelled: AtomicBool,
    wakes: Mutex<Vec<Weak<readiness::Wake>>>,
}
impl Cancel {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn cancel(&self) {
        self.0.cancelled.store(true, Ordering::Release);
        for wake in self
            .0
            .wakes
            .lock()
            .unwrap()
            .iter()
            .filter_map(Weak::upgrade)
        {
            wake.notify();
        }
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.cancelled.load(Ordering::Acquire)
    }
    pub(crate) fn register(&self, wake: &Arc<readiness::Wake>) {
        let mut wakes = self.0.wakes.lock().unwrap();
        wakes.retain(|wake| wake.strong_count() != 0);
        wakes.push(Arc::downgrade(wake));
        if self.is_cancelled() {
            wake.notify();
        }
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
