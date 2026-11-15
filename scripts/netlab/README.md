# Hostile-network lab

This Linux/root harness runs the real `serve`, `view`, and `latch-relay`
binaries. It uses the software H.264 encoder/decoder, a 320 × 192 moving
test pattern at 30 fps (640 × 360 random noise in d1 and d2), synthetic quarter-second audio clicks played into
a private PulseAudio null sink, and one synthetic key press per second.
Input timing measures the decoded square changing on the viewer's clock.

## Topology

```text
                   emulated tailnet, routed IPv4, MTU 1280
 host (h) ───────────── campus router (c) ───────────── viewer (v)
 100.64.0.1             nftables / tc netem             100.64.0.2
     │                         │
     │                         ├─ CONNECT proxy :3128
     │                         ├─ ALPN inspector / TLS interceptor :8443
     │                         ├─ captive portal :8080 + local DNS
     │                         └─ DNS64 + tayga NAT64 (scenario m)
     │                         │
     └──────── internet (i) ───┘
               real latch-relay, relay.netlab.test, 93.184.216.34:443
```

Each scenario owns four network namespaces and four veth pairs. The
host/router/viewer links carry control TCP 47860 and media UDP. The host
and router each have a separate routed link to the internet namespace.
Every link has MTU 1280. **CI emulates the Tailscale path with routed
links.** Tailscale, DERP, NAT traversal, physical Wi-Fi, desktop capture,
GPU codecs and physical audio devices require separate testing.

No namespace has a route to an external network. The relay hostname maps
to the disposable namespace daemon. Private test roots preserve hostname
verification. The interception scenario presents a different private CA,
which the viewer never receives as a trust anchor. Scenarios m and m2 retain the
IPv4 control/tailnet exception and remove both endpoints' IPv4 default
routes. Each endpoint resolves a DNS64-only AAAA response and reaches
the relay through IPv6 and tayga. Scenario m uses `64:ff9b::/96`, and m2
uses the global-looking network-specific `2001:4860:64::/96`. The IPv4
relay address exists inside the lab and passes the production public-address
filter. Private TLS roots are applied after the product builds its relay
configuration, preserving that filter and future proxy settings.

## Run

On Linux, install the tools, build, and run as root. The build uses the
workspace's optimized development profile and four Cargo jobs.

```sh
sudo bash scripts/netlab/install.sh
bash scripts/netlab/build.sh
sudo python3 scripts/netlab/netlab.py --jobs 2
```

On macOS or Linux with Docker, run the same build and Python harness in a
privileged Ubuntu 24.04 container. Apple Silicon uses native linux/arm64.
The container has four CPUs and 6 GiB memory. Image tags and containers
created by the runner are removed on exit. Named Cargo registry and target
volumes remain as caches. Results are bind-mounted into the checkout.

```sh
scripts/netlab/run-in-docker.sh
NETLAB_VALIDATE=1 scripts/netlab/run-in-docker.sh
scripts/netlab/run-in-docker.sh --scenarios c,g,n --jobs 2
```

`NETLAB_VALIDATE=1` also runs workspace Clippy, rustfmt, ShellCheck, and the
Python assertion/parser checks. Local output goes under a unique
`target/netlab/run-*` directory. Linux output defaults to
`target/netlab`. CI uses the runner's temporary directory to keep results
outside the Rust build cache. Choose a fresh `--output PATH` for each invocation.

### Carrier throughput comparison

```sh
scripts/netlab/run-in-docker.sh --carrier-comparison
# Optional shorter selection, retaining the same topology and defaults.
scripts/netlab/run-in-docker.sh --carrier-comparison --paths 150 --rates 5,20
```

This scenario builds the release relay and bounded paired probe. Both probe
roles run in the viewer namespace and connect independently to the namespace
relay, matching the public probe's two outbound legs. The existing tinyproxy
handles the CONNECT variant for both roles. All namespace routes stay isolated
from external networks. The container retains the four-CPU limit.

Each path runs native TLS, WebSocket, and WebSocket through CONNECT at 5, 20,
and 50 Mbps, with 60 fps frames. Default runs last 20 seconds including five
seconds at one-quarter bitrate. `--warmup 0 --seconds 15` selects cold starts.
Netem adds half the requested RTT to each direction of the viewer/campus link
for 150 ms, 150 ms with independent 0.5% packet loss per direction, 300 ms,
and 20 ms. Loss draws are stochastic. Veth MTU is 1280 and segmentation/receive
offloads are disabled by the shared topology helper. The 20,000-packet queue
avoids incidental netem queue drops in the loss-free cases.

