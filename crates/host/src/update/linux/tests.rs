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
    if name == "systemd_fixture_session" {
        command.arg("--ignored");
    }
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
#[ignore = "requires an explicitly isolated systemd user session and unused latch.service"]
fn linux_systemd_update_survives_restart_rolls_back_and_recovers() {
    if linux::prerequisites_for(true).is_err() {
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
        "REFUSE real systemd fixture because a latch.service user unit already exists"
    );
    assert!(
        std::net::TcpListener::bind(("127.0.0.1", latch_core::CONTROL_PORT)).is_ok(),
        "real systemd fixture requires an unused control port"
    );
    let dir = tempfile::tempdir().unwrap();
    let result = helper("systemd_fixture_session")
        .env("LATCH_SYSTEMD_FIXTURE", "1")
        .env("LATCH_DATA_DIR", dir.path())
        .output()
        .unwrap();
    if !result.status.success() {
        panic!(
            "systemd fixture failed\n{}\n{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
    }
    // Keep one top-level Rust test result in the job log. The script checks it.
    for line in String::from_utf8_lossy(&result.stdout)
        .lines()
        .filter(|line| line.starts_with("PASS "))
    {
        println!("{line}");
    }
}

fn wait_for(description: &str, mut condition: impl FnMut() -> bool) {
    let _diagnostics = FixtureDiagnostics;
    let deadline = Instant::now() + Duration::from_secs(110);
    while !condition() {
        if Instant::now() >= deadline {
            panic!("timed out waiting for {description}");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

// systemd v255 formats string states verbatim and u/i counters as decimal.
// Job has a special formatter which emits an empty value for the idle (0, /).
// See systemctl-show.c print_property() and org.freedesktop.systemd1.xml.
#[derive(Debug)]
struct UnitState {
    active: String,
    sub: String,
    result: String,
    restarts: u32,
    exit_status: i32,
    pid: u32,
}

impl UnitState {
    fn read() -> Result<Self> {
        let output = Command::new("systemctl")
            .args([
                "--user",
                "show",
                "--property=ActiveState,SubState,Result,NRestarts,ExecMainStatus,MainPID",
                "latch.service",
            ])
            .output()?;
        ensure!(
            output.status.success(),
            "read fixture unit state: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Self::parse(&String::from_utf8(output.stdout)?)
    }

    fn parse(text: &str) -> Result<Self> {
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
        Ok(Self {
            active: value("ActiveState")?.into(),
            sub: value("SubState")?.into(),
            result: value("Result")?.into(),
            restarts: value("NRestarts")?.parse()?,
            exit_status: value("ExecMainStatus")?.parse()?,
            pid: value("MainPID")?.parse()?,
        })
    }

    fn running(&self, previous_pid: u32) -> bool {
        self.active == "active"
            && self.sub == "running"
            && self.pid != 0
            && self.pid != previous_pid
    }

    fn crash_limited(&self, starts: usize) -> bool {
        self.active == "failed"
            && self.sub == "failed"
            && self.pid == 0
            && matches!(self.result.as_str(), "exit-code" | "start-limit-hit")
            && self.exit_status == 7
            && starts >= 3
            && self.restarts >= 2
    }
}

const DIAGNOSTIC_PROPERTIES: &str =
    "--property=LoadState,ActiveState,SubState,Result,MainPID,NRestarts,ExecMainStatus,Job";

fn systemd_diagnostics() {
    if std::env::var_os("LATCH_SYSTEMD_FIXTURE").is_none() {
        return;
    }
    let mut units = vec!["latch.service".to_string()];
    if let Some(root) = std::env::var_os("LATCH_DATA_DIR").map(PathBuf::from) {
        for name in [
            "linux-update.json",
            "starts.log",
            "health.log",
            "monitor-started",
        ] {
            use std::io::Read;
            let mut text = String::new();
            match fs::File::open(root.join(name))
                .and_then(|file| file.take(64 * 1024).read_to_string(&mut text))
            {
                Ok(_) => {
                    eprintln!("fixture {name}\n{text}");
                    if name == "linux-update.json" {
                        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) {
                            if let Some(unit) = value.get("unit").and_then(|unit| unit.as_str()) {
                                units.push(unit.into());
                            }
                        }
                    }
                }
                Err(error) => eprintln!("fixture {name}: {error}"),
            }
        }
    }
    for unit in units {
        for (program, args) in [
            (
                "systemctl",
                vec!["--user", "show", DIAGNOSTIC_PROPERTIES, unit.as_str()],
            ),
            (
                "journalctl",
                vec![
                    "--user",
                    "--unit",
                    unit.as_str(),
                    "--lines=80",
                    "--no-pager",
                    "--output=short-monotonic",
                ],
            ),
        ] {
            match Command::new(program).args(&args).output() {
                Ok(mut output) => {
                    output.stdout.truncate(64 * 1024);
                    output.stderr.truncate(64 * 1024);
                    eprintln!(
                        "{program} {args:?} ({})\n{}\n{}",
                        output.status,
                        String::from_utf8_lossy(&output.stdout),
                        String::from_utf8_lossy(&output.stderr)
                    )
                }
                Err(error) => eprintln!("{program} {args:?}: {error}"),
            }
        }
    }
}

struct FixtureDiagnostics;
impl Drop for FixtureDiagnostics {
    fn drop(&mut self) {
        if std::thread::panicking() {
            systemd_diagnostics();
        }
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

const SYSTEMD_FIXTURE_SOURCE: &str = r#"
        use std::io::{Read, Write};
        use std::process::Command;
        fn main() {
            if std::env::args().any(|a| a == "--version") {
                println!("latch-host {}", env!("FIXTURE_VERSION")); return;
            }
            Command::new(std::env::var_os("LATCH_RECOVERY_HELPER").unwrap())
                .args(["--exact", "update::linux::tests::recovery_fixture_entry", "--nocapture"])
                .status().unwrap();
            let data = std::path::PathBuf::from(std::env::var_os("LATCH_DATA_DIR").unwrap());
            let mut starts = std::fs::OpenOptions::new().create(true).append(true).open(data.join("starts.log")).unwrap();
            writeln!(starts, "{}", env!("FIXTURE_VERSION")).unwrap();
            if env!("FIXTURE_MODE") == "exit" {
                // Let the driver observe active/running before crashing, so
                // this case reaches the automatic restart limit.
                while !data.join("crash-release").exists() {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                std::process::exit(7);
            }
            let listener = std::net::TcpListener::bind(("0.0.0.0", 47850)).unwrap();
            if data.join("monitor-enabled").exists() {
                Command::new(std::env::var_os("LATCH_RECOVERY_HELPER").unwrap())
                    .args(["--exact", "update::linux::tests::running_monitor_fixture_entry", "--nocapture"])
                    .spawn().unwrap();
            }
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
    "#;

#[test]
#[ignore = "called only by the opt-in systemd fixture"]
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
    fs::write(&source, SYSTEMD_FIXTURE_SOURCE).unwrap();
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
        "[Unit]\nStartLimitIntervalSec=60\nStartLimitBurst=3\n[Service]\nRestart=on-failure\nRestartSec=50ms\nType=simple\nExecStart={}\nKillMode=control-group\nEnvironment=LATCH_SYSTEMD_FIXTURE=1\nEnvironment=LATCH_DATA_DIR={}\nEnvironment=LATCH_RECOVERY_HELPER={}\nEnvironment=LATCH_HEALTH_LOG={}\n",
        exe.display(), dir.display(), std::env::current_exe().unwrap().display(), dir.join("health.log").display()
    )).unwrap();
    let _unit = FixtureUnit { unit };
    let _diagnostics = FixtureDiagnostics;
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
    for (version, mode, scenario) in [
        ("999.0.0", "healthy", "healthy"),
        ("999.0.1", "wrong", "wrong-version"),
        ("999.0.2", "healthy", "killed-supervisor"),
        ("999.0.3", "exit", "crash-on-start"),
        ("999.0.4", "healthy", "monitor-recovery"),
    ] {
        if scenario == "monitor-recovery" {
            fs::write(dir.join("monitor-enabled"), []).unwrap();
        }
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
        if matches!(scenario, "killed-supervisor" | "monitor-recovery") {
            wait_for("replacement health probe before supervisor kill", || {
                main_pid().is_ok_and(|pid| {
                    pid != 0
                        && pid != old_pid
                        && linux::owns_listener(pid, &exe, latch_core::CONTROL_PORT).is_ok()
                }) && fs::read_to_string(dir.join("health.log"))
                    .is_ok_and(|log| log.lines().any(|line| line == version))
            });
            if scenario == "monitor-recovery" {
                wait_for("running service monitor", || {
                    fs::read_to_string(dir.join("monitor-started"))
                        .is_ok_and(|text| text == version)
                });
            }
            assert!(Command::new("systemctl")
                .args(["--user", "kill", "--signal=KILL", "--kill-whom=main"])
                .arg(&state.unit)
                .status()
                .unwrap()
                .success());
            wait_for("supervisor death", || {
                !linux::UpdateState::load(&path).unwrap().unwrap().alive()
            });
            if scenario == "killed-supervisor" {
                restart().unwrap();
            }
        }
        let crash_started = Instant::now();
        if scenario == "crash-on-start" {
            wait_for("replacement activation before fixture crash", || {
                let current = linux::UpdateState::load(&path).unwrap().unwrap();
                assert!(
                    !current.terminal(),
                    "replacement failed before activation: {}",
                    current.reason
                );
                UnitState::read().is_ok_and(|unit| unit.running(old_pid))
            });
            fs::write(dir.join("crash-release"), []).unwrap();
            let mut failed_since = None;
            wait_for("replacement start limit", || {
                let current = linux::UpdateState::load(&path).unwrap().unwrap();
                assert!(
                    !current.terminal(),
                    "replacement finished before the start limit: {}",
                    current.reason
                );
                let starts = fs::read_to_string(dir.join("starts.log"))
                    .unwrap()
                    .lines()
                    .filter(|line| *line == version)
                    .count();
                let Ok(unit) = UnitState::read() else {
                    return false;
                };
                if !unit.crash_limited(starts) {
                    failed_since = None;
                    return false;
                }
                match failed_since {
                    Some((restarts, since)) if restarts == unit.restarts => {
                        Instant::now().duration_since(since) >= Duration::from_millis(250)
                    }
                    _ => {
                        failed_since = Some((unit.restarts, Instant::now()));
                        false
                    }
                }
            });
            assert!(
                fs::read_to_string(dir.join("starts.log"))
                    .unwrap()
                    .lines()
                    .filter(|line| *line == version)
                    .count()
                    >= 3
            );
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
        if scenario != "crash-on-start" {
            assert!(
                fs::read_to_string(dir.join("health.log"))
                    .unwrap()
                    .lines()
                    .any(|line| line == version),
                "health check reached the replacement"
            );
        }
        let expected = "999.0.0";
        wait_for("retained or restored service", || {
            UnitState::read().is_ok_and(|unit| {
                unit.running(0)
                    && answering_process(
                        unit.pid,
                        &exe,
                        latch_core::CONTROL_PORT,
                        super::super::answering_version,
                    )
                    .as_deref()
                        == Some(expected)
            })
        });
        if scenario == "crash-on-start" {
            assert!(
                crash_started.elapsed() < Duration::from_secs(15),
                "definitive failure must roll back well before probation expires"
            );
            assert_eq!(outcome.failure, linux::FailureKind::Installation);
            assert!(
                outcome
                    .reason
                    .contains("replacement latch.service definitively failed"),
                "{}",
                outcome.reason
            );
            assert!(
                outcome
                    .reason
                    .contains("previous Linux release was restored and restarted"),
                "{}",
                outcome.reason
            );
            println!("PASS definitive systemd failure rolled back within 15 seconds");
            let restored = UnitState::read().unwrap();
            assert_eq!(restored.result, "success");
            assert_eq!(restored.exit_status, 0);
            assert_eq!(
                restored.restarts, 0,
                "reset-failed must clear the restart counter"
            );
        }
        linux::verify_version(&exe, &Version::parse(expected).unwrap()).unwrap();
        wait_for("staging cleanup", || {
            !outcome.stage.as_ref().unwrap().exists()
        });
        if matches!(scenario, "killed-supervisor" | "monitor-recovery") {
            assert_eq!(outcome.failure, linux::FailureKind::Recovery);
            assert!(
                outcome
                    .reason
                    .contains("supervisor stopped before completion"),
                "{}",
                outcome.reason
            );
        }
        match scenario {
            "crash-on-start" => println!("PASS crash-on-start replacement, start limit reached, previous service restored"),
            "monitor-recovery" => println!("PASS running-service monitor recovery, previous service restored"),
            _ => println!("PASS real systemd update {version} ({scenario}), {:?}, health probe ran, service survived or recovered, staging removed", outcome.phase),
        }
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
            linux::FailureKind::Installation,
        )
        .unwrap();
    let running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    monitor(
        running.clone(),
        std::sync::Arc::new(parking_lot::Mutex::new(Vec::new())),
    );
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

#[test]
fn running_monitor_fixture_entry() {
    if std::env::var_os("LATCH_SYSTEMD_FIXTURE").is_none() {
        return;
    }
    let path = linux::state_path().unwrap();
    let state = linux::UpdateState::load(&path).unwrap().unwrap();
    fs::write(
        path.parent().unwrap().join("monitor-started"),
        state.version.to_string(),
    )
    .unwrap();
    let running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    monitor(
        running.clone(),
        std::sync::Arc::new(parking_lot::Mutex::new(Vec::new())),
    );
    wait_for("running service monitor completion", || {
        !running.load(std::sync::atomic::Ordering::Acquire)
    });
}

#[test]
fn the_health_closure_rejects_a_right_version_from_another_process() {
    use std::io::{Read, Write};
    let dir = tempfile::tempdir().unwrap();
    let address_file = dir.path().join("address");
    let mut child = helper("impostor_health_fixture_entry")
        .env("LATCH_IMPOSTOR_ADDRESS", &address_file)
        .spawn()
        .unwrap();
    struct Stop<'a>(&'a mut Child);
    impl Drop for Stop<'_> {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let stop = Stop(&mut child);
    wait_for("impostor address", || address_file.exists());
    let address: std::net::SocketAddr = fs::read_to_string(address_file).unwrap().parse().unwrap();
    let response = || {
        let mut tcp = std::net::TcpStream::connect(address).unwrap();
        tcp.write_all(b"GET /v1/status HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .unwrap();
        let mut body = String::new();
        tcp.read_to_string(&mut body).unwrap();
        assert!(body.contains("999.0.0"));
        Some("999.0.0".to_string())
    };
    let exe = std::env::current_exe().unwrap();
    assert_eq!(response().as_deref(), Some("999.0.0"));
    // This PID has the expected executable but the other process owns the port.
    assert_eq!(
        answering_process(std::process::id(), &exe, address.port(), response),
        None
    );
    // The answering PID has the listener, but a different executable inode.
    let other = dir.path().join("expected-host");
    fs::copy(&exe, &other).unwrap();
    assert_eq!(
        answering_process(stop.0.id(), &other, address.port(), response),
        None
    );
    assert_eq!(
        answering_process(stop.0.id(), &exe, address.port(), response).as_deref(),
        Some("999.0.0")
    );
}

#[test]
fn impostor_health_fixture_entry() {
    use std::io::{Read, Write};
    let Some(path) = std::env::var_os("LATCH_IMPOSTOR_ADDRESS") else {
        return;
    };
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    fs::write(path, listener.local_addr().unwrap().to_string()).unwrap();
    for mut socket in listener.incoming().flatten() {
        let mut request = [0; 4096];
        assert!(socket.read(&mut request).unwrap() > 0);
        socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 21\r\nConnection: close\r\n\r\n{\"version\":\"999.0.0\"}").unwrap();
    }
}

#[test]
fn launch_waits_for_the_supervisors_fsynced_acknowledgement() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("linux-update.json");
    let state =
        linux::UpdateState::request(Path::new("/unused"), &Version::new(999, 0, 0), &[], &path)
            .unwrap();
    let root = dir.path().to_owned();
    let (done, result) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let mut command = helper("acknowledgement_fixture_entry");
        command.env("LATCH_ACK_ROOT", &root);
        done.send(start_with(&mut command, &state, &path)).unwrap();
    });
    wait_for("supervisor before acknowledgement", || {
        dir.path().join("ready").exists()
    });
    let early = result.recv_timeout(Duration::from_millis(200));
    fs::write(dir.path().join("go"), []).unwrap();
    if early.is_ok() {
        fs::write(dir.path().join("observed"), []).unwrap();
    }
    assert!(matches!(
        early,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout)
    ));
    let launched = result.recv_timeout(Duration::from_secs(5)).unwrap();
    fs::write(dir.path().join("observed"), []).unwrap();
    launched.unwrap();
    worker.join().unwrap();
}

#[test]
fn acknowledgement_fixture_entry() {
    let Some(root) = std::env::var_os("LATCH_ACK_ROOT").map(PathBuf::from) else {
        return;
    };
    fs::write(root.join("ready"), []).unwrap();
    wait_for("acknowledgement release", || root.join("go").exists());
    let path = root.join("linux-update.json");
    let mut state = linux::UpdateState::load(&path).unwrap().unwrap();
    state.supervisor = std::process::id();
    state.supervisor_start = linux::process_start(state.supervisor);
    state.phase = linux::Phase::Started;
    state.save(&path).unwrap();
    // Keep the process alive until launch observes its persisted acknowledgement.
    wait_for("acknowledgement observed", || {
        root.join("observed").exists()
    });
}

#[test]
fn a_failed_transient_unit_is_reported_without_acknowledgement() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    for (name, text) in [
        ("systemd-run", "#!/bin/sh\nexit 0\n"),
        ("systemctl", "#!/bin/sh\nprintf 'failed\\n'\n"),
    ] {
        let path = dir.path().join(name);
        fs::write(&path, text).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }
    assert!(helper("failed_unit_fixture_entry")
        .env("LATCH_FAILED_UNIT_ROOT", dir.path())
        .env("PATH", dir.path())
        .status()
        .unwrap()
        .success());
}

#[test]
fn failed_unit_fixture_entry() {
    let Some(root) = std::env::var_os("LATCH_FAILED_UNIT_ROOT").map(PathBuf::from) else {
        return;
    };
    let path = root.join("linux-update.json");
    let mut state =
        linux::UpdateState::request(Path::new("/unused"), &Version::new(999, 0, 0), &[], &path)
            .unwrap();
    state.systemd = true;
    state.save(&path).unwrap();
    let start = Instant::now();
    let error = start_with(&mut Command::new("/unused"), &state, &path).unwrap_err();
    assert!(error
        .to_string()
        .contains("unit stopped before supervisor startup"));
    assert!(start.elapsed() < Duration::from_secs(2));
    let outcome = linux::UpdateState::load(&path).unwrap().unwrap();
    assert_eq!(outcome.phase, linux::Phase::Failed);
    assert_eq!(outcome.failure, linux::FailureKind::Transient);
}

#[test]
fn crashing_fixture_waits_for_completed_activation_before_exiting() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("fixture.rs");
    let exe = dir.path().join("fixture");
    fs::write(&source, SYSTEMD_FIXTURE_SOURCE).unwrap();
    assert!(Command::new("rustc")
        .arg(&source)
        .arg("-o")
        .arg(&exe)
        .env("FIXTURE_VERSION", "999.0.3")
        .env("FIXTURE_MODE", "exit")
        .status()
        .unwrap()
        .success());
    let mut child = Command::new(exe)
        .env("LATCH_DATA_DIR", dir.path())
        .env("LATCH_RECOVERY_HELPER", "/usr/bin/true")
        .spawn()
        .unwrap();
    struct Stop<'a>(&'a mut Child);
    impl Drop for Stop<'_> {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let stop = Stop(&mut child);
    wait_for("crashing fixture startup", || {
        dir.path().join("starts.log").exists()
    });
    std::thread::sleep(Duration::from_millis(200));
    assert!(stop.0.try_wait().unwrap().is_none());
    fs::write(dir.path().join("crash-release"), []).unwrap();
    wait_for("fixture crash after activation", || {
        stop.0.try_wait().unwrap().is_some()
    });
    assert_eq!(stop.0.wait().unwrap().code(), Some(7));
}

