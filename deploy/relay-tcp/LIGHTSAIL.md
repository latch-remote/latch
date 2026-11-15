# Seoul Lightsail operations

As of 2026-11-15 UTC, `latch-relay-seoul-1` carries the public native TCP/TLS
relay and the operator tailnet's Tailscale UDP peer relay in `ap-northeast-2a`.
Account `043189681495`, Ubuntu 24.04, bundle `small_3_0`, static allocation
`latch-relay-seoul-1-ip`, public IPv4 `54.116.93.176`. Existing ownership tags
are `Project=Latch`, `Purpose=native-tcp-relay` and `Release=6.0.0`.

## Account and access

Check the account before every AWS mutation. Management SSH uses the pinned
configuration at `/Users/daniel/.ssh/latch6-lightsail.conf`, alias `latch6-relay`.

```bash
set -euo pipefail
test "$(aws sts get-caller-identity --query Account --output text)" = 043189681495
aws lightsail get-instance --region ap-northeast-2 --instance-name latch-relay-seoul-1
aws lightsail get-static-ip --region ap-northeast-2 --static-ip-name latch-relay-seoul-1-ip
aws lightsail get-instance-port-states --region ap-northeast-2 --instance-name latch-relay-seoul-1
ssh -F /Users/daniel/.ssh/latch6-lightsail.conf latch6-relay
```

| Inbound port | Scope and purpose |
| --- | --- |
| TCP 22 | Operator IPv4 /32 and current IPv6 /128, pinned key-only SSH |
| TCP 80 | Public IPv4/IPv6, HTTP probe and ACME webroot after the README upgrade |
| TCP 443 | Public IPv4/IPv6, native ALPN and HTTPS WebSocket |
| UDP 443 | Public IPv4/IPv6, Tailscale peer relay |
| UDP 41641 | Public IPv4/IPv6, tailscaled transport and discovery |

The SSH rule matched the Mac at inspection. When the operator moves networks,
update only that rule to the current `/32` and IPv6 `/128`. Individual Lightsail port operations
preserve the other rules. Tailscale manages host netfilter rules. UFW is inactive.

## Relay identity and recovery

The verified EC2 backup preceded migration of the relay identity to Lightsail.
The old EC2 instance and security group were terminated/deleted on
2026-11-15 at approximately 06:15 UTC. Lightsail is the only relay.

Kernel-mode tailscaled 1.104.7 uses `/dev/net/tun`. Route acceptance, DNS
acceptance and Tailscale SSH are disabled. The node advertises no routes.
Give the relay a dedicated tag whose policy allows it only as a peer-relay
destination and grants it no access into the tailnet.

```bash
sudo tailscale set --relay-server-port=443 --relay-server-static-endpoints=54.116.93.176:443
sudo systemctl enable tailscaled.service
sudo tailscale debug peer-relay-sessions
```

Retain a root-only backup of `/var/lib/tailscale`. During recovery, stop the
existing daemon before restoring state to a replacement and keep only one
running copy of the identity. Verify the peer-relay path from both clients.

## Reapply the host profile

```bash
sudo apt-get install ethtool
sudo bash deploy/relay-tcp/tune-host.sh
sudo systemctl is-enabled latch-relay tailscaled latch-relay-offloads
sudo systemctl show latch-relay tailscaled -p MainPID -p ActiveState -p LimitNOFILE -p MemoryHigh -p MemoryMax
sudo tc qdisc show
sudo ethtool -k ens5
```

