# TCP/TLS and HTTPS WebSocket relay

This kit installs the `latch-relay` crate on a dedicated Linux machine with
systemd. The public endpoint is `relay.latch.rune.foo:443` on the dedicated
`latch-relay-seoul-1` Lightsail instance. The parent release task owns final
application integration. The original EC2 backup relay carries a separate
active workload.
The completed read-only inspection and exact validation results are recorded
in [the implementation handoff](INSPECTION.md).
Current deployment and capacity results are recorded in [the deployment report](DEPLOYED.md).

## Build and test

Build on the target Linux architecture and a compatible glibc baseline. The relay
package builds with the Rust toolchain and Graviola's Rust implementation. Its runtime needs
systemd and the platform C runtime. TLS is implemented in the binary. The
installer also uses OpenSSL, util-linux, coreutils and the system account tools.

```sh
cargo build --locked --release -p latch-relay
cargo test --locked -p latch-relay
python3 deploy/relay-tcp/selftest.py --binary target/release/latch-relay
```

The binary selftest creates temporary loopback-only TLS identities and ledgers.
It checks the shipped limits JSON, TLS/ALPN, ledger permissions and exclusive
locking, dual-stack IPv4/IPv6 acceptance, ordinary HTTP and WebSocket upgrade,
missing/corrupt ledger rejection, preserved reservations on restart,
initialization refusing an existing ledger, and budget exhaustion across restart.
The crate tests exercise authenticated two-lane transport and whole frames.
`python3 deploy/relay-tcp/selftest.py` checks the kit without starting a daemon.
ShellCheck runs when available.

The actual Graviola CPU prerequisites are AES, PCLMULQDQ, BMI1, ADX, AVX and AVX2
on x86-64, and NEON, AES, PMULL and SHA2 on AArch64. Run the binary selftest on
the chosen instance before exposing it. A bundle's vCPU count cannot establish
crypto compatibility or sustainable throughput.

## TLS and network access

Create the DNS A record after reserving and attaching the instance's static IP.
The supplied service binds `[::]:443` with `IPV6_V6ONLY` disabled, accepting
IPv4 and IPv6 on one listener. Publish an AAAA record after enabling the
instance's public IPv6 route and its TCP 443 firewall rules. DNS64/NAT64 clients
can also use the public A record. Mapped IPv4 and well-known NAT64 /96 addresses
share the corresponding IPv4 admission key. Network-specific translation
normally reaches the daemon through a translator's IPv4 source. Native IPv6
and unrecognized translated IPv6 sources share a /64 admission key.

Use a public WebPKI certificate whose SAN includes `relay.latch.rune.foo`.
The deployed certificate uses Certbot standalone HTTP-01, with public TCP 80
reserved for ACME. The enabled `certbot.timer` handles renewal. This daemon
serves native TLS with ALPN `latch-relay/1` and HTTP/1.1 with `http/1.1` or
absent ALPN on the same port. `GET /relay` supports RFC 6455 WebSocket upgrade.
Root GET/HEAD requests receive 200, unknown paths 404, unsupported methods 405,
malformed requests 400, incomplete `/relay` upgrades 426 with WebSocket version
13, and excessive headers 431. Both
carriers share admission limits, persistent transfer quota and deadlines.
Cold clients try native TLS then WebSocket in a two-second connection ladder.
Cloned configurations cache the winning carrier/address for 30 seconds. Stable
warm attach opens use at most 1.25 s. With 200 ms RTT, cold grant plus concurrent
attach opens, grant/attach/key confirmation and a control hop total 4.05 s.
Fresh host lanes share fallback selection before upgrade completion, bounding
cold grant plus cold attach and those exchanges to 4.8 s at 200 ms RTT.
An explicit HTTP CONNECT proxy carries both rungs when supplied by the caller.
Production clients verify public WebPKI roots and report untrusted issuer,
portal, proxy and upgrade failures to the caller.

