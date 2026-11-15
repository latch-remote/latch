#[cfg(target_os = "linux")]
#[test]
fn the_real_host_version_output_passes_the_exact_update_check() {
    latch_core::update::linux::verify_version(
        std::path::Path::new(env!("CARGO_BIN_EXE_latch-host")),
        &latch_core::update::current(),
    )
    .unwrap();
    assert!(latch_core::update::linux::verify_version(
        std::path::Path::new(env!("CARGO_BIN_EXE_latch-host")),
        &semver::Version::new(999, 0, 0),
    )
    .is_err());
}

#[cfg(target_os = "linux")]
#[test]
fn updater_bookkeeping_errors_never_prevent_the_real_service_from_answering() {
    use latch_core::update::linux::{FailureKind, Phase, UpdateState};
    use std::fs;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};
    struct Running(Child);
    impl Drop for Running {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    // This is an ordinary startup test, never a systemd unit installation.
    let root = tempfile::tempdir().unwrap();
    let exe = root.path().join("latch-host");
    fs::copy(env!("CARGO_BIN_EXE_latch-host"), &exe).unwrap();
    assert!(
        std::net::TcpListener::bind(("127.0.0.1", latch_core::CONTROL_PORT)).is_ok(),
        "startup fixture requires an unused control port"
    );
    for case in [
        "mode",
        "owner",
        "json",
        "future",
        "recovery",
        "cleanup",
        "data-path",
    ] {
        let dir = tempfile::tempdir_in(root.path()).unwrap();
        let data = dir.path().join("data");
        fs::create_dir(&data).unwrap();
        let journal = data.join("linux-update.json");
        if case == "data-path" {
            fs::remove_dir(&data).unwrap();
            fs::write(&data, b"blocked data directory").unwrap();
        } else if case == "cleanup" {
            fs::create_dir(data.join("linux-update.json.lock")).unwrap();
        } else {
            let mut state =
                UpdateState::request(&exe, &semver::Version::new(999, 0, 0), &[], &journal)
                    .unwrap();
            match case {
                "mode" => fs::set_permissions(&journal, fs::Permissions::from_mode(0o644)).unwrap(),
                "owner" => {
                    // An unprivileged runner can plant a foreign-owned journal via
                    // a symlink. The root Docker run also exercises a regular file.
                    if Command::new("id").arg("-u").output().unwrap().stdout == b"0\n" {
                        assert!(Command::new("chown")
                            .arg("65534:65534")
                            .arg(&journal)
                            .status()
                            .unwrap()
                            .success());
                    } else {
                        fs::remove_file(&journal).unwrap();
                        std::os::unix::fs::symlink("/etc/passwd", &journal).unwrap();
                    }
                }
                "json" => fs::write(&journal, b"{broken").unwrap(),
                "future" => {
                    let mut value = serde_json::to_value(state).unwrap();
                    value["phase"] = serde_json::json!("FuturePhase");
                    value["future_field"] = serde_json::json!(true);
                    fs::write(&journal, serde_json::to_vec(&value).unwrap()).unwrap();
                }
                "recovery" => {
                    let stage = root.path().join(".latch-update-startup-recovery");
                    fs::create_dir(&stage).unwrap();
                    fs::set_permissions(&stage, fs::Permissions::from_mode(0o700)).unwrap();
                    fs::write(stage.join("owner"), exe.as_os_str().as_encoded_bytes()).unwrap();
                    fs::write(stage.join("previous"), b"retained recovery").unwrap();
                    let installed = fs::metadata(&exe).unwrap();
                    state.original = Some((installed.dev(), installed.ino() + 1));
                    state.replacement = Some((installed.dev(), installed.ino() + 2));
                    state.stage = Some(stage);
                    state.phase = Phase::Swapping;
                    state.supervisor = 1;
                    state.save(&journal).unwrap();
                }
                _ => unreachable!(),
            }
        }
        let mut command = Command::new(&exe);
        command
            .arg("--background")
            .env("LATCH_DATA_DIR", &data)
            .env("HOME", dir.path())
            .env("XDG_CONFIG_HOME", dir.path().join("config"))
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut service = Running(command.spawn().unwrap());
        let deadline = Instant::now() + Duration::from_secs(30);
        let status = loop {
            assert!(
                service.0.try_wait().unwrap().is_none(),
                "service exited for {case}"
            );
            if let Ok(response) = latch_core::http::request_with(
                ("127.0.0.1", latch_core::CONTROL_PORT),
                "GET",
                "/v1/status",
                &[],
                &[],
                Duration::from_millis(500),
            ) {
                if response.status == 200 {
                    break response.body;
                }
            }
            assert!(
                Instant::now() < deadline,
                "service never answered for {case}"
            );
            std::thread::sleep(Duration::from_millis(50));
        };
        let expected = match case {
            "mode" | "owner" | "json" => "quarantined",
            "future" => "FuturePhase",
            "recovery" => "recovery failed",
            "cleanup" | "data-path" => "updater maintenance failed",
            _ => unreachable!(),
        };
        assert!(
            status.contains(expected),
            "{case} missing diagnostic {expected} in {status}"
        );
        drop(service);
        if case == "data-path" {
            fs::remove_file(&data).unwrap();
            fs::create_dir(&data).unwrap();
        }
        if case == "cleanup" {
            fs::remove_dir(data.join("linux-update.json.lock")).unwrap();
        }
        let mut later =
            UpdateState::request(&exe, &semver::Version::new(999, 0, 1), &[], &journal).unwrap();
        later
            .finish(&journal, &Ok(()), FailureKind::Installation)
            .unwrap();
        if case == "recovery" {
            assert_eq!(
                fs::read(root.path().join(".latch-update-startup-recovery/previous")).unwrap(),
                b"retained recovery"
            );
        }
    }
}
