use crate::tailscale::{self, NetCheck, Status};
use serde::Deserialize;
use std::time::{Duration, Instant};

/// Stable identifiers for the wiring layer. Match these values or `code()`,
/// never the human-readable message. Unknown evidence stays unknown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TailscaleReason {
    Unknown,
    NotInstalled,
    DaemonNotRunning,
    LoggedOut,
    NeedsApproval,
    OtherAccount,
    Stopped,
    ControlUnreachable,
    DerpUnreachable,
    Connected,
    PeerOffline,
    PeerUnknown,
    PathUnknown,
    Direct,
    PeerRelay,
    DerpOnly,
}
impl TailscaleReason {
    pub fn code(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::NotInstalled => "not_installed",
            Self::DaemonNotRunning => "daemon_not_running",
            Self::LoggedOut => "logged_out",
            Self::NeedsApproval => "needs_approval",
            Self::OtherAccount => "other_account",
            Self::Stopped => "stopped",
            Self::ControlUnreachable => "control_unreachable",
            Self::DerpUnreachable => "derp_unreachable",
            Self::Connected => "connected",
            Self::PeerOffline => "peer_offline",
            Self::PeerUnknown => "peer_unknown",
            Self::PathUnknown => "path_unknown",
            Self::Direct => "direct",
            Self::PeerRelay => "peer_relay",
            Self::DerpOnly => "derp_only",
        }
    }
}
#[derive(Debug, Clone, PartialEq)]
pub struct TailscaleDiagnosis {
    pub reason: TailscaleReason,
    pub message: String,
    /// Path's DERP region, or local preferred region when no peer is selected.
    pub derp_region: Option<String>,
    pub peer_relay: Option<String>,
    /// Independent local UDP/IPv4/IPv6 evidence, never inferred from a path.
    pub netcheck: Option<NetCheck>,
}
impl TailscaleDiagnosis {
    fn new(reason: TailscaleReason, message: &str) -> Self {
        Self {
            reason,
            message: message.into(),
            derp_region: None,
            peer_relay: None,
            netcheck: None,
        }
    }
    pub(super) fn unknown() -> Self {
        Self::new(
            TailscaleReason::Unknown,
            "Tailscale could not be checked. Try again shortly.",
        )
    }
}

#[derive(Default, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
struct Health {
    health: Option<Vec<String>>,
}

/// Parse through core's existing status/netcheck representations. Health
/// details stay local so no control API or existing callers change meaning.
pub(super) fn diagnose(
    status_json: &str,
    netcheck_json: Option<&str>,
    peer: Option<&str>,
) -> TailscaleDiagnosis {
    let Ok(status) = tailscale::parse_status(status_json) else {
        return TailscaleDiagnosis::unknown();
    };
    let health = serde_json::from_str::<Health>(status_json).unwrap_or_default();
    let netcheck = netcheck_json.and_then(|json| tailscale::parse_netcheck(json).ok());
    let mut result = classify(&status, health.health.as_deref().unwrap_or_default(), peer);
    if result.derp_region.is_none() {
        let region = if !status.self_node.relay.is_empty() {
            status.self_node.relay.as_str()
        } else {
            netcheck
                .as_ref()
                .map(|n| tailscale::derp_code(n.preferred_derp))
                .unwrap_or("")
        };
        result.derp_region = (!region.is_empty()).then(|| region.into());
    }
    result.netcheck = netcheck;
    result
}

