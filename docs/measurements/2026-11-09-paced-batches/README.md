# Paced batches, 2026-11-09

Results below include sender isolation measurements, **not decoded
viewer latency or clean-NIC throughput claims**. No installed host or port 47850
was used. No dependency was added.

## Machine and method

Apple M3 Pro, Mac15,6, 12 CPUs, arm64, Darwin 25.6.0. Shared interactive machine;
initial load averages were 29.72/28.87/19.93. Other agents compiled concurrently.
Single three-second samples are noisy and cannot establish small differences.

`server::tests::paced_socket_measurement` is an ignored release test using the
production `paced_video`, packetizer, batch sender, controller and reassembler.
It sends synthetic encoded payloads at 60 fps, initially requesting twice the
FIFO capacity, with an initial 8x frame (larger than the FIFO). UDP goes through
real loopback sockets and the deterministic `Link` rate-limited 50 ms FIFO.
Propagation is half the named RTT; feedback is sampled every 50 ms in-process
and uses known queue delay, rather than measuring TCP report/ping scheduling.
Subsequent frames follow the controller's FEC-aware video allowance; recovery
requests obey the production keyframe gap. Ordinary parity is 10%.

Goodput counts payload in reference-valid frames completed at the reassembler,
including a two-second drain, divided by the three-second source window.
Queue age is encoder-output timestamp to sender completion including parity;
frame delay is output timestamp to reassembly completion. Neither includes a
real codec, capture or physical display. CPU is CLOCK_THREAD_CPUTIME_ID for
just the sender. The macOS syscall count instruments send calls in test builds
(no production instrumentation cost); batch retries can overcount if a partial
batch fails. Other platforms currently print zero and must not interpret that
as measured CPU/syscalls. Lost and skipped come from the production reassembler;
keyframes count sender completions. These are **not** a noise-pattern software
codec benchmark. The initial huge frame can lose before pacing activates.

## Exact commands

```sh
export PATH=$HOME/.cargo/bin:$HOME/.local/bin:$PATH TMPDIR=/tmp
cargo test --release -p latch-engine --lib paced_socket_measurement --no-run
sh docs/measurements/2026-11-09-paced-batches/run.sh
# Retain the just-built binary to compare while editing without Cargo rebuilds:
cp target/release/deps/latch_engine-921171a8f3a810a1 /tmp/pb-baseline
PB_TEST_BIN=/tmp/pb-baseline sh docs/measurements/2026-11-09-paced-batches/run.sh
```

`PB_SECONDS` changes source duration. `PB_CAP=0 PB_RTT=0 PB_REQUEST=100000`
(or 1000000) selects uncapped loopback. Baseline is origin/main 69539f0 with
measurement hooks only. Raw measurements: before.txt; quantum.txt is batching
alone. See the validation section for final local gates and remaining limits.

## Timer measurement

1000 `thread::sleep` calls per duration, measured with Instant, under shared
load. Exact source will be retained beside sleep.txt. Requested/actual p50,
p95, maximum in microseconds:

| Requested | p50 | p95 | max |
|---:|---:|---:|---:|
| 10 | 15.2 | 19.2 | 46.4 |
| 100 | 129.0 | 140.0 | 429.6 |
| 500 | 630.8 | 641.0 | 831.2 |
| 1000 | 1257.4 | 1269.0 | 1610.2 |

No final spin has been added: quantum CPU/goodput measurements must first
justify burning a core fraction. Windows timer behavior is unverified.

## Remaining verification

Decoded-viewer latency percentiles; clean high-rate codec throughput; independent
feedback transport in the sender-isolation harness; Linux GSO on a NIC; Windows
USO on hardware. These limitations must
remain explicit even when the functional suites pass.

Validation scheduling caveat: preliminary workspace and engine runs overlapped
several native VideoToolbox test processes on this shared Mac. Duplicate own
runs were cancelled rather than treating device contention as a product result;
final runs must be serial. Other worktrees also ran native tests concurrently.
No assertion was changed.

