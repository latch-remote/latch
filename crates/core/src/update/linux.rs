//! Transactions for user-owned Linux release binaries. The caller serializes
//! stream admission and keeps the supervisor outside latch.service's cgroup.
use super::{Asset, Release};
use anyhow::{bail, ensure, Context, Result};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub fn asset_name() -> Result<&'static str> {
    match std::env::consts::ARCH {
        "x86_64" => Ok("latch-host-linux-x64.tar.gz"),
        "aarch64" => Ok("latch-host-linux-arm64.tar.gz"),
        arch => bail!("no Linux release for {arch}"),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Managed {
    PackageManager,
    Nix,
    Container,
    ReadOnly,
    Permissions,
}

impl std::fmt::Display for Managed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::PackageManager => "update with your package manager (pacman/AUR)",
            Self::Nix => "update your Nix package or flake and rebuild",
            Self::Container => "rebuild and recreate the container image",
            Self::ReadOnly => "replace the read-only installation through its deployment tool",
            Self::Permissions => "have the installation owner replace the release binary",
        })
    }
}

#[derive(Default)]
struct Ownership {
    package: bool,
    container: bool,
    read_only: bool,
    writable: bool,
}

fn managed(path: &Path, f: Ownership) -> Option<Managed> {
    if path.starts_with("/nix/store") {
        Some(Managed::Nix)
    } else if f.package || system_package_path(path) {
        Some(Managed::PackageManager)
    } else if f.container {
        Some(Managed::Container)
    } else if f.read_only {
        Some(Managed::ReadOnly)
    } else if !f.writable {
        Some(Managed::Permissions)
    } else {
        None
    }
}

/// Resolve symlinks before checking ownership, package registration and the
/// backing filesystem. Replacement always targets that resolved file.
pub fn installation(path: &Path) -> Result<std::result::Result<PathBuf, Managed>> {
    let exe = path
        .canonicalize()
        .context("resolve the installed executable")?;
    let parent = exe.parent().context("executable has no parent")?;
    let meta = fs::metadata(&exe)?;
    let dir = fs::metadata(parent)?;
    let uid = unsafe { libc::geteuid() };
    let cpath = std::ffi::CString::new(parent.as_os_str().as_encoded_bytes())?;
    let (read_only, image) = filesystem(&exe)?;
    let (parent_read_only, parent_image) = filesystem(parent)?;
    let package = Command::new("pacman")
        .args(["-Qo", "--"])
        .arg(&exe)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    let facts = Ownership {
        package,
        container: container_install(
            Path::new("/"),
            image || parent_image,
            std::env::var_os("container").is_some()
                || std::env::var_os("LATCH_CONTAINER_INSTALL").is_some(),
        ),
        read_only: read_only || parent_read_only,
        writable: user_writable(&meta, &dir, uid)
            && parent.ancestors().all(|ancestor| {
                fs::metadata(ancestor).is_ok_and(|m| {
                    (m.uid() == 0 || m.uid() == uid)
                        && (m.mode() & 0o022 == 0 || m.mode() & 0o1000 != 0)
                })
            })
            && unsafe { libc::access(cpath.as_ptr(), libc::W_OK) } == 0,
    };
    Ok(match managed(&exe, facts) {
        Some(reason) => Err(reason),
        None => Ok(exe),
    })
}

fn container_install(root: &Path, image: bool, environment: bool) -> bool {
    image
        || environment
        || root.join(".dockerenv").exists()
        || root.join("run/.containerenv").exists()
}

fn filesystem(path: &Path) -> Result<(bool, bool)> {
    let path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())?;
    let mut vfs = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    ensure!(
        unsafe { libc::statvfs(path.as_ptr(), vfs.as_mut_ptr()) } == 0,
        "cannot inspect the installation filesystem"
    );
    let vfs = unsafe { vfs.assume_init() };
    let mut stat = std::mem::MaybeUninit::<libc::statfs>::uninit();
    ensure!(
        unsafe { libc::statfs(path.as_ptr(), stat.as_mut_ptr()) } == 0,
        "cannot inspect the installation filesystem type"
    );
    let stat = unsafe { stat.assume_init() };
    Ok((
        vfs.f_flag & libc::ST_RDONLY != 0,
        image_filesystem(stat.f_type as u64),
    ))
}

