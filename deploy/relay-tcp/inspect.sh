#!/usr/bin/env bash
# Read-only host inspection. Output excludes environment, command lines, unit
# definitions, logs, user data, authentication state and private key material.
set -euo pipefail
uname -srmo
lscpu | awk -F: '$1 ~ /^(Architecture|CPU\(s\)|Model name|Hypervisor vendor|Virtualization type)$/ { print }'
python3 - <<'PY'
from pathlib import Path
import json
text = Path('/proc/cpuinfo').read_text().lower()
flags = set()
for line in text.splitlines():
    if line.startswith(('flags\t', 'features\t')):
        flags.update(line.split(':', 1)[1].split())
print(json.dumps({'crypto_cpu_features': {name: name in flags for name in (
    'aes', 'pclmulqdq', 'bmi1', 'adx', 'avx', 'avx2', 'asimd', 'pmull', 'sha2')}}))
PY
free -m
df -hT / /var/lib
lsblk -o NAME,SIZE,TYPE,FSTYPE,MOUNTPOINTS
uptime
systemctl list-units --type=service --state=running --no-legend --no-pager |
    awk '{print $1, $3, $4}'
ps -eo pid,comm,pcpu,pmem,rss --sort=-pcpu | head -n 16 || true
# ss process metadata contains executable names and PIDs, without argv.
ss -H -lntup
ss -H -nt state established | awk '{local=$3; sub(/^.*:/,"",local); count[local]++} END {for (port in count) print "established_tcp_local_port",port,count[port]}'
sysctl net.core.default_qdisc net.ipv4.tcp_congestion_control net.ipv4.tcp_available_congestion_control
if command -v tc >/dev/null; then
    tc qdisc show
else
    echo 'tc unavailable. Effective interface qdiscs require the OS traffic-control tool.'
fi
ip -s link show
if command -v ethtool >/dev/null; then
    for interface in /sys/class/net/*; do
        ethtool -S "${interface##*/}" 2>/dev/null |
            awk '/bw_in_allowance_exceeded|bw_out_allowance_exceeded|pps_allowance_exceeded|conntrack_allowance_exceeded|linklocal_allowance_exceeded/ {print}' || true
    done
fi
if command -v tailscale >/dev/null; then
    tailscale version | head -n 1
    tailscale debug prefs 2>/dev/null | python3 -c '
import json,sys
d=json.load(sys.stdin)
print(json.dumps({"tailscale_relay_port":d.get("RelayServerPort"),"tailscale_wanted_running":d.get("WantRunning")}))' || true
    tailscale debug metrics 2>/dev/null | python3 -c '
import json,re,sys
totals={}
for line in sys.stdin:
    m=re.match(r"([a-zA-Z_:][a-zA-Z0-9_:]*)(?:\{[^}]*\})?\s+([0-9.eE+-]+)(?:\s|$)",line)
    if m and any(word in m[1].lower() for word in ("peerrelay","peer_relay","relayserver","relay_server")):
        totals[m[1]]=totals.get(m[1],0)+float(m[2])
print(json.dumps({"tailscale_relay_metric_totals":totals}))' || true
fi
# Sample CPU, memory and block IO for five seconds without generating traffic.
vmstat 1 6
