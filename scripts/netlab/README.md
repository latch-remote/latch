# Hostile-network lab

This Linux/root harness runs the real `serve`, `view`, and `latch-relay`
binaries. It uses the software H.264 encoder/decoder, a 320 × 192 moving
test pattern at 30 fps (random noise in scenario d), synthetic quarter-second audio clicks played into
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
               real latch-relay, relay.netlab.test, 198.18.0.1:443
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
which the viewer never receives as a trust anchor. Scenario m retains the
IPv4 control/tailnet exception and removes the viewer's IPv4 default
route. Its internet connectivity uses IPv6, a DNS64-only AAAA response,
and tayga translating to the IPv4 relay.

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
scripts/netlab/run-in-docker.sh --scenarios cgn --jobs 2
```

`NETLAB_VALIDATE=1` also runs workspace Clippy, rustfmt, ShellCheck, and the
Python assertion/parser checks. Local output goes under a unique
`target/netlab/run-*` directory. Linux output defaults to
`target/netlab`. CI uses the runner's temporary directory to keep results
outside the Rust build cache. Choose a fresh `--output PATH` for each invocation.

The workflow has a 15-minute timeout and runs two scenarios concurrently
inside the runner, with unique namespace names, interfaces, ports,
certificates, quota ledgers and audio sockets. Each PulseAudio daemon runs
in foreground user mode with its own root-owned runtime/state directory, so
it requires no login session or D-Bus service. A `pactl` preflight checks the
private null sink before starting the real viewer. Scenario c runs for 85
seconds to cover main's 60-second UDP reassessment dwell. The remaining
scenarios run for 16–32 seconds each. A warm two-worker run takes about
four minutes. Scenario m already passes on native TLS through DNS64/NAT64
on main and is enabled.

## Pass criteria

All streaming scenarios require control connection within **3 s**, a
complete sampler trace through the requested duration, **zero session
terminations**, at least **15 decoded fps over the final 6 s**, **no decoded
stall longer than 5 s**, at least **3 input-to-picture round trips in the
final 7 s**, each within **1.5 s**, and at least **4 decoded/played audio
clicks in the final 7 s**. FPS is computed from decoded frame sequence
deltas and elapsed time. Input and audio must continue during recovery.
The headless example's exit status is checked in addition to these
assertions.

| ID | Impairment | Additional numeric criteria |
|---|---|---|
| a | Clean links | First frame ≤3 s, UDP throughout, zero relay SYNs from either endpoint |
| b | Drop tailnet UDP from startup | First frame ≤7 s, final 6 s on relay |
| c | Drop UDP at 6 s, restore at 18 s | First frame ≤7 s, relay by 13 s, UDP within 70 s of restoration, final 6 s on UDP |
| d | UDP netem 17% loss, 35% correlation, 25±10 ms delay with 25% correlation, 3 Mbps TBF | First frame ≤7 s, common health criteria on either path |
| e | Only outbound TCP 443, with tailnet control exception | First frame ≤7 s, final 6 s on relay |
| f | Drop viewer relay TCP 443 while UDP is healthy | First frame ≤3 s, UDP throughout, zero relay SYNs reaching daemon |
| g | Start with UDP denied, restore at 12 s, kill active relay at 14 s | Relay observed during 12–14 s, UDP observed during 14–20 s, continued session |
| h | Silently drop UDP datagrams with UDP length >600 bytes | First frame ≤4 s, MTU-safe UDP recovered for final 6 s, common health criteria |
| i | Explicit tinyproxy CONNECT proxy, direct viewer internet blocked | First frame ≤7 s, final 6 s on relay |
| j | Reset TLS ClientHello unless every ALPN is h2 or http/1.1 | First frame ≤7 s, final 6 s on relay |
| k | Campus TLS interceptor with an untrusted private CA | Zero decoded frames, explicit certificate/trust error to viewer within 8 s |
| l | DNAT all viewer TCP 80/443 to a 302 sign-in redirect, local catch-all DNS | Zero decoded frames, explicit captive-portal/sign-in error to viewer within 8 s |
| m | IPv6-only internet, DNS64 and tayga NAT64 to IPv4 relay | First frame ≤7 s, final 6 s on relay |
| n | Relay TCP silently drops IPv4 packets >600 bytes in either direction, TCP MTU probing disabled | First frame ≤4 s, final 6 s on relay, common health criteria |

The security scenarios require connection within 3 s and a complete
sampler trace. Their rejection may terminate the session. They must expose
a specific security error through viewer events. Internal TLS logs or a
generic media timeout do not satisfy that assertion.

Scenario h deliberately checks recovery to usable UDP with smaller
datagrams. Existing native relay recovery alone cannot satisfy that MTU
probing requirement. Scenario n covers a TCP PMTU black hole after SYN/MSS
negotiation. Kernel TCP MTU probing is disabled in each namespace to make
the product's behavior reproducible.

## Results and expected failures

`results.json` contains the source revision, wall runtime and every
scenario result. Each scenario also saves `result.json`, the viewer's
`events.jsonl`, daemon/host/viewer/audio/middlebox logs, command output,
firewall counters, and independent preflight results. JSON includes
connection (accepted control negotiation) and first-frame times, decoded fps over time, media paths and
switches, session terminations, input round trips, audio samples, errors,
and each assertion verdict. Private keys are deleted during cleanup. Artifact
upload explicitly selects only logs, JSON/JSONL results and Markdown summaries,
excluding private keys and the daemon’s mode-0600 quota ledger even when setup
or cleanup fails. `summary.md` is copied into the GitHub job summary.

Before measurement, probes validate real TLS/ALPN, CONNECT tunneling,
interceptor certificate rejection, the portal redirect, and DNS64/NAT64
connectivity as applicable. Setup failures are always `ERROR`, including
for expected-failure cases. Test probes are excluded from relay SYN counts.

`expected-failures.json` records observed assertion IDs, the reason, and a
fix reference. The comparison requires the exact failed assertion set and the recorded
error pattern where supplied.

- A passing scenario with a failed assertion makes CI fail.
- A listed scenario must fail exactly its recorded assertions. Additional
  failures make CI fail.
- A listed scenario satisfying every assertion is `UNEXPECTED-PASS` and
  makes CI fail. The feature PR enables it by deleting its entry from
  `expected-failures.json` after running the lab.

Pending feature branches are linked through GitHub comparison URLs until
their PRs are opened. Update those references to their PR URLs as they
become available. Product fixes belong to those PRs. The harness keeps
its assertions unchanged.

## Measured main result

The linux/arm64 Docker run on 2026-11-15 UTC used main `bf5da0d` plus the
example-only hooks in this PR. All 14 scenarios completed in **206.27 s**
with two workers. Software encode/decode and the null audio sink were real.
The full Linux workspace Clippy check, rustfmt, ShellCheck, seven Python
checks and the native-code audit also passed. macOS workspace Clippy and
rustfmt passed.

| Scenario | Result | Connect ms | First frame ms | Tail decoded fps | Switches |
|---|---|---:|---:|---:|---:|
| a clean UDP | PASS | 6 | 21 | 30.0 | 0 |
| b UDP denied at startup | PASS | 5 | 1141 | 29.9 | 1 |
| c UDP outage and restoration | PASS | 5 | 21 | 30.1 | 2 |
| d 17% burst loss, jitter, 3 Mbps UDP | PASS | 5 | 52 | 30.1 | 0 |
| e campus TCP 443 only | PASS | 6 | 1142 | 30.1 | 1 |
| f unreachable relay, healthy UDP | PASS | 6 | 21 | 30.0 | 0 |
| g relay dies during stream | PASS | 6 | 1106 | 29.9 | 2 |
| h UDP MTU black hole | EXPECTED-FAILURE | 5 | 1104 | 29.9 | 1 |
| i explicit CONNECT proxy only | EXPECTED-FAILURE | 7 | — | 0.0 | 0 |
| j HTTPS-only ALPN inspector | EXPECTED-FAILURE | 5 | — | 0.0 | 0 |
| k TLS interception | EXPECTED-FAILURE | 6 | — | 0.0 | 0 |
| l captive portal | EXPECTED-FAILURE | 6 | — | 0.0 | 0 |
| m IPv6-only internet with DNS64/NAT64 | PASS | 6 | 1148 | 30.1 | 1 |
| n relay TCP MSS black hole | EXPECTED-FAILURE | 6 | — | 0.0 | 0 |

Case d exercised both impairments with 693 netem drops and 35,095 TBF
overlimits. Case h sustained relay video, audio and input, while failing
its MTU-safe UDP recovery assertion. Cases i, j and n decoded zero frames
and reached the eight-second first-picture watchdog. Cases k and l also
decoded zero frames and reached that watchdog, while failing the required
specific certificate/portal diagnosis. Independent preflights verified
certificate rejection and the HTTP 302 sign-in redirect. Case m passed
through DNS64 and tayga NAT64, so it is enabled in CI.

## Test hooks

The runtime measurement hooks are confined to examples. `serve` and `view`
share an example-only helper accepting `--relay-host`, `--relay-port`,
and `--relay-ca`. It calls the existing `ClientConfig::with_roots` API.
Production root selection and public-address enforcement stay in
`ClientConfig::production`. No new environment-variable bypass, feature
flag, dependency, or trust-all verifier enters shipped application code.

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