`summary.json` records whole-run payload Mbps and frame p50/p95/p99/max,
source drops, completion status, and approximate client/relay CPU as percentages
of one core. Client CPU combines host and viewer. CPU uses process ticks sampled
every 100 ms, including connection setup. Raw probe logs and per-run qdisc
statistics accompany the summary. Payload Mbps includes warm-up and final
drain. The ideal offered average before drain is 81.25% of the target with
the defaults. Fresh grants and processes retain the default transport deadlines
and queue/byte bounds. Only disposable-daemon admission rate limits are raised
to permit rapid comparisons after failures. A failed probe remains a result
with its exit code and error log.

`PAIRED_PROBE_CA` selects a disposable PEM trust store in the probe example.
TLS hostname verification and production public-address filtering still apply.
The cross-platform relay integration test
`websocket_matches_native_video_through_a_delayed_path` uses a bounded loopback
delay shim at 150 ms RTT, warms at 5 Mbps, then offers 20 Mbps at 60 fps. It
requires WebSocket delivery of at least 85% of native, at least 10 Mbps on each
carrier, frame p95 below 500 ms, maximum below 750 ms, and complete drain.

The workflow has a 15-minute timeout and runs two scenarios concurrently
inside the runner, with unique namespace names, interfaces, ports,
certificates, quota ledgers and audio sockets. Each PulseAudio daemon runs
in foreground user mode with its own root-owned runtime/state directory, so
it requires no login session or D-Bus service. A `pactl` preflight checks the
private null sink before starting the real viewer. The virtual audio output
uses the existing `LATCH_AUDIO_BUFFER=960` setting for a 20 ms device buffer.
Scenario c runs for 95 seconds to cover main's 60-second UDP reassessment dwell. The remaining
scenarios run for 16–40 seconds each. A warm two-worker run takes about
five minutes. Startup rejects leftover netlab namespaces. SIGINT and
SIGTERM trigger process and namespace cleanup. Each diagnostic has a
three-second timeout, and teardown continues after individual failures.

## Pass criteria

All streaming scenarios require control connection within **3 s**, a
complete sampler trace through the requested duration, **zero session
terminations**, at least **15 decoded fps over the final 6 s**, **no decoded
stall longer than 5 s**, at least **3 input-to-picture round trips in the
final 7 s**, each within **500 ms**, and at least **8 decoded/played audio
clicks in the final 7 s**. FPS is computed from decoded frame sequence
deltas and elapsed time. Freeze durations use individual decoded-frame
timestamps observed every 5 ms. A stall is a gap longer than 500 ms.
Input and audio must continue during recovery.
The headless example's exit status is checked in addition to these
assertions.

| ID | Impairment | Additional numeric criteria |
|---|---|---|
| a | Clean links | First frame ≤3 s, UDP throughout, zero relay SYNs from either endpoint |
| b | Drop tailnet UDP from startup | First frame ≤7 s, final 6 s on relay |
| c | Drop UDP at 6 s, restore at 18 s | First frame ≤7 s, relay by 13 s, UDP within 70 s of restoration, final 6 s on UDP |
| d1 | 17% bursty host-to-viewer UDP loss | Final 6 s ≥25 fps, ≤1 stall, longest freeze ≤1 s, delivered video ≥75% of requested 6 Mbps, stable UDP or relay path |
| d2 | Host-to-viewer UDP policed to 3 Mbps | Same quality criteria as d1, real policer drops >0 |
| e | Only outbound TCP 443, with tailnet control exception | First frame ≤7 s, final 6 s on relay |
| f | Drop relay TCP 443 from both endpoints while UDP is healthy | First frame ≤3 s, UDP throughout, zero relay SYNs reaching daemon |
| g | Start with UDP denied, restore at 12 s, black-hole relay TCP at 14 s | Relay observed during 12–14 s, UDP observed during 14–20 s, continued session |
| h | Silently drop UDP datagrams with UDP length >600 bytes | First frame ≤4 s, MTU-safe UDP recovered for final 6 s, common health criteria |
| i | Explicit tinyproxy CONNECT proxy, direct viewer internet blocked | First frame ≤7 s, final 6 s on relay |
| i2 | CONNECT proxy requires credentials absent from environment | Zero decoded frames, specific proxy-authentication message within 8 s |
| j | Reset TLS ClientHello unless every ALPN is h2 or http/1.1 | First frame ≤7 s, final 6 s on relay |
| k | Campus TLS interceptor with an untrusted private CA | Zero decoded frames, explicit certificate/trust error to viewer within 8 s |
| l | DNAT all viewer TCP 80/443 to a 302 sign-in redirect, local catch-all DNS | Zero decoded frames, explicit captive-portal/sign-in error to viewer within 8 s |
| l2 | Captive portal opens at 10 s | Sign-in notice within 8 s, first frame in 10–14 s, same session, final 6 s on relay |
| m | Both endpoints IPv6-only for internet, DNS64 and well-known NAT64 /96 | First frame ≤7 s, final 6 s on relay, media counters at both endpoints |
| m2 | Both endpoints through global-looking network-specific NAT64 /96 | Same criteria as m |
| n | Relay TCP silently drops IPv4 packets >600 bytes in either direction, TCP MTU probing disabled | First frame ≤4 s, final 6 s on relay, common health criteria |

