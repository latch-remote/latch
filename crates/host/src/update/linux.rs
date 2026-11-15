//! Linux uses the existing authenticated update endpoint and stream admission.
//! A transient systemd user service keeps the rollback supervisor alive while
//! latch.service restarts. Unsupervised services use a separate child process.
use anyhow::{ensure, Context, Result};
use latch_core::update::{self, linux};
use semver::Version;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

pub fn request_update() -> Result<()> {
    let exe = std::env::current_exe()?;
    linux::owned(&exe)?;
    let release = linux::latest()?;
    if !release.is_newer_than(&update::current()) {
        println!("Latch {} is up to date", update::current());
        return Ok(());
    }
    let status: latch_core::api::Status = latch_core::http::get_json(
        ("127.0.0.1", latch_core::CONTROL_PORT),
        "/v1/status",
        Duration::from_secs(2),
    )
    .context("read local service version before downloading")?;
    if Version::parse(&status.version)? >= release.version {
        println!("Latch {} is installed; reopen Latch", status.version);
        return Ok(());
    }
    let body = linux::download_release(&release)?;
    let version = release.version.to_string();
    let sha = update::sha256_hex(&body);
    let token =
        latch_core::config::local_control_token().context("start Latch's local service first")?;
    let response = latch_core::http::request_with(
        ("127.0.0.1", latch_core::CONTROL_PORT),
        "POST",
        latch_core::api::UPDATE_PATH,
        &[
            (latch_core::api::UPDATE_VERSION_HEADER, &version),
            (latch_core::api::UPDATE_SHA256_HEADER, &sha),
            (latch_core::config::LOCAL_CONTROL_HEADER, &token),
            ("Content-Type", "application/octet-stream"),
        ],
        &body,
        Duration::from_secs(120),
    )?;
    ensure!(response.status == 200, "update refused: {}", response.body);
    let deadline = std::time::Instant::now() + Duration::from_secs(250);
    loop {
        if let Some(state) = reconcile()? {
            if state.terminal() {
                ensure!(state.phase == linux::Phase::Succeeded, "{}", state.reason);
                println!("{}", state.reason);
                return Ok(());
            }
        }
        ensure!(
            std::time::Instant::now() < deadline,
            "Linux update outcome timed out; see linux-update.json"
        );
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// Same request headers and receiver-resolved manifest as Windows. The Linux
/// body is its architecture's tarball. No sender-supplied URL is accepted.
pub fn authenticate(req: &latch_core::http::Request, exe: &Path) -> Result<Version> {
    linux::owned(exe)?;
    let version = req
        .header(latch_core::api::UPDATE_VERSION_HEADER)
        .context("missing update version")?;
    let version = Version::parse(version)?;
    ensure!(
        version > update::current(),
        "this host already runs this version or newer"
    );
    let sha = req
        .header(latch_core::api::UPDATE_SHA256_HEADER)
        .context("missing update SHA-256")?;
    ensure!(
        update::sha256_hex(&req.body).eq_ignore_ascii_case(sha),
        "upload SHA-256 mismatch"
    );
    let release = linux::for_version(&version)?;
    let asset = release
        .asset(linux::asset_name()?)
        .context("Linux release asset")?;
    update::verify_host_asset(&release, &version, &req.body, &asset.name)?;
    Ok(version)
}

/// Launch returns only after the independent supervisor has fsynced its startup.
pub fn start(exe: &Path, version: &Version, body: &[u8]) -> Result<()> {
    linux::owned(exe)?;
    let path = linux::state_path()?;
    let mut state = linux::UpdateState::request(exe, version, body, &path)?;
    state.systemd = crate::unix_setup::supervised();
    state.save(&path)?;
    let mut command = Command::new(exe);
    command
        .arg("--linux-update")
        .arg(version.to_string())
        .arg("--linux-update-exe")
        .arg(exe)
        .arg("--linux-update-pid")
        .arg(std::process::id().to_string());
    if state.systemd {
        command.arg("--linux-update-systemd");
    }
    start_with(&mut command, &state, &path)
}

pub(crate) fn start_with(
    command: &mut Command,
    state: &linux::UpdateState,
    path: &Path,
) -> Result<()> {
    let result = launch(command, state, path);
    if let Err(error) = &result {
        match state.fail_pending(path, &format!("{error:#}")) {
            Ok(true) => {}
            Ok(false) => {
                if let Ok(Some(current)) = linux::UpdateState::load(path) {
                    if current.unit == state.unit && current.alive() && !current.terminal() {
                        return Ok(());
                    }
                }
            }
            Err(record_error) => {
                tracing::warn!("could not record supervisor launch failure: {record_error:#}");
            }
        }
    }
    result
}

fn launch(command: &mut Command, state: &linux::UpdateState, path: &Path) -> Result<()> {
    if state.systemd {
        // systemd-run exits once the unit has started. No --wait client remains in
        // latch.service's control group when that service is restarted.
        let mut environment = std::ffi::OsString::from("--setenv=LATCH_DATA_DIR=");
        environment.push(
            path.parent()
                .context("update data directory")?
                .canonicalize()?,
        );
        let mut launcher = Command::new("systemd-run");
        launcher
            .args([
                "--user",
                "--collect",
                "--quiet",
                "--service-type=exec",
                "--property=RuntimeMaxSec=240",
                "--property=TimeoutStopSec=10",
            ])
            .arg(format!("--unit={}", state.unit))
            .arg(environment);
        for (key, value) in command.get_envs() {
            if let Some(value) = value {
                let mut setting = std::ffi::OsString::from("--setenv=");
                setting.push(key);
                setting.push("=");
                setting.push(value);
                launcher.arg(setting);
            }
        }
        let output = launcher
            .arg(command.get_program())
            .args(command.get_args())
            .stdin(Stdio::null())
            .output()
            .context("start the Linux update unit")?;
        ensure!(
            output.status.success(),
            "Linux update unit failed to start: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    } else {
        // Preserve the existing unsupervised service's session environment.
        // Its replacement must also outlive the supervisor process.
        use std::os::unix::process::CommandExt;
        let mut child = command
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("start the independent Linux update process")?;
        std::thread::spawn(move || {
            let _ = child.wait();
        });
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let current = linux::UpdateState::load(path)?.context("missing supervisor state")?;
        if current.phase == linux::Phase::Failed {
            anyhow::bail!("{}", current.reason);
        }
        if current.supervisor != 0 && (current.alive() || current.terminal()) {
            return Ok(());
        }
        ensure!(
            !state.systemd || unit_active(&state.unit)?,
            "Linux update unit stopped before supervisor startup"
        );
        ensure!(
            std::time::Instant::now() < deadline,
            "Linux update supervisor startup timed out"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn unit_active(unit: &str) -> Result<bool> {
    let output = Command::new("systemctl")
        .args(["--user", "show", "--property=ActiveState", "--value", unit])
        .output()?;
    ensure!(
        output.status.success(),
        "could not read Linux update unit state: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(matches!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "active" | "activating" | "reloading"
    ))
}

/// Called on startup and while a service remains alive after a helper failure.
/// A live PID and its birth time protect against PID reuse. Unit state exposes
/// launch failures even before a PID is recorded.
pub fn reconcile() -> Result<Option<linux::UpdateState>> {
    let path = linux::state_path()?;
    if let Some(problem) = linux::UpdateState::repair(&path)? {
        anyhow::bail!("{problem}");
    }
    let Some(mut state) = linux::UpdateState::load(&path)? else {
        return Ok(None);
    };
    if !state.alive() {
        state.cleanup_completed()?;
    }
    if !state.terminal() && !state.alive() {
        // A newly submitted unit may still be executing its startup handshake.
        let starting = std::fs::metadata(&path)?
            .modified()?
            .elapsed()
            .is_ok_and(|age| age < Duration::from_secs(15));
        if state.supervisor == 0
            && (starting || state.systemd && unit_active(&state.unit).unwrap_or(false))
        {
            return Ok(Some(state));
        }
        let needs_restart = matches!(
            state.phase,
            linux::Phase::Swapping | linux::Phase::Restoring
        );
        let restored = match state.recover(&path) {
            Ok(restored) => restored,
            Err(e) if e.to_string().contains("another updater is already running") => {
                // Another startup monitor is recovering this same journal.
                return Ok(Some(state));
            }
            Err(e) => {
                return Err(e);
            }
        };
        if (restored || needs_restart) && state.systemd {
            // Queue, so restarting our own cgroup cannot interrupt systemctl.
            ensure!(
                Command::new("systemctl")
                    .args(["--user", "--no-block", "restart", "latch.service"])
                    .status()?
                    .success(),
                "restart restored Linux service failed"
            );
        } else if restored || needs_restart {
            use std::os::unix::fs::MetadataExt;
            let this = std::fs::metadata("/proc/self/exe")?;
            if Some((this.dev(), this.ino())) == state.replacement {
                super::start_service(&state.exe, &["--replaces", &std::process::id().to_string()])?;
                std::process::exit(0);
            }
        }
    }
    Ok(Some(state))
}

pub fn monitor(
    running: std::sync::Arc<std::sync::atomic::AtomicBool>,
    issues: std::sync::Arc<parking_lot::Mutex<Vec<String>>>,
) {
    std::thread::spawn(move || loop {
        match reconcile() {
            Ok(Some(state)) if !state.terminal() => (),
            Ok(_) => {
                running.store(false, std::sync::atomic::Ordering::Release);
                break;
            }
            Err(e) => {
                let problem = format!("Linux update recovery failed: {e:#}");
                tracing::error!("{problem}");
                let mut messages = issues.lock();
                if !messages.contains(&problem) {
                    messages.push(problem);
                }
                running.store(false, std::sync::atomic::Ordering::Release);
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(250));
    });
}

pub fn supervise(exe: &Path, old_pid: u32, version: &Version, systemd: bool) -> Result<()> {
    let result = (|| {
        let exe = linux::owned(exe)?;
        // Reject delayed helpers running a different installed inode.
        drop(linux::Process::open(std::process::id(), &exe)?);
        supervise_with(&exe, old_pid, version, systemd, || {
            linux::for_version(version)
        })
    })();
    if result.is_err() {
        let path = linux::state_path()?;
        if let Some(mut state) = linux::UpdateState::load(&path)? {
            if state.phase == linux::Phase::Pending && state.version == *version {
                state.finish(&path, &result, linux::FailureKind::Installation)?;
            }
        }
    }
    result
}

fn supervise_with(
    exe: &Path,
    old_pid: u32,
    version: &Version,
    systemd: bool,
    manifest: impl FnOnce() -> Result<update::Release>,
) -> Result<()> {
    let path = linux::state_path()?;
    let mut state = linux::UpdateState::load(&path)?.context("missing update request")?;
    ensure!(
        state.exe == exe && state.version == *version && state.phase == linux::Phase::Pending,
        "supervisor request does not match its journal"
    );
    state.supervisor = std::process::id();
    state.supervisor_start = linux::process_start(state.supervisor);
    state.phase = linux::Phase::Started;
    state.reason = "Verifying Linux update".into();
    state.save(&path)?;
    let mut failure = linux::FailureKind::Installation;
    let result = (|| -> Result<()> {
        let mut transaction = linux::Transaction::begin(exe)?;
        transaction.record(&path, &mut state)?;
        let process = linux::Process::open(old_pid, exe)?;
        // The supervisor obtains its own pinned manifest and re-hashes the
        // private upload. The service's digest decision is never trusted.
        let release = manifest().inspect_err(|_| {
            failure = linux::FailureKind::Transient;
        })?;
        transaction.prepare_uploaded(&release, &state.upload)?;
        let mut child: Option<Child> = None;
        transaction.install(
            || {
                if systemd {
                    restart()?;
                } else {
                    if let Some(child) = child.as_mut() {
                        let _ = child.kill();
                        child.wait()?;
                    }
                    process.terminate()?;
                    child = Some(super::start_service(
                        exe,
                        &["--replaces", &old_pid.to_string()],
                    )?);
                }
                Ok(())
            },
            || watch_replacement(exe, version, systemd),
        )
    })();
    // commit() has already persisted success before deleting recovery files.
    let mut current = linux::UpdateState::load(&path)?.context("missing supervisor state")?;
    if current.phase != linux::Phase::Succeeded {
        if matches!(
            current.phase,
            linux::Phase::Swapping | linux::Phase::Restoring
        ) {
            current.reason = format!(
                "Linux update needs recovery: {}",
                result.as_ref().unwrap_err()
            );
            current.save(&path)?;
        } else {
            current.finish(&path, &result, failure)?;
        }
    }
    result
}

fn watch_replacement(exe: &Path, version: &Version, systemd: bool) -> Result<()> {
    let mut failed_since = None;
    super::watch_checked(
        || {
            if systemd {
                if let Some(reason) = service_failure()
                    .inspect_err(|error| {
                        tracing::debug!("could not read latch.service failure state: {error:#}")
                    })
                    .ok()
                    .flatten()
                {
                    if failed_since
                        .get_or_insert_with(std::time::Instant::now)
                        .elapsed()
                        >= Duration::from_secs(1)
                    {
                        return Err(reason);
                    }
                } else {
                    failed_since = None;
                }
            }
            Ok((|| {
                let pid = if systemd {
                    main_pid().ok()?
                } else {
                    linux::listener_pid(exe, latch_core::CONTROL_PORT).ok()?
                };
                answering_process(pid, exe, latch_core::CONTROL_PORT, super::answering_version)
            })())
        },
        &version.to_string(),
        super::PROBATION,
        super::HEALTHY_FOR,
        Duration::from_millis(500),
    )
    .map_err(anyhow::Error::msg)
}

fn answering_process(
    pid: u32,
    exe: &Path,
    port: u16,
    response: impl FnOnce() -> Option<String>,
) -> Option<String> {
    linux::owns_listener(pid, exe, port).ok()?;
    let version = response()?;
    linux::owns_listener(pid, exe, port).ok()?;
    Some(version)
}

fn restart() -> Result<()> {
    for args in [
        ["--user", "reset-failed", "latch.service"],
        ["--user", "restart", "latch.service"],
    ] {
        let output = Command::new("systemctl").args(args).output()?;
        ensure!(
            output.status.success(),
            "{} failed: {}",
            args[1],
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

fn service_failure() -> Result<Option<String>> {
    let output = Command::new("systemctl")
        .args([
            "--user",
            "show",
            "--property=ActiveState,SubState,Result,MainPID,Job",
            "latch.service",
        ])
        .output()?;
    ensure!(
        output.status.success(),
        "could not read latch.service state"
    );
    definitive_failure(&String::from_utf8(output.stdout)?)
}

fn definitive_failure(text: &str) -> Result<Option<String>> {
    let properties: std::collections::BTreeMap<_, _> = text
        .lines()
        .filter_map(|line| line.split_once('='))
        .collect();
    let value = |name| {
        properties
            .get(name)
            .copied()
            .with_context(|| format!("missing unit property {name}"))
    };
    let result = value("Result")?;
    let failed = value("ActiveState")? == "failed"
        && value("SubState")? == "failed"
        && value("MainPID")? == "0"
        && value("Job")?.is_empty();
    Ok(failed.then(|| format!("replacement latch.service definitively failed (Result={result})")))
}

fn main_pid() -> Result<u32> {
    let output = Command::new("systemctl")
        .args([
            "--user",
            "show",
            "--property=MainPID",
            "--value",
            "latch.service",
        ])
        .output()?;
    ensure!(
        output.status.success(),
        "could not read latch.service MainPID"
    );
    Ok(String::from_utf8(output.stdout)?.trim().parse()?)
}

#[cfg(test)]
mod tests;
