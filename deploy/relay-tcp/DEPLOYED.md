# Dedicated Seoul relay deployment

Production was upgraded on 2026-11-15 at 09:03:28 UTC, then reapplied at 09:10:02 UTC to the 6.0.0
native TLS/HTTPS WebSocket relay from main `7684946`, with the diagnostic
forced-carrier option included in this PR. The executable SHA-256 is
`8382f4f604af565ae71744fd19c696b5deac0012227c0ff0243a399e2094a9f9`.
It listens on `[::]:443` with IPv4-mapped acceptance. The existing
`small_3_0` instance remains the single $12/month production relay.
[WebSocket deployment](#websocket-upgrade-on-2026-11-15)

Deployment began on 2026-11-14 UTC. The dedicated endpoint is
`relay.latch.rune.foo:443`, DNS-only A record `54.116.93.176`.
The parent-provisioned Lightsail instance is `latch-relay-seoul-1`, bundle
`small_3_0`, Ubuntu 24.04, `ap-northeast-2a`. The attached static IP is
`latch-relay-seoul-1-ip`. Its ownership tags are `Project=Latch`,
`Purpose=native-tcp-relay` and `Release=6.0.0`.

The initial service/process/socket inventory contained OS management services,
SSH and local DNS/time services. That inventory established its dedicated
purpose before installing the relay. SSH uses the parent's pinned configuration
at `/Users/daniel/.ssh/latch6-lightsail.conf`, alias `latch6-relay`. Public TCP
22 is restricted to the operator /32. TCP 80 supports standalone
HTTP-01 renewal. TCP 443 serves native TLS ALPN `latch-relay/1`.

## Identity and operational configuration

Let's Encrypt issued a public certificate with CN `relay.latch.rune.foo`,
issuer `YE2`, valid 2026-11-14 09:03:49 through 2027-02-12 09:03:48 UTC.
Its SHA-256 fingerprint is
`04:AD:A3:52:1F:44:82:56:D4:84:C5:F3:38:3D:8B:C1:04:5E:91:05:E2:E8:EF:F4:69:26:F7:0D:35:90:0B:1C`.
Certbot used the authorized registration-without-email option. Its renewal
timer is enabled. The standalone renewal dry run with `--run-deploy-hooks`
succeeded, including a real execution of the installed deployment hook.

The hook at `/etc/letsencrypt/renewal-hooks/deploy/50-latch-relay` is root-owned
mode `0750`. It validates hostname, expiry and key agreement, locks deployment,
stops the service while replacing the pair, restarts it and restores the prior
pair if deployment fails. The live full chain and private key are
`root:latch-relay` mode `0640` in a `0750` directory. A different certificate
lineage was ignored successfully during a scope check.

The enabled systemd service runs as non-login account `latch-relay`, binds IPv4
TCP 443, and retains its ledger in `/var/lib/latch-relay/quota`, owned by
`latch-relay:latch-relay`, mode `0600`. The unit grants only
`CAP_NET_BIND_SERVICE`, applies filesystem/device/kernel restrictions, and has
384 MiB memory pressure and 512 MiB hard limits. Failures restart after 30 s.

| Operator setting | Deployed value |
| --- | --- |
| Monthly combined encrypted TCP ingress and egress | 500,000,000,000 bytes |
| Concurrent sessions, global / creator IP | 4 / 2 |
| TCP connections, global / IP | 24 / 8 |
| Session lifetime | 3,600 seconds |
| Combined session transfer cap | 17,179,869,184 bytes, 16 GiB |
| Grant attach expiry | 60 seconds |
| Handshake / send-stall / idle deadline | 10 s / 750 ms / 30 s |
| Accept rate per minute, global / IP | 120 / 30 |

Finite session limits and any lane failure require the application to obtain a
fresh grant and fresh keys through authorized control. The benchmark rotates
fresh pairs before its negotiated limits. Windows host/viewer recovery belongs
to the parent's application release validation.

Configuration SHA-256 digests are recorded below.

| File | SHA-256 |
| --- | --- |
| `/etc/latch-relay/limits.json` | `1f038572819969d3a2e24952f93aec87b59b6800a069388b1ed208644d6f5c46` |
| `/etc/latch-relay/budget.conf` | `86a30ba76f6a72d72766b7fad71c63155a8c5a1a633bddb4f849015c88d1cc9b` |
| `/etc/systemd/system/latch-relay.service` | `b82954b66e7179df95042f9e57ee9b4869b2bca0bb71b401466334817aa47dab` |

## Linux validation and recovery

The initial deployment used the parent's already-built 5.3.0 source snapshot
and Rust `1.100.0 (0ef1bc386 2026-11-09)`. Initial binary SHA-256 was
`5505288daed4286546a74578980ad5a1cf96c89657ed5fe4135657c629fe71ba`.
The confirmed foundation update was then copied and rebuilt as 6.0.0, including
the 256-entry ACK metadata window, unchanged 750 ms deadline, RFC 5869 test and
temporary derived-key zeroization. That first 6.0.0 binary SHA-256 was
`9d3a309ac7d3e58bb188d244e0818c92c9cc0042896d0636c8a131fef918f1b5`.
The captured 33-file relay/proto/workspace source snapshot digest was
`e76088572617e4c49beea8f6dd4a1a8293a8afee4e0f720ec33b5355b415831a`.
Local and remote snapshots matched before the build. Paths are recorded in
`results/source-snapshot.json`. The benchmark example subsequently gained an
optional explicit warm-up for capacity diagnostics.

The 6.0.0 rebuild passed 14 library and 12 transport integration tests on Linux
x86-64, including the delayed/withheld ACK regression. Clippy and the real Linux
release binary deployment selftest also passed. The upgrade preserved the
ledger's exact SHA-256
`7d3ce9cab915e9be1d828c919a11af000b80b9441e50aecf3207c4045433ffbc`.
The installed binary passed systemd verification and public WebPKI/ALPN checks.

The snapshot's 13 library and 11 transport integration tests passed natively
on Linux x86-64. Clippy passed with `-D warnings` after installing the missing
Clippy component. Cargo emitted four existing workspace manifest metadata
warnings. The deployment kit's real binary selftest passed against the Linux
release binary, including private ledger permissions, locking, missing/corrupt
ledger rejection and exhaustion across restart. The installed systemd unit
passed `systemd-analyze verify` using its actual executable.

A deliberate SIGKILL recovered in 30.238 seconds. The service PID changed from
6284 to 6415 and `NRestarts` became 1. The ledger's SHA-256 remained exactly
`44389084d44e365b8a83a8be95c42b65f18354c05226bf3c8faef432a1218c55`,
with 4,259,840 bytes reserved.

A reboot changed boot ID from `9a8a3bcc-89a2-4556-b23e-79fe862d404f` to
`169083d8-cc73-42a9-ad3a-cafc406b7bb2`. The enabled service started automatically,
and its idle ledger retained the exact same SHA-256. After reboot, the kernel
reported BBR, default `fq`, and both public-interface `ens5` transmit queues
used `fq` beneath `mq`.

### Quota-admission and diagnostic deployment

The parent confirmed the completed field run and idle deployment window. On
2026-11-14 at 11:29:55 UTC, the verified inactive candidate was atomically
promoted and the service restarted. Current installed 6.0.0 SHA-256 is
`0b2b3f500cf5c9a42b49fd361dc443be309b9f92b9b7049c99e41781302c02a7`.
Its captured 59-file source digest is
`682ad11d20e4aa796e50a9ad60c53cfc6e7d419744b4049772f4f9b34e3a1f64`.
The later client documentation-only edit was outside this captured snapshot.

The fresh Linux build passed 16 library, 12 transport and one probe example
tests, strict Clippy and the deployment selftest. The strengthened selftest
verified admission rejection while the exhausted daemon stays alive, including
after restart. The former binary failed this regression as expected. A stale
Cargo cache attempt was detected from its old hash/test count and replaced by
a clean rebuild before promotion.

The previous executable has a root-owned atomic hard-link backup at
`/usr/local/lib/latch-relay/backups/latch-relay-9d3a309ac7d3e58bb188d244e0818c92c9cc0042896d0636c8a131fef918f1b5`.
The limits, budget, systemd unit, certificate chain and renewal-hook hashes
matched across promotion. Ledger device/inode `66305/532979`, owner/group
`999/988`, mode `0600`, length 32 and UTC month 24322 matched. Reserved bytes
advanced from 4,092,002,304 to 4,092,067,840 during the first TLS health check.
After the campus diagnostic it reached 4,092,133,376 bytes.

PID 3142 was replaced by PID 6541, active with zero automatic restarts.
Restart plus Linux public TLS verification took 0.222 seconds. A campus Mac
check verified the public WebPKI hostname/chain, TLS 1.3 and ALPN
`latch-relay/1`. A controlled two-byte incomplete record then received EOF
after 1.038 seconds, exercising the bounded diagnostic lifecycle without
creating a paired bulk stream. The configured send/record deadline is 750 ms.
At 11:30:28.009052 UTC the journal recorded
`phase=request role=none lane=none category=partial_record_timeout kind=TimedOut`.
A normal health-check disconnect also logged `peer_closed` with
`UnexpectedEof`, demonstrating both safe diagnostic paths on the live service.

Monthly exhaustion now rejects fresh admission while the daemon keeps running.
Persistence errors retain fail-closed behavior. Diagnostics emit fixed phase,
role, lane, category and I/O kind, with eight details plus one suppression line
per minute. Deployment artifacts and source manifest are under
`results/candidate-20261114T110808Z/`.

Hardware is Intel Xeon Platinum 8259CL, 2 vCPUs, with all required crypto flags.
The instance initially reported 1,905 MiB RAM, 1,491 MiB available and 54 GiB
free root storage. The kernel is `7.0.0-1014-aws`. Capacity under public paired
traffic is being measured separately from these idle observations.

## Public paired transport measurements

The repeatable example is `crates/relay/examples/paired_probe.rs`. It creates a
fresh grant and keys, verifies both Ready events, paces synthetic complete
frames, validates frame contents/order, measures enqueue-to-delivery age and
authenticated ACK age, and echoes priority datagrams/control once per second.
Its CLI limits modeled combined transfer with headroom to 50 GB. Both roles
run in the campus Mac process and connect independently to the public relay,
so a frame traverses two campus-to-Seoul WAN legs. Delivery is authenticated
whole-frame transport. Codec rendering and Windows endpoint integration need
the parent's application tests.

The initial Mac probe binary SHA-256 was
`8ca42e9c4d19b26f506c2cad8ff23814d4f07f79091166505c035dd4c2416634`.
At 20 Mbps/60 fps, the pre-fix baseline reached Ready and closed with `TimedOut`
after 0.786 s. It delivered four frames, observed 37 source drops, 301 ms
priority RTT, 639 ms p95 ACK age and 714 ms maximum frame age.

At 5 Mbps/60 fps, the 30-second baseline completed with 776 frames delivered,
1,024 source drops and 2.144 Mbps received payload, approximately 25.7 fps.
P95 frame age was 294 ms, maximum 528 ms, p95 ACK age 440 ms, maximum 681 ms,
and priority RTT p95 301 ms. All 30 priority and 30 control echoes returned.
This matches the original eight-frame ACK metadata ceiling on this path.
The confirmed foundation update increased that bounded window while retaining
the 750 ms deadline.

After the 6.0.0 rebuild, a cold 20-Mbps/60-fps attempt closed with `TimedOut`
after 0.788 s, delivering four frames with a 639 ms p95 ACK age. A cold
5-Mbps/60-fps run completed 60 seconds, delivering 3,588 frames with 12 source
drops. Its steady final window measured 4.992 Mbps, 156 ms p95 frame age,
309 ms p95 ACK age, and all 60 priority/control echoes returned.

A 30-second quarter-bitrate warm-up at 60 fps allowed the following
20-Mbps/60-fps stage to deliver 19.90–19.99 Mbps in full steady 30-second
windows, with 168–170 ms p95 frame age and 313–319 ms p95 ACK age. The run
then closed with `TimedOut` at 136.422 seconds. The last window reached
569 ms unsent queue age. The daemon retained PID 3142 and zero restarts.

A 5-Mbps/120-fps attempt delivered 6,332 frames before a terminal timeout at
53.088 seconds. Steady received payload was 4.999 Mbps, with 154 ms p95 frame
age and 299 ms p95 ACK age. There were 21 source drops and 18 accepted frames
still outstanding at closure. All 53 priority/control echoes had returned.

The Linux-local diagnostic against the same public endpoint completed
20 Mbps/60 fps for 30 seconds with 1,800 frames, zero source drops and
23 ms p95 ACK age. The WAN findings therefore require evaluating path behavior
and the fixed deadlines separately from local forwarding capacity. A planned
ten-minute WAN run used explicit fresh-process/fresh-grant retries. It was
interrupted at the parent's request to isolate actual Windows field testing.
Completed traces remain under `results/`. Sustained ten-minute single-session
capacity and the desired one-hour/20-Mbps run remain unverified.

### Windows field-window correlation

Synthetic traffic overlapped the parent's Windows field run. The daemon's
10:40:24.413895 UTC counter sample recorded eight connections and two sessions.
The 10:40:54.414075 sample recorded four connections and one session. Rejected
accepts were zero in both samples. These counts are below global admission
limits of 24 connections and four sessions.

The 10:40:31.409201 UTC resource sample showed 4,360 KiB process RSS,
1,183,744 bytes of cgroup memory, and 2.949% of one core averaged over the prior
30 seconds. Its durable reservation counter was 1,238,630,400 bytes against
500,000,000,000 bytes configured. At 10:40:54 the journal reported
1,245,446,144 reserved bytes. The fresh field session's reported 24 seconds
at roughly 3 Mbps also lies far below its one-hour/16-GiB session limits.

The 10:40 UTC Lightsail sample reported 4.628% average host CPU and 5.550%
maximum. Burst capacity grew from 1.259% at 10:35 to 1.536% at 10:40 and
1.811% at 10:45. These coarse readings establish CPU headroom. Sub-second
I/O or wire stalls require finer reason instrumentation.

Synthetic attempt 6 began 10:39:51.976744 UTC. It sent quarter-rate 2.5 Mbps
for 30 seconds, then targeted 10 Mbps until its own timeout around 10:40:28.878.
The next attempt began 10:40:38.885446 and closed in under a second. Therefore
the field window included shared network traffic and a synthetic fresh setup.
These records cannot establish the network contention's contribution to the
field failure. CPU, memory and configured quota/admission headroom were ample.

The installed daemon logs aggregate counters every 30 seconds. It discards
individual `step` errors when removing connections. Its wire deadline check
combines pending-write age, partial-record age and idle age. Consequently the
10:40:39/44/49 UTC closes have no retrospective server reason identifying which
750 ms condition fired. Parent client/wire reason-label work needs a surfaced
daemon error for exact server-side attribution.

Bulk synthetic traffic and its local retry runner were terminated at the
parent's request. A subsequent read-only socket check showed the field's two
campus and two Windows sockets, with daemon PID 3142 and zero restarts. The
passive capacity sampler continues gathering read-only counters. Synthetic bulk
testing awaits parent coordination. Exact journal/resource evidence is saved
in `results/field-1040-relay-journal.log`, `results/field-1040-summary.json` and
`results/field-1040-lightsail.json`.

### Parent-reported completed application field run

Before this diagnostic deployment, the parent completed a 600-second actual
20-Mbps application field run. A late client video ACK timeout occurred near
548 seconds, with 725 ms ACK age and 809 ms RTT. Automatic relay recovery
took 4.96 seconds, the application session stayed connected and bitrate
recovered. These are parent-reported application measurements against the
previous daemon. The parent owns adaptive ACK-deadline work and subsequent
client/software builds. Synthetic WAN bulk testing remains paused.

## Cost boundary

The provisioned bundle costs $12/month, billed hourly up to that ceiling,
before taxes and any transfer overage. Its 3,072 GB nominal allowance counts
both ingress and egress. Seoul outbound overage is $0.13 per provider-billed GB.
The attached static IP is included. The 500 GB operator ceiling leaves margin
for conservative remaining-month proration. Provider network overhead,
retransmissions, management traffic and anonymous pre-admission traffic also
affect billing. The byte ledger meters observed encrypted TCP payload.

The two vCPUs have a documented 20% baseline each. Launch-time build/test work
spent the initial CPU burst reserve. The 10:05 UTC API sample reported zero
remaining burst percentage/time, so the sustained workload needs direct credit
and CPU observation. Final operational capacity remains under measurement.

## Dual-role consolidation on 2026-11-15

AWS STS confirmed account `043189681495` before inventory or mutation. Both
roles now run on the original `small_3_0` instance with its attached static
IPv4 `54.116.93.176`. TCP 443 serves the native relay. UDP 443 serves the
Tailscale peer relay and UDP 41641 serves tailscaled transport/discovery.
TCP 80 remains available for HTTP-01 and SSH stays restricted to the operator /32.

The verified private backup preceded migration of the relay identity from
EC2. Lightsail runs kernel-mode tailscaled 1.104.7 with relay port 443 and
static endpoint `54.116.93.176:443`. DNS acceptance, route acceptance and
Tailscale SSH are disabled, with no advertised routes. The relay needs a
dedicated tag that may only be a peer-relay destination and has no access
into the tailnet.

The old EC2 instance and its security group were terminated/deleted on
2026-11-15 at approximately 06:15 UTC after backup verification. Lightsail
is the only relay.

The native binary remains SHA-256
`0b2b3f500cf5c9a42b49fd361dc443be309b9f92b9b7049c99e41781302c02a7`.
Public WebPKI, TLS and ALPN checks passed after tuning. Both services and
the offload unit are enabled for boot. Live module loading, effective settings, service ordering, sockets and durable
quota were verified without a reboot. **Boot persistence remains unverified.** A future maintenance window must
coordinate the shared Mac–Windows peer-relay dependency.
[Live verification evidence](results/live-verification-20261115/README.md)

### Persistent relay tuning

`network.sh` installs the following profile. Buffer numbers are bytes. This
intentionally replaces the previous policy of preserving kernel TCP buffer
defaults. The values are retained on first-principles grounds. The matched
measurements demonstrated no throughput or latency improvement.

| Setting | Before | Applied value and reason |
| --- | --- | --- |
| TCP congestion control / default qdisc | `bbr` / `fq` | Retain BBR and fq pacing, confirmed on both `ens5` TX queues |
| `net.core.rmem_max`, `wmem_max` | 4,194,304 each | 16,777,216 each for larger UDP/TCP windows |
| `net.ipv4.tcp_rmem` | `4096 131072 14868224` | `4096 131072 16777216`, autotuned ceiling |
| `net.ipv4.tcp_wmem` | `4096 16384 4194304` | `4096 65536 16777216`, moderate initial allocation and larger ceiling |
| `udp_rmem_min`, `udp_wmem_min` | Kernel defaults | 16,384 each |
| `tcp_mtu_probing` | 0 | 2, always probe so a PMTU black hole cannot stall a lane (1 during the measurements below) |
| `tcp_notsent_lowat` | 4,294,967,295 | 131,072, bound queued unsent data while preserving the congestion window |
| `tcp_slow_start_after_idle` | 1 | 0, preserve the window across brief video pauses under fq pacing |
| `nf_conntrack_max` | 1,048,576 | 131,072, reduce worst-case table memory on this 2-GB host |
| Daemon `LimitNOFILE` | Native 1,024 | 65,536 for each daemon, applied live with `prlimit` |
| Tailscaled memory high / max | Unlimited | 384 / 512 MiB, matching the native daemon's existing containment |
| `rx-udp-gro-forwarding` | Off | On, helps kernel-forwarded traffic, no demonstrated peer-relay benefit |
| `rx-gro-list` | Off | Off, compatible with forwarding GRO |

A 20-Mbps stream over a 150-ms path has about 375 KB of bandwidth-delay
product. The 16-MiB ceilings leave room for many-tens-of-Mbps paths and
autotuning. They allocate on demand. The 128-KiB unsent threshold represents
about 52 ms at 20 Mbps and keeps excess application data closer to its bounded
queue. Conntrack entries at the new ceiling occupy roughly 40 MiB before
hash/extension overhead. Occupancy was 123 at verification. The script checks
occupancy before lowering that ceiling. ENA bandwidth, PPS and conntrack
allowance-exceeded counters were zero.

Generic software GSO and GRO remain enabled. ENA reports hardware UDP
segmentation `off [fixed]`, so that NIC feature cannot be enabled here.
`latch-relay-offloads.service` discovers the default interface and applies
GRO forwarding after network setup and before tailscaled at boot. Tailscale
recommends this for kernel-forwarded traffic on subnet routers and exit nodes.
This node advertises no routes. The setting is harmless here, with no evidence
of benefit for its userspace peer relay.

Conntrack loads through `/etc/modules-load.d/latch-relay.conf` before boot
sysctls. The script also loads it before writing configuration, so a missing
module fails before any profile files are changed. Settings persist in
`/etc/sysctl.d/90-latch-relay.conf`, daemon `relay.conf`
drop-ins and `/etc/systemd/system/latch-relay-offloads.service`. The boot
helper is `/usr/local/sbin/latch-relay-network`. `tune-host.sh` reapplies this
profile and masks ModemManager, multipathd and its socket, udisks2, fwupd and
its refresh service/timer. The one-disk NVMe inventory had no multipath devices.
SSH, chrony, Certbot, unattended security updates, logs and SSM stay available.

The Lightsail Xeon 8259CL exposes AES, PCLMULQDQ, BMI1, ADX, AVX and AVX2,
satisfying the native crypto library's requirements. The host has 1,905 MiB
RAM and more than 1.4 GiB available after the tests. Measured native cgroup
memory stayed below 7 MiB, with tailscaled around 19 MiB in the matched test.
Swap remains absent. Each daemon's 512-MiB hard ceiling bounds process growth,
leaving approximately 881 MiB for the OS at their combined ceiling. An OOM
failure ends the affected sessions and systemd restarts that service. A
tailscaled OOM also drops Tailscale management reachability. Short
tests leave multi-stream peak memory and long-duration OOM behavior unverified.

### Bounded before and after measurements

All native probe runs had both roles on one Mac on campus Wi-Fi, giving two
campus–Seoul WAN legs per frame. Matched probes used 25 seconds, 60 fps and
five seconds of quarter-rate warm-up at each target rate. All-run payload
rates therefore include the warm-up.

| Native target Mbps | Before payload Mbps / frame p95 ms | After payload Mbps / frame p95 ms | Outcome |
| --- | --- | --- | --- |
| 5 | 4.226 / 164.9 | 4.226 / 167.3 | Completed both |
| 20 | 16.866 / 248.3 | 16.865 / 169.5 | Completed both |
| 50 | 31.427 / 272.6 | 23.226 / 1,045.5 | Timed out both |

Tuning showed no measurable throughput benefit at 5 or 20 Mbps. The single
50-Mbps after sample was worse, 23.226 Mbps versus 31.427 Mbps before, with
p95 1,045.5 ms versus 272.6 ms. No throughput or latency improvement was
demonstrated. The verified target range from this campus Wi-Fi client is at
most 20 Mbps per paired session. **50 Mbps is not a verified capacity.**

The high-rate after row uses a retry after an initial Ready failure. Its
timeout occurred after 11.194 seconds. The corresponding before attempt
ran 18.402 seconds. Native service CPU around the 20-Mbps target was
3.54% before and 3.57% after, expressed as one-core utilization. That is
0.210 and 0.212 CPU percentage points per forwarded Mbps. The sample includes
setup and idle-tail CPU. A single before/after sample cannot establish causality.

Mac and Windows both reported `peer-relay 54.116.93.176:443:vni:3`. Recorded
Mac pings measured 147–150 ms and Windows pings measured 136–144 ms. Matched
SFTP downloads of the existing 13,172,176-byte Windows kernel measured
2.792 Mbps over EC2 and 3.101 Mbps over Lightsail, taking 37.740 and 33.987
seconds. Their SHA-256 values matched. Relay process CPU was 1.71% and 2.47%
of one core, equivalent to 0.613 and 0.797 CPU percentage points per Mbps.
The CPUs and WAN conditions also differ between those samples.

A separate RAM-only Mac-to-Windows TCP sink received 8,650,752 bytes in
20.015 seconds, 3.458 Mbps, with about 2.98% of one relay core. Windows used
its existing PowerShell/.NET tools. The initial stop-and-wait echo attempt
stalled and exceeded the intended short window before cancellation. A retry
timed out. Those attempts gave way to the bounded SFTP and upload tests.

The conservative allocation for all tests is below 3 GB combined transfer.
Available provider metrics advanced from 5.31 to 5.97 GB for the month.
The provider may report later samples after aggregation. The native guard
remains 500,000,000,000 combined bytes. Tailscale traffic shares the provider
allowance. Raw probe output, CPU samples, method, failures and path evidence
are retained with the results. [Measurement record](results/consolidation-20261115/README.md)

### Sustainable load and cost

The small bundle's two vCPUs have a 20% baseline each, equivalent to 0.4
sustained vCPU total. Burst reserve reached about 64.5%, or 11,139 seconds
in the final hourly average. Tests used seconds of CPU, leaving endurance
after credit depletion unverified. CPU credit depletion lowers sustainable
compute to the baseline. It creates no Lightsail CPU-credit surcharge.

Retain the $12 small bundle for its 2 GiB RAM, 0.4-vCPU baseline and 3,072-GB
combined allowance. Micro saves $5 but halves RAM, sustained CPU and some
transfer headroom. Medium costs $24 while retaining the same CPU baseline.
Assuming 60 hours/month at 20 Mbps and 10% overhead gives 1,188 GB combined
and approximately $12/month. The 500-GB native guard permits about 25.3 hours
at that rate. The full provider allowance supports about 155 hours under
the same model. The FREE account plan expires on 2026-12-21. Separately, the
Free Tier API reported $162 remaining credits on 2026-11-15. The instance
costs $12/month. Detailed bundle, overage and credit figures are in the
operating guide. [Cost model](LIGHTSAIL.md#bundle-and-transfer-economics)

IPv6 is currently disabled at Lightsail and DNS has no AAAA. Cloudflare
hosts the zone and its A record. Dual-stack enablement, IPv6 firewall rules, the later native listener
deployment and a DNS-only AAAA are the remaining steps. The assigned IPv6
address would be separate from the attached IPv4 static allocation.
[IPv6 findings](LIGHTSAIL.md#ipv6-and-dns)

The old EC2 instance and security group were removed after backup verification
on 2026-11-15 at approximately 06:15 UTC. The recurring relay instance cost is
Lightsail `small_3_0` only. [Completed teardown](LIGHTSAIL.md#completed-teardown)

## WebSocket upgrade on 2026-11-15

### Build and maintenance

The host is Ubuntu 24.04 x86-64 with glibc 2.39. The replacement was built on
the Mac in an Ubuntu 24.04 x86-64 Docker container using Rust
`1.100.0 (0ef1bc386 2026-11-09)`, `cargo build --locked --release -j 4 -p latch-relay`.
Its highest referenced GLIBC symbol version is 2.34. Docker's Rosetta CPU lacked
ADX, and Ubuntu's QEMU 8.2 crashed during startup. QEMU 10.2.3 with `-cpu max`
and the matching Ubuntu x86-64 loader/libraries ran the unmodified executable's
complete `selftest.py --binary` successfully. This covered dual-stack sockets,
TLS/ALPN, HTTP, WebSocket, limits, exclusive ledger locking, retained reservations,
exhaustion across restart and corruption rejection. Mac relay tests and strict
Clippy passed, including forced-carrier cache and fallback regressions.

The baseline service was active with PID 6541 and zero automatic restarts.
Its executable SHA-256 was
`0b2b3f500cf5c9a42b49fd361dc443be309b9f92b9b7049c99e41781302c02a7`.
TCP 443 listened on IPv4. UDP 443 and UDP 41641 belonged to the existing
Tailscale process. Initial relay metrics showed 703,599,461 network bytes,
4,894,883,840 reserved bytes, 1,560 accepts, 110 rejects and zero active
connections/sessions. Every managed sysctl matched the persistent tuning table
except MTU probing, which was still `1`.

The installer ran during a single brief `latch-relay` stop, using the current
certificate lineage and the existing 500,000,000,000-byte budget. It preserved
the exact stopped ledger and local limits. The stopped ledger contained
5,101,453,312 reserved bytes and had SHA-256
`478b5525fcd497e8b0265a8bae673168019538b50e91190fcfdd1090bcd6b42a`.
The only live tuning change was `net.ipv4.tcp_mtu_probing=2`, with the same
value written to `/etc/sysctl.d/90-latch-relay.conf`. Existing offloads and daemon
drop-ins were retained. The measured stop/install/start interval through a
successful local native WebPKI/ALPN handshake was **0.877 seconds**. A Mac
paired native probe passed immediately afterward.

Tailscaled retained PID 10234, start time 2026-11-15 04:24:38 UTC and zero
restarts. Network management retained PID 482 and its prior start time.
The Mac still reported the Windows peer through `peer-relay 54.116.93.176:443`.
The host stayed online throughout the maintenance.

### Bounded public transport measurements

Both roles ran on the UCLA campus Mac over Wi-Fi `en0`. macOS withheld the
SSID. The existing public IPv4 SSH rule matched the campus connection, and
public IPv6 HTTPS worked from the Mac. Each run lasted 40 seconds at 60 fps,
including a 10-second quarter-rate warm-up. Throughput and frame-delay
percentiles use the reporting windows identified in the table. Completed
runs use the final approximately 10-second full-rate window. Failed runs can
include warm-up or end before reaching full rate.
CPU is the service CPU delta over the whole run, including setup and warm-up,
as a percentage of one core. Frame delay measures enqueue to complete-frame
receipt across both Mac–Seoul WAN legs. These short measurements include
normal path variability.

| Run / target Mbps | Stream seconds | Payload Mbps | Frame delay p50 / p95 ms | CPU, one core | Result / reporting window |
| --- | --- | --- | --- | --- | --- |
| before-native-5 | 40.14 | 5.007 | 151.3 / 167.6 | 2.21% | complete, final 10 s |
| before-native-20 | 40.14 | 20.027 | 158.7 / 308.7 | 3.45% | complete, final 10 s |
| after-native-5 | 40.13 | 5.008 | 152.6 / 158.8 | 2.22% | complete, final 10 s |
| after-native-20 | 33.07 | 17.570 | 314.0 / 500.2 | 3.10% | timeout, last 3.1 s |
| after-websocket-5 | 12.67 | 1.790 | 236.8 / 400.9 | 2.03% | timeout, mixed warm-up/full rate |
| after-websocket-20 | 5.04 | 3.406 | 745.6 / 1141.1 | 1.90% | timeout, warm-up only |
| rollback-native-20 | 3.02 | 3.643 | 264.9 / 716.5 | 2.26% | timeout, warm-up only |
| retry-native-20 | 15.40 | 9.483 | 162.3 / 401.9 | 2.72% | timeout, mixed warm-up/full rate |
| retry-websocket-5 | 21.34 | 3.167 | 190.0 / 367.5 | 2.05% | timeout, mixed warm-up/full rate |
| retry-websocket-20 | 5.25 | 4.426 | 156.5 / 417.9 | 2.46% | timeout, warm-up only |

The old build supported only native TLS. Every post-upgrade measurement with
observed sockets used IPv4, matching the initial IPv4-only deployment.
The first 5-Mbps upgrade run completed. The first native 20-Mbps timeout
triggered restoration of the prior binary/unit at 09:08:57 UTC, retaining the
live quota, and withdrawal of the upgrade's AAAA record. That prior binary also
timed out, after 3.02 seconds during its quarter-rate warm-up. The replacement
was reapplied after reproducing the failure on the prior version. Subsequent
native/WebSocket 1-Mbps paired health checks completed after the certificate
renewal hook finished. The bounded capacity retries above still timed out.
These runs establish carrier functionality and document the observed capacity
limit on this changing campus path. Sustained 20-Mbps operation remains
unverified. The transport deadlines and tests retain their existing settings.

The second installation preserved 5,286,395,904 reserved bytes exactly, ledger
SHA-256 `f283efec2c0ca3b0ce5b4db0a40f6b19178cd88b883abb44c364badbe2982624`.
Its stop/install/start/local-ALPN verification interval was **0.937 seconds**.
The intervening rollback stop-to-listen interval was **0.333 seconds** from the
journal. The requested Certbot dry run invoked its deployment hook at
09:10:07 UTC, with a further **0.055-second** stop-to-listen interval.
The four recorded interruption intervals total approximately **2.20 seconds**.
A health attempt overlapping that renewal restart ended with EOF, followed by
successful native and WebSocket checks after renewal.


### Live verification

- Native ALPN paired health passed immediately after the first upgrade and
  again after the final installation/renewal. Forced grant and all four lanes
  reported `Native`, with complete frames and priority/control echoes.
- Forced WebSocket paired health passed, with the grant and all four lanes
  reporting `WebSocket`, complete frames and priority/control echoes.
- A loopback-only HTTP CONNECT proxy carried both forced carriers to the live
  relay, five tunnels each. Both six-second 1-Mbps probes passed. The proxy
  shut down after ten verified tunnels.
- Plain `curl https://relay.latch.rune.foo/` returned HTTP 200 and `OK`.
  `GET /relay` without upgrade returned HTTP 426 and WebSocket version 13.
- `selftest.py --live relay.latch.rune.foo` passed public hostname, certificate
  chain and native ALPN validation.
- `certbot renew --dry-run --run-deploy-hooks --cert-name relay.latch.rune.foo`
  succeeded, including the deployed certificate hook and retained quota.
- The production listener accepted both IP families. Public IPv6 certificate,
  native ALPN and HTTP checks passed before each AAAA publication.
- The existing Tailscale UDP listeners and peer-relay path were retained.
  Its process/start time and network manager process/start time stayed equal.

The optional full x86-64 library suite under QEMU passed 40 tests and hit an
emulation limitation in the self-spawning CPU-rejection test. The complete
Mac suite and native CI provide coverage for that subprocess behavior. The
required matching-userspace Linux executable selftest passed under QEMU 10.2.3.

### IPv6 and DNS

AWS STS matched account `043189681495` before AWS changes. Lightsail dual-stack
assigned `2406:da12:417:7f00:7baf:3615:172a:fe6f`. The running host acquired
its address and default route automatically within several minutes. Existing
network management continued running. Its live IPv6 lease renewed during
verification. The TCP 80/443 and UDP 443/41641 rules gained public IPv6 scope.
SSH gained only the current operator IPv6 `/128`. Existing IPv4 firewall rules
were verified equal before and after the changes.

Direct public IPv6 from the Mac verified TCP 443, the public certificate chain
and hostname, native `latch-relay/1` ALPN and HTTP 200. Only after those checks,
the existing Cloudflare DNS-edit credential created the DNS-only AAAA for the
assigned address, TTL 300. The existing DNS-only A record remained exactly
`54.116.93.176`. Authoritative Cloudflare DNS and a public recursive resolver
returned the AAAA. Boot persistence awaits a separately coordinated maintenance
window. Tailscaled retains its existing IPv4 peer-relay static endpoint.

### Backups and rollback

The dated root-only host backup is
`/var/backups/latch-relay/pre-wss-20261115T085100Z/`. It contains the previous
executable, unit/drop-ins, limits, budget, TLS identity, sysctl file, network
helper and quota snapshots. The Mac backup is
`/Volumes/Backup/latch-relay-seoul-1-pre-wss-2026-11-15/`, containing
`pre-wss.tar.gz`, the exact `quota.stopped` and `quota.redeploy-stopped` captured before each replacement,
and `EXCLUSIONS.txt`. Certificate private keys, ACME account state, SSH keys and
Tailscale state were excluded from the Mac copy. The archive and ELF executable
were read back successfully.

The rollback script passed shell syntax, backup hash, executable, retained
limits/budget and live-ledger prerequisite checks before the upgrade. Run it
through the existing pinned SSH alias if rollback is needed.

```sh
ssh -F "$HOME/.ssh/latch6-lightsail.conf" latch6-relay \
  'sudo bash /var/backups/latch-relay/pre-wss-20261115T085100Z/rollback.sh'
```

It briefly stops `latch-relay`, restores the previous executable/unit/configuration
and MTU probing value, then starts the service with the current live quota.
It retains `[::]:443` for the published AAAA. The prior executable passed a
disposable-ledger IPv4/IPv6 native TLS test before updating the rollback script.
Retain all reservations accumulated after the upgrade when rolling back.

### Future port 80 probe

The host currently leaves TCP 80 to Certbot standalone HTTP-01. For the later
anonymous `GET /netenv/probe` returning an empty 204, give the relay a plain
HTTP port-80 listener and serve `/.well-known/acme-challenge/` from a restricted
ACME webroot directory. Switch Certbot to the webroot authenticator and retain
the validated certificate deployment hook. One listener can then answer both
paths continuously through renewal. This follow-up requires implementation and
its own renewal verification.