fn classify(status: &Status, health: &[String], peer: Option<&str>) -> TailscaleDiagnosis {
    use TailscaleReason as R;
    let (reason, words) = match status.backend_state.as_str() {
        "NeedsLogin" => (R::LoggedOut, "Sign in to Tailscale on this machine."),
        "NeedsMachineAuth" => (R::NeedsApproval, "Ask your Tailscale administrator to approve this machine."),
        "InUseOtherUser" => (R::OtherAccount, "Tailscale is in use by another account on this computer. Open Tailscale in that account."),
        "Stopped" => (R::Stopped, "Tailscale is paused. Open Tailscale and connect."),
        "NoState" | "" => (R::DaemonNotRunning, "Start Tailscale on this machine."),
        "Running" | "Starting" => (R::Connected, "Tailscale is connected."),
        _ => return TailscaleDiagnosis::unknown(),
    };
    if reason != R::Connected {
        return TailscaleDiagnosis::new(reason, words);
    }
    // A missing UDP response or absent CurAddr alone says nothing about
    // control/DERP reachability. Only explicit daemon health evidence counts.
    for issue in health {
        let issue = issue.to_ascii_lowercase();
        if (issue.contains("coordination server") || issue.contains("control server"))
            && (issue.contains("unable to connect")
                || issue.contains("could not connect")
                || issue.contains("can't reach")
                || issue.contains("cannot reach")
                || issue.contains("not connected"))
        {
            return TailscaleDiagnosis::new(R::ControlUnreachable, "Tailscale cannot reach its sign-in service. Sign in to this network or try another connection.");
        }
        if (issue.contains("derp") || issue.contains("relay server"))
            && (issue.contains("could not connect to any")
                || issue.contains("unable to connect to any")
                || issue.contains("cannot connect to any"))
        {
            return TailscaleDiagnosis::new(R::DerpUnreachable, "Tailscale cannot reach its relays. Sign in to this network or try another connection.");
        }
    }
    if status.backend_state == "Starting" {
        return TailscaleDiagnosis::unknown();
    }
    let Some(peer) = peer else {
        return TailscaleDiagnosis::new(reason, words);
    };
    let node = status.peer.get(peer).or_else(|| {
        status
            .peer
            .values()
            .find(|node| node.id == peer || node.tailscale_ips.iter().any(|ip| ip == peer))
    });
    let Some(node) = node else {
        return TailscaleDiagnosis::new(R::PeerUnknown, "This machine is absent from Tailscale's peer list. Check that both machines use the same account.");
    };
    if !node.online {
        return TailscaleDiagnosis::new(
            R::PeerOffline,
            "The other machine is offline. Wake it and open Tailscale there.",
        );
    }
    if !node.cur_addr.is_empty() {
        return TailscaleDiagnosis::new(
            R::Direct,
            "Tailscale has a direct connection to the other machine.",
        );
    }
    if !node.peer_relay.is_empty() {
        let mut result = TailscaleDiagnosis::new(
            R::PeerRelay,
            "Tailscale is using a relay on your network. This path still needs working UDP.",
        );
        result.peer_relay = Some(node.peer_relay.clone());
        return result;
    }
    if node.active {
        let message = if node.relay.is_empty() {
            "Tailscale is using a shared relay. A different network may allow a direct connection."
                .into()
        } else {
            format!("Tailscale is using its {} relay. A different network may allow a direct connection.", tailscale::derp_city(&node.relay))
        };
        let mut result = TailscaleDiagnosis::new(R::DerpOnly, &message);
        result.derp_region = (!node.relay.is_empty()).then(|| node.relay.clone());
        return result;
    }
    TailscaleDiagnosis::new(
        R::PathUnknown,
        "The other machine is online. Tailscale will choose a path when you connect.",
    )
}

pub(super) fn discover(peer: Option<&str>, deadline: Instant) -> TailscaleDiagnosis {
    let Some(cli) = tailscale::cli() else {
        return TailscaleDiagnosis::new(
            TailscaleReason::NotInstalled,
            "Install Tailscale and sign in on this machine.",
        );
    };
    let remaining = || deadline.saturating_duration_since(Instant::now());
    if remaining().is_zero() {
        return TailscaleDiagnosis::unknown();
    }
    let status = tailscale::run_limited(
        std::process::Command::new(&cli).args(["status", "--json"]),
        remaining().min(Duration::from_secs(2)),
    );
    let status = match status {
        Ok(status) => status,
        Err(error) if tailscale::daemon_not_running(&error.to_string()) => {
            return TailscaleDiagnosis::new(
                TailscaleReason::DaemonNotRunning,
                "Start Tailscale on this machine.",
            )
        }
        Err(_) => return TailscaleDiagnosis::unknown(),
    };
    if tailscale::parse_status(&status).is_ok_and(|s| !s.running()) || remaining().is_zero() {
        return diagnose(&status, None, peer);
    }
    let netcheck = tailscale::run_limited(
        std::process::Command::new(cli).args(["netcheck", "--format=json"]),
        remaining()
            .saturating_sub(Duration::from_millis(100))
            .max(Duration::from_millis(1)),
    );
    diagnose(&status, netcheck.as_deref().ok(), peer)
}