`network.sh` loads conntrack before applying sysctls and persists it through
`/etc/modules-load.d/latch-relay.conf`. It installs daemon drop-ins and a
boot-time offload unit.
It applies live descriptor and memory limits without restarting either daemon.
`tune-host.sh` also masks unnecessary modem, multipath, removable-media and
firmware services. See the exact settings and rationale in the deployment
report. [Deployment settings](DEPLOYED.md#dual-role-consolidation-on-2026-11-15)

During relay upgrades on the shared host, keep tailscaled, interfaces and network
management continuously running. Apply individual changed sysctls live and
update their persistent file. The WebSocket upgrade changed only MTU probing
from `1` to `2`. Reserve a brief `latch-relay` stop for executable/unit replacement.

## Bundle and transfer economics

Live Seoul API prices on 2026-11-15 were as follows. All plans have two vCPUs.
Baseline percentages apply to each vCPU's utilization.

| Bundle | USD/month | RAM GiB | Baseline per vCPU | Sustainable total vCPU | Combined GB/month | 20-Mbps hours with 10% overhead |
| --- | --- | --- | --- | --- | --- | --- |
| `nano_3_0` | 5 | 0.5 | 5% | 0.10 | 1,024 | 51.7 |
| `micro_3_0` | 7 | 1 | 10% | 0.20 | 2,048 | 103.4 |
| `small_3_0` | 12 | 2 | 20% | 0.40 | 3,072 | 155.2 |
| `medium_3_0` | 24 | 4 | 20% | 0.40 | 4,096 | 206.9 |

Recommend retaining `small_3_0`. The native 20-Mbps sample cost 0.212% of one
core per forwarded Mbps. Peer samples around 3 Mbps cost 0.797–0.862% of one
core per Mbps. Extrapolating the latter to 20 Mbps gives 0.16–0.17 vCPU before
background work. Adding roughly 0.05-vCPU background work reaches the micro
bundle's baseline. Two concurrent 20-Mbps flows, one per role, project about
0.26 vCPU including background work. These projections need sustained-load
confirmation. Nano has insufficient RAM for this profile. Medium doubles
price while retaining the small bundle's CPU baseline.

The illustrative expected use is 60 stream-hours/month at 20 Mbps, two hours
per day, one forwarded stream at a time, with 10% transfer overhead. This uses
1,188 GB combined and costs about $12/month on small. At 50 Mbps for those same
60 hours, the model uses 2,970 GB. The campus tests did not sustain 50 Mbps.
The native 500-GB guard permits about 25.3 hours at 20 Mbps with that overhead
factor. Peer traffic also consumes the provider allowance.

Both ingress and egress consume the Lightsail allowance. Beyond it, Seoul
public internet egress costs $0.13/GB. For steady symmetric relay traffic,
modeled billed egress is half the combined excess. Traffic timing and
retransmissions affect actual billing. These estimates use decimal GB and
conservatively take the API's allowance numbers at face value.

| 20-Mbps hours/month | Combined GB with 10% overhead | Nano cost | Micro cost | Small cost | Medium cost |
| --- | --- | --- | --- | --- | --- |
| 60 | 1,188 | $15.66 | $7.00 | $12.00 | $24.00 |
| 120 | 2,376 | $92.88 | $28.32 | $12.00 | $24.00 |
| 180 | 3,564 | $170.10 | $105.54 | $43.98 | $24.00 |

An attached static IPv4 has no additional charge. The available November
network metrics totaled 5.97 GB after testing. Billing is authoritative for
first-month allowance adjustments. The native 500-GB pilot guard retains
substantial headroom. The recurring instance cost is the Lightsail
`small_3_0` bundle at $12/month. The old EC2 instance and security group have
been removed.

The FREE account plan expires **2026-12-21 14:35:44 UTC**. The Free Tier API
separately reported $162 remaining credits on 2026-11-15. The instance costs
$12/month. Verify credit expiration and plan conversion in AWS billing before
the plan expiry date.

Sources include the live regional bundle, Pricing and Free Tier APIs, plus
AWS's baseline and transfer documentation.
[AWS CPU baselines](https://docs.aws.amazon.com/lightsail/latest/userguide/baseline-cpu-performance.html)
[AWS pricing](https://aws.amazon.com/lightsail/pricing/)
[AWS transfer accounting](https://aws.amazon.com/lightsail/faq/)

## IPv6 and DNS

On 2026-11-15, `set-ip-address-type --ip-address-type dualstack` assigned
`2406:da12:417:7f00:7baf:3615:172a:fe6f`. The running host acquired its global
address and default route automatically within several minutes. Its existing
network manager stayed running. Keep the host online while waiting for router
advertisements/DHCPv6 and inspect `ip -6 addr` and `ip -6 route` before taking
further action. IPv6 address assignment persists across stop/start. Disabling
IPv6 or deleting the instance releases the address.

The four public port rules now include `::/0`. SSH retains its existing IPv4
scope and adds only the current operator IPv6 `/128`. IPv4 rules were verified
equal before and after the change. Tailscaled retains its IPv4 static peer-relay
endpoint and its existing process/start time.

The production relay binds `[::]:443` with IPv4-mapped acceptance. Direct public
IPv6 checks from the campus Mac verified WebPKI hostname/chain, native ALPN and
HTTP 200 before publication. Cloudflare now serves a DNS-only AAAA record for
`relay.latch.rune.foo` at that address, TTL 300. The existing DNS-only A record
remains `54.116.93.176`. An existing zone DNS-edit credential authorized the
single AAAA creation. Authoritative DNS and a public recursive resolver returned
the new record. Boot persistence awaits a separately coordinated maintenance
window.
[AWS dual-stack behavior](https://docs.aws.amazon.com/lightsail/latest/userguide/amazon-lightsail-enable-disable-ipv6.html)

## Completed teardown

The old EC2 relay instance and security group were removed on 2026-11-15 at
approximately 06:15 UTC, after backup verification. The Lightsail static IP,
native binary, certificate automation and durable quota remain in service.

Operational changes and bounded test failures are recorded with UTC times.
[Results log](results/consolidation-20261115/CHANGES.md)