#[test]
fn systemd_255_properties_gate_activation_and_automatic_restart_exhaustion() {
    let running = "ActiveState=active\nSubState=running\nResult=success\nNRestarts=0\nExecMainStatus=0\nMainPID=123\nJob=\n";
    let unit = UnitState::parse(running).unwrap();
    assert!(unit.running(42));
    assert!(!unit.running(123));
    assert!(!unit.crash_limited(3));
    assert!(
        !UnitState::parse(&running.replace("MainPID=123", "MainPID=0"))
            .unwrap()
            .running(42)
    );
    assert!(
        !UnitState::parse(&running.replace("SubState=running", "SubState=auto-restart"))
            .unwrap()
            .running(42)
    );
    for result in ["exit-code", "start-limit-hit"] {
        let failed = format!("ActiveState=failed\nSubState=failed\nResult={result}\nNRestarts=3\nExecMainStatus=7\nMainPID=0\nJob=\n");
        let unit = UnitState::parse(&failed).unwrap();
        assert!(unit.crash_limited(3));
        assert!(!unit.crash_limited(2));
        for (from, to) in [
            ("ActiveState=failed", "ActiveState=activating"),
            ("SubState=failed", "SubState=auto-restart"),
            ("NRestarts=3", "NRestarts=1"),
            ("ExecMainStatus=7", "ExecMainStatus=0"),
            ("MainPID=0", "MainPID=123"),
        ] {
            assert!(!UnitState::parse(&failed.replace(from, to))
                .unwrap()
                .crash_limited(3));
        }
    }
    assert!(UnitState::parse("ActiveState=active").is_err());
    assert!(UnitState::parse(&running.replace("NRestarts=0", "NRestarts=unknown")).is_err());
}

