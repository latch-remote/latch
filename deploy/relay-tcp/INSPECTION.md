# Relay inspection and implementation handoff

Observed on 2026-11-14 UTC through the restored AWS session for account
`043189681495`. Worktree was `codex/latch-6-network`.

## Management resources created

| Resource | Identifier |
| --- | --- |
| IAM role | `arn:aws:iam::043189681495:role/LatchRelayManagementRole` |
| Role ID | `AROAQUDSOHVLUGEXJNIPC` |
| IAM instance profile | `arn:aws:iam::043189681495:instance-profile/LatchRelayManagementProfile` |
| Profile ID | `AIPAQUDSOHVLQK3SDHMCZ` |
| Profile association | `iip-assoc-0af6784e7a86836a5` |
| Managed EC2 instance | `i-037a6a2906c8cf54c` in `ap-northeast-2` |

The role uses standard `ec2.amazonaws.com` assume-role trust and the single
managed policy `arn:aws:iam::aws:policy/AmazonSSMManagedInstanceCore`.
Role and profile have `Project=Latch` and
`ManagedInstance=i-037a6a2906c8cf54c` tags. The association reached `associated`.
SSM registered online with agent `3.3.5828.0` after the association propagated.

The first read-only SSM inspection stopped at a missing optional `tc` command.
The inspection script now tolerates that missing tool. The repeat completed
successfully, command `7fcac3e6-6486-4d0f-a62e-74afd4c44a96`, from
09:32:08 to 09:32:13 UTC. A routing/sample/unit-verification command also
succeeded, `f0c1b56f-5b97-44cd-9fa0-cb3f2d6d5c99`, from 09:33:17 to
09:33:27 UTC. User data and console output were processed in memory for narrow
diagnostic flags. Inspection output contained selected counters and settings.

## Existing workload

The instance is a `c7i-flex.large`, 2 vCPUs, with Intel Xeon Platinum 8488C.
Linux is `6.18.62-127.179.amzn2023.x86_64`. All six required x86 Graviola flags
were present, including AES, PCLMULQDQ, BMI1, ADX, AVX and AVX2.

Tailscale `1.104.7` runs as native `tailscaled`, PID 4449. Its configured peer
relay port is UDP 443, bound on IPv4 and IPv6, alongside UDP 41641. SSH listens
on TCP 22. Tailscale also has private tailnet TCP listeners on 38902 and 62944.
The running service/process inventory consists of Tailscale, SSM and standard
OS services. The measured `tailscaled` RSS was 61,424 KiB, with lifetime process
CPU reported as 5.7% by `ps`.

The relay has `WantRunning=true`, `RunSSH=false`, empty advertised routes,
`RouteAll=false` and disabled IPv4/IPv6 forwarding. SSH permits user `jump`,
public-key authentication and TCP forwarding, with root and password login
disabled. The successful EC2 Instance Connect key publication for `ec2-user`
was followed by public-key refusal on both public and tailnet SSH paths.
The observed `AllowUsers jump` setting explains that administrative route's
restriction. SSM now supplies the authorized administrative path.

At 09:33:17 UTC, a macOS tailnet peer was active. Across the following 10.0103
seconds, the public interface received 17,195,212 bytes and transmitted
17,195,728 bytes, equivalent to 13.74197 and 13.74238 Mbps. Both drop counters
had zero delta. This confirms live forwarding activity during inspection.
The observations establish aggregate relay traffic and service ownership.
End-user application identity and Windows session content require the parent's
paired application check.

Host memory at 09:32 was 3,815 MiB total, 253 MiB used and 3,348 MiB available,
with zero configured swap. Root XFS had 8.0 GiB total, 1.8 GiB used and 6.2 GiB
available. The encrypted `vol-0912d03686a89f848` is gp3, 8 GiB, 3,000 provisioned
IOPS and 125 MiB/s. A five-second `vmstat` sample showed 97–99% idle CPU and
zero steal. That short sample reflects the inspected interval.

