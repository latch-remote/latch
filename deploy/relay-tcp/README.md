# TCP/TLS and HTTPS WebSocket relay

This kit installs the `latch-relay` crate on a dedicated Linux machine with
systemd. The public endpoint is `relay.latch.rune.foo:443` on the dedicated
`latch-relay-seoul-1` Lightsail instance. Since 2026-11-15, this instance also
carries the operator tailnet's kernel-mode Tailscale UDP peer relay on
`54.116.93.176:443`, with tailscaled transport on UDP 41641. The old EC2 instance and security group were removed after backup
verification on 2026-11-15 at approximately 06:15 UTC. Lightsail is the only relay. The parent release task owns final application integration.
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
The production certificate currently uses Certbot standalone HTTP-01. Switch it
to the webroot authenticator during this upgrade using the steps below. The
updated unit owns TCP 80 for the anonymous probe and ACME webroot. The enabled
`certbot.timer` handles renewal. This daemon
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

Expose TCP 80 and 443 publicly for both enabled address families on the dedicated
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

For a fresh certificate, issue through webroot from the start. Stop
`certbot.timer` and finish any active renewal before binding port 80. Open the
DNS/firewall paths described above and create the webroot, then run a temporary
HTTP server until the daemon has its first certificate. Run these commands on
the deployment host, with `RELAY_HOST` set to the certificate hostname.

```sh
sudo systemctl stop certbot.timer
sudo install -d -m 0755 /var/www/latch-relay/.well-known/acme-challenge
sudo python3 -m http.server 80 --bind :: --directory /var/www/latch-relay
```

In a second terminal, issue the first certificate with
`sudo certbot certonly --webroot -w /var/www/latch-relay -d "$RELAY_HOST"`.
Stop the temporary HTTP server, then install using
`/etc/letsencrypt/live/$RELAY_HOST/fullchain.pem` and
`/etc/letsencrypt/live/$RELAY_HOST/privkey.pem`. The saved renewal configuration
already uses webroot. Keep the timer stopped through installation and the
renewal dry-run, then enable it.