#[test]
fn a_failed_systemd_wait_reports_both_units_and_fixture_history() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("linux-update.json");
    let state =
        linux::UpdateState::request(Path::new("/unused"), &Version::new(999, 0, 0), &[], &path)
            .unwrap();
    fs::write(
        root.path().join("starts.log"),
        "999.0.0\n999.0.0\n999.0.0\n",
    )
    .unwrap();
    for name in ["systemctl", "journalctl"] {
        let script = root.path().join(name);
        fs::write(&script, format!("#!/bin/sh\nprintf '%s %s\\n' '{name}' \"$*\" >> \"$LATCH_DATA_DIR/calls\"\nprintf 'ActiveState=failed\\nNRestarts=3\\nfixture journal lines\\n'\n")).unwrap();
        fs::set_permissions(script, fs::Permissions::from_mode(0o700)).unwrap();
    }
    for scenario in ["wait", "assertion"] {
        let output = helper("diagnostic_wait_fixture_entry")
            .env("LATCH_SYSTEMD_FIXTURE", "1")
            .env("LATCH_DIAGNOSTIC_SCENARIO", scenario)
            .env("LATCH_DATA_DIR", root.path())
            .env("PATH", root.path())
            .output()
            .unwrap();
        assert!(!output.status.success());
        let calls = fs::read_to_string(root.path().join("calls")).unwrap();
        for unit in ["latch.service", &state.unit] {
            assert!(
                calls.contains(&format!(
                    "systemctl --user show {DIAGNOSTIC_PROPERTIES} {unit}"
                )),
                "{calls}"
            );
            assert!(
                calls.contains(&format!("journalctl --user --unit {unit} --lines=80")),
                "{calls}"
            );
        }
        let error = String::from_utf8_lossy(&output.stderr);
        for text in [
            "ActiveState=failed",
            "NRestarts=3",
            "fixture starts.log",
            "fixture linux-update.json",
            "injected failed",
        ] {
            assert!(error.contains(text), "{error}");
        }
    }
}

