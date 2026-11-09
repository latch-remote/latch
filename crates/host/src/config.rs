//! The host's few settings, in `%LOCALAPPDATA%\Latch\host.toml`.

use serde::{Deserialize, Serialize};

pub const FILE: &str = "host.toml";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct HostConfig {
    /// Let a Mac on the tailnet sleep, restart, or shut this PC down.
    pub power_allowed: bool,
    /// Explicit consent to the same-user elevated input route (Windows setup).
    #[serde(default)]
    pub elevated_input: bool,
    pub clipboard_sharing: bool,
    /// Required on tagged organisation hosts: local API has no membership assertion.
    pub allowed_logins: Vec<String>,
    /// Keep the background service registered to start at logon. On by
    /// default: a PC nobody can walk up to must come back reachable after
    /// every restart. The service re-adds the registry entry if it is gone.
    pub start_with_windows: bool,
    /// Do not idle-sleep when plugged in. Tailscale dies with the PC; a Mac
    /// on another network cannot wake it. On by default. Explicit Sleep
    /// from the Mac or the Start menu still works.
    pub stay_awake: bool,
    /// Take a newer `latch-host.exe` that a machine on the owner's account
    /// sends (`/v1/update`), which is how a Mac keeps its PCs current. Off,
    /// the PC refuses every push and is updated only at the PC itself.
    pub accept_updates: bool,
    /// Shell commands run before and after a stream, with the viewer's size.
    pub prep_do: String,
    pub prep_undo: String,
}

impl Default for HostConfig {
    fn default() -> Self {
        Self {
            power_allowed: true,
            elevated_input: false,
            clipboard_sharing: true,
            allowed_logins: Vec::new(),
            start_with_windows: true,
            stay_awake: true,
            accept_updates: true,
            prep_do: String::new(),
            prep_undo: String::new(),
        }
    }
}

impl HostConfig {
    fn safest() -> Self {
        Self {
            power_allowed: false,
            accept_updates: false,
            clipboard_sharing: false,
            ..Self::default()
        }
    }

    fn load_path(path: &std::path::Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(text) if !text.trim().is_empty() => match latch_core::config::parse_lenient(&text) {
                Ok((config, dropped)) if dropped.is_empty() => config,
                _ => Self::safest(),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::default(),
            _ => Self::safest(),
        }
    }

    pub fn load() -> Self {
        match latch_core::config::data_dir() {
            Ok(dir) => {
                let path = dir.join(FILE);
                #[cfg(windows)]
                Self::migrate_input(&path);
                Self::load_path(&path)
            }
            Err(_) => Self::safest(),
        }
    }

    #[cfg(windows)]
    fn migrate_input(path: &std::path::Path) {
        // Presence of the field is the one-time migration marker, including false.
        let text = std::fs::read_to_string(path).unwrap_or_default();
        if !input_migration_needed(&text) {
            return;
        }
        let enabled = crate::setup::powershell(
            "if (Get-ScheduledTask -TaskName 'LatchInput' -ErrorAction SilentlyContinue) { 'present' }"
        ).is_ok_and(|s| s.trim() == "present");
        // A malformed existing file must not be replaced with permissive defaults.
        let _ = latch_core::config::update(FILE, |c: &mut Self| c.elevated_input = enabled);
    }

    /// Read host.toml for a service that already runs on settings: an error
    /// rather than defaults for a file that cannot be read, does not parse,
    /// or is empty (an editor saving in place truncates it first), and the
    /// file left alone. Defaults mean remote power on and no prep command.
    pub fn try_load() -> anyhow::Result<Self> {
        let path = latch_core::config::data_dir()?.join(FILE);
        if std::fs::read_to_string(&path).is_ok_and(|text| text.trim().is_empty()) {
            anyhow::bail!("{} is empty", path.display());
        }
        latch_core::config::try_load(FILE)
    }
    /// Update the window's settings, preserving prep commands edited on disk.
    pub fn save(&self) -> anyhow::Result<()> {
        latch_core::config::update(FILE, |disk: &mut Self| {
            disk.power_allowed = self.power_allowed;
            disk.elevated_input = self.elevated_input;
            disk.clipboard_sharing = self.clipboard_sharing;
            disk.start_with_windows = self.start_with_windows;
            disk.stay_awake = self.stay_awake;
            disk.accept_updates = self.accept_updates;
        })
    }
}

#[cfg(any(windows, test))]
fn input_migration_needed(text: &str) -> bool {
    !text
        .lines()
        .any(|l| l.trim_start().starts_with("elevated_input"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn damaged_config_disables_remote_actions_without_rewriting_it() {
        let path = std::env::temp_dir().join(format!("latch-config-{}", std::process::id()));
        for text in [
            "power_allowed = false\n[",
            "power_allowed = \"false\"",
            "",
            " \n\t",
        ] {
            std::fs::write(&path, text).unwrap();
            let cfg = HostConfig::load_path(&path);
            assert!(!cfg.power_allowed && !cfg.accept_updates);
            assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
        }
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn input_upgrade_only_applies_before_an_explicit_choice() {
        assert!(input_migration_needed("power_allowed = true"));
        assert!(input_migration_needed(""));
        assert!(!input_migration_needed("elevated_input = true"));
        assert!(!input_migration_needed("elevated_input = false"));
    }

    #[test]
    fn defaults_allow_power_and_autostart() {
        let c = HostConfig::default();
        assert!(c.power_allowed);
        assert!(!c.elevated_input);
        assert!(c.clipboard_sharing);
        assert!(c.allowed_logins.is_empty());
        assert!(c.start_with_windows);
        assert!(c.stay_awake);
        assert!(c.accept_updates, "pushed updates stay on unless turned off");
        assert!(c.prep_do.is_empty() && c.prep_undo.is_empty());
        let old: HostConfig =
            toml::from_str("sunshine_user = \"old\"\nsunshine_pass = \"secret\"").unwrap();
        assert_eq!(old, c);
        // An old host.toml without the field gets the default too.
        let c: HostConfig = toml::from_str("power_allowed = false").unwrap();
        assert!(!c.power_allowed && c.start_with_windows && c.stay_awake && c.accept_updates);
        let c: HostConfig = toml::from_str("accept_updates = false").unwrap();
        assert!(!c.accept_updates);
    }
}
