# Paced batches, 2026-11-09

Work in progress. Results below are sender isolation measurements, **not decoded
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
alone. Record final results and gates before treating this report as complete.

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

Decoded-viewer latency; requested-rate noise-pattern codec loopback; independent
feedback transport; dropping queued GOPs versus backpressure; mixed-version
live matrix; Linux GSO on a NIC; Windows USO on hardware. These limitations must
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