#[test]
fn diagnostic_wait_fixture_entry() {
    if std::env::var_os("LATCH_SYSTEMD_FIXTURE").is_some() {
        if std::env::var("LATCH_DIAGNOSTIC_SCENARIO").as_deref() == Ok("assertion") {
            let _diagnostics = FixtureDiagnostics;
            assert_eq!(1, 2, "injected failed assertion outside a wait");
        }
        wait_for("injected failure", || panic!("injected failed wait"));
    }
}

#[test]
fn a_replaced_request_keeps_the_original_launch_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("linux-update.json");
    let state =
        linux::UpdateState::request(Path::new("/unused"), &Version::new(999, 0, 0), &[], &path)
            .unwrap();
    state.fail_pending(&path, "cancelled").unwrap();
    let replacement =
        linux::UpdateState::request(Path::new("/unused"), &Version::new(999, 0, 1), &[], &path)
            .unwrap();
    let error = start_with(
        &mut Command::new(dir.path().join("missing-supervisor")),
        &state,
        &path,
    )
    .unwrap_err();
    assert!(
        format!("{error:#}").contains("start the independent Linux update process"),
        "{error:#}"
    );
    assert!(
        format!("{error:#}").contains("No such file or directory"),
        "{error:#}"
    );
    let current = linux::UpdateState::load(&path).unwrap().unwrap();
    assert_eq!(current.unit, replacement.unit);
    assert_eq!(current.phase, linux::Phase::Pending);
}