fn system_package_path(exe: &Path) -> bool {
    exe.starts_with("/usr/bin") || exe.starts_with("/usr/lib")
}

fn image_filesystem(kind: u64) -> bool {
    matches!(kind, 0x794c7630 | 0x73717368 | 0xe0f5e1e2)
}

fn user_writable(file: &fs::Metadata, dir: &fs::Metadata, uid: u32) -> bool {
    uid != 0
        && file.is_file()
        && file.uid() == uid
        && dir.uid() == uid
        && file.mode() & 0o200 != 0
        && file.mode() & 0o6022 == 0
        && dir.mode() & 0o300 == 0o300
        && dir.mode() & 0o022 == 0
}

pub fn owned(path: &Path) -> Result<PathBuf> {
    installation(path)?.map_err(|why| anyhow::anyhow!("{why}"))
}

/// Fetch the release archive into a private, disposable download directory.
pub fn download_release(release: &Release) -> Result<Vec<u8>> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("release.tar.gz");
    let asset = release
        .asset(asset_name()?)
        .context("Linux release asset")?;
    super::download(asset, None, &path)?;
    fs::read(path).context("read the verified Linux archive")
}

struct UpdateLock(File);

impl Drop for UpdateLock {
    fn drop(&mut self) {
        // A concurrently forked child can briefly inherit the open description
        // before exec closes CLOEXEC descriptors. Explicit unlock releases it
        // immediately even while that child still holds its descriptor copy.
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

/// The lock inode remains in place across updates. Removing it would let a
/// waiter and a new updater lock different inodes for the same installation.
pub struct Transaction {
    exe: PathBuf,
    original: (u64, u64),
    stage: tempfile::TempDir,
    swapped: bool,
    replacement: Option<(u64, u64)>,
    _lock: UpdateLock,
}

impl Transaction {
    pub fn begin(exe: &Path) -> Result<Self> {
        Self::begin_resolved(&owned(exe)?)
    }

    fn begin_resolved(exe: &Path) -> Result<Self> {
        let parent = exe.parent().context("installation has no parent")?;
        let name = exe
            .file_name()
            .context("installation has no name")?
            .to_string_lossy();
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(parent.join(format!(".{name}.update.lock")))?;
        let meta = lock.metadata()?;
        ensure!(
            meta.is_file() && meta.uid() == unsafe { libc::geteuid() } && meta.mode() & 0o077 == 0,
            "unsafe updater lock"
        );
        ensure!(
            unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
            "another updater is already running"
        );
        let lock = UpdateLock(lock);
        let stage = tempfile::Builder::new()
            .prefix(".latch-update-")
            .tempdir_in(parent)?;
        fs::set_permissions(stage.path(), fs::Permissions::from_mode(0o700))?;
        let meta = fs::symlink_metadata(exe)?;
        ensure!(meta.is_file(), "installation changed to a symlink");
        Ok(Self {
            exe: exe.into(),
            original: (meta.dev(), meta.ino()),
            _lock: lock,
            stage,
            swapped: false,
            replacement: None,
        })
    }

    pub fn prepare(&self, release: &Release) -> Result<()> {
        self.prepare_with(release, |asset, path| super::download(asset, None, path))
    }

    fn prepare_with(
        &self,
        release: &Release,
        download: impl FnOnce(&Asset, &Path) -> Result<()>,
    ) -> Result<()> {
        ensure!(
            release.tag == format!("v{}", release.version)
                && release.is_newer_than(&super::current()),
            "release must match its tag and be newer"
        );
        let name = asset_name()?;
        let asset = release
            .asset(name)
            .context("release has no matching Linux archive")?;
        super::validate_published_asset(asset, &release.tag)?;
        let archive = self.stage.path().join("release.tar.gz");
        download(asset, &archive)?;
        fs::set_permissions(&archive, fs::Permissions::from_mode(0o600))?;
        super::verify_asset(asset, &archive)?;
        let new = self.stage.path().join("new");
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&new)?;
        // Write only the selected member to a file we created. Archive paths,
        // symlinks, permissions and other members never enter the filesystem.
        let member = format!("{}/latch-host", name.trim_end_matches(".tar.gz"));
        let mut tar = Command::new("tar")
            .arg("-xOzf")
            .arg(&archive)
            .arg("--")
            .arg(member)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let copied = std::io::copy(
            &mut tar
                .stdout
                .take()
                .context("tar stdout")?
                .take(512 * 1024 * 1024 + 1),
            &mut file,
        );
        if copied.as_ref().map_or(true, |n| *n > 512 * 1024 * 1024) {
            let _ = tar.kill();
        }
        let status = tar.wait()?;
        ensure!(
            copied? <= 512 * 1024 * 1024 && status.success(),
            "cannot extract the release binary"
        );
        file.sync_all()?;
        drop(file);
        let mut header = [0; 20];
        File::open(&new)?.read_exact(&mut header)?;
        let machine = match std::env::consts::ARCH {
            "x86_64" => 62u16,
            "aarch64" => 183,
            _ => unreachable!(),
        };
        ensure!(
            header[..7] == *b"\x7fELF\x02\x01\x01" && header[18..20] == machine.to_le_bytes(),
            "release binary has the wrong ELF architecture"
        );
        fs::set_permissions(&new, fs::Permissions::from_mode(0o755))?;
        verify_version(&new, &release.version)?;
        File::open(&new)?.sync_all()?;
        Ok(())
    }

    /// A hard link preserves the previous inode without removing the install
    /// path. The only visible replacement is a same-filesystem atomic rename.
    fn swap(&mut self) -> Result<()> {
        let meta = fs::symlink_metadata(&self.exe)?;
        ensure!(
            meta.is_file() && (meta.dev(), meta.ino()) == self.original,
            "installation changed while updating"
        );
        fs::hard_link(&self.exe, self.stage.path().join("previous"))?;
        File::open(self.stage.path().join("previous"))?.sync_all()?;
        File::open(self.stage.path())?.sync_all()?;
        let replacement = fs::symlink_metadata(self.stage.path().join("new"))?;
        ensure!(replacement.is_file(), "staged binary changed to a symlink");
        self.replacement = Some((replacement.dev(), replacement.ino()));
        fs::rename(self.stage.path().join("new"), &self.exe)?;
        self.swapped = true;
        File::open(self.exe.parent().unwrap())?.sync_all()?;
        Ok(())
    }

    fn rollback(&mut self) -> Result<()> {
        if self.swapped {
            let meta = fs::symlink_metadata(&self.exe)?;
            ensure!(
                meta.is_file() && Some((meta.dev(), meta.ino())) == self.replacement,
                "installation changed during probation; recovery copy was preserved"
            );
            fs::rename(self.stage.path().join("previous"), &self.exe)
                .context("restore the previous binary")?;
            self.swapped = false;
            File::open(self.exe.parent().unwrap())?.sync_all()?;
        }
        Ok(())
    }

    /// Restart and probation share one rollback path, including failures to
    /// launch. Keep the lock until the restored service has been restarted.
    pub fn install(
        mut self,
        mut restart: impl FnMut() -> Result<()>,
        health: impl FnOnce() -> Result<()>,
    ) -> Result<()> {
        self.swap()?;
        if let Err(why) = restart().and_then(|()| health()) {
            self.rollback()?;
            restart().context("restart the previous binary after rollback")?;
            return Err(why.context("the previous Linux release was restored and restarted"));
        }
        self.commit()
    }

    fn commit(mut self) -> Result<()> {
        ensure!(self.swapped, "update has not been swapped");
        self.swapped = false;
        fs::remove_dir_all(self.stage.path())?;
        Ok(())
    }
}

impl Drop for Transaction {
    fn drop(&mut self) {
        if let Err(e) = self.rollback() {
            // Preserve the recovery copy when rollback itself fails.
            tracing::error!(
                "Linux update rollback failed, recovery at {}: {e:#}",
                self.stage.path().display()
            );
            let _ = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(self.stage.path().join("ROLLBACK_FAILED"))
                .and_then(|mut file| file.write_all(self.exe.as_os_str().as_encoded_bytes()));
            // TempDir::disable_cleanup preserves the only recovery binary.
            self.stage.disable_cleanup(true);
        }
    }
}

fn verify_version(exe: &Path, want: &semver::Version) -> Result<()> {
    let output = tempfile::tempfile()?;
    let mut child = Command::new(exe)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(output.try_clone()?)
        .stderr(Stdio::null())
        .spawn()?;
    let start = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if start.elapsed() > Duration::from_secs(10) {
            let _ = child.kill();
            let _ = child.wait();
            bail!("release version check timed out");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    use std::io::{Seek, SeekFrom};
    let mut output = output;
    output.seek(SeekFrom::Start(0))?;
    let mut text = String::new();
    output.take(1024).read_to_string(&mut text)?;
    ensure!(
        status.success() && text.trim() == format!("latch-host {want}"),
        "release executable version does not exactly match {want}"
    );
    Ok(())
}

/// A kernel process handle pins the service across the download and swap.
pub struct Process(std::os::fd::OwnedFd);
impl Process {
    pub fn open(pid: u32, exe: &Path) -> Result<Self> {
        use std::os::fd::FromRawFd;
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
        ensure!(fd >= 0, "cannot pin the running service process");
        let fd = unsafe { std::os::fd::OwnedFd::from_raw_fd(fd as i32) };
        let process = fs::metadata(format!("/proc/{pid}/exe"))?;
        let installed = fs::metadata(exe)?;
        ensure!(
            (process.dev(), process.ino()) == (installed.dev(), installed.ino()),
            "service process is running a different installation"
        );
        Ok(Self(fd))
    }

    pub fn terminate(&self) -> Result<()> {
        let sent = unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                self.0.as_raw_fd(),
                libc::SIGTERM,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            )
        };
        if sent < 0 && std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH) {
            bail!("cannot stop the previous service");
        }
        let mut poll = libc::pollfd {
            fd: self.0.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        ensure!(
            unsafe { libc::poll(&mut poll, 1, 10_000) } > 0,
            "previous service did not stop"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use semver::Version;
    use std::io::Write;
    use std::os::unix::fs::symlink;

    #[test]
    fn every_external_install_rule_has_update_instructions() {
        assert_eq!(
            managed(
                Path::new("/home/ada/.local/bin/latch-host"),
                Ownership {
                    writable: true,
                    ..Default::default()
                }
            ),
            None
        );
        for (facts, expected, wording) in [
            (
                Ownership {
                    package: true,
                    writable: true,
                    ..Default::default()
                },
                Managed::PackageManager,
                "package manager",
            ),
            (
                Ownership {
                    container: true,
                    writable: true,
                    ..Default::default()
                },
                Managed::Container,
                "container",
            ),
            (
                Ownership {
                    read_only: true,
                    writable: true,
                    ..Default::default()
                },
                Managed::ReadOnly,
                "read-only",
            ),
            (Ownership::default(), Managed::Permissions, "owner"),
        ] {
            assert_eq!(
                managed(Path::new("/home/ada/.local/bin/latch-host"), facts),
                Some(expected)
            );
            assert!(expected.to_string().contains(wording));
        }
    }

    #[test]
    fn image_mounts_package_paths_and_real_file_permissions_are_classified() {
        assert_eq!(
            managed(
                Path::new("/nix/store/hash-latch/bin/latch-host"),
                Ownership::default()
            ),
            Some(Managed::Nix)
        );
        for path in ["/usr/bin/latch-host", "/usr/lib/latch/latch-host"] {
            assert_eq!(
                managed(
                    Path::new(path),
                    Ownership {
                        writable: true,
                        ..Default::default()
                    }
                ),
                Some(Managed::PackageManager)
            );
        }
        let package_link = tempfile::tempdir().unwrap();
        let link = package_link.path().join("latch-host");
        symlink("/usr/bin/true", &link).unwrap();
        assert_eq!(
            installation(&link).unwrap().unwrap_err(),
            Managed::PackageManager
        );
        let markers = tempfile::tempdir().unwrap();
        assert!(!container_install(markers.path(), false, false));
        assert!(container_install(markers.path(), true, false));
        assert!(container_install(markers.path(), false, true));
        fs::write(markers.path().join(".dockerenv"), b"").unwrap();
        assert!(container_install(markers.path(), false, false));
        fs::remove_file(markers.path().join(".dockerenv")).unwrap();
        fs::create_dir(markers.path().join("run")).unwrap();
        fs::write(markers.path().join("run/.containerenv"), b"").unwrap();
        assert!(container_install(markers.path(), false, false));
        for kind in [0x794c7630, 0x73717368, 0xe0f5e1e2] {
            assert!(image_filesystem(kind));
        }
        assert!(!image_filesystem(0xef53)); // ext4
        for path in ["/usr/bin/latch-host", "/usr/lib/latch/latch-host"] {
            assert!(system_package_path(Path::new(path)));
        }
        assert!(!system_package_path(Path::new(
            "/home/ada/.local/bin/latch-host"
        )));
        let (dir, exe) = install();
        let meta = fs::metadata(&exe).unwrap();
        let parent = fs::metadata(dir.path()).unwrap();
        assert!(!user_writable(&meta, &parent, 0));
        assert!(!user_writable(&meta, &parent, meta.uid() + 1));
        fs::set_permissions(&exe, fs::Permissions::from_mode(0o555)).unwrap();
        assert!(!user_writable(
            &fs::metadata(&exe).unwrap(),
            &parent,
            meta.uid()
        ));
        fs::set_permissions(&exe, fs::Permissions::from_mode(0o755)).unwrap();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o777)).unwrap();
        assert!(!user_writable(
            &fs::metadata(&exe).unwrap(),
            &fs::metadata(dir.path()).unwrap(),
            meta.uid()
        ));
    }