```sh
sudo bash deploy/relay-tcp/install.sh \
  "$PWD/target/release/latch-relay" \
  "/etc/letsencrypt/live/$RELAY_HOST/fullchain.pem" \
  "/etc/letsencrypt/live/$RELAY_HOST/privkey.pem" \
  500000000000 --initialize-quota
sudo install -o root -g root -m 0750 deploy/relay-tcp/certbot-deploy.sh \
  /etc/letsencrypt/renewal-hooks/deploy/50-latch-relay
sudo apt-get install ethtool
sudo bash deploy/relay-tcp/network.sh
sudo systemctl start latch-relay.service
sudo systemctl is-active latch-relay.service
sudo journalctl -u latch-relay.service --since '5 minutes ago' --no-pager
python3 deploy/relay-tcp/selftest.py --live "$RELAY_HOST"
sudo certbot renew --dry-run --run-deploy-hooks --cert-name "$RELAY_HOST"
sudo systemctl enable --now certbot.timer
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

`network.sh` persists BBR/fq, 16-MiB TCP/UDP socket ceilings, always-on TCP
packet-size probing (`net.ipv4.tcp_mtu_probing=2`, which Linux applies to IPv4
and IPv6, so the daemon's sending path survives networks that drop ICMP Packet
Too Big; Linux clients advertise 1200- then 512-byte MSS before connect.
macOS can apply MSS only after connect and Windows skips these recovery rungs,
so both rely on relay TCP MTU probing for its certificate flight),
128-KiB unsent low-water mark, idle-window preservation and a 131,072-entry
conntrack ceiling. It installs a boot-time UDP GRO forwarding unit and daemon
drop-ins with 65,536-file limits. Tailscaled receives 384/512-MiB memory
high/max limits. Live process limits are applied without daemon restarts.
The existing `ens5` queues already use fq. Verify effective qdiscs after link
recreation. `tune-host.sh` additionally masks unnecessary Ubuntu modem,
multipath, removable-media and firmware services on this dedicated host.
The deployment report explains each value and the retained no-swap profile.
[Applied tuning](DEPLOYED.md#persistent-relay-tuning)

For an upgrade, back up the executable, unit and drop-ins, operator limits,
budget and quota to a dated root-only directory. Keep an off-host copy with
private keys excluded and verify the rollback executable and hashes. Schedule
a brief interruption, stop `latch-relay`, capture the stopped ledger, repeat
the installer with the same ledger and omit `--initialize-quota`, then start it.
Verify native ALPN immediately and restore the previous executable/unit if it
fails. Rollback must retain the current ledger, including reservations spent
since the backup. The installer refuses an active native service. A reinstall
preserves the quota and local limits while replacing the binary, unit,
certificates and explicitly supplied budget.

On the shared peer-relay host, keep tailscaled and network management running
throughout maintenance. Apply only changed sysctls directly. For the WebSocket
upgrade, replace `net.ipv4.tcp_mtu_probing = 1` with `2` in
`/etc/sysctl.d/90-latch-relay.conf` and run
`sudo sysctl -w net.ipv4.tcp_mtu_probing=2`. Keep the existing offloads and daemon
limits in place. Validate the replacement Linux binary in a matching container
on the build machine, using `-j 4` on the shared Mac.

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

Live Seoul API results on 2026-11-15 showed `small_3_0` at $12/month with 2 GiB,
2 vCPUs and 3,072 GB transfer, and `micro_3_0` at $7/month with 1 GiB, 2 vCPUs and
2,048 GB. These are Linux bundles with public IPv4. Retain the current small
bundle for the combined workload's RAM, CPU baseline and transfer headroom.
The free burst capacity can hide an inadequate sustained baseline in a short test.

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

Follow the Lightsail operating guide for the combined box, bundle comparison,
monthly transfer model, credit expiration, IPv6/DNS findings and exact teardown
state. At an assumed 60 hours/month and 20 Mbps with 10% overhead, the provider
cost is about $12/month. The native 500-GB guard allows about 25.3 such hours.
The Free Tier API reported $162 remaining credit and a FREE-plan expiration
of 2026-12-21. The recurring instance cost is Lightsail `small_3_0` only.
[Lightsail operations](LIGHTSAIL.md)

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
# Force each carrier and print grant/lane carrier diagnostics.
target/release/examples/paired_probe relay.latch.rune.foo 40 5 60 10 native
target/release/examples/paired_probe relay.latch.rune.foo 40 20 60 10 websocket
# Optional explicit local HTTP CONNECT proxy, with public relay TLS verification.
target/release/examples/paired_probe relay.latch.rune.foo 6 1 30 0 websocket 127.0.0.1:3128
```