Keep certificate automation state root-only. `certbot-deploy.sh` validates the
new hostname, expiry and matching key before stopping the relay, copies the
renewed pair as `root:latch-relay` mode `0640`, and starts the service. Its
failure trap restores the prior identity. A lock serializes hook executions.
The `/etc/latch-relay` directory is `root:latch-relay` mode `0750`. The restart
ends current relay sessions, which need a fresh grant and fresh session keys
through the authenticated control channel.

Expose TCP 443 publicly for both enabled address families on the dedicated
instance. Restrict SSH TCP 22 to
the operator's current `/32` or a verified private management route. Retain
key-based SSH, disable password/root login, and verify a second login before
closing the original session. Apply the same port policy in any enabled host
firewall. The installer leaves firewall and SSH configuration to the operator.
SSM management uses outbound HTTPS and requires no inbound management port.

## Install

The deployed monthly combined ingress-plus-egress ceiling is explicitly
500,000,000,000 bytes. Retain headroom against the remaining current-month
provider allowance and other traffic.

```sh
sudo bash deploy/relay-tcp/install.sh \
  "$PWD/target/release/latch-relay" \
  /root/relay-cert/fullchain.pem /root/relay-cert/privkey.pem \
  500000000000 --initialize-quota
sudo install -o root -g root -m 0750 deploy/relay-tcp/certbot-deploy.sh \
  /etc/letsencrypt/renewal-hooks/deploy/50-latch-relay
sudo bash deploy/relay-tcp/network.sh
sudo systemctl start latch-relay.service
sudo systemctl enable --now certbot.timer
sudo systemctl is-active latch-relay.service
sudo journalctl -u latch-relay.service --since '5 minutes ago' --no-pager
python3 deploy/relay-tcp/selftest.py --live relay.latch.rune.foo
sudo certbot renew --dry-run --run-deploy-hooks --cert-name relay.latch.rune.foo
```

The installer enables the service for boot and leaves the first public start
explicit. Quota initialization runs the native CLI as `latch-relay` against
`127.0.0.1:0` for two seconds. An unsupported CPU, invalid limits or failed ledger
initialization fails installation. The initializer accepts only a missing ledger.
Ordinary service startup always requires the retained ledger.

The service has a non-login account, a read-only filesystem except its systemd
state directory and private temporary directory, and only the capability needed
to bind a low port. It has a 512 MiB memory ceiling and 384 MiB soft pressure
threshold. These are pilot containment limits. Measure peak RSS on the selected
Linux machine under the intended stream count before selecting a smaller bundle.
An OOM kill ends current sessions and systemd restarts the service with the same
quota. Local `limits.json` changes survive reinstalling the kit.

`network.sh` enables and persists `net.ipv4.tcp_mtu_probing=2`. Linux applies
this TCP setting to both IPv4 and IPv6, starting with conservative segments and
probing upward even when ICMP Packet Too Big messages vanish. This protects
the daemon's sending path. Client Unix retries can request a 1200-byte MSS.
The script also enables BBR and the `fq` default when the kernel supports both.
It retains the existing TCP buffer defaults.
Interface qdisc changes require interface recreation or a reboot. Check the
effective qdisc and congestion control after reboot. Apply this script during
setup of the dedicated relay.

For an upgrade, schedule a brief interruption, stop the native relay, repeat
the installer with the same ledger and omit `--initialize-quota`, then start it.
The installer refuses an active native service. Back up local operator limits
before editing them. A reinstall preserves the quota and local limits while
replacing the binary, unit, certificates and explicitly supplied budget.

## Limits and durable budget

The shipped operator limits extend the crate's default session lifetime and
transfer cap for the authorized dedicated deployment.

| Limit | Value |
| --- | --- |
| Concurrent TCP connections | 24 globally, 8 per source key |
| Concurrent sessions | 4 globally, 2 per source key |
| Connection accepts per minute | 120 globally, 30 per source key |
| Source-rate entries | 256 |
| Grant lifetime | 60 seconds |
| Session lifetime | 3,600 seconds |
| Session transfer | 16 GiB combined ingress and egress |
| Handshake/request deadline | 10 seconds |
| Send stall / idle deadline | 750 ms / 30 seconds |

