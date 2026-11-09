//! Bounded caller waits for drivers that cannot safely be interrupted.
use anyhow::{bail, Result};
use std::sync::{mpsc, Mutex};
use std::time::{Duration, Instant};

#[cfg(windows)]
pub(super) const DEADLINE: Duration = Duration::from_secs(1);
const WINDOW: Duration = Duration::from_secs(600);
const MAX_STALLS: usize = 3;
static STALLS: Mutex<Vec<Instant>> = Mutex::new(Vec::new());

fn available(stalls: &mut Vec<Instant>, now: Instant) -> bool {
    stalls.retain(|at| now.saturating_duration_since(*at) < WINDOW);
    stalls.len() < MAX_STALLS
}

/// Lets Baseline's existing software fallback handle an unsuccessful reopen,
/// even if the old hardware session previously produced pictures.
#[derive(Debug)]
pub(super) struct ReopenFailed;
impl std::fmt::Display for ReopenFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("could not reopen Media Foundation decoder")
    }
}
impl std::error::Error for ReopenFailed {}

#[cfg(windows)]
pub(super) fn check() -> Result<()> {
    if !available(
        &mut STALLS.lock().unwrap_or_else(|e| e.into_inner()),
        Instant::now(),
    ) {
        bail!("Media Foundation stalled repeatedly; decoder recovery budget exhausted");
    }
    Ok(())
}

pub(super) fn receive<T>(rx: &mpsc::Receiver<T>, timeout: Duration) -> Result<T> {
    match rx.recv_timeout(timeout) {
        Ok(value) => Ok(value),
        Err(mpsc::RecvTimeoutError::Timeout) => {
            STALLS
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(Instant::now());
            bail!("Media Foundation worker timed out; abandoning driver-owned resources");
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => bail!("Media Foundation worker stopped"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovery_budget_expires_on_a_controlled_clock() {
        assert!(anyhow::Error::new(ReopenFailed).is::<ReopenFailed>());
        let now = Instant::now();
        let mut stalls = vec![now; MAX_STALLS];
        assert!(!available(
            &mut stalls,
            now + WINDOW - Duration::from_nanos(1)
        ));
        assert!(available(&mut stalls, now + WINDOW));
    }

    #[test]
    fn a_blocked_or_disconnected_worker_cannot_hold_its_caller() {
        let (tx, rx) = mpsc::channel::<()>();
        assert!(receive(&rx, Duration::ZERO)
            .unwrap_err()
            .to_string()
            .contains("timed out"));
        drop(tx);
        assert!(receive(&rx, Duration::ZERO)
            .unwrap_err()
            .to_string()
            .contains("stopped"));
        let (tx, rx) = mpsc::channel();
        tx.send(42).unwrap();
        assert_eq!(receive(&rx, Duration::ZERO).unwrap(), 42);
    }
}