The arguments are hostname, total seconds, target Mbps, source fps, optional
warm-up seconds per fresh pair, optional carrier (`auto`, `native`, `websocket`),
and optional CONNECT proxy `HOST:PORT`. Automatic selection is the default.
Forced selection applies to the grant and all four lanes, including cached
paths. A run is capped at one hour and a modeled
50 GB combined transfer ceiling with headroom. Received payload Mbps, complete
frame age, authenticated ACK age, unsent queue age, source drops and priority/
control echoes are emitted every 30 seconds. The probe checks frame contents
and ordering. The optional warm-up uses one-quarter frame size at the same fps.
Latency percentiles and maxima accumulate across the whole run. `received_mbps`
describes the latest reporting interval, `payload_mbps` includes warm-up and
drain, and `post_warmup_payload_mbps` measures delivered bytes since the latest
full-rate transition, including its drain. It includes warm-up frames still in
flight at that transition. With no warm-up, both cumulative rates cover the run.
The isolated carrier comparison uses a disposable CA through `PAIRED_PROBE_CA`.
[Carrier comparison lab](../../scripts/netlab/README.md#carrier-throughput-comparison)
It rotates a fresh grant and fresh keys before the negotiated session time or
byte cap. Accepted frames still in flight during a planned rotation are reported
in `accepted_not_received`. A terminal transport failure exits with an error.

The earlier cold 20-Mbps/60-fps campus test timed out. A warmed run sustained roughly
20 Mbps for 106 seconds after its 30-second warm-up, then hit the unchanged
750 ms deadline. Use [the deployment report](DEPLOYED.md) for the measured
operating range and limitations. Matched 2026-11-15 runs completed the 5- and
20-Mbps targets with warm-up. The 50-Mbps attempts timed out before and after
tuning. Windows SFTP over the migrated peer path measured 3.10 Mbps.
[Consolidation measurements](results/consolidation-20261115/README.md)

Collect read-only Linux service CPU, RSS, cgroup memory and durable reservations
during a bounded test using the supplied sampler.

```sh
sudo python3 deploy/relay-tcp/sample-capacity.py 3700 --interval 30
```

The sampler emits UTC timestamps and one-core CPU percentages. Fetch Lightsail
CPU and burst-capacity metrics over the same UTC interval. Separate application
test overlap when interpreting global daemon counters.

## Enable the anonymous probe and switch Certbot to webroot

Arrange a brief interruption and tell connected users before stopping the relay.
On the live host, inspect recent metrics and active TCP connections. Metrics are
printed every 30 seconds. Pick a quiet moment with `sessions=0` and no active
stream connections, then confirm the latest sample immediately before proceeding.
If sessions remain active, arrange their interruption with the users.
The inspection block below causes no relay interruption (0 seconds).

```sh
sudo journalctl -u latch-relay.service --since '2 minutes ago' --no-pager | tail -20
sudo ss -tnp '( sport = :443 )'
sudo systemctl is-active certbot.service
```

Set the hostname, existing Certbot certificate name, reviewed Linux binary path
and retained monthly budget. The commands below run on the deployment host.
The installer installs the binary and unit together. The root-only backup covers
the executable, unit and drop-ins, environment/configuration including the TLS
identity, renewal configuration, and a consistent stopped quota ledger. Keep an
off-host copy with private keys excluded as described in the general upgrade
instructions above. Rollback retains the current ledger, including reservations
spent after the backup. The variable-setting block causes no relay interruption
(0 seconds).

```sh
export RELAY_HOST='your-relay-hostname'
export RELAY_CERT_NAME="$RELAY_HOST"
export RELAY_BINARY='/absolute/path/to/reviewed/latch-relay'
export MONTHLY_BYTES='500000000000'
```

Run the following block from the checkout containing this kit. Every exit after
the timer is stopped attempts to re-enable it and verifies that it is active.
Certbot 2.3 or later uses `reconfigure`. Older versions use the renewal-file
webroot settings in the fallback branch. A failure after stopping the relay
restores the prior executable, unit, config
and renewal settings, restarts it, and checks both carriers. Retain the printed
backup directory for a later rollback. An unsuccessful rollback or timer restart
exits with an error and needs operator attention.

For this upgrade block, plan 5–10 seconds of relay interruption between the stop
and start commands on a healthy host, plus client reconnection time. Certificate
reconfiguration and staging validation can take minutes after the relay starts.
If automatic rollback runs, allow another 5–10 seconds for restoring the saved
files and restarting. A failed restart needs manual recovery and can extend the
interruption beyond these estimates. The automatic rollback also restores
standalone authentication and removes the port-80 probe until the upgrade is
successfully repeated.

```bash
sudo env RELAY_HOST="$RELAY_HOST" RELAY_CERT_NAME="$RELAY_CERT_NAME" \
  RELAY_BINARY="$RELAY_BINARY" MONTHLY_BYTES="$MONTHLY_BYTES" \
  RELAY_KIT="$PWD/deploy/relay-tcp" bash <<'BASH'
set -euo pipefail
umask 077
renewal="/etc/letsencrypt/renewal/$RELAY_CERT_NAME.conf"
backup=$(mktemp -d /root/latch-relay-upgrade.XXXXXX)
printf 'Backup directory %s\n' "$backup"
service_stopped=0
rollback() {
    systemctl stop latch-relay.service || return 1
    cp -a /var/lib/latch-relay/quota "$backup/quota.at-rollback" || return 1
    sha256sum --check "$backup/SHA256SUMS" || return 1
    install -m 0755 "$backup/latch-relay" /usr/local/bin/latch-relay || return 1
    cp -a "$backup/latch-relay.service" /etc/systemd/system/latch-relay.service || return 1
    cp -a "$backup/config/." /etc/latch-relay/ || return 1
    cp -a "$backup/renewal.conf" "$renewal" || return 1
    rm -rf /etc/systemd/system/latch-relay.service.d || return 1
    if [[ -d "$backup/drop-ins" ]]; then
        cp -a "$backup/drop-ins" /etc/systemd/system/latch-relay.service.d || return 1
    fi
    systemctl daemon-reload || return 1
    systemctl start latch-relay.service || return 1
    systemctl is-active --quiet latch-relay.service || return 1
    python3 "$RELAY_KIT/selftest.py" --live "$RELAY_HOST" || return 1
}
finish() {
    result=$?
    trap - EXIT HUP INT TERM
    if (( result != 0 && service_stopped )); then
        rollback || { echo 'Rollback failed. Use the retained backup to repair the service.' >&2; result=1; }
    fi
    systemctl enable --now certbot.timer || result=1
    systemctl is-active --quiet certbot.timer || { echo 'Certbot timer is inactive. Restore renewal scheduling now.' >&2; result=1; }
    exit "$result"
}
trap finish EXIT
trap 'exit 130' INT
trap 'exit 143' HUP TERM
systemctl stop certbot.timer
certbot_state=$(systemctl is-active certbot.service || true)
case "$certbot_state" in
    inactive|failed) ;;
    *) echo 'A Certbot job is still running. Let it finish and rerun this block.' >&2; exit 1 ;;
esac
cp -a /usr/local/bin/latch-relay "$backup/latch-relay"
cp -a /etc/systemd/system/latch-relay.service "$backup/latch-relay.service"
cp -a /etc/latch-relay "$backup/config"
cp -a "$renewal" "$backup/renewal.conf"
if [[ -d /etc/systemd/system/latch-relay.service.d ]]; then
    cp -a /etc/systemd/system/latch-relay.service.d "$backup/drop-ins"
fi
sha256sum "$backup/latch-relay" "$backup/latch-relay.service" "$backup/renewal.conf" > "$backup/SHA256SUMS"
journalctl -u latch-relay.service --since '1 minute ago' --no-pager | tail -10
service_stopped=1
systemctl stop latch-relay.service
cp -a /var/lib/latch-relay/quota "$backup/quota.before"
bash "$RELAY_KIT/install.sh" "$RELAY_BINARY" \
  "/etc/letsencrypt/live/$RELAY_CERT_NAME/fullchain.pem" \
  "/etc/letsencrypt/live/$RELAY_CERT_NAME/privkey.pem" "$MONTHLY_BYTES"
grep -F -- '--http-listen [::]:80 --acme-webroot /var/www/latch-relay' /etc/systemd/system/latch-relay.service
systemctl start latch-relay.service
systemctl is-active --quiet latch-relay.service
python3 "$RELAY_KIT/selftest.py" --live "$RELAY_HOST"
for family in 4 6; do
    [[ $(curl "-$family" --noproxy '*' -fsS --max-time 10 -o /dev/null \
        -w '%{http_code}' "http://$RELAY_HOST/netenv/probe") == 204 ]]
done
# Remove this certificate's obsolete standalone pre/post hooks before any dry-run.
# The deploy hook is retained. Confirm that global pre/post hook directories
# likewise contain no standalone hook that stops this relay.
python3 - "$renewal" <<'PY'
import configparser
import io
import pathlib
import sys
path = pathlib.Path(sys.argv[1])
config = configparser.RawConfigParser()
config.optionxform = str
config.read_string("[DEFAULT]\n" + path.read_text())
config.remove_option('renewalparams', 'pre_hook')
config.remove_option('renewalparams', 'post_hook')
output = io.StringIO()
config.write(output)
path.write_text(output.getvalue().removeprefix('[DEFAULT]\n'))
PY
certbot_version=$(certbot --version | awk '{print $2}')
if dpkg --compare-versions "$certbot_version" ge 2.3; then
    certbot reconfigure --cert-name "$RELAY_CERT_NAME" --webroot -w /var/www/latch-relay
else
    python3 - "$renewal" "$RELAY_HOST" <<'PY'
import configparser
import io
import pathlib
import sys
path = pathlib.Path(sys.argv[1])
config = configparser.RawConfigParser()
config.optionxform = str
config.read_string("[DEFAULT]\n" + path.read_text())
config['renewalparams']['authenticator'] = 'webroot'
config['renewalparams']['webroot_path'] = '/var/www/latch-relay,'
config['[webroot_map]'] = {sys.argv[2]: '/var/www/latch-relay'}
output = io.StringIO()
config.write(output)
path.write_text(output.getvalue().removeprefix('[DEFAULT]\n'))
PY
fi
certbot renew --cert-name "$RELAY_CERT_NAME" --dry-run
systemctl is-active --quiet latch-relay.service
journalctl -u latch-relay.service --since '2 minutes ago' --no-pager
# The EXIT handler re-enables and verifies certbot.timer on success too.
BASH
```

### Roll back later

For an immediate failure, the block above runs rollback automatically. For a later
rollback, set `RELAY_HOST`, `RELAY_CERT_NAME` and `backup` to the saved values,
then run this block during another arranged interruption, inside `tmux` or
`screen` so a dropped SSH session cannot interrupt it. It runs in its own root
shell, so the renewal timer is restored when it ends, whether it succeeds or
fails, and a failure does not close the operator's shell.

The current quota stays at its live path. The saved ledger is for audit and
recovery of damaged storage, and must never overwrite accumulated usage.

The saved configuration contains the certificate that was in service before the
upgrade. A renewal since then makes that copy older than the current one, and
Certbot would not replace it until its own next renewal. The block therefore
reinstalls the current certificate after restoring the configuration.

For this later rollback block, plan 5–10 seconds of relay interruption between
stop and start on a healthy host, plus client reconnection time. The standalone
renewal dry-run adds a second interruption for HTTP-01 validation, typically
tens of seconds. Backup, certificate validity and key matching are checked
before stopping. A failed restart needs manual recovery and can extend the
interruption beyond those estimates. If a command fails after stopping the
relay, the block can leave it stopped. Repair the reported problem and rerun
the block with the same backup to restore service safely.

The saved `renewal.conf` points back at standalone authentication, including its
relay stop/start hooks. Restoring the saved binary and unit also makes the
port-80 probe unavailable again. Native TLS and WebSocket validation below
checks port 443. The dry-run then exercises the restored standalone renewal
and stop/start hooks while the interruption is still arranged. To restore the
probe, repeat the upgrade and webroot conversion above with the current
certificate and a reviewed probe-capable binary. Keep standalone renewal's
port-80 ownership and stop/start hooks while running the restored unit. Those
renewals interrupt relay service for the HTTP-01 validation and restart,
typically tens of seconds, with longer interruptions if validation fails.
Plan that maintenance with connected users.

```bash
sudo env RELAY_HOST="$RELAY_HOST" RELAY_CERT_NAME="$RELAY_CERT_NAME" \
  backup="$backup" RELAY_KIT="$PWD/deploy/relay-tcp" bash <<'BASH'
set -euo pipefail
umask 077
renewal="/etc/letsencrypt/renewal/$RELAY_CERT_NAME.conf"
lineage="/etc/letsencrypt/live/$RELAY_CERT_NAME"
restore_timer() {
    result=$?
    trap - EXIT HUP INT TERM
    systemctl enable --now certbot.timer || result=1
    systemctl is-active --quiet certbot.timer || { echo 'Certbot timer is inactive. Restore renewal scheduling now.' >&2; result=1; }
    exit "$result"
}
trap restore_timer EXIT
trap 'exit 130' INT
trap 'exit 143' HUP TERM
systemctl stop certbot.timer
certbot_state=$(systemctl is-active certbot.service || true)
case "$certbot_state" in inactive|failed) ;; *) echo 'Certbot is running' >&2; exit 1 ;; esac
# Check the backup and the current certificate before interrupting service.
sha256sum --check "$backup/SHA256SUMS"
openssl x509 -in "$lineage/fullchain.pem" -noout -checkend 86400
openssl pkey -in "$lineage/privkey.pem" -passin pass: -check -noout >/dev/null
cmp -s <(openssl x509 -in "$lineage/fullchain.pem" -pubkey -noout) \
    <(openssl pkey -in "$lineage/privkey.pem" -passin pass: -pubout)
systemctl stop latch-relay.service
cp -a /var/lib/latch-relay/quota "$backup/quota.at-rollback" \
    || echo 'The audit copy of the quota failed. Continuing with the rollback.' >&2
install -m 0755 "$backup/latch-relay" /usr/local/bin/latch-relay
cp -a "$backup/latch-relay.service" /etc/systemd/system/latch-relay.service
cp -a "$backup/config/." /etc/latch-relay/
install -o root -g latch-relay -m 0640 "$lineage/fullchain.pem" /etc/latch-relay/fullchain.pem
install -o root -g latch-relay -m 0640 "$lineage/privkey.pem" /etc/latch-relay/privkey.pem
cp -a "$backup/renewal.conf" "$renewal"
rm -rf /etc/systemd/system/latch-relay.service.d
if [[ -d "$backup/drop-ins" ]]; then
    cp -a "$backup/drop-ins" /etc/systemd/system/latch-relay.service.d
fi
systemctl daemon-reload
systemctl start latch-relay.service
systemctl is-active --quiet latch-relay.service
openssl x509 -in /etc/latch-relay/fullchain.pem -noout -enddate
python3 "$RELAY_KIT/selftest.py" --live "$RELAY_HOST"
certbot renew --cert-name "$RELAY_CERT_NAME" --dry-run
systemctl is-active --quiet latch-relay.service
BASH
```

### Renewal and expiry checks

The deployed certificate registration has no expiry e-mail address. Check renewal
failures and remaining validity explicitly. The relay serves its own copy,
`/etc/latch-relay/fullchain.pem`, which the deploy hook replaces after each
renewal, so check that copy as well as Certbot's: a fresh Certbot certificate
does not prove the relay is serving it. Each `-checkend` command exits nonzero
when fewer than 30 days remain, and `is-failed` exits zero when the last
renewal run failed. A monitoring wrapper must capture each status individually
and avoid `set -e` around this block so every check runs. Run them daily through
the operator's monitoring or scheduler and alert on those results. The checks
below cause no relay interruption (0 seconds).

```sh
sudo openssl x509 -in /etc/latch-relay/fullchain.pem -noout -enddate
sudo openssl x509 -in /etc/latch-relay/fullchain.pem -noout -checkend 2592000
sudo openssl x509 -in "/etc/letsencrypt/live/$RELAY_CERT_NAME/fullchain.pem" -noout -checkend 2592000
sudo systemctl is-failed certbot.service
sudo journalctl -u certbot.service --since '7 days ago' --no-pager
sudo systemctl is-active certbot.timer
```

The HTTP listener bounds request size/time, concurrent connections and per-source
(IPv4 /32, IPv6 /64) plus global admission. Normal traffic has 120 global and
30 per-source admissions per minute. Two slots and 16 admissions per minute
reserve capacity for existing challenge files during ordinary traffic. Headers
have an absolute 1.5-second deadline and each source can hold one idle connection.
Under slot pressure the oldest incomplete header yields to a new arrival.
Completed responses retain their slots. After normal admission credits are
exhausted, incomplete headers have a 250 ms grace for ACME classification.
Idle requests cannot occupy the reserved slots. This reduces ordinary-traffic
starvation. A bounded unauthenticated listener remains susceptible to deliberate
resource exhaustion, so renewal failures and certificate expiry need monitoring.
The listener logs no request paths or addresses. A missing probe on an older
relay remains silent. The new unit requires the new binary, and its port-80
ownership requires the webroot renewal configuration.