Source keys cover one IPv4 address or IPv6 /64. Mapped IPv4 and well-known
NAT64 /96 use the corresponding IPv4 key. The bounded rate table evicts its
oldest window to admit new sources. Per-source rate refusals preserve global
accept credit. Early TLS closes report "blocked or relay busy" because the
client cannot distinguish exhausted admission from a network close. Request
rejections send `[0]` when existing quota credit can deliver it. Decoy responses
increment `http_responses` in periodic metrics and consume no diagnostic lines.

A session uses four TCP connections, two lanes per peer. The per-source limits
permit one complete session whose peers share a public NAT. Admission is bounded
and anonymous. An unauthenticated client can spend quota or occupy limited slots.
The monthly budget remains the economic admission boundary for this pilot.

`/etc/latch-relay/budget.conf` supplies `MONTHLY_BYTES`. The daemon reserves
64 KiB leases durably before consuming encrypted TCP socket bytes, including TLS
setup. It charges ingress plus egress. A forwarded byte therefore costs roughly
two bytes plus TLS overhead. Unused reservations stay charged after disconnect
or restart. A missing, corrupt, locked or unwritable ledger fails closed. A
larger explicitly configured budget can reopen capacity. Restarts retain usage.
The ledger advances at UTC calendar-month rollover, and a backwards clock does
not reopen an already spent month. Keep time synchronization healthy.

The counter covers application-observed encrypted TCP bytes. IP/TCP headers,
retransmissions, traffic rejected before a socket read, kernel responses, SSH,
package updates and other host traffic also affect provider billing. Leave
headroom and inspect provider counters. This application quota bounds its
metered traffic. A provider-level dollar cap also needs controls for traffic
outside that meter. Systemd retries failures every 30 seconds indefinitely. A
depleted ledger stays closed until its month advances or the operator changes
the budget. Missing/corrupt storage requires operator repair from a current
durable copy. Reinitializing it loses the usage history.

Back up a stopped ledger, together with its month and operator budget. Restoring
an older copy can reopen already spent capacity. Retain the ledger on the same
persistent root disk through reboots and binary upgrades. Destruction and
recreation of an instance require a deliberate current-ledger migration.

## Monthly cost and proration

Live Seoul API results on 2026-11-14 showed `small_3_0` at $12/month with 2 GiB,
2 vCPUs and 3,072 GB transfer, and `micro_3_0` at $7/month with 1 GiB, 2 vCPUs and
2,048 GB. These are Linux bundles with public IPv4. Parent selection remains
pending the workload and on-instance CPU/memory checks. The free burst capacity
can hide an inadequate sustained CPU baseline during a short test.

