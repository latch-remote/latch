use std::net::{IpAddr, Ipv6Addr, ToSocketAddrs};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Nat64Prefix {
    pub address: Ipv6Addr,
    pub length: u8,
}
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IpAvailability {
    /// Operational non-loopback, non-link-local, non-Tailscale interfaces.
    /// Interface presence is not proof of internet reachability.
    pub ipv4: Option<bool>,
    pub ipv6: Option<bool>,
    /// Local interface family, including DNS64 evidence below.
    pub ipv6_only: Option<bool>,
    /// RFC 7050 evidence from the system resolver. Empty is not proof that
    /// translation is absent. VPN/split DNS can hide or supply DNS64.
    pub nat64_prefixes: Vec<Nat64Prefix>,
}

/// RFC 7050 + RFC 6052. Either well-known IPv4 constant can identify a
/// prefix. Ambiguous occurrences are discarded so the other constant can
/// resolve the prefix. Preserve the resolver's record order.
pub(super) fn nat64(addresses: &[Ipv6Addr]) -> Vec<Nat64Prefix> {
    let mut candidates: Vec<(Nat64Prefix, u8)> = Vec::new();
    let mut constants = 0;
    for address in addresses {
        let bytes = address.octets();
        if bytes[8] != 0 {
            continue;
        }
        let mut matches = Vec::new();
        for length in [32u8, 40, 48, 56, 64, 96] {
            let positions: Vec<_> = (usize::from(length) / 8..16)
                .filter(|i| length == 96 || *i != 8)
                .take(4)
                .collect();
            let embedded: Vec<_> = positions.iter().map(|i| bytes[*i]).collect();
            if embedded != [192, 0, 0, 170] && embedded != [192, 0, 0, 171] {
                continue;
            }
            matches.push((length, positions, embedded));
        }
        // Different RFC 6052 positions in one answer are ambiguous, even
        // when they contain different WKAs. Another answer must settle it.
        if matches.len() != 1 {
            continue;
        }
        let (length, positions, embedded) = matches.pop().unwrap();
        let contiguous = positions.windows(2).all(|pair| pair[1] == pair[0] + 1);
        let occurrences =
            bytes.windows(4).filter(|part| *part == embedded).count() + usize::from(!contiguous);
        if occurrences != 1 {
            continue;
        }
        let mut prefix = [0u8; 16];
        prefix[..usize::from(length) / 8].copy_from_slice(&bytes[..usize::from(length) / 8]);
        let candidate = Nat64Prefix {
            address: prefix.into(),
            length,
        };
        let constant = if embedded[3] == 170 { 1 } else { 2 };
        constants |= constant;
        if let Some((_, seen)) = candidates.iter_mut().find(|(old, _)| *old == candidate) {
            *seen |= constant;
        } else {
            candidates.push((candidate, constant));
        }
    }
    // If both WKAs yielded usable answers, their prefix candidates must agree.
    // A single usable WKA is sufficient under RFC 7050 section 3.
    candidates
        .into_iter()
        .filter(|(_, seen)| constants != 3 || *seen == 3)
        .map(|(prefix, _)| prefix)
        .collect()
}

/// CLAT's service-continuity range can appear as IPv4 on IPv6-only access.
pub(super) fn ipv6_only(ips: &[IpAddr], prefixes: &[Nat64Prefix]) -> bool {
    let native: Vec<_> = ips
        .iter()
        .copied()
        .filter(|ip| {
            prefixes.is_empty()
                || !matches!(ip, IpAddr::V4(ip) if u32::from(*ip) & 0xffff_fff8 == 0xc000_0000)
        })
        .collect();
    let (ipv4, ipv6) = families(&native);
    ipv6 && !ipv4
}

pub(super) fn families(ips: &[IpAddr]) -> (bool, bool) {
    let usable = |ip: &&IpAddr| !ip.is_loopback() && !ip.is_unspecified() && !ip.is_multicast();
    let ipv4 = ips
        .iter()
        .filter(usable)
        .any(|ip| matches!(ip, IpAddr::V4(ip) if !ip.is_link_local()));
    let ipv6 = ips
        .iter()
        .filter(usable)
        .any(|ip| matches!(ip, IpAddr::V6(ip) if !ip.is_unicast_link_local()));
    (ipv4, ipv6)
}

pub(super) fn discover() -> IpAvailability {
    let mut result = IpAvailability::default();
    let mut local_ips = None;
    if let Ok(interfaces) = if_addrs::get_if_addrs() {
        let tailscale_names: Vec<_> = interfaces.iter().filter(|interface| {
            interface.name.to_ascii_lowercase().contains("tailscale") || matches!(interface.ip(), IpAddr::V6(ip) if ip.octets()[..6] == [0xfd,0x7a,0x11,0x5c,0xa1,0xe0])
        }).map(|interface| interface.name.as_str()).collect();
        let ips: Vec<_> = interfaces
            .iter()
            .filter(|interface| {
                interface.is_oper_up() && !tailscale_names.contains(&interface.name.as_str())
            })
            .map(|interface| interface.ip())
            .collect();
        let (ipv4, ipv6) = families(&ips);
        result.ipv4 = Some(ipv4);
        result.ipv6 = Some(ipv6);
        local_ips = Some(ips);
    }
    // No custom DNS server, identifier, or application payload. RFC 7050
    // requires the reserved ipv4only.arpa name via the network's resolver.
    if let Ok(addresses) = ("ipv4only.arpa", 0).to_socket_addrs() {
        let addresses: Vec<_> = addresses
            .filter_map(|addr| match addr.ip() {
                IpAddr::V6(ip) => Some(ip),
                _ => None,
            })
            .collect();
        result.nat64_prefixes = nat64(&addresses);
    }
    if let Some(ips) = local_ips {
        result.ipv6_only = Some(ipv6_only(&ips, &result.nat64_prefixes));
    }
    result
}