    fn install() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("latch-host");
        fs::write(&exe, b"old").unwrap();
        (dir, exe)
    }

    fn stage(tx: &Transaction, bytes: &[u8]) {
        fs::write(tx.stage.path().join("new"), bytes).unwrap();
    }

    #[test]
    fn swap_rollback_and_symlink_resolution_preserve_the_install_path() {
        let (dir, exe) = install();
        let link = dir.path().join("linked-host");
        symlink(&exe, &link).unwrap();
        let mut tx = Transaction::begin_resolved(&link.canonicalize().unwrap()).unwrap();
        assert_eq!(fs::metadata(tx.stage.path()).unwrap().mode() & 0o777, 0o700);
        stage(&tx, b"new");
        let original = fs::metadata(&exe).unwrap().ino();
        tx.swap().unwrap();
        assert_eq!(fs::read(&link).unwrap(), b"new");
        assert_eq!(
            fs::metadata(tx.stage.path().join("previous"))
                .unwrap()
                .ino(),
            original
        );
        assert!(fs::symlink_metadata(&link).unwrap().is_symlink());
        tx.rollback().unwrap();
        assert_eq!(fs::read(&link).unwrap(), b"old");
        assert_eq!(fs::metadata(&exe).unwrap().ino(), original);
    }

    #[test]
    fn a_failed_rollback_preserves_recovery_without_overwriting_an_external_install() {
        let (dir, exe) = install();
        let mut tx = Transaction::begin_resolved(&exe).unwrap();
        stage(&tx, b"new");
        tx.swap().unwrap();
        let external = dir.path().join("external");
        fs::write(&external, b"external install").unwrap();
        fs::rename(external, &exe).unwrap();
        assert!(tx.rollback().is_err());
        let recovery = tx.stage.path().to_owned();
        drop(tx);
        assert_eq!(fs::read(&exe).unwrap(), b"external install");
        assert_eq!(fs::read(recovery.join("previous")).unwrap(), b"old");
        assert_eq!(
            fs::metadata(recovery.join("ROLLBACK_FAILED"))
                .unwrap()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[test]
    fn a_second_updater_and_a_symlink_lock_are_refused() {
        let (dir, exe) = install();
        let tx = Transaction::begin_resolved(&exe).unwrap();
        assert!(Transaction::begin_resolved(&exe)
            .err()
            .unwrap()
            .to_string()
            .contains("another updater"));
        let inherited = tx._lock.0.try_clone().unwrap();
        drop(tx);
        // An inherited descriptor must not keep a completed transaction locked.
        let next = Transaction::begin_resolved(&exe).unwrap();
        drop(next);
        drop(inherited);
        fs::remove_file(dir.path().join(".latch-host.update.lock")).unwrap();
        let unrelated = dir.path().join("unrelated");
        fs::write(&unrelated, b"keep").unwrap();
        symlink(&unrelated, dir.path().join(".latch-host.update.lock")).unwrap();
        assert!(Transaction::begin_resolved(&exe).is_err());
        assert_eq!(fs::read(&unrelated).unwrap(), b"keep");
    }

    #[test]
    fn missing_stage_changed_install_and_failed_restart_keep_the_old_binary() {
        let (dir, exe) = install();
        let mut tx = Transaction::begin_resolved(&exe).unwrap();
        assert!(tx.swap().is_err());
        assert_eq!(fs::read(&exe).unwrap(), b"old");
        // swap may already have preserved the recovery inode.
        drop(tx);
        let mut tx = Transaction::begin_resolved(&exe).unwrap();
        stage(&tx, b"new");
        let other = dir.path().join("other");
        fs::write(&other, b"external install").unwrap();
        fs::rename(&other, &exe).unwrap();
        assert!(tx.swap().is_err());
        assert_eq!(fs::read(&exe).unwrap(), b"external install");
        drop(tx);
        fs::write(&exe, b"old").unwrap();
        let tx = Transaction::begin_resolved(&exe).unwrap();
        stage(&tx, b"new");
        let mut restarts = 0;
        assert!(tx
            .install(
                || {
                    restarts += 1;
                    bail!("restart failed")
                },
                || Ok(())
            )
            .is_err());
        assert_eq!(restarts, 2);
        assert_eq!(fs::read(&exe).unwrap(), b"old");
    }

    #[test]
    fn a_process_handle_rejects_other_installations_and_stops_only_its_process() {
        let mut child = Command::new("sleep").arg("30").spawn().unwrap();
        let (_dir, other) = install();
        assert!(Process::open(child.id(), &other).is_err());
        assert!(child.try_wait().unwrap().is_none());
        let exe = fs::read_link(format!("/proc/{}/exe", child.id())).unwrap();
        let process = Process::open(child.id(), &exe).unwrap();
        process.terminate().unwrap();
        assert!(!child.wait().unwrap().success());
        // The pinned handle remains safe once the PID can be reused.
        process.terminate().unwrap();
    }

    fn release(bytes: &[u8], version: &str) -> Release {
        let name = asset_name().unwrap();
        super::super::parse_manifest(&serde_json::to_vec(&serde_json::json!({
            "version": version, "tag_name": format!("v{version}"), "prerelease": false,
            "assets": [{"name": name, "size": bytes.len(),
                "digest": format!("sha256:{}", super::super::sha256_hex(bytes)),
                "url": format!("https://github.com/{}/releases/download/v{version}/{name}", super::super::REPO)}]
        })).unwrap()).unwrap()
    }

    #[test]
    fn interrupted_download_full_disk_bad_digest_and_downgrade_never_swap() {
        let (_dir, exe) = install();
        let release = release(b"archive", "999.0.0");
        for error in [libc::ENOSPC, libc::ECONNRESET] {
            let tx = Transaction::begin_resolved(&exe).unwrap();
            assert!(tx
                .prepare_with(&release, |_, path| {
                    fs::write(path, b"partial")?;
                    Err(std::io::Error::from_raw_os_error(error).into())
                })
                .is_err());
            assert_eq!(fs::read(&exe).unwrap(), b"old");
            let path = tx.stage.path().to_owned();
            drop(tx);
            assert!(!path.exists());
        }
        let tx = Transaction::begin_resolved(&exe).unwrap();
        assert!(tx
            .prepare_with(&release, |_, path| Ok(fs::write(path, b"corrupt")?))
            .is_err());
        let old = self::release(b"archive", "0.0.1");
        assert!(tx
            .prepare_with(&old, |_, _| panic!("a downgrade must not download"))
            .is_err());
        assert_eq!(fs::read(&exe).unwrap(), b"old");
    }

    #[test]
    fn an_archive_symlink_or_script_cannot_become_an_executable() {
        let (dir, exe) = install();
        let root = dir
            .path()
            .join(asset_name().unwrap().trim_end_matches(".tar.gz"));
        fs::create_dir(&root).unwrap();
        let script = dir.path().join("script");
        fs::write(&script, b"#!/bin/sh\n: > \"${0%/*}/executed\"\n").unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        let tarball = dir.path().join("archive.tar.gz");
        for linked in [true, false] {
            let member = root.join("latch-host");
            if linked {
                symlink(&script, &member).unwrap();
            } else {
                fs::copy(&script, &member).unwrap();
            }
            assert!(Command::new("tar")
                .arg("-czf")
                .arg(&tarball)
                .arg("-C")
                .arg(dir.path())
                .arg(root.file_name().unwrap())
                .status()
                .unwrap()
                .success());
            let bytes = fs::read(&tarball).unwrap();
            let release = release(&bytes, "999.0.0");
            let tx = Transaction::begin_resolved(&exe).unwrap();
            assert!(tx
                .prepare_with(&release, |_, path| Ok(fs::write(path, &bytes)?))
                .is_err());
            assert!(!tx.stage.path().join("executed").exists());
            assert_eq!(fs::read(&exe).unwrap(), b"old");
            fs::remove_file(member).unwrap();
        }
    }

    fn fixture_transaction(exe: &Path) -> Transaction {
        match installation(exe).unwrap() {
            Ok(_) => Transaction::begin(exe).unwrap(),
            // Container installs deliberately refuse production self-updates.
            // Exercise the filesystem transaction through the private test seam.
            Err(Managed::Container) => Transaction::begin_resolved(exe).unwrap(),
            Err(why) => panic!("fixture installation ownership was refused: {why}"),
        }
    }

    /// Compiled ELF fixtures exercise version execution, private tar extraction,
    /// real local HTTP transport, replacement and post-update failure recovery.
    /// Both native Linux CI architectures execute this test through cargo test.
    #[test]
    fn linux_release_fixture_updates_and_rolls_back() {
        let (dir, exe) = install();
        let source = dir.path().join("fixture.rs");
        fs::write(
            &source,
            r#"
            fn main() {
                let args: Vec<_> = std::env::args().collect();
                if args.get(1).map(String::as_str) == Some("--version") {
                    println!("latch-host {}", env!("FIXTURE_VERSION"));
                } else if env!("FIXTURE_HEALTH") != "healthy" {
                    std::process::exit(7);
                }
            }
        "#,
        )
        .unwrap();
        let build = |path: &Path, version: &str, health: &str| {
            assert!(Command::new("rustc")
                .arg(&source)
                .arg("-o")
                .arg(path)
                .env("FIXTURE_VERSION", version)
                .env("FIXTURE_HEALTH", health)
                .status()
                .unwrap()
                .success());
        };
        build(&exe, "1.0.0", "healthy");
        for (version, health) in [("999.0.0", "healthy"), ("999.0.1", "failed")] {
            let root = dir
                .path()
                .join(asset_name().unwrap().trim_end_matches(".tar.gz"));
            fs::create_dir_all(&root).unwrap();
            build(&root.join("latch-host"), version, health);
            let tarball = dir.path().join("fixture.tar.gz");
            assert!(Command::new("tar")
                .arg("-czf")
                .arg(&tarball)
                .arg("-C")
                .arg(dir.path())
                .arg(root.file_name().unwrap())
                .status()
                .unwrap()
                .success());
            let artifact = fs::read(&tarball).unwrap();
            let expected = release(&artifact, version);
            let manifest = serde_json::to_vec(&serde_json::json!({
                "version": version, "tag_name": expected.tag, "prerelease": false,
                "assets": [{"name": expected.assets[0].name, "url": expected.assets[0].url,
                    "size": artifact.len(), "digest": format!("sha256:{}", super::super::sha256_hex(&artifact))}]
            })).unwrap();
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            let server = std::thread::spawn(move || {
                for body in [manifest, artifact] {
                    let (mut socket, _) = listener.accept().unwrap();
                    let mut request = [0; 4096];
                    let _ = socket.read(&mut request).unwrap();
                    write!(
                        socket,
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    )
                    .unwrap();
                    socket.write_all(&body).unwrap();
                }
            });
            let manifest = crate::http::request_with(
                addr,
                "GET",
                "/release.json",
                &[],
                &[],
                Duration::from_secs(5),
            )
            .unwrap();
            let parsed = super::super::parse_manifest(manifest.body.as_bytes()).unwrap();
            assert_eq!(parsed, expected);
            let tx = fixture_transaction(&exe);
            tx.prepare_with(&parsed, |asset, path| {
                // Rewrite transport only. Manifest URLs retain the production
                // allowlist and shared download size/digest verification.
                super::super::download_with(asset, path, |_, sink| {
                    let mut socket = std::net::TcpStream::connect(addr)?;
                    write!(
                        socket,
                        "GET /artifact HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
                    )?;
                    let mut reader = std::io::BufReader::new(socket);
                    let (status, headers) = crate::http::read_response_head(&mut reader)?;
                    crate::http::read_response_body(
                        &mut reader,
                        &headers,
                        sink,
                        512 * 1024 * 1024,
                        false,
                    )?;
                    Ok(status)
                })
            })
            .unwrap();
            assert_eq!(
                fs::metadata(tx.stage.path().join("release.tar.gz"))
                    .unwrap()
                    .mode()
                    & 0o777,
                0o600
            );
            assert_eq!(
                fs::metadata(tx.stage.path().join("new")).unwrap().mode() & 0o777,
                0o755
            );
            let mut starts = 0;
            let outcome = tx.install(
                || {
                    starts += 1;
                    Ok(())
                },
                || {
                    ensure!(
                        Command::new(&exe).arg("--health").status()?.success(),
                        "new binary failed health check"
                    );
                    Ok(())
                },
            );
            if health == "healthy" {
                outcome.unwrap();
                verify_version(&exe, &Version::parse(version).unwrap()).unwrap();
                assert_eq!(starts, 1);
            } else {
                assert!(outcome.unwrap_err().to_string().contains("restored"));
                verify_version(&exe, &Version::parse("999.0.0").unwrap()).unwrap();
                assert!(Command::new(&exe)
                    .arg("--health")
                    .status()
                    .unwrap()
                    .success());
                assert_eq!(starts, 2);
            }
            server.join().unwrap();
        }
        let root = dir
            .path()
            .join(asset_name().unwrap().trim_end_matches(".tar.gz"));
        build(&root.join("latch-host"), "998.0.0", "healthy");
        let tarball = dir.path().join("wrong-version.tar.gz");
        assert!(Command::new("tar")
            .arg("-czf")
            .arg(&tarball)
            .arg("-C")
            .arg(dir.path())
            .arg(root.file_name().unwrap())
            .status()
            .unwrap()
            .success());
        let bytes = fs::read(tarball).unwrap();
        let tx = fixture_transaction(&exe);
        assert!(tx
            .prepare_with(&release(&bytes, "999.0.2"), |_, path| Ok(fs::write(
                path, bytes
            )?))
            .is_err());
        verify_version(&exe, &Version::parse("999.0.0").unwrap()).unwrap();
    }
}