d1 uses Gilbert-Elliott loss with good-to-bad probability 10.24%,
bad-to-good probability 50%, loss 100% in the bad state and 0% in the
good state. Its stationary mean is 17%, with a two-packet mean burst.
The qdisc is on campus `viewer` egress, where host-to-viewer media flows.
An explicit priomap keeps every other protocol in the clean band. All
veth TSO/GSO/GRO features are disabled and checked. The measured netem
drop fraction, `drops / (packets + drops)`, must be 15–18% over at least
1,000 real media packets. An invalid impairment is ERROR.

d2 uses a tc police action at 3 Mbps with an 8 KiB burst. The noise stream
requests 6 Mbps and must produce policer drops and a measured one-second
offered rate above 3 Mbps. Its filter/action statistics
are saved alongside qdisc statistics. UDP feedback remains clean in both
cases. Either path must meet every quality criterion, so relay fallback
is expected when UDP cannot sustain the requested picture.

Endpoint nftables counters measure received direct-media UDP and relay TCP
bytes once per second. The final media path must carry over 10 KiB in its
measurement window. c and g also require independently observed relay media
in the outage window. Scenarios with denied UDP require real dropped UDP
attempts. Cases a and f require zero viewer relay attempts, with f separately
preflighted from both endpoints.

For i, j, k, l and n, post-preflight evidence must prove that the viewer
attempted the relay and reached the middlebox. Evidence comes from proxy
access logs, DPI ClientHello logs, interceptor handshake logs, portal
connection logs and counted MSS-black-hole drops respectively. These guards
are required for both PASS and EXPECTED-FAILURE. Preflight bytes and log
entries are excluded. Scenario i additionally requires a successful product
CONNECT request. Native direct TLS attempts are redirected to the same
HTTP-only proxy for a logged rejection. Discovered proxy-only settings select
CONNECT first. Scenario i2 supplies no credentials while its independent preflight
uses disposable lab credentials to prove the proxy's tunnel is usable.

The security scenarios require connection within 3 s and a complete
sampler trace. Their rejection may terminate the session. They must expose
a specific security error through viewer events. Internal TLS logs or a
generic media timeout do not satisfy that assertion.

Scenario h deliberately checks recovery to usable UDP with smaller
datagrams. Existing native relay recovery alone cannot satisfy that MTU
probing requirement. Scenario n covers a TCP PMTU black hole after SYN/MSS
negotiation. Kernel TCP MTU probing is disabled in each namespace to make
the product's behavior reproducible. The relay retries two stalled TLS carriers using fresh connections with
1200-byte then 512-byte MSS. The 512-byte retry fits this path's 600-byte
IP-packet budget. The connection diagnostics retain the applied MSS and cause.

## Results and expected failures

`results.json` contains the source revision, wall runtime and every
scenario result. Each scenario also saves `result.json`, the viewer's
`events.jsonl`, daemon/host/viewer/audio/middlebox logs, command output,
firewall counters, and independent preflight results. JSON includes
connection (accepted control negotiation) and first-frame times, decoded fps over time, media paths and
switches, session terminations, input round trips, audio samples, errors,
and each assertion verdict. Private keys are deleted during cleanup. Artifact
staging copies only logs, JSON/JSONL results and Markdown summaries into a
runner-readable directory. Original file permissions are preserved, and
private keys and the daemon’s mode-0600 quota ledger are excluded even when
setup or cleanup fails. `summary.md` is copied into the GitHub job summary.

