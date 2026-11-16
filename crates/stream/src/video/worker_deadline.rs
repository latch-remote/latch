//! Bounded caller waits for drivers that cannot safely be interrupted.
use anyhow::{bail, Result};
use std::time::{Duration, Instant};
use std::{
    collections::VecDeque,
    sync::{mpsc, Mutex},
};

/// One access unit. A healthy decoder answers in milliseconds, and the
/// VideoToolbox path gives up after the same second.
#[cfg(windows)]
pub(super) const DECODE: Duration = Duration::from_secs(1);
/// Opening a decoder: Media Foundation, a Direct3D device and the driver's
/// session, which a cold system can take seconds over.
#[cfg(windows)]
pub(super) const STARTUP: Duration = Duration::from_secs(5);
/// Probing every codec once per process: three decoders opened and a
/// picture decoded on each. Giving up early would report a working GPU as
/// having no hardware decoder for as long as Latch runs.
#[cfg(windows)]
pub(super) const PROBE: Duration = Duration::from_secs(20);
const WINDOW: Duration = Duration::from_secs(600);
const MAX_STALLS: usize = 3;
static STALLS: Mutex<Vec<Instant>> = Mutex::new(Vec::new());

/// Shared with the caller so a blocked COM call can describe itself without
/// asking the worker to run any more code or release driver-owned objects.
pub(super) struct Progress(Mutex<Operation>);
struct Operation {
    phase: &'static str,
    phase_started: Instant,
    longest: (&'static str, Duration),
    started: Instant,
    configuration: String,
    outputs: u32,
    recent_output: VecDeque<(i32, u32)>,
    late_access_units: u64,
    picture_wait_reason: Option<&'static str>,
    picture_wait_pending: Option<i64>,
}
impl Progress {
    pub(super) fn new(phase: &'static str) -> Self {
        let now = Instant::now();
        Self(Mutex::new(Operation {
            phase,
            phase_started: now,
            longest: (phase, Duration::ZERO),
            started: now,
            configuration: String::new(),
            outputs: 0,
            recent_output: VecDeque::new(),
            late_access_units: 0,
            picture_wait_reason: None,
            picture_wait_pending: None,
        }))
    }
    pub(super) fn begin(&self, phase: &'static str) {
        let mut operation = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        operation.phase = phase;
        operation.started = now;
        operation.phase_started = now;
        operation.longest = (phase, Duration::ZERO);
        operation.outputs = 0;
        operation.recent_output.clear();
    }
    pub(super) fn phase(&self, phase: &'static str) {
        let mut operation = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let elapsed = operation.phase_started.elapsed();
        if elapsed > operation.longest.1 {
            operation.longest = (operation.phase, elapsed);
        }
        operation.phase = phase;
        operation.phase_started = Instant::now();
    }
    pub(super) fn configure(&self, configuration: String) {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .configuration = configuration;
    }
    pub(super) fn output(&self, code: i32, flags: u32) {
        let mut operation = self.0.lock().unwrap_or_else(|e| e.into_inner());
        operation.outputs = operation.outputs.saturating_add(1);
        if operation.recent_output.len() == 16 {
            operation.recent_output.pop_front();
        }
        operation.recent_output.push_back((code, flags));
    }
    pub(super) fn late_output(&self) {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .late_access_units += 1;
    }
    pub(super) fn picture_wait(&self, reason: Option<&'static str>, pending: Option<i64>) {
        let mut operation = self.0.lock().unwrap_or_else(|e| e.into_inner());
        operation.picture_wait_reason = reason;
        operation.picture_wait_pending = pending;
    }
    pub(super) fn describe(&self) -> String {
        let operation = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let elapsed = operation.phase_started.elapsed();
        let longest = if elapsed > operation.longest.1 {
            (operation.phase, elapsed)
        } else {
            operation.longest
        };
        format!(
            "phase={} phase_elapsed={:?} operation_elapsed={:?} longest_phase={} longest_elapsed={:?} output_calls={} recent_output={:?} late_access_units={} picture_wait_disabled={} picture_wait_reason={:?} picture_wait_pending={:?} {}",
            operation.phase,
            elapsed,
            operation.started.elapsed(),
            longest.0,
            longest.1,
            operation.outputs,
            operation.recent_output.iter().map(|&(code, flags)| (format!("0x{:08x}", code as u32), flags)).collect::<Vec<_>>(),
            operation.late_access_units,
            operation.picture_wait_reason.is_some(),
            operation.picture_wait_reason,
            operation.picture_wait_pending,
            operation.configuration,
        )
    }
}

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
    let started = Instant::now();
    match rx.recv_timeout(timeout) {
        Ok(value) => Ok(value),
        Err(mpsc::RecvTimeoutError::Timeout) => {
            STALLS
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(Instant::now());
            bail!("Media Foundation worker timed out after {:?} (limit {timeout:?}); abandoning driver-owned resources", started.elapsed());
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
        let progress = Progress::new("ProcessOutput");
        let error = receive(&rx, Duration::ZERO)
            .unwrap_err()
            .context(progress.describe());
        let diagnostic = format!("{error:#}");
        assert!(diagnostic.contains("timed out after"), "{diagnostic}");
        assert!(diagnostic.contains("phase=ProcessOutput"), "{diagnostic}");
        drop(tx);
        assert!(receive(&rx, Duration::ZERO)
            .unwrap_err()
            .to_string()
            .contains("stopped"));
        let (tx, rx) = mpsc::channel();
        tx.send(42).unwrap();
        assert_eq!(receive(&rx, Duration::ZERO).unwrap(), 42);
    }

    #[test]
    fn worker_diagnostics_preserve_phase_and_elapsed_time() {
        let progress = Progress::new("queued startup");
        progress.configure("software H.264".into());
        progress.begin("queued decode");
        progress.output(-1, 256);
        progress.late_output();
        progress.picture_wait(Some("retained delay"), Some(42));
        progress.phase("ProcessOutput");
        let error = progress.describe();
        assert!(error.contains("phase=ProcessOutput"), "{error}");
        assert!(error.contains("phase_elapsed="), "{error}");
        assert!(error.contains("operation_elapsed="), "{error}");
        assert!(error.contains("output_calls=1"), "{error}");
        assert!(error.contains("0xffffffff"), "{error}");
        assert!(error.contains("late_access_units=1"), "{error}");
        assert!(error.contains("picture_wait_disabled=true"), "{error}");
        assert!(error.contains("retained delay"), "{error}");
        assert!(error.contains("picture_wait_pending=Some(42)"), "{error}");
        assert!(error.contains("software H.264"), "{error}");
        for code in 0..20 {
            progress.output(code, 0);
        }
        assert_eq!(progress.0.lock().unwrap().recent_output.len(), 16);
        progress.begin("ProcessOutput");
        assert!(progress.describe().contains("picture_wait_disabled=true"));
        progress.0.lock().unwrap().outputs = u32::MAX;
        progress.output(0, 0);
        assert_eq!(progress.0.lock().unwrap().outputs, u32::MAX);
        progress.0.lock().unwrap().phase_started = Instant::now() - Duration::from_secs(2);
        progress.phase("send decoded pictures");
        let finished = progress.describe();
        assert!(
            finished.contains("phase=send decoded pictures"),
            "{finished}"
        );
        assert!(
            finished.contains("longest_phase=ProcessOutput"),
            "{finished}"
        );
        assert!(progress.0.lock().unwrap().longest.1 >= Duration::from_secs(2));
    }
}