#[test]
fn only_final_failed_units_abort_probation() {
    for result in ["exit-code", "start-limit-hit", "oom-kill", "signal"] {
        let failed =
            format!("ActiveState=failed\nSubState=failed\nMainPID=0\nJob=\nResult={result}\n");
        assert!(definitive_failure(&failed)
            .unwrap()
            .unwrap()
            .contains(result));
        for (from, to) in [
            ("ActiveState=failed", "ActiveState=activating"),
            ("SubState=failed", "SubState=auto-restart"),
            ("SubState=failed", "SubState=auto-restart-queued"),
            ("Job=\n", "Job=123 /org/freedesktop/systemd1/job/123\n"),
            ("Job=\n", "Job=0\n"),
            ("MainPID=0", "MainPID=123"),
        ] {
            assert!(definitive_failure(&failed.replace(from, to))
                .unwrap()
                .is_none());
        }
    }
    assert!(definitive_failure("ActiveState=failed").is_err());
}

#[test]
fn a_definitively_failed_fake_systemd_unit_aborts_probation_promptly() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("systemctl");
    fs::write(
        &script,
        "#!/bin/sh\nif [ -f \"$LATCH_FAILED_PROBATION_ROOT/error\" ]; then /bin/rm \"$LATCH_FAILED_PROBATION_ROOT/error\"; exit 1; fi\ncat \"$LATCH_FAILED_PROBATION_ROOT/state\"\n",
    )
    .unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
    let output = helper("failed_probation_fixture_entry")
        .env("LATCH_FAILED_PROBATION_ROOT", dir.path())
        .env("PATH", format!("{}:/usr/bin:/bin", dir.path().display()))
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&output.stderr)
        .contains("could not read latch.service failure state"));
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn failed_probation_fixture_entry() {
    let Some(root) = std::env::var_os("LATCH_FAILED_PROBATION_ROOT").map(PathBuf::from) else {
        return;
    };
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .with_writer(std::io::stderr)
        .init();
    for result in ["exit-code", "start-limit-hit"] {
        fs::write(
            root.join("state"),
            format!("ActiveState=failed\nSubState=failed\nMainPID=0\nJob=\nResult={result}\n"),
        )
        .unwrap();
        let start = Instant::now();
        let verdict = watch_replacement(Path::new("/unused"), &Version::new(999, 0, 0), true);
        assert!(verdict.unwrap_err().to_string().contains(result));
        assert!(start.elapsed() < Duration::from_secs(2));
    }
    fs::write(root.join("error"), []).unwrap();
    let start = Instant::now();
    assert!(
        watch_replacement(Path::new("/unused"), &Version::new(999, 0, 0), true)
            .unwrap_err()
            .to_string()
            .contains("start-limit-hit")
    );
    assert!(start.elapsed() < Duration::from_secs(3));
    for state in [
        "ActiveState=activating\nSubState=auto-restart\nMainPID=0\nJob=\nResult=exit-code\n",
        "ActiveState=failed\nSubState=failed\nMainPID=0\nJob=10 /job/10\nResult=exit-code\n",
    ] {
        fs::write(root.join("state"), state).unwrap();
        super::super::watch_checked(
            || {
                assert!(service_failure().unwrap().is_none());
                Ok(Some("999.0.0".into()))
            },
            "999.0.0",
            Duration::from_secs(1),
            Duration::from_millis(20),
            Duration::from_millis(5),
        )
        .unwrap();
    }
}