## Sender isolation results

Each cell is before → after. Single samples; shared-load differences are not
statistically established. CPU is total sender milliseconds in a 3 s source run.
Frame delay ends at reassembly, not the decoded viewer.

| FIFO Mbps / RTT ms (request Mbps) | Goodput Mbps | Send age p50/p95/max ms | Frame delay p50/p95 ms | Lost/skipped | Keyframes | Syscalls/s (Mac) | CPU ms |
|---|---|---|---|---|---|---|---|
| 5 / 20 (10) | 2.062 → 2.078 | 0.126/2.271/7.512 → 0.082/1.929/7.196 | 18.655/19.717 → 18.571/18.733 | 3/3 → 3/3 | 2 → 2 | 337.3 → 339.0 | 21.644 → 16.152 |
| 5 / 150 (10) | 1.003 → 0.906 | 0.120/0.259/8.919 → 0.083/1.781/8.463 | 79.589/80.196 → 79.482/82.803 | 6/5 → 6/5 | 3 → 3 | 239.0 → 228.7 | 20.988 → 13.684 |
| 20 / 20 (40) | 8.413 → 7.507 | 0.314/5.191/5.598 → 0.382/5.514/8.922 | 18.344/29.477 → 17.581/19.091 | 3/4 → 3/4 | 2 → 2 | 1263.3 → 1172.7 | 70.703 → 55.827 |
| 20 / 150 (40) | 6.126 → 5.391 | 0.239/4.763/6.873 → 0.176/4.903/10.704 | 81.613/82.231 → 80.498/81.630 | 6/5 → 6/5 | 3 → 3 | 1075.7 → 960.0 | 52.756 → 35.651 |
| 50 / 20 (100) | 25.856 → 26.577 | 0.719/5.307/10.872 → 0.628/5.954/16.696 | 21.027/21.794 → 20.952/24.350 | 3/4 → 3/4 | 2 → 2 | 3710.0 → 3798.7 | 135.100 → 102.244 |
| 50 / 150 (100) | 14.626 → 14.054 | 0.518/5.227/11.371 → 0.542/5.549/15.383 | 81.309/82.308 → 81.026/81.749 | 6/5 → 6/5 | 3 → 3 | 2522.0 → 2458.0 | 144.328 → 103.482 |
| 0 / 0 (100) | 81.084 → 82.406 | 1.746/2.752/30.493 → 1.122/2.857/16.867 | 3.099/53.417 → 2.073/6.806 | 2/1 → 0/0 | 2 → 1 | 9808.0 → 9798.0 | 288.909 → 216.950 |
| 0 / 0 (1000) | 71.851 → 175.194 | 5.517/174.438/306.310 → 7.605/114.541/218.412 | 5.210/50.123 → 6.472/78.230 | 43/22 → 46/21 | 13 → 13 | 29704.0 → 49900.0 | 808.335 → 1102.699 |

The high-rate uncapped case is **not clean**: the synthetic relay/socket path
loses frames at 1 Gbps requested. It cannot certify clean-path throughput or
isolate the sender from relay CPU. macOS still sends one syscall per datagram;
the change preserves Linux/Windows offload entry points, whose savings have
not been measured on hardware here.

## Backlog policy comparison

Exact commands (same final test binary):

```sh
PB_CAP=5000 PB_RTT=150 PB_START_PACED=1 /tmp/pb-final paced_socket_measurement --ignored --nocapture
PB_CAP=5000 PB_RTT=150 PB_START_PACED=1 PB_DROP=1 /tmp/pb-final paced_socket_measurement --ignored --nocapture
```

The test-only alternative drops queued output older than 1/60 s, abandons its
reference chain and requests a gated keyframe. Both start with a keyframe
larger than the 50 ms FIFO. Raw files: backlog-preserve.txt and backlog-drop.txt.

