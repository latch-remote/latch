# Dedicated Seoul relay deployment

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
22 is restricted to the campus operator address. TCP 80 supports standalone
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
