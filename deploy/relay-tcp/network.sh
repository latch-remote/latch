#!/usr/bin/env bash
# Apply on the dedicated dual-role relay. Does not restart either daemon.
set -euo pipefail
[[ $(id -u) == 0 && $(uname -s) == Linux ]] || {
    echo 'Run as root on the dedicated Linux relay.' >&2
    exit 1
}
apply_offloads() {
    interface=$(ip -o route show default | awk '{print $5; exit}')
    [[ -n $interface ]] || { echo 'No default interface.' >&2; exit 1; }
    # Helps kernel-forwarded traffic. No measured peer-relay benefit on this node.
    ethtool -K "$interface" rx-udp-gro-forwarding on rx-gro-list off
}
if [[ ${1:-} == --offloads-only ]]; then
    apply_offloads
    exit 0
fi
[[ $# == 0 ]] || { echo 'usage: network.sh [--offloads-only]' >&2; exit 1; }
command -v ethtool >/dev/null
# Fail before writing configuration if conntrack is unavailable.
modprobe nf_conntrack
modprobe tcp_bbr 2>/dev/null || true
modprobe sch_fq 2>/dev/null || true
if ! sysctl -n net.ipv4.tcp_available_congestion_control | tr ' ' '\n' | grep -qx bbr; then
    echo 'BBR is unavailable on this kernel.' >&2
    exit 1
fi
# Refuse to lower the conntrack ceiling below current occupancy.
if [[ -r /proc/sys/net/netfilter/nf_conntrack_count ]]; then
    [[ $(cat /proc/sys/net/netfilter/nf_conntrack_count) -lt 131072 ]] || {
        echo 'Conntrack occupancy exceeds this dedicated-relay profile.' >&2
        exit 1
    }
fi
install -d -m 0755 /etc/sysctl.d /etc/modules-load.d
printf 'nf_conntrack\n' > /etc/modules-load.d/latch-relay.conf
chmod 0644 /etc/modules-load.d/latch-relay.conf
cat > /etc/sysctl.d/90-latch-relay.conf <<'SETTINGS'
net.core.default_qdisc = fq
net.ipv4.tcp_congestion_control = bbr
# Ceilings are per socket. TCP autotuning retains modest initial allocations.
net.core.rmem_max = 16777216
net.core.wmem_max = 16777216
net.ipv4.tcp_rmem = 4096 131072 16777216
net.ipv4.tcp_wmem = 4096 65536 16777216
net.ipv4.udp_rmem_min = 16384
net.ipv4.udp_wmem_min = 16384
# Always probe packet sizes: a path that drops large segments without ICMP
# would otherwise stall a lane past its send deadline before detection.
net.ipv4.tcp_mtu_probing = 2
net.ipv4.tcp_notsent_lowat = 131072
net.ipv4.tcp_slow_start_after_idle = 0
# About 40 MiB of entries at saturation, instead of a million-entry ceiling.
net.netfilter.nf_conntrack_max = 131072
SETTINGS
chmod 0644 /etc/sysctl.d/90-latch-relay.conf

# Run after link setup at every boot. The default interface is discovered then.
if [[ $(readlink -f "$0") != /usr/local/sbin/latch-relay-network ]]; then
    install -m 0755 "$(readlink -f "$0")" /usr/local/sbin/latch-relay-network
fi
cat > /etc/systemd/system/latch-relay-offloads.service <<'UNIT'
[Unit]
Description=UDP GRO forwarding offloads for the Latch peer relay
Wants=network-online.target
After=network-online.target
Before=tailscaled.service

[Service]
Type=oneshot
ExecStart=/usr/local/sbin/latch-relay-network --offloads-only
RemainAfterExit=yes

[Install]
WantedBy=multi-user.target
UNIT
install -d -m 0755 /etc/systemd/system/tailscaled.service.d
cat > /etc/systemd/system/tailscaled.service.d/relay.conf <<'UNIT'
[Unit]
Wants=latch-relay-offloads.service
After=latch-relay-offloads.service

[Service]
LimitNOFILE=65536
MemoryHigh=384M
MemoryMax=512M
UNIT
install -d -m 0755 /etc/systemd/system/latch-relay.service.d
cat > /etc/systemd/system/latch-relay.service.d/relay.conf <<'UNIT'
[Service]
LimitNOFILE=65536
UNIT
sysctl -p /etc/sysctl.d/90-latch-relay.conf
systemctl daemon-reload
systemctl enable latch-relay-offloads.service
systemctl restart latch-relay-offloads.service
# Apply limits to running processes without interrupting production sessions.
for service in latch-relay tailscaled; do
    pid=$(systemctl show "$service.service" -p MainPID --value 2>/dev/null || true)
    if [[ ${pid:-0} != 0 ]]; then
        prlimit --pid "$pid" --nofile=65536:65536
    fi
done
if systemctl is-active --quiet tailscaled.service; then
    systemctl set-property --runtime tailscaled.service MemoryHigh=384M MemoryMax=512M
fi
tc qdisc show
echo 'Confirm fq on every public-interface TX queue. The default applies at link creation.'