AWS documents a 10% baseline per vCPU for the $7/1 GiB plan and 20% per vCPU
for the $12/2 GiB plan, equivalent to 0.2 and 0.4 continuously available vCPU
across each two-vCPU instance. Measure the native relay's actual crypto load
on that CPU and watch credits during the intended session duration.
[AWS CPU baseline documentation](https://docs.aws.amazon.com/lightsail/latest/userguide/baseline-cpu-performance.html)

Inbound and outbound transfer both consume the Lightsail allowance. Overage for
Seoul instance internet egress is $0.13/GB in the live AWS FAQ. Instance charges
are hourly and capped at the monthly price. For this deployment, conservatively
prorate the transfer budget by the remaining fraction of the UTC month, subtract
other usage and leave headroom. Confirm the actual current-month allowance in
AWS billing before raising that ceiling. The live FAQ describes monthly pooled
allowances and preserved usage after deletion/recreation. It does not establish
a partial-month transfer formula. Retain a substantial reserve for TLS, transport
overhead and management traffic. At month rollover the relay's automatic quota
reset retains the configured numeric ceiling, so raise a first-month pilot
ceiling only after confirming the next month's provider allowance.

At a continuous 20 Mbps forwarded stream, 30 days consume approximately 6.48 TB
in one direction and 12.96 TB combined, before transport overhead. Session count
and transfer budget constrain economical use much earlier than advertised
network peak speed. An attached Lightsail static IP has no additional charge.
An unattached static IP and retained snapshots can accrue charges. Stopped
Lightsail instances continue billing until deleted. Verify current prices and
proration immediately before provisioning.

Sources are the live `aws lightsail get-bundles --region ap-northeast-2` response,
[AWS Lightsail pricing](https://aws.amazon.com/lightsail/pricing/) and
[AWS Lightsail FAQ](https://aws.amazon.com/lightsail/faq/).

## Provisioning and health

Follow [the Lightsail CLI plan](LIGHTSAIL.md) after the parent chooses a bundle
and authorizes provisioning. The plan includes tagged resource reuse, explicit
SSH CIDR, static IP, reboot validation and recoverable cleanup.

On the new machine, inspect system state without dumping secrets.

```sh
sudo bash deploy/relay-tcp/inspect.sh
sudo systemctl show latch-relay.service \
  -p ActiveState -p SubState -p NRestarts -p MemoryCurrent -p MemoryPeak \
  -p CPUUsageNSec -p TasksCurrent
sudo stat -c '%U %G %a %s %n' /var/lib/latch-relay/quota /etc/latch-relay/privkey.pem
```

Metrics appear every 30 seconds in the journal. Check `network_bytes`,
`quota_reserved_bytes`, accepted/rejected connections, active connections and
sessions. Process counters restart from zero while the durable reservation
ledger retains its total for the month. Check those metrics under a real paired
host/viewer session as well as a public TLS probe. Test outbound-only TCP 443
with UDP unavailable, disconnect/reconnect, a service restart and a machine
reboot. The transport's authorized control path must remain reachable to obtain
fresh grants and keys during recovery.

Measure a sustained session beyond the instance's available burst capacity.
Inspect Lightsail `BurstCapacityPercentage`, `BurstCapacityTime`, CPU and
network metrics alongside process RSS, CPU and kernel allowance counters.
Confirm the quota remains unchanged across an idle reboot and increases under
traffic. Recovery must retain the same ledger. This kit supplies daemon process
and boot recovery. Host/viewer automatic fallback and retry integration belongs
to the release task.

## Repeatable public transport measurement

Build the bounded paired probe on the campus Mac or another intended client.
Both roles connect outbound to the public endpoint. Keep its output free of
capabilities and keys.

```sh
cargo build --locked --release -p latch-relay --example paired_probe
target/release/examples/paired_probe relay.latch.rune.foo 600 5 60
target/release/examples/paired_probe relay.latch.rune.foo 60 5 120
# Optional 30-second quarter-bitrate warm-up, retaining 60 fps throughout.
target/release/examples/paired_probe relay.latch.rune.foo 3600 20 60 30
```

The arguments are hostname, total seconds, target Mbps, source fps, and optional
warm-up seconds per fresh pair. A run is capped at one hour and a modeled
50 GB combined transfer ceiling with headroom. Received payload Mbps, complete
frame age, authenticated ACK age, unsent queue age, source drops and priority/
control echoes are emitted every 30 seconds. The probe checks frame contents
and ordering. The optional warm-up uses one-quarter frame size at the same fps.
It rotates a fresh grant and fresh keys before the negotiated session time or
byte cap. Accepted frames still in flight during a planned rotation are reported
in `accepted_not_received`. A terminal transport failure exits with an error.

The cold 20-Mbps/60-fps campus test timed out. A warmed run sustained roughly
20 Mbps for 106 seconds after its 30-second warm-up, then hit the unchanged
750 ms deadline. Use [the deployment report](DEPLOYED.md) for the measured
operating range and limitations.

Collect read-only Linux service CPU, RSS, cgroup memory and durable reservations
during a bounded test using the supplied sampler.

```sh
sudo python3 deploy/relay-tcp/sample-capacity.py 3700 --interval 30
```

The sampler emits UTC timestamps and one-core CPU percentages. Fetch Lightsail
CPU and burst-capacity metrics over the same UTC interval. Separate application
test overlap when interpreting global daemon counters.