The EC2 type API reports 0.39 Gbps baseline and 12.5 Gbps peak network capacity.
ENA counters for inbound/outbound bandwidth allowance, PPS, conntrack and
link-local allowance exceeded were all zero. Effective public-interface qdisc
was `mq`; tailnet qdisc was `fq_codel`. The kernel default was `fq_codel` with
`cubic`, and available congestion controls were `reno cubic`. BBR module
availability on this machine requires a separate setup-time check. Kernel
network credit balance and CPU burst credit metrics were unavailable through
the inspected EC2 metric inventory.

## Historical demand

CloudWatch requested 2026-11-13 09:22:40 through 2026-11-14 09:22:40 UTC using
300-second periods. CPU and network metrics returned 287 samples. Status checks
returned 285 samples.

| Metric | Observed value |
| --- | --- |
| CPU mean / p95 five-minute average / maximum | 5.8362% / 31.3407% / 46.5450% |
| Inbound bytes across samples | 236,639,059,665 |
| Outbound bytes across samples | 237,915,460,476 |
| Combined bytes across samples | 474,554,520,141 |
| Inbound mean / p95 / peak five-minute Mbps | 21.9874 / 133.4175 / 168.3851 |
| Outbound mean / p95 / peak five-minute Mbps | 22.1060 / 134.6154 / 168.6082 |
| Status-check failure maximum | 0 |

The combined traffic is 474.55 decimal GB across the returned intervals. A
similar daily workload would spend a multi-terabyte monthly allowance within
days. The observed workload therefore needs an explicit admission budget on
any economical replacement. Egress above Seoul Lightsail's allowance costs
$0.13 per provider-billed GB. Ingress also consumes the allowance.

Live Seoul bundle API checks confirmed `micro_3_0`, $7/month, 1 GiB, 2 vCPUs,
2,048 GB transfer, and `small_3_0`, $12/month, 2 GiB, 2 vCPUs, 3,072 GB transfer.
AWS documents 10% and 20% CPU baselines per vCPU for these two plans. The
existing machine's memory footprint permits considering 1 GiB for a pilot.
The new TLS daemon's peak RSS and crypto CPU cost require measurement on the
chosen instance. Its 512 MiB service ceiling is a containment choice.
The historical CPU p95 also exceeds both bundles' percentage baselines, with
hardware and workload differences preventing a direct capacity equivalence.

The deployment instructions use conservative remaining-month proration for
the operator's transfer ceiling and retain a 10 GiB example pilot budget. The
live AWS FAQ confirms hourly instance charge proration and monthly pooled
transfer usage. Its wording leaves partial-month transfer calculation to
provider billing verification. Parent selection and paid provisioning remain
pending these capacity and budget decisions.

## Validation and limits

Successful local commands were `cargo test -p latch-relay --locked` with
13 library tests and 11 transport integration tests on the final run, and
`cargo build -p latch-relay --locked`. The CPU-rejection library test also
ran its child-process check. These used the existing macOS ARM64 toolchain.

`python3 deploy/relay-tcp/selftest.py --binary target/debug/latch-relay` passed
kit validation and real loopback daemon checks for TLS/ALPN, the shipped pilot
JSON, private ledger mode, exclusive locking, retained quota on restart,
exhaustion across restart, existing-ledger initialization refusal, and missing
or corrupt ledger rejection. Bash syntax and ShellCheck passed for all three
shell scripts. Extracted Lightsail plan Bash blocks passed syntax and
ShellCheck checks. AWS CLI input skeleton checks validated the planned
creation, public-port and metric command shapes.

Linux `systemd-analyze verify` passed against the staged service unit with
its executable path changed to `/usr/bin/true` for parsing. The installed
binary, public TLS, root installer, sysctl application, certificate renewal,
service restart/reboot recovery and sustained native Linux paired streams
require execution on the selected dedicated instance. The verification also
emitted an existing OS `acpid.socket` legacy `/var/run` path warning.

The kit chooses IPv4 public binding, a two-second explicit loopback quota
initializer and shipped pilot limits. Operators must obtain a fresh grant and
keys after an interrupted session. Application quota covers observed encrypted
TCP ingress plus egress. Provider accounting also includes transport overhead,
retransmissions and other host traffic. These checks establish the listed
behaviors under the tested conditions. Release integration and operational
validation remain with the parent.