| Policy | Goodput Mbps | Age p50/p95/max ms | Delivered | Network lost/skipped | Not emitted | Keyframes | CPU ms |
|---|---:|---|---:|---|---:|---:|---:|
| Backpressure before capture | 1.006 | 0.066/0.893/51.018 | 169 | 6/5 | 0 | 3 | 14.675 |
| Drop stale output + recovery | 1.009 | 0.085/2.485/51.450 | 171 | 1/0 | 8 | 3 | 17.589 |

Choose backpressure: similar delivered goodput, lower p95 output age, no
extra reference discontinuity or recovery policy. Neither creates a keyframe
storm in this sample; the claim that dropping necessarily storms is **not**
established. Dropping changes loss accounting by removing frames before wire
number assignment; network loss alone is misleading. Both retain unavoidable
serialization and initial-keyframe loss through a too-small queue.

## Live protocol compatibility

Rebuilt exact tags v5.1.0 and v5.2.0 using their existing `serve`/`view`
examples; this branch's examples were rebuilt too. `python3 compat.py` ran
sweep 640×360@60, 20 Mbps requested, three seconds per session. All five
sessions exited zero: new→5.1, 5.1→new, new→5.2, 5.2→new, new→new.
Raw summaries and viewer/host logs are retained as compat*.txt. This checks
interoperability, not impaired-link quality. Servers were terminated by the
script in every case. The protocol number, datagram layouts and capability
negotiation remain unchanged; probes still require the existing opt-in.

Build commands (archives extracted into the named scratch directories):

```sh
git archive v5.1.0 | tar -x -C /tmp/pb-v51
git archive v5.2.0 | tar -x -C /tmp/pb-v52
CARGO_TARGET_DIR=/tmp/pb-compat-target cargo build --release --manifest-path /tmp/pb-v51/Cargo.toml -p latch-engine --example serve -p latch-stream --example view
# Copy the examples to /tmp/pb-compat-bins/{serve,view}-v51; repeat for v52.
cargo build --release -p latch-engine --example serve -p latch-stream --example view
# Copy to /tmp/pb-compat-bins/{serve,view}-new.
python3 docs/measurements/2026-11-09-paced-batches/compat.py
```

Existing `probe_transport` and `steady_transport` pass (probes-after.txt).
The former retains its clean-resume 8/28/78/128 ms assertions; the latter's
idle_motion remains 13 ms and both idle-drop cases retain zero lost/skipped.
These deterministic suites still model historical packet pacing/probe emission;
they verify controller regressions, **not** the new production probe scheduler's
full idle-resume distribution. A direct sender unit test separately verifies
probe byte accounting and cancellation under pressure. Real Windows/Linux
sender timing and production idle→motion timing across the RTT matrix remain
unverified.

## Real noise-pattern codec / decoded viewer

`noise_socket_measurement` in bottleneck.rs uses the shared real UDP relay,
now with a downstream 50 ms FIFO. 640×360@60, software H.264 test pattern,
hardware decoded viewer. Five seconds per case. The baseline is exact 69539f0
extracted into /tmp/pb-before-src with only this test and support/relay.rs
copied in. It was built in /tmp/pb-compat-target. Commands:

```sh
CARGO_TARGET_DIR=/tmp/pb-compat-target cargo test --release --manifest-path /tmp/pb-before-src/Cargo.toml -p latch-engine --test bottleneck --no-run
sh docs/measurements/2026-11-09-paced-batches/noise.sh /tmp/pb-compat-target/release/deps/bottleneck-edefa656cdb9f7cc
cargo test --release -p latch-engine --test bottleneck --no-run
sh docs/measurements/2026-11-09-paced-batches/noise.sh target/release/deps/bottleneck-edefa656cdb9f7cc
```

Each cell is before → after. Payload Mbps is the **last stats-window mean**,
not whole-run goodput. Frame intervals are sampled from published frame sequence
changes every 1 ms; gaps do not measure capture-to-decode delay. A polling gap
can conceal intermediate publications. Raw logs contain the complete last-window
viewer stats, losses/drops and host pacing queue maxima. No sender CPU/syscalls
are inferred from these runs.