Before measurement, probes validate real TLS/ALPN, CONNECT tunneling,
interceptor certificate rejection, the portal redirect, and DNS64/NAT64
connectivity as applicable. Setup failures are always `ERROR`, including
for expected-failure cases. Test probes are excluded from relay SYN counts.

`expected-failures.json` records observed assertion IDs, the reason, and a
fix reference. The comparison requires the recorded failure signature and the recorded
error pattern where supplied.

d1 and d2 require the recorded requested-video-bitrate failure. Their
`related_failures` list names accompanying quality failures observed across
stochastic runs. FPS, audio and stall counts vary, and d2 can also remain on
UDP with delayed input. Every numeric assertion still runs and is
reported. A repaired bitrate with a different remaining failure is FAIL.
Control, sampler, termination, path-counter and middlebox failures always
remain outside this quality signature.

- A passing scenario with a failed assertion makes CI fail.
- A listed scenario must fail exactly its recorded assertions. Additional
  failures outside its recorded related quality failures make CI fail.
- A listed scenario satisfying every assertion is `UNEXPECTED-PASS` and
  makes CI fail. The feature PR enables it by deleting its entry from
  `expected-failures.json` after running the lab.

Pending feature branches are linked through GitHub comparison URLs until
their PRs are opened. Update those references to their PR URLs as they
become available. Product fixes belong to those PRs. The harness keeps
its assertions unchanged.

## Measured results

The current per-scenario table is generated into `summary.md` and the GitHub
job summary. It includes final decoded fps, stalls, longest freeze, delivered
and requested video bitrate, input RTT, played clicks, active path, measured
loss and policer drops. `results.json` retains every individual assertion,
kernel traffic sample and failure. The expected-failure entries record the
observed product gaps and their source revision.

The earlier combined d case measured about 4.4% effective loss and zero TBF
drops. d1 and d2 replace that case with separately verified impairments.

## Test hooks

The runtime measurement hooks are confined to examples. `serve` and `view`
share an example-only helper accepting `--relay-host`, `--relay-port`,
and `--relay-ca`. The helper lives in
`crates/stream/examples/support/relay.rs`. It first calls
`latch_stream::session::relay_config` with the requested endpoint, then
replaces only its TLS roots using `ClientConfig::with_tls_roots`. The
production public-address policy and transport settings survive this step.
Hostname verification remains enabled.

### Shared proxy configuration for i and i2

`latch_stream::session::relay_config(&RelaySettings)` attaches cached, asynchronous
`latch_core::netenv` discovery for the configured relay host. Product sessions and
measurement examples share this entry point. A discovered proxy is mapped to
`ProxyConfig`, with supplied credentials only, then re-evaluated on fresh relay
connections. The lab supplies `HTTPS_PROXY=http://10.77.2.1:3128` and
`NO_PROXY=100.64.0.1` to the viewer. The control connection keeps its direct
tailnet route. The private-root helper preserves both discovery and the TLS
verifier. The lab daemon serves `/netenv/probe` on its isolated port 80.

Scenario l2 removes portal redirection at 10 seconds, after the ordinary
first-picture timeout. It retains denied UDP, proving that a fresh relay grant
starts media through the original session without restarting the viewer.
The original scenarios' assertions and d1/d2/h expected-failure signatures remain.

`view --events PATH` uses the existing session, frame slot, media path
query, audio probe and synthetic input APIs. It samples every 250 ms,
observes frame/input changes every 5 ms, flushes JSONL events immediately,
and stops the actual session at the end. The ordinary measurement mode
continues through its existing entry point when `--events` is absent.

## Add a scenario

1. Add an ID, descriptive name and bounded duration to `SCENARIOS`.
2. Put namespace firewall/qdisc/middlebox setup in `Lab.configure`, with
   an independent `probe.py` preflight for new middleboxes. Use the
   existing process tracking so cleanup covers every child.
3. Add named numeric assertions in `evaluate` and document them above.
4. Run the Docker harness. A known product gap gets an expected-failure
   entry containing the exact failed assertion IDs, observed behavior,
   source revision and owning fix reference. Infrastructure failures
   require fixing the lab.
5. Run `python3 -m unittest discover -s scripts/netlab -p 'test_*.py' -v`
   and `shellcheck scripts/netlab/*.sh`.
