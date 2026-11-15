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
