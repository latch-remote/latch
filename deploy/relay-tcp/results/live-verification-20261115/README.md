# Live profile verification, 2026-11-15 UTC

**The real reboot persistence check is still pending.** The operator deferred
it because long-lived coding-agent TCP connections use the Mac–Windows peer
relay path. The orchestrator will perform the single reboot check when the
new relay build is deployed and fewer agents are running.

The corrected `network.sh` and `tune-host.sh` were applied live without daemon
restarts. The installed `/usr/local/sbin/latch-relay-network` was then run
without arguments twice successfully. Unit verification exited 0 for
`latch-relay.service`, `tailscaled.service` and `latch-relay-offloads.service`.

- `nf_conntrack` is loaded now and listed in
  `/etc/modules-load.d/latch-relay.conf`.
- Every profile sysctl matches the intended value, including
  `net.netfilter.nf_conntrack_max = 131072`.
- `systemd-modules-load.service` is ordered before `systemd-sysctl.service`.
  The offload unit is ordered after networking and before tailscaled.
- Native TCP 443 and tailscaled UDP 443/41641 listeners are active.
  Both daemons retain 65,536-file limits and 384/512-MiB memory limits.
- Both public-interface TX queues use `fq`. Generic GSO/GRO are on,
  `rx-udp-gro-forwarding` is on and `rx-gro-list` is off.
- All seven documented unnecessary units remain masked.
- The binary SHA-256 remains
  `0b2b3f500cf5c9a42b49fd361dc443be309b9f92b9b7049c99e41781302c02a7`.
  The quota ledger remains 32 bytes, mode 0600, with 4,869,783,552 reserved
  bytes and unchanged SHA-256
  `563f80b0c23087f21a4d06b3ba98f9c66e6cc79fd9a810afca74015681a390ea`.
- Recent native counters show zero connections and sessions. The peer path
  continues carrying coding-agent traffic, which prevented the reboot check.

The deployment self-test passed on this Linux host against the installed
binary with a disposable loopback ledger. It also exercised the new stubbed
installer, missing-module failure, installed-helper re-run and simulated
modules-load/sysctl boot sequence. These checks establish live configuration
and boot dependencies. Reboot persistence remains unverified.

Redacted live values, counters and ordering are in [live.json](live.json).
The Mac peer-path check is summarized in [peer-path.txt](peer-path.txt).