| FIFO Mbps / RTT ms (request Mbps) | Published frames | Gap p50/p95 ms | Last-window payload Mbps | Keyframes delivered |
|---|---|---|---|---|
| 5 / 20 (10) | 274 → 286 | 16.508/22.180 → 16.559/20.951 | 0.723 → 1.496 | 1 → 1 |
| 5 / 150 (10) | 232 → 219 | 16.426/44.629 → 17.161/23.776 | 2.158 → 3.296 | 4 → 3 |
| 20 / 20 (40) | 294 → 297 | 16.438/26.629 → 16.523/18.964 | 9.081 → 9.049 | 1 → 2 |
| 20 / 150 (40) | 301 → 300 | 16.418/20.175 → 16.500/19.175 | 7.935 → 9.022 | 2 → 2 |
| 50 / 20 (100) | 294 → 255 | 16.445/19.508 → 17.146/35.782 | 9.092 → 6.003 | 2 → 2 |
| 50 / 150 (100) | 298 → 215 | 16.490/20.696 → 21.014/42.240 | 9.049 → 6.594 | 2 → 2 |
| 0 / 0 (100) | 251 → 207 | 17.079/39.748 → 23.547/37.458 | 5.403 → 4.799 | 2 → 2 |
| 0 / 0 (1000) | 299 → 300 | 16.471/19.168 → 16.523/19.970 | 9.094 → 9.038 | 2 → 2 |

The existing noise pattern underfills high-rate requests (about 9 Mbps payload
in uncapped loopback), and H.264 negotiation caps the 1 Gbps request at 800 Mbps.
Therefore this does not establish real-codec 100 Mbps or 1 Gbps sender capacity.
The synthetic encoded-payload harness deliberately avoids that encoder limit,
but its uncapped 1 Gbps relay is itself lossy. A clean high-rate sender/NIC
measurement remains outstanding. Decoded-viewer capture-to-display p50/p95
remain outstanding; existing stats lack a per-picture cross-stage identity.

## Decisions and limits

- Keep paced batches: the source proves pressure formerly bypassed GSO/USO,
  and the new path reuses those paths without a new wire format. On this Mac,
  constrained sender CPU is lower in most samples; goodput improvements are
  not established and clean-loopback variation is substantial. Mac syscall
  count is unchanged by design. Physical offload benefits are unverified.
- Keep pre-capture backpressure only while paced: the direct policy comparison
  has similar goodput, lower p95 send age and fewer sender-side discontinuities
  than dropping. Clean paths retain their existing overlap. It bounds encoded
  backlog, not hardware-internal capture/encode queues or keyframe wire time.
- Keep ordinary sleeps with absolute deadlines and no spin: the timer test
  proves oversleep, but these results do not justify extra spin CPU. A stricter
  high-rate pacing accuracy qualification remains outstanding.
- Send probes on the same bucket and account successful bytes, aborting on
  motion/pressure. The direct sender test verifies this. Existing fake-clock
  idle-resume tests pass but do not yet model this new probe emission schedule.
- Enforce the shared maximum and explicit errors: the boundary unit test accepts
  exactly the representable layout and rejects one byte more without emitting
  or consuming a frame number. Sender recovery also skips dependent output.

This work does **not** fulfill a claim that Latch cannot be a bottleneck. Missing
per-frame decoded-viewer delay instrumentation, an independently controlled
clean high-rate path, and real Linux/Windows execution prevent that conclusion.

## Local validation

- `cargo fmt --all -- --check`: pass.
- `cargo clippy --workspace --all-targets -- -D warnings`: pass.
- `cargo test --workspace`: pass (785 passing executions across result groups, including subprocess helper executions). A final repeat is also being retained.
- Five serial runs of all nine engine integration targets and `cargo test -p latch-engine --lib server::tests`: pass, with no assertion changes.
- Round five overlapped `CARGO_TARGET_DIR=/tmp/pb-hammer-target cargo build -p latch-engine --lib --target x86_64-apple-darwin`: both pass.
- Existing release probe/steady suites and new direct pacer/probe units: pass.
- No dependency changed, so a new local native audit was not required; PR CI native-audit passed on the first implementation head.

