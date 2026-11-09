//! The window authenticates as the OS user owning this service.
use anyhow::{Context, Result};
use std::{fs::OpenOptions, io::Write, time::Duration};
pub const HEADER: &str = "x-latch-local-control";
pub const FILE: &str = "local-control-token";

pub fn token() -> Result<String> {
    token_at(&latch_core::config::data_dir()?.join(FILE))
}
fn token_at(path: &std::path::Path) -> Result<String> {
    if path.exists() {
        return read_token(path);
    }
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let staged = path.with_extension(format!("token-{:016x}", rand::random::<u64>()));
    match options.open(&staged) {
        Ok(mut file) => {
            let value = format!(
                "{:032x}{:032x}",
                rand::random::<u128>(),
                rand::random::<u128>()
            );
            file.write_all(value.as_bytes())?;
            file.sync_all()?;
            drop(file);
            let published = std::fs::hard_link(&staged, path);
            let _ = std::fs::remove_file(&staged);
            match published {
                Ok(()) => Ok(value),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => read_token(path),
                Err(e) => Err(e).context("publish local control token"),
            }
        }

        Err(e) => Err(e).context("create local control token"),
    }
}
fn read_token(path: &std::path::Path) -> Result<String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        anyhow::ensure!(
            std::fs::metadata(path)?.permissions().mode() & 0o077 == 0,
            "local control token must be readable only by its owner"
        );
    }
    let value = std::fs::read_to_string(path)?;
    anyhow::ensure!(
        value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit()),
        "invalid local control token"
    );
    Ok(value)
}

pub fn request(method: &str, path: &str, timeout: Duration) -> Result<latch_core::http::Response> {
    let token = token()?;
    latch_core::http::request_with(
        ("127.0.0.1", latch_core::CONTROL_PORT),
        method,
        path,
        &[("Content-Type", "application/json"), (HEADER, &token)],
        b"",
        timeout,
    )
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn secret_is_random_persistent_and_private() {
        let path = std::env::temp_dir().join(format!("latch-secret-{}", rand::random::<u64>()));
        let a = token_at(&path).unwrap();
        assert_eq!(a, token_at(&path).unwrap());
        assert_eq!(a.len(), 64);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        std::fs::remove_file(path).unwrap();
    }
}
