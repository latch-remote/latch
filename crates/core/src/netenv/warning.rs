/// UI severity for a fixed, credential-free diagnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum WarningSeverity {
    Informational,
    Actionable,
}

/// Stable warning kinds. Raw OS errors and settings never enter diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum NetworkWarning {
    ChecksPending,
    ChecksTimedOut,
    InvalidProxy,
    InvalidDestination,
    UnsupportedPacProtocol,
    MalformedPacCandidate,
    PacEvaluatorUnavailable,
    IncompleteSystemProxy,
    LinuxPacUnavailable,
    PacFailed,
    UnsupportedSystemProtocol,
    MacStaticFallback,
    MacSettingsUnavailable,
    WpadFailed,
    KeychainCredentialsUnread,
    WindowsServiceSettings,
}

impl NetworkWarning {
    pub fn code(self) -> &'static str {
        match self {
            Self::ChecksPending => "checks_pending",
            Self::ChecksTimedOut => "checks_timed_out",
            Self::InvalidProxy => "invalid_proxy",
            Self::InvalidDestination => "invalid_destination",
            Self::UnsupportedPacProtocol => "unsupported_pac_protocol",
            Self::MalformedPacCandidate => "malformed_pac_candidate",
            Self::PacEvaluatorUnavailable => "pac_evaluator_unavailable",
            Self::IncompleteSystemProxy => "incomplete_system_proxy",
            Self::LinuxPacUnavailable => "linux_pac_unavailable",
            Self::PacFailed => "pac_failed",
            Self::UnsupportedSystemProtocol => "unsupported_system_protocol",
            Self::MacStaticFallback => "mac_static_fallback",
            Self::MacSettingsUnavailable => "mac_settings_unavailable",
            Self::WpadFailed => "wpad_failed",
            Self::KeychainCredentialsUnread => "keychain_credentials_unread",
            Self::WindowsServiceSettings => "windows_service_settings",
        }
    }
    pub fn severity(self) -> WarningSeverity {
        match self {
            Self::ChecksPending => WarningSeverity::Informational,
            Self::ChecksTimedOut => WarningSeverity::Actionable,
            Self::InvalidProxy => WarningSeverity::Actionable,
            Self::InvalidDestination => WarningSeverity::Actionable,
            Self::UnsupportedPacProtocol => WarningSeverity::Actionable,
            Self::MalformedPacCandidate => WarningSeverity::Actionable,
            Self::PacEvaluatorUnavailable => WarningSeverity::Actionable,
            Self::IncompleteSystemProxy => WarningSeverity::Actionable,
            Self::LinuxPacUnavailable => WarningSeverity::Actionable,
            Self::PacFailed => WarningSeverity::Actionable,
            Self::UnsupportedSystemProtocol => WarningSeverity::Actionable,
            Self::MacStaticFallback => WarningSeverity::Informational,
            Self::MacSettingsUnavailable => WarningSeverity::Actionable,
            Self::WpadFailed => WarningSeverity::Informational,
            Self::KeychainCredentialsUnread => WarningSeverity::Informational,
            Self::WindowsServiceSettings => WarningSeverity::Informational,
        }
    }
    pub fn message(self) -> &'static str {
        match self {
            Self::ChecksPending => "A network check is still running for earlier inputs. Try again shortly.",
            Self::ChecksTimedOut => "Some network checks timed out. Try again after connecting to the network.",
            Self::InvalidProxy => "A configured proxy is malformed or uses an unsupported protocol.",
            Self::InvalidDestination => "The HTTPS destination hostname is invalid.",
            Self::UnsupportedPacProtocol => "The PAC result includes an unsupported proxy protocol.",
            Self::MalformedPacCandidate => "The PAC result includes a malformed candidate.",
            Self::PacEvaluatorUnavailable => "Automatic proxy settings require the macOS PAC evaluator.",
            Self::IncompleteSystemProxy => "The system HTTPS proxy is incomplete.",
            Self::LinuxPacUnavailable => "A PAC file or automatic proxy discovery is configured. Linux has no system PAC evaluator. Set an explicit HTTP proxy or direct connection in Latch.",
            Self::PacFailed => "Automatic proxy evaluation failed or timed out. Check this network's proxy settings.",
            Self::UnsupportedSystemProtocol => "The system selected an unsupported proxy protocol.",
            Self::MacStaticFallback => "CFNetwork settings were unavailable. Static system proxy settings were read with scutil.",
            Self::MacSettingsUnavailable => "macOS proxy settings are unavailable.",
            Self::WpadFailed => "Automatic proxy discovery failed or timed out. DHCP and DNS WPAD did not return a usable PAC result.",
            Self::KeychainCredentialsUnread => "Proxy credentials are stored in the system keychain, not read.",
            Self::WindowsServiceSettings => "Windows proxy discovery reads the calling account's Internet settings. A background service may see different settings from the signed-in desktop. Configure an explicit proxy for that service when needed.",
        }
    }
}
