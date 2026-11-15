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
    let release = update::latest(None)?;
    if !release.is_newer_than(&update::current()) {
        println!("Latch {} is up to date", update::current());
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
    println!("Installing Latch {version}; reopen the window after the service restarts. See update.log for rollback results.");
    Ok(())
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
    let release = update::for_version(&version, None)?;
    let asset = release
        .asset(linux::asset_name()?)
        .context("Linux release asset")?;
    update::verify_host_asset(&release, &version, &req.body, &asset.name)?;
    Ok(version)
}

pub fn start(exe: &Path, version: &Version) -> Result<Child> {
    let supervised = crate::unix_setup::supervised();
    let mut command = if supervised {
        let mut c = Command::new("systemd-run");
        let mut environment = std::ffi::OsString::from("--setenv=LATCH_DATA_DIR=");
        environment.push(latch_core::config::data_dir()?.canonicalize()?);
        c.args(["--user", "--wait", "--collect", "--quiet"])
            .arg(format!("--unit=latch-update-{}", std::process::id()))
            .arg(environment)
            .arg(exe);
        c
    } else {
        Command::new(exe)
    };
    command
        .arg("--linux-update")
        .arg(version.to_string())
        .arg("--linux-update-exe")
        .arg(exe)
        .arg("--linux-update-pid")
        .arg(std::process::id().to_string());
    if supervised {
        command.arg("--linux-update-systemd");
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("start the Linux update supervisor")
}

pub fn supervise(exe: &Path, old_pid: u32, version: &Version, systemd: bool) -> Result<()> {
    let exe = linux::owned(exe)?;
    let transaction = linux::Transaction::begin(&exe)?;
    // A delayed supervisor must still run the installed inode. This also
    // rejects an old helper after another updater has already completed.
    drop(linux::Process::open(std::process::id(), &exe)?);
    // Pin the old process before network work so a reused PID is never killed.
    let process = linux::Process::open(old_pid, &exe)?;
    let release = update::for_version(version, None)?;
    transaction.prepare(&release)?;
    let mut child: Option<Child> = None;
    transaction.install(
        || {
            if systemd {
                // A crashing replacement may have exhausted StartLimitBurst.
                // Clear that counter so the restored release can start too.
                ensure!(
                    Command::new("systemctl")
                        .args(["--user", "reset-failed", "latch.service"])
                        .status()?
                        .success(),
                    "could not reset the systemd user service failure counter"
                );
                ensure!(
                    Command::new("systemctl")
                        .args(["--user", "restart", "latch.service"])
                        .status()?
                        .success(),
                    "systemd user service restart failed"
                );
            } else {
                if let Some(child) = child.as_mut() {
                    let _ = child.kill();
                    child.wait()?;
                }
                process.terminate()?;
                child = Some(super::start_service(
                    &exe,
                    &["--replaces", &old_pid.to_string()],
                )?);
            }
            Ok(())
        },
        || {
            super::watch(
                super::answering_version,
                &version.to_string(),
                super::PROBATION,
                super::HEALTHY_FOR,
                Duration::from_millis(500),
            )
            .map_err(anyhow::Error::msg)
        },
    )
}
