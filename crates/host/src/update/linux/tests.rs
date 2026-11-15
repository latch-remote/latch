use super::*;
use std::fs;
use std::path::PathBuf;
use std::time::Instant;

fn helper(name: &str) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command.args([
        "--exact",
        &format!("update::linux::tests::{name}"),
        "--nocapture",
    ]);
    command
}

#[test]
fn supervisor_fixture_entry() {
    if std::env::var_os("LATCH_SYSTEMD_FIXTURE").is_none() {
        return;
    }
    let state = linux::UpdateState::load(&linux::state_path().unwrap())
        .unwrap()
        .unwrap();
    let manifest = state.exe.parent().unwrap().join("release.json");
    supervise_with(
        &state.exe,
        main_pid().unwrap(),
        &state.version,
        true,
        || update::parse_manifest(&fs::read(manifest)?),
    )
    .unwrap_or_else(|e| {
        eprintln!("supervisor fixture outcome: {e:#}");
    });
}

#[test]
fn request_fixture_entry() {
    if std::env::var_os("LATCH_SYSTEMD_FIXTURE").is_none() {
        return;
    }
    let path = linux::state_path().unwrap();
    let state = linux::UpdateState::load(&path).unwrap().unwrap();
    let mut command = helper("supervisor_fixture_entry");
    command.env("LATCH_SYSTEMD_FIXTURE", "1");
    launch(&mut command, &state, &path).unwrap();
}

#[test]
fn recovery_fixture_entry() {
    if std::env::var_os("LATCH_SYSTEMD_FIXTURE").is_some() {
        reconcile().unwrap();
    }
}

#[test]
fn linux_systemd_update_survives_restart_rolls_back_and_recovers() {
    if linux::prerequisites().is_err() {
        eprintln!("SKIP real systemd Linux updater integration: systemd-run or a reachable user manager is unavailable");
        assert!(
            std::env::var_os("LATCH_REQUIRE_SYSTEMD_TEST").is_none(),
            "CI requires the real systemd updater integration"
        );
        return;
    }
    let loaded = Command::new("systemctl")
        .args([
            "--user",
            "show",
            "--property=LoadState",
            "--value",
            "latch.service",
        ])
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&loaded.stdout).trim(),
        "not-found",
        "real systemd fixture requires an unused latch.service"
    );
    assert!(
        std::net::TcpListener::bind(("127.0.0.1", latch_core::CONTROL_PORT)).is_ok(),
        "real systemd fixture requires an unused control port"
    );
    let dir = tempfile::tempdir().unwrap();
    let result = helper("systemd_fixture_session")
        .env("LATCH_SYSTEMD_FIXTURE", "1")
        .env("LATCH_DATA_DIR", dir.path())
        .status()
        .unwrap();
    assert!(result.success());
}

