# Seoul relay consolidation, 2026-11-15 UTC

The Mac used campus Wi-Fi. Windows used a residential connection.
Mac Tailscale was 1.104.4. Both relay hosts used 1.104.7 during migration.
Identities, client addresses and operator access details are omitted.

## Native TCP/TLS probe

Built the repository's `paired_probe` example locally with Cargo. Each matched
run used `paired_probe relay.latch.rune.foo 25 RATE 60 5`. All native runs had both probe roles
on one Mac on campus Wi-Fi, giving two campus–Seoul legs per frame. The first five seconds
used quarter-size frames. The table reports the whole run, including warm-up.
Frame age measures enqueue through authenticated whole-frame delivery.

| Target Mbps | Before payload Mbps | After payload Mbps | Before frame p95 ms | After frame p95 ms | Outcome |
| --- | --- | --- | --- | --- | --- |
| 5 | 4.226 | 4.226 | 164.9 | 167.3 | Both completed |
| 20 | 16.866 | 16.865 | 248.3 | 169.5 | Both completed |
| 50 | 31.427 | 23.226 | 272.6 | 1,045.5 | Both timed out |

Tuning showed no measurable throughput benefit at 5 or 20 Mbps. The single
50-Mbps after sample was worse, 23.226 Mbps versus 31.427 Mbps before, with
p95 1,045.5 ms versus 272.6 ms. No throughput or latency improvement was
demonstrated. These values are retained on first-principles grounds. The
verified target range from this campus Wi-Fi client is at most 20 Mbps per
paired session. **50 Mbps is not a verified capacity.**

The after 50-Mbps row uses the retry. Its first attempt failed before Ready.
The before high-rate attempt ran 18.402 seconds and the retry ran 11.194 seconds.
The configured 750 ms transport deadline and daemon binary stayed in service.
These short runs establish this connection's observed behavior. Sustained
50 Mbps and a depleted-CPU-credit endurance run need further measurement.
The single matched sample at each rate cannot establish a tuning speedup.

| Target Mbps | Before CPU, one-core % | After CPU, one-core % | Before CPU % per forwarded Mbps | After CPU % per forwarded Mbps |
| --- | --- | --- | --- | --- |
| 5 | 2.34 | 2.40 | 0.553 | 0.568 |
| 20 | 3.54 | 3.57 | 0.210 | 0.212 |
| 50 | 5.04 | 5.54 | 0.160 | 0.238 |

CPU comes from service cgroup `usage_usec` deltas over the full sampler window.
One-core CPU % is those core-seconds divided by probe elapsed time, times 100.
CPU % per Mbps is `100 * CPU core-seconds / received payload megabits`.
Sampling includes connection setup and the short idle tail, so these are
conservative process-cost estimates. They exclude kernel and other-service CPU.
Raw host CPU counters are included for inspection. The 50-Mbps samples include
failed-session behavior and should be excluded from sustained-capacity estimates.

## Tailscale peer relay

`peer-path-before.txt` records the original EC2 endpoint.
`peer-path-after.txt` contains redacted role/path and RTT summaries showing
`peer-relay 54.116.93.176:443:vni:3`. Mac pings were 147–150 ms and Windows
pings were 136–144 ms in the recorded after sample. Earlier discovery checks
reached 155 ms. The native relay's TCP 443 listener remained active throughout.

The matched download copied the existing 13,172,176-byte Windows kernel through
SFTP to the Mac. Both local copies have SHA-256
`f6f275f67c7ef44fc2df2d828658fb31eaffd41018ac5a0fd9440e08d9912c30`.
Windows received no test files. Each run used a fresh SSH connection.

| Path | Seconds | Payload Mbps | Relay CPU, one-core % | Relay CPU % per forwarded Mbps |
| --- | --- | --- | --- | --- |
| Windows → Mac via EC2 | 37.740 | 2.792 | 1.71 | 0.613 |
| Windows → Mac via Lightsail | 33.987 | 3.101 | 2.47 | 0.797 |
| Mac → Windows through Lightsail peer relay | 20.015 | 3.458 | 2.98 | 0.862 |

An earlier EC2 SFTP sample measured 3.120 Mbps. The upload used
a temporary .NET TCP listener on Windows's Tailscale address, port 52388, with
a RAM buffer. It received 8,650,752 bytes. It closed after the bounded test.
Its script used existing PowerShell 5.1 and .NET facilities.

SFTP includes SSH setup and application overhead. The upload uses a different
method, so its rate has a separate row. The before and after SFTP runs used
different EC2 and Lightsail CPUs. Throughput differences include WAN variation.
The initial 64-KiB stop-and-wait echo attempt stalled and exceeded the intended
short test window before cancellation. A larger-block retry timed out. Those
attempts were replaced by bounded SFTP runs and the one-way socket sink.

## Transfer, quota and safety

The conservative allocation for every attempt in this task is below 3 GB of
combined relay ingress and egress, including failed attempts and overhead.
The six planned native runs plus the high-rate retry model at most 1.25 GB
combined payload at full target rates for their entire configured durations.
The bounded Windows attempts add less than 0.4 GB before overhead. The task's
15-GB ceiling retained substantial headroom.

Provider metrics before testing totaled 5,308,520,812 bytes for the month.
The final `NetworkIn-after.json` and `NetworkOut-after.json` contain the
provider's available observations. Provider aggregation can lag the test.
The native daemon retains its 500,000,000,000-byte monthly combined guard.
Tailscale peer-relay bytes use the provider allowance alongside native traffic.
The application guard covers the native service's observed socket bytes.

The private SSD backup matched 572 source regular files, 20,481,599 bytes.
The stopped identity archive additionally matched all eight files, 20,367 bytes.
Archives, user-data, policy observations and node keys are retained privately.
The timestamped operational log is [CHANGES.md](CHANGES.md).