Exact repeated-suite command (run for rounds 1 through 5):

```sh
cargo test -p latch-engine --test bottleneck --test ceiling --test impaired --test lifecycle --test loopback --test probe_transport --test recovery --test steady_transport --test transport_scenarios
cargo test -p latch-engine --lib server::tests
```

All native encoder unit tests ran in the workspace gate; the five repetitions
cover every engine integration target plus sender/lifecycle unit tests, not five
repetitions of every native hardware encoder unit test. Logs are repeat-1.txt
through repeat-5.txt and hammer.txt. The hammer target compiled for 47.42 s
while round five ran. CI state belongs to the PR head, not these local results.


### Loaded-baseline flake found in the final workspace repeat

The first complete workspace gate and all five transport repetitions passed.
The next workspace repeat failed bottleneck's queue phase: the first unloaded
viewer stats had RTT 42 ms / variation 21 ms, versus stable 46 ms after the
configured downstream delay changed from 5 to 45 ms. The old check required
≥62 ms forever. Frames kept arriving at about 12 fps with no loss. This is a
contaminated initial RTT snapshot, not a delivery failure. Failure output is
workspace-loaded-baseline-failure.txt.

The test now compares both phases against the fixed 25 ms midpoint of its
known relay delays (queued ≥25 ms, recovered <25 ms), while retaining its
frame-progress, FPS, loss and recovery requirements and deadlines. This removes
the random first-window baseline from the oracle; it does not raise tolerances
or extend deadlines. The deterministic controller test still checks rate
adaptation against known RTT. Fresh reruns are recorded separately.

The subsequent workspace attempt passed bottleneck but hit impaired's existing
loaded-runner budget-collapse assertion: 58.8 fps average, FEC rebuilt 118
shards, zero post-FEC loss, but final budgets 2137/2563/3049 kbps (required
≥4800). The Nov 4 review58c validation records the same class of loaded-runner
failure. No impaired assertion or controller threshold was changed. Retained
as workspace-impaired-loaded-failure.txt; a standalone rerun and another full
workspace run follow. Passing runs do not make this observed instability vanish.

The revised bottleneck test passed five fresh full bottleneck-target runs
(bottleneck-fixed-1.txt through -5.txt). `impaired` remained unchanged except
for diagnostic RTT, local queue and application-limited fields in its log.
Its first standalone debug rerun also failed (retained), so it was compared
against exact main before considering controller changes.

Three alternating **release** pairs, exact main 69539f0 versus this branch,
all pass (impaired-main-*.txt / impaired-new-*.txt). These paired release runs
do not reproduce or resolve the debug loaded-runner failure. No causal
controller regression is established by them; debug scheduling instability
remains a limitation, and is not hidden by the passing runs.

Exact commands:

```sh
# Same instrumented impaired.rs copied into the main archive.
CARGO_TARGET_DIR=/tmp/pb-compat-target cargo test --release --manifest-path /tmp/pb-before-src/Cargo.toml -p latch-engine --test impaired --no-run
cargo test --release -p latch-engine --test impaired --no-run
# Copy each printed executable to /tmp/pb-impaired-before and -after.
/tmp/pb-impaired-before --nocapture
/tmp/pb-impaired-after --nocapture
# Repeat the pair three times.
cargo test -p latch-engine --test bottleneck  # five fresh runs after oracle fix
```

Final local gates after the test-oracle fix and diagnostic logging: fmt, workspace clippy with warnings denied, and workspace tests **pass** (785 passing executions across result groups, including subprocess helpers). See workspace-final.txt and clippy-final.txt. All failures above remain retained.