fn wait_for(description: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(110);
    while !condition() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {description}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

struct FixtureUnit {
    unit: PathBuf,
}
impl Drop for FixtureUnit {
    fn drop(&mut self) {
        let _ = Command::new("systemctl")
            .args(["--user", "stop", "latch.service"])
            .status();
        let _ = fs::remove_file(&self.unit);
        let _ = Command::new("systemctl")
            .args(["--user", "daemon-reload"])
            .status();
    }
}

#[test]
fn systemd_fixture_session() {
    if std::env::var_os("LATCH_SYSTEMD_FIXTURE").is_none() {
        return;
    }
    let dir = latch_core::config::data_dir()
        .unwrap()
        .canonicalize()
        .unwrap();
    let exe = dir.join("latch-host");
    let source = dir.join("fixture.rs");
    fs::write(&source, r#"
        use std::io::{Read, Write};
        use std::process::Command;
        fn main() {
            if std::env::args().any(|a| a == "--version") {
                println!("latch-host {}", env!("FIXTURE_VERSION")); return;
            }
            Command::new(std::env::var_os("LATCH_RECOVERY_HELPER").unwrap())
                .args(["--exact", "update::linux::tests::recovery_fixture_entry", "--nocapture"])
                .status().unwrap();
            if env!("FIXTURE_MODE") == "exit" { std::process::exit(7); }
            let listener = std::net::TcpListener::bind(("0.0.0.0", 47850)).unwrap();
            for mut socket in listener.incoming().flatten() {
                let mut request = [0; 4096];
                let n = socket.read(&mut request).unwrap();
                if String::from_utf8_lossy(&request[..n]).starts_with("GET /start ") {
                    Command::new(std::env::var_os("LATCH_RECOVERY_HELPER").unwrap())
                        .args(["--exact", "update::linux::tests::request_fixture_entry", "--nocapture"])
                        .status().unwrap();
                }
                let mut log = std::fs::OpenOptions::new().create(true).append(true)
                    .open(std::env::var_os("LATCH_HEALTH_LOG").unwrap()).unwrap();
                writeln!(log, "{}", env!("FIXTURE_VERSION")).unwrap();
                let version = if env!("FIXTURE_MODE") == "wrong" { "0.0.0" } else { env!("FIXTURE_VERSION") };
                let body = format!("{{\"version\":\"{version}\"}}");
                let _ = write!(socket, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            }
        }
    "#).unwrap();
    assert_eq!(latch_core::CONTROL_PORT, 47850);
    let build = |path: &Path, version: &str, mode: &str| {
        assert!(Command::new("rustc")
            .arg(&source)
            .arg("-o")
            .arg(path)
            .env("FIXTURE_VERSION", version)
            .env("FIXTURE_MODE", mode)
            .status()
            .unwrap()
            .success());
    };
    build(&exe, "1.0.0", "healthy");
    let unit_dir = PathBuf::from(std::env::var_os("HOME").unwrap()).join(".config/systemd/user");
    fs::create_dir_all(&unit_dir).unwrap();
    let unit = unit_dir.join("latch.service");
    assert!(!unit.exists(), "fixture requires an unused latch.service");
    fs::write(&unit, format!(
        "[Service]\nType=simple\nExecStart={}\nKillMode=control-group\nEnvironment=LATCH_SYSTEMD_FIXTURE=1\nEnvironment=LATCH_DATA_DIR={}\nEnvironment=LATCH_RECOVERY_HELPER={}\nEnvironment=LATCH_HEALTH_LOG={}\n",
        exe.display(), dir.display(), std::env::current_exe().unwrap().display(), dir.join("health.log").display()
    )).unwrap();
    let _unit = FixtureUnit { unit };
    assert!(Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .status()
        .unwrap()
        .success());
    // A fresh fixture unit has no loaded failed state to reset yet.
    assert!(Command::new("systemctl")
        .args(["--user", "start", "latch.service"])
        .status()
        .unwrap()
        .success());
    wait_for("original listener", || {
        main_pid()
            .is_ok_and(|pid| linux::owns_listener(pid, &exe, latch_core::CONTROL_PORT).is_ok())
    });
    let path = linux::state_path().unwrap();
    for (version, mode) in [
        ("999.0.0", "healthy"),
        ("999.0.1", "wrong"),
        ("999.0.2", "healthy"),
    ] {
        let root = dir.join(linux::asset_name().unwrap().trim_end_matches(".tar.gz"));
        fs::create_dir_all(&root).unwrap();
        build(&root.join("latch-host"), version, mode);
        let archive = dir.join("release.tar.gz");
        assert!(Command::new("tar")
            .arg("-czf")
            .arg(&archive)
            .arg("-C")
            .arg(&dir)
            .arg(root.file_name().unwrap())
            .status()
            .unwrap()
            .success());
        let bytes = fs::read(&archive).unwrap();
        fs::write(dir.join("release.json"), serde_json::to_vec(&serde_json::json!({
            "version": version, "tag_name": format!("v{version}"), "prerelease": false,
            "assets": [{"name": linux::asset_name().unwrap(), "size": bytes.len(),
                "digest": format!("sha256:{}", update::sha256_hex(&bytes)),
                "url": format!("https://github.com/{}/releases/download/v{version}/{}", update::REPO, linux::asset_name().unwrap())}]
        })).unwrap()).unwrap();
        let mut state =
            linux::UpdateState::request(&exe, &Version::parse(version).unwrap(), &bytes, &path)
                .unwrap();
        state.systemd = true;
        state.save(&path).unwrap();
        let old_pid = main_pid().unwrap();
        // The launcher really runs inside latch.service's control group.
        let _ = latch_core::http::request_with(
            ("127.0.0.1", latch_core::CONTROL_PORT),
            "GET",
            "/start",
            &[],
            &[],
            Duration::from_secs(15),
        );
        if version == "999.0.2" {
            wait_for("replacement health probe before supervisor kill", || {
                main_pid().is_ok_and(|pid| {
                    pid != 0
                        && pid != old_pid
                        && linux::owns_listener(pid, &exe, latch_core::CONTROL_PORT).is_ok()
                }) && fs::read_to_string(dir.join("health.log"))
                    .is_ok_and(|log| log.lines().any(|line| line == version))
            });
            assert!(Command::new("systemctl")
                .args(["--user", "kill", "--signal=KILL", "--kill-whom=main"])
                .arg(&state.unit)
                .status()
                .unwrap()
                .success());
            wait_for("supervisor death", || {
                !linux::UpdateState::load(&path).unwrap().unwrap().alive()
            });
            restart().unwrap();
        }
        wait_for("durable terminal outcome", || {
            linux::UpdateState::load(&path).unwrap().unwrap().terminal()
        });
        let outcome = linux::UpdateState::load(&path).unwrap().unwrap();
        assert_eq!(
            outcome.phase,
            if version == "999.0.0" {
                linux::Phase::Succeeded
            } else {
                linux::Phase::Failed
            }
        );
        // The driver has made no health requests to this replacement. These
        // records therefore prove the supervisor's real HTTP probation ran.
        assert!(
            fs::read_to_string(dir.join("health.log"))
                .unwrap()
                .lines()
                .any(|line| line == version),
            "health check reached the replacement"
        );
        let expected = "999.0.0";
        wait_for("retained or restored service", || {
            super::super::answering_version().as_deref() == Some(expected)
        });
        linux::verify_version(&exe, &Version::parse(expected).unwrap()).unwrap();
        wait_for("staging cleanup", || {
            !outcome.stage.as_ref().unwrap().exists()
        });
        println!("PASS real systemd update {version} ({mode}): {:?}; health probe ran, service survived or recovered, staging removed", outcome.phase);
    }
}

#[test]
fn a_failed_supervisor_outcome_releases_the_busy_flag() {
    let dir = tempfile::tempdir().unwrap();
    assert!(helper("monitor_failure_fixture_entry")
        .env("LATCH_SYSTEMD_FIXTURE", "1")
        .env("LATCH_DATA_DIR", dir.path())
        .status()
        .unwrap()
        .success());
}

#[test]
fn monitor_failure_fixture_entry() {
    if std::env::var_os("LATCH_SYSTEMD_FIXTURE").is_none() {
        return;
    }
    let path = linux::state_path().unwrap();
    let mut state =
        linux::UpdateState::request(Path::new("/unused"), &Version::new(999, 0, 0), &[], &path)
            .unwrap();
    state
        .finish(
            &path,
            &Err(anyhow::anyhow!("supervisor verification failed")),
        )
        .unwrap();
    let running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    monitor(running.clone());
    wait_for("busy flag after supervisor failure", || {
        !running.load(std::sync::atomic::Ordering::Acquire)
    });
    assert!(linux::UpdateState::load(&path)
        .unwrap()
        .unwrap()
        .reason
        .contains("verification failed"));
}

#[test]
fn a_direct_supervisor_keeps_the_unsupervised_services_environment() {
    let dir = tempfile::tempdir().unwrap();
    assert!(helper("direct_launch_fixture_entry")
        .env("LATCH_DIRECT_FIXTURE", "1")
        .env("LATCH_DATA_DIR", dir.path())
        .env("LATCH_FIXTURE_SESSION", "preserved")
        .status()
        .unwrap()
        .success());
}

#[test]
fn direct_launch_fixture_entry() {
    if std::env::var_os("LATCH_DIRECT_FIXTURE").is_none() {
        return;
    }
    let path = linux::state_path().unwrap();
    let state =
        linux::UpdateState::request(Path::new("/unused"), &Version::new(999, 0, 0), &[], &path)
            .unwrap();
    assert!(!state.systemd);
    launch(
        &mut helper("direct_supervisor_fixture_entry"),
        &state,
        &path,
    )
    .unwrap();
    let outcome = linux::UpdateState::load(&path).unwrap().unwrap();
    assert_eq!(outcome.reason, "preserved");
    assert_eq!(outcome.phase, linux::Phase::Succeeded);
}

#[test]
fn direct_supervisor_fixture_entry() {
    if std::env::var_os("LATCH_DIRECT_FIXTURE").is_none() {
        return;
    }
    let path = linux::state_path().unwrap();
    let mut state = linux::UpdateState::load(&path).unwrap().unwrap();
    state.supervisor = std::process::id();
    state.supervisor_start = linux::process_start(state.supervisor);
    state.phase = linux::Phase::Succeeded;
    state.reason = std::env::var("LATCH_FIXTURE_SESSION").unwrap();
    state.save(&path).unwrap();
}