#[test]
fn each_interrupted_swap_and_restore_queues_a_service_restart() {
    use std::os::unix::fs::PermissionsExt;
    for point in [
        "before-swap-rename",
        "after-swap-rename",
        "probation-failed-before-restoring",
        "restoring-before-rename",
        "restored-before-completion",
        "legacy-restored-before-journal",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let systemctl = dir.path().join("systemctl");
        fs::write(
            &systemctl,
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$LATCH_DATA_DIR/systemctl-calls\"\n",
        )
        .unwrap();
        fs::set_permissions(&systemctl, fs::Permissions::from_mode(0o700)).unwrap();
        let output = helper("interrupted_recovery_fixture_entry")
            .env("LATCH_RECOVERY_POINT", point)
            .env("LATCH_DATA_DIR", dir.path())
            .env("PATH", dir.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{point}: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn interrupted_recovery_fixture_entry() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let Ok(point) = std::env::var("LATCH_RECOVERY_POINT") else {
        return;
    };
    let root = latch_core::config::data_dir().unwrap();
    let path = linux::state_path().unwrap();
    let exe = root.join("latch-host");
    let new = root.join("replacement");
    fs::write(&exe, b"old").unwrap();
    fs::write(&new, b"new").unwrap();
    let original = fs::metadata(&exe).unwrap();
    let replacement = fs::metadata(&new).unwrap();
    let stage = root.join(".latch-update-recovery-test");
    fs::create_dir(&stage).unwrap();
    fs::set_permissions(&stage, fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(stage.join("owner"), exe.as_os_str().as_encoded_bytes()).unwrap();
    fs::hard_link(&exe, stage.join("previous")).unwrap();
    if point != "before-swap-rename" {
        fs::rename(&new, &exe).unwrap();
    }
    if matches!(
        point.as_str(),
        "restored-before-completion" | "legacy-restored-before-journal"
    ) {
        fs::rename(stage.join("previous"), &exe).unwrap();
    }
    let mut state =
        linux::UpdateState::request(&exe, &Version::new(999, 0, 0), &[], &path).unwrap();
    state.phase = if matches!(
        point.as_str(),
        "restoring-before-rename" | "restored-before-completion"
    ) {
        linux::Phase::Restoring
    } else {
        linux::Phase::Swapping
    };
    state.systemd = true;
    state.supervisor = u32::MAX;
    state.supervisor_start = Some("dead".into());
    state.stage = Some(stage);
    state.original = Some((original.dev(), original.ino()));
    state.replacement = Some((replacement.dev(), replacement.ino()));
    state.save(&path).unwrap();
    let recovered = reconcile().unwrap().unwrap();
    assert_eq!(fs::read(&exe).unwrap(), b"old");
    assert_eq!(recovered.phase, linux::Phase::Failed);
    assert_eq!(
        recovered.failure,
        if point == "before-swap-rename" {
            linux::FailureKind::Transient
        } else {
            linux::FailureKind::Recovery
        }
    );
    assert_eq!(
        fs::read_to_string(root.join("systemctl-calls")).unwrap(),
        "--user --no-block restart latch.service\n"
    );
    reconcile().unwrap();
    assert_eq!(
        fs::read_to_string(root.join("systemctl-calls")).unwrap(),
        "--user --no-block restart latch.service\n"
    );
}
