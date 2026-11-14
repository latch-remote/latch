//! Local TLS relay shared by host unit and engine/viewer runtime tests.
use latch_relay::{
    daemon::{Daemon, DaemonConfig},
    Cancel, ClientConfig, Endpoint,
};
use rustls::pki_types::{pem::PemObject, CertificateDer, PrivateKeyDer};
use std::{
    io,
    net::Ipv4Addr,
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    thread::{self, JoinHandle},
    time::Duration,
};

pub(super) struct Pilot {
    pub(super) config: ClientConfig,
    cancel: Cancel,
    pub(super) worker: Option<JoinHandle<io::Result<()>>>,
    quota: PathBuf,
    pub(super) metrics: Arc<latch_relay::daemon::Metrics>,
}
impl Pilot {
    pub(super) fn start() -> Self {
        Self::with_limits(|_| {})
    }
    pub(super) fn with_limits(configure: impl FnOnce(&mut DaemonConfig)) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let ca =
            CertificateDer::from_pem_slice(include_bytes!("../../../relay/tests/certs/ca.pem"))
                .unwrap();
        let cert =
            CertificateDer::from_pem_slice(include_bytes!("../../../relay/tests/certs/server.pem"))
                .unwrap();
        let key = PrivateKeyDer::from_pem_slice(include_bytes!(
            "../../../relay/tests/certs/server-key.pem"
        ))
        .unwrap();
        let quota = std::env::temp_dir().join(format!(
            "latch-runtime-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let mut daemon_config = DaemonConfig::new(
            "127.0.0.1:0".parse().unwrap(),
            vec![cert],
            key,
            quota.clone(),
            64 * 1024 * 1024,
        )
        .unwrap();
        daemon_config.initialize_quota = true;
        configure(&mut daemon_config);
        let daemon = Daemon::bind(daemon_config).unwrap();
        let metrics = daemon.metrics();
        let endpoint = Endpoint::new("localhost", daemon.local_addr().unwrap().port()).unwrap();
        let mut roots = rustls::RootCertStore::empty();
        roots.add(ca).unwrap();
        let mut config = ClientConfig::with_roots(endpoint, roots).unwrap();
        config.limits.setup_timeout = Duration::from_secs(3);
        let cancel = Cancel::new();
        let run_cancel = cancel.clone();
        let worker = thread::spawn(move || daemon.run(&run_cancel));
        Self {
            config,
            cancel,
            worker: Some(worker),
            quota,
            metrics,
        }
    }
    pub(super) fn restart(&mut self) {
        assert!(self.worker.is_none());
        let cert =
            CertificateDer::from_pem_slice(include_bytes!("../../../relay/tests/certs/server.pem"))
                .unwrap();
        let key = PrivateKeyDer::from_pem_slice(include_bytes!(
            "../../../relay/tests/certs/server-key.pem"
        ))
        .unwrap();
        let config = DaemonConfig::new(
            (Ipv4Addr::LOCALHOST, self.config.endpoint.port()).into(),
            vec![cert],
            key,
            self.quota.clone(),
            64 * 1024 * 1024,
        )
        .unwrap();
        let daemon = Daemon::bind(config).unwrap();
        self.cancel = Cancel::new();
        let cancel = self.cancel.clone();
        self.worker = Some(thread::spawn(move || daemon.run(&cancel)));
    }

    pub(super) fn stop(&mut self) {
        self.cancel.cancel();
        if let Some(worker) = self.worker.take() {
            worker.join().unwrap().unwrap();
        }
    }
}
impl Drop for Pilot {
    fn drop(&mut self) {
        self.stop();
        let _ = std::fs::remove_file(&self.quota);
    }
}
