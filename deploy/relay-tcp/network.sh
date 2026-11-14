#!/usr/bin/env bash
# Apply only on the dedicated native relay. Preserve kernel TCP buffer defaults.
set -euo pipefail
[[ $(id -u) == 0 && $(uname -s) == Linux ]] || {
    echo 'Run as root on the dedicated Linux relay.' >&2
    exit 1
}
modprobe tcp_bbr 2>/dev/null || true
modprobe sch_fq 2>/dev/null || true
if ! sysctl -n net.ipv4.tcp_available_congestion_control | tr ' ' '\n' | grep -qx bbr; then
    echo 'BBR is unavailable. Kernel network settings were retained.'
    exit 0
fi
# Writing default_qdisc validates that the kernel can load this qdisc.
old_qdisc=$(sysctl -n net.core.default_qdisc)
if ! sysctl -q -w net.core.default_qdisc=fq; then
    exit 1
fi
if ! sysctl -q -w net.ipv4.tcp_congestion_control=bbr; then
    sysctl -q -w "net.core.default_qdisc=$old_qdisc"
    exit 1
fi
install -d -m 0755 /etc/sysctl.d
printf '%s\n' 'net.core.default_qdisc = fq' 'net.ipv4.tcp_congestion_control = bbr' > /etc/sysctl.d/90-latch-relay.conf
chmod 0644 /etc/sysctl.d/90-latch-relay.conf
sysctl net.core.default_qdisc net.ipv4.tcp_congestion_control
echo 'Existing interface qdiscs take effect on interface recreation or reboot. Verify after reboot.'
