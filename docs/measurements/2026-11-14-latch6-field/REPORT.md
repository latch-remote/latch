# Latch 6.0.0 field validation

Measurements began on 2026-11-14 UTC. The tested branch was `codex/latch-6-network`, based on commit `103e72c51fa8e50bd8e8738189e29d95b06e8107` with uncommitted implementation included. The two-minute controller follow-up maintained useful bitrate. The ten-minute run completed with one relay recovery. Subsequent scoped outage, natural-routing, and reverse-direction runs completed. Final native checks cover a later endpoint revision, identified separately below.

## Findings and reproductions

### TCP video budget collapses during a real stream

Run the synthetic VideoToolbox host with `LATCH_TEST_PATTERN=noise LATCH_TEST_VIDEOTOOLBOX=1 LATCH_TEST_AUDIO=clicks`, then run `run-viewer.ps1 -Label diagnostic-auto-noise-1080-60 -Fps 60 -Seconds 95 -Automatic -Audio`. Deny only the field forwarder's UDP after eight seconds for 58 seconds. The viewer requests 1920×1080 at 60 fps and 20,000 kbps.

The public relay remained active for approximately 85 seconds, with two actual TCP connections to `54.116.93.176:443`. The requested bitrate fell to an applied 188 kbps and a 1,000 kbps wire budget. Repeated decode-queue resets occurred. Relay-phase decoded-frame counts give 48.66 fps and renderer submissions give 47.63 fps. The median cached stats field gives 52.61 decoded fps. The parent identified decoder `Report.queue_ms` being used to reduce the network budget. The refreshed controller passed the follow-up below. [Viewer metrics](/Users/daniel/Desktop/T3_Projects/latch/docs/measurements/2026-11-14-latch6-field/diagnostic-auto-noise-1080-60.err) · [Viewer events](/Users/daniel/Desktop/T3_Projects/latch/docs/measurements/2026-11-14-latch6-field/diagnostic-auto-noise-1080-60.out) · [Host metrics](/Users/daniel/Desktop/T3_Projects/latch/docs/measurements/2026-11-14-latch6-field/mac-diagnostic-serve.err)

### Reverse-direction stream terminates during synthetic input

Start `run-host.ps1` on Windows, then run the Mac command below.

```sh
RUST_LOG=latch_stream=warn target/debug/examples/view 100.106.23.40 \
  --port 47968 --width 1280 --height 720 --fps 60 --kbps 10000 \
  --seconds 12 --keys 10 --audio --mute
```

The connection took 682.7 ms. Two of ten synthetic presses produced decoded picture changes in 179.27–180.55 ms. Eight presses timed out after the host ended the stream with code 2 and `H.264 encoder setting: frame exceeds eight bitrate intervals at the allowed QP`. The measurement example exited zero after receiving that termination, so its exit code alone cannot establish a successful stream. [Input result](/Users/daniel/Desktop/T3_Projects/latch/docs/measurements/2026-11-14-latch6-field/windows-host-mac-input.json) · [Failure events](/Users/daniel/Desktop/T3_Projects/latch/docs/measurements/2026-11-14-latch6-field/windows-host-mac-input.err)

### Earlier public relay attempts expired

An earlier 1080p60 automatic run denied fixture UDP from 10:40:13 through 10:40:48 UTC. The viewer switched to the relay at 10:40:14.996. The host became active at 10:40:15.801 and closed at 10:40:39.988 with `TimedOut` and `relay deadline expired`. Two subsequent host attempts closed after 0.880 and 0.778 seconds. The viewer reported `UnexpectedEof` and `relay socket closed` at 10:40:39.359. The deployment agent had bulk probes active during that run.

After the parent paused bulk probes, the later 95-second run completed without a transport close. It still exhibited the bitrate collapse above. The earlier binary supplied a generic timeout message. Frame-byte size and oldest-ACK age were unavailable in those logs. [Earlier host events](/Users/daniel/Desktop/T3_Projects/latch/docs/measurements/2026-11-14-latch6-field/mac-noise-serve.err) · [Earlier viewer events](/Users/daniel/Desktop/T3_Projects/latch/docs/measurements/2026-11-14-latch6-field/auto-noise-1080-60.out)

## Source attribution and build handling

The first snapshot contains 920 Git-tracked and nonignored files totaling 10,591,306 bytes. Its manifest SHA-256 is `95a30ad914b87e45177d8d0a1a26b6b4f26fd1ef64874266143d0445aa76f692`. The second snapshot contains 928 files. Its manifest SHA-256 is `2a79c5950885ac4e657771c5f993b0f8dcfa864c1bb8b2b96b4f1c8ea329676b`. Each archive was transferred using SCP and extracted into a separate scratch directory. Git metadata, ignored build outputs, and account credentials were excluded. [Initial manifest](/Users/daniel/Desktop/T3_Projects/latch/docs/measurements/2026-11-14-latch6-field/initial-source-manifest.json) · [Second manifest](/Users/daniel/Desktop/T3_Projects/latch/docs/measurements/2026-11-14-latch6-field/source-manifest.json)

Subsequent parent-authorized updates have per-file hashes. The effective manifest tracks the cumulative updates. The completed-field manifest is preserved separately. All 928 files matched on Windows immediately before the final endpoint refresh. The preserved frozen Mac source also matched all 928 completed-field hashes. Its manifest SHA-256 is `e5ff62ac903c33ffcc90b85d5e7acaef31b5216ce7755153b058eca7c5c08618`. The resolver source at that point was `537d2dd2b3d03dbfe31afe3ea0bb2b10d745112381b7603ed73257e3e87cc5c0`. The diagnostic stream used the four source updates listed in `diagnostic-refresh.json`. [Effective identity](/Users/daniel/Desktop/T3_Projects/latch/docs/measurements/2026-11-14-latch6-field/effective-source-identity.json) · [Effective file manifest](/Users/daniel/Desktop/T3_Projects/latch/docs/measurements/2026-11-14-latch6-field/effective-source-manifest.json) · [Windows verification](/Users/daniel/Desktop/T3_Projects/latch/docs/measurements/2026-11-14-latch6-field/windows-manifest-verification.log) · [Diagnostic source updates](/Users/daniel/Desktop/T3_Projects/latch/docs/measurements/2026-11-14-latch6-field/diagnostic-refresh.json)

Early tar update entries used modification time zero. Cargo reused a stale dependency variant during a later build. Those attempts were invalidated. The refresh scripts now set current modification times. The isolated Windows cache's Latch packages were cleaned, sources were touched, and selected checks were rerun with actual recompilation. The rebuilt diagnostic viewer was checked for its new timeout string. Its Mac host binary hash was recorded. [Refresh helper](/Users/daniel/Desktop/T3_Projects/latch/docs/measurements/2026-11-14-latch6-field/refresh.py) · [Mac diagnostic build](/Users/daniel/Desktop/T3_Projects/latch/docs/measurements/2026-11-14-latch6-field/mac-diagnostic-build.log) · [Mac diagnostic binary](/Users/daniel/Desktop/T3_Projects/latch/docs/measurements/2026-11-14-latch6-field/mac-diagnostic-binary.json)

All field binaries used the workspace's optimized development profile. The installed Windows Rust 1.98.0 stable toolchain was sufficient.

## Native Windows checks

Windows reported NT 10.0.26300, an AMD Radeon RX 6600 with driver 32.0.21045.16010, and a Virtual Display Driver. Tests ran in `C:\Users\Daniel\dev\latch6-field-final-20261114`, with a dedicated target directory under the earlier isolated scratch tree.

The following completed pass preceded the final End/control refresh.

| Command | Result |
| --- | --- |
| `cargo test --locked -p latch-core -p latch-proto -p latch-relay --all-targets` | 200 passed, 3 ignored |
| `cargo test --locked -p latch-engine -p latch-stream -p latch-client --lib` | 378 passed, 33 ignored |
| `cargo test --locked -p latch-engine --test lifecycle -- --test-threads=1` | 6 passed |
| `cargo test --locked -p latch-engine --test relay_runtime -- --test-threads=1` | 8 passed in 121.20 seconds |
| Native `serve`, `pipeline_bench`, `present`, `view`, and `pads` builds | Passed |
| `cargo check --locked --workspace --all-targets` | Passed |

The native runtime pass cases covered real TLS blackhole recovery, ordered input, finite lifetime and byte-grant renewals, resource rejection, monthly quotas, stale offers, and cancellation. The earlier failed audio/microphone case attempted desktop enumeration in SSH session 0 because its helper omitted the synthetic pattern environment variable. An isolated invocation with `LATCH_TEST_PATTERN=still` passed in 0.65 seconds. The canonical helper was refreshed, recompiled, and passed its exact isolated test in 0.75 seconds with the pattern variable absent from the invocation environment. [Native command results](/Users/daniel/Desktop/T3_Projects/latch/docs/measurements/2026-11-14-latch6-field/final2-native-results.jsonl) · [Foundation output](/Users/daniel/Desktop/T3_Projects/latch/docs/measurements/2026-11-14-latch6-field/final2-foundation.out) · [Platform output](/Users/daniel/Desktop/T3_Projects/latch/docs/measurements/2026-11-14-latch6-field/final2-platform-libs.out) · [Runtime output](/Users/daniel/Desktop/T3_Projects/latch/docs/measurements/2026-11-14-latch6-field/final2-relay-runtime.out)

### Windows networking regressions

The original relay transport suite failed 11 of 12 tests with Windows error 10022. The socket fixture reproduced `connect` returning 10035, `take_error` returning no error, and `peer_addr` returning the destination while first reads and writes returned 10057. `set_nodelay(true)` then returned 10022. [Socket reproduction](/Users/daniel/Desktop/T3_Projects/latch/docs/measurements/2026-11-14-latch6-field/socket-repro.rs) · [Socket output](/Users/daniel/Desktop/T3_Projects/latch/docs/measurements/2026-11-14-latch6-field/windows-socket-repro.log)

Real TCP readiness removed error 10022. A dual-stack localhost endpoint still spent 2,024.6–2,048.4 ms refusing IPv6 before connecting to IPv4 in 0.42–0.48 ms. The single-address implementation passed all 12 tests sequentially in 49.82 seconds and failed default-parallel runs. [Connect timing](/Users/daniel/Desktop/T3_Projects/latch/docs/measurements/2026-11-14-latch6-field/windows-connect-timing.log) · [DNS timing](/Users/daniel/Desktop/T3_Projects/latch/docs/measurements/2026-11-14-latch6-field/windows-connect-dns-timing.log) · [Sequential result](/Users/daniel/Desktop/T3_Projects/latch/docs/measurements/2026-11-14-latch6-field/relay-final-sequential.out)

The parent's address-racing implementation passed all 12 tests under default parallel execution in 2.64 and 2.67 seconds. A clean-cache foundation run also passed all 12. Cancellation, public-root rejection, attachment replay, wrong-key rejection, bounded queues, and the ACK-window regression were included. [Parallel run one](/Users/daniel/Desktop/T3_Projects/latch/docs/measurements/2026-11-14-latch6-field/relay-diagnostic-parallel-1.out) · [Parallel run two](/Users/daniel/Desktop/T3_Projects/latch/docs/measurements/2026-11-14-latch6-field/relay-diagnostic-parallel-2.out)

The adaptive ACK regression passed native Windows, including learned delayed ACK tolerance and permanent ACK-blackhole termination. Before the resolver fix, the adaptive ACK suite passed all 13 sequential tests in 11.51 seconds while two default-parallel invocations each failed two of 13 tests with resolver queue admission or Ready timeout errors. With the parent's bounded admission wait, expired-request skipping, and one-second successful-result cache, all 13 transport tests passed twice in 2.67 and 2.65 seconds. The resolver client file SHA-256 was `537d2dd2b3d03dbfe31afe3ea0bb2b10d745112381b7603ed73257e3e87cc5c0`. The later End/control refresh includes a further client change. [Resolver refresh](/Users/daniel/Desktop/T3_Projects/latch/docs/measurements/2026-11-14-latch6-field/resolver-refresh.json) · [Parallel one](/Users/daniel/Desktop/T3_Projects/latch/docs/measurements/2026-11-14-latch6-field/resolver-parallel-1.out) · [Parallel two](/Users/daniel/Desktop/T3_Projects/latch/docs/measurements/2026-11-14-latch6-field/resolver-parallel-2.out)

Independent OS resolution timing for localhost reported zero errors. Twenty serial lookups averaged 0.411 ms with p95/max 4.800 ms. Sixteen workers performing 320 lookups averaged 0.796 ms with p95 1.003 ms and maximum 1.151 ms. This attributes the earlier parallel test contention to the application resolver path under that workload. [Timing fixture](/Users/daniel/Desktop/T3_Projects/latch/docs/measurements/2026-11-14-latch6-field/dns-load.rs) · [Native timing](/Users/daniel/Desktop/T3_Projects/latch/docs/measurements/2026-11-14-latch6-field/windows-dns-load.log)

## Campus-to-Korea stream measurements

The Mac used Tailscale address `100.111.100.57`, and Windows used `100.106.23.40`. Tailscale reported the existing peer relay at `3.37.166.202:443` for the ordinary peer path. The public TLS endpoint resolved to `54.116.93.176`. Standard WebPKI hostname verification succeeded with TLS 1.3, ALPN `latch-relay/1`, and a Let's Encrypt YE2 certificate. A curl probe without the required ALPN was inconclusive. [TLS verification](/Users/daniel/Desktop/T3_Projects/latch/docs/measurements/2026-11-14-latch6-field/public-tls.json)

The hardware-encoded direction used a Mac synthetic IOSurface source, VideoToolbox H.264 encoding, and Windows Media Foundation DXVA decoding with the application's DX12 renderer. An unsigned native Mac listener completed handshakes but did not receive accepted connections. A field-only Python forwarder restricted to the Windows peer carried tailnet TCP/UDP to the loopback-bound synthetic host. Public relay media connected directly from both endpoints to AWS. The additional local forwarding hop is part of the UDP measurements. [Forwarder](/Users/daniel/Desktop/T3_Projects/latch/docs/measurements/2026-11-14-latch6-field/tailnet-forward.py) · [Viewer fixture generator](/Users/daniel/Desktop/T3_Projects/latch/docs/measurements/2026-11-14-latch6-field/make-viewer.py)

Values below exclude the first five seconds and derive decoded/render rates from frame counters over sampled elapsed time. Video Mbps and RTT are per-sample medians. The adaptive bitrate and relay load differed between runs.

| Host → viewer | Requested mode | Decoded fps | Renderer submissions/s | Video Mbps | RTT ms |
| --- | --- | ---: | ---: | ---: | ---: |
| Mac hardware → Windows hardware | 1080p60 UDP, noise, 45 s | 50.39 | 49.51 | 4.84 | 157 |
| Mac hardware → Windows hardware | 1080p60 automatic, relay phase, diagnostic 95 s | 48.66 | 47.63 | 2.69 | 185 |
| Mac hardware → Windows hardware | 720p120 UDP, noise, 35 s | 96.43 | 94.23 | 11.98 | 148 |
| Mac hardware → Windows hardware | 720p120 automatic, startup UDP denial, 50 s | 105.63 | 102.81 | 0.91 | 183 |
| Windows software → Mac hardware | 720p60 UDP, sweep, 35 s | 49.06 | 48.70 | 0.42 | 152 |

The 1080p60 diagnostic relay phase had median decode time 2.49 ms, median viewer queue time 0.20 ms, and up to 26 dropped frames in a reported stats interval. Its decode-to-render preparation times were p50 0.205 ms, p95 0.367 ms, and p99 5.258 ms. Renderer counters record CPU rendering preparation. Physical scanout and capture-to-present frame age were unmeasured. The source protocol supplied stage durations without a cross-machine capture timestamp. [Parsed measurements](/Users/daniel/Desktop/T3_Projects/latch/docs/measurements/2026-11-14-latch6-field/stream-summary.json) · [Raw measurement summaries](/Users/daniel/Desktop/T3_Projects/latch/docs/measurements/2026-11-14-latch6-field/stream-summary.txt)

Two Windows sockets for the same viewer process connected from LAN address `192.168.55.57` to `54.116.93.176:443`, while control remained connected from `100.106.23.40` to `100.111.100.57:47964`. This establishes direct public TCP routing. [Socket observations](/Users/daniel/Desktop/T3_Projects/latch/docs/measurements/2026-11-14-latch6-field/diagnostic-auto-noise-1080-60.sockets.jsonl)

## Controller follow-up

The controller refresh changed `engine/server.rs`, `engine/congestion.rs`, `stream/session.rs`, the runtime fixture, and the local relay daemon source. Per-file hashes are recorded. The public daemon remained on its existing deployment during the 120-second and 600-second controller measurements. Afterward the deployment owner activated daemon SHA-256 `0b2b3f500cf5c9a42b49fd361dc443be309b9f92b9b7049c99e41781302c02a7`, preserving its ledger. The scoped outage, natural-routing, and final reverse runs used that deployment. [Controller update manifest](/Users/daniel/Desktop/T3_Projects/latch/docs/measurements/2026-11-14-latch6-field/controller-refresh.json)

The first short attempt was invalidated when the field forwarder's original 30-minute lifetime expired at 11:07:40 UTC, closing the private control connection. A fresh bounded forwarder was started before repeating the test and was stopped after field work.

The fresh 120-second 1080p60 automatic test completed with exit zero. Its cold Connected event arrived in 0.868 seconds. Fixture UDP was denied at 11:09:11 UTC and the viewer selected public relay at 11:09:12.807, approximately 1.81 seconds later. Impairment timestamps have one-second precision. After fixture-only UDP denial, public TCP carried a median 13.262 Mbps of encoded video while the wire budget remained 20,000 kbps and the applied video target remained 15,189 kbps. Relay-phase count-derived delivery was 50.83 decoded fps and 49.83 renderer submissions/s. Median RTT was 180 ms. Reported frame loss and drops stayed zero, and the maximum sampled viewer queue was 1.50 ms. The run produced 6,053 decoded frames and 5,931 renderer submissions. [Viewer metrics](/Users/daniel/Desktop/T3_Projects/latch/docs/measurements/2026-11-14-latch6-field/controller2-auto-noise-1080-60.err) · [Viewer events](/Users/daniel/Desktop/T3_Projects/latch/docs/measurements/2026-11-14-latch6-field/controller2-auto-noise-1080-60.out) · [Host controller decisions](/Users/daniel/Desktop/T3_Projects/latch/docs/measurements/2026-11-14-latch6-field/mac-controller-serve.err)

A separate hardware-decoded headless run on the same controller build kept fixture UDP denied from startup. All ten synthetic input presses produced decoded picture changes. Key-to-picture latency was 280.56 ms minimum, 282.67 ms median, and 339.43 ms p95/maximum. Decoded delivery was 52.06 fps. Synthetic audio reached the WASAPI endpoint during the run. [Input measurements](/Users/daniel/Desktop/T3_Projects/latch/docs/measurements/2026-11-14-latch6-field/controller-input.json) · [Input and audio events](/Users/daniel/Desktop/T3_Projects/latch/docs/measurements/2026-11-14-latch6-field/controller-input.err)

The 600-second run used the same controller binaries, 1080p60, 20 Mbps requested, synthetic noise and audio. It decoded 30,789 frames and exited zero. Relay-phase count-derived delivery was 51.80 fps with median video 13.362 Mbps and RTT 186 ms. The final wire budget and applied video target recovered to 20,000 and 15,189 kbps. This run preceded the adaptive ACK endpoint update. [Long-run metrics](/Users/daniel/Desktop/T3_Projects/latch/docs/measurements/2026-11-14-latch6-field/controller-long-noise-1080-60.err) · [Events](/Users/daniel/Desktop/T3_Projects/latch/docs/measurements/2026-11-14-latch6-field/controller-long-noise-1080-60.out)

At approximately 548 seconds the host closed with `relay Video: video acknowledgment deadline expired`. Immediately preceding telemetry recorded own-video ACK RTT 725.24 ms, host RTT 809.68 ms, sender queue 0.011 ms, sent 13,276 kbps, received 3,854 kbps, and decoder queue zero. A fresh relay attempt became active 4.96 seconds later while the session/control connection remained alive. The run ended with 41 audio underruns and four reported lost audio packets. [Host events](/Users/daniel/Desktop/T3_Projects/latch/docs/measurements/2026-11-14-latch6-field/mac-controller-serve.err)

The original measurement fixture retained at most 16,384 render timings. Its renderer count saturated at 330.762 seconds, so later renderer-rate samples are censored. The uncensored renderer rate was 49.87 submissions/s. The 30,789 decoded-frame count remained usable. Later fixtures drain render timings into their own history. Physical scanout and end-to-end frame age remain unmeasured. [Parser](/Users/daniel/Desktop/T3_Projects/latch/docs/measurements/2026-11-14-latch6-field/summarize.py) · [Fixture generator](/Users/daniel/Desktop/T3_Projects/latch/docs/measurements/2026-11-14-latch6-field/make-viewer.py)

## Audio, input, and hardware availability

Synthetic click audio reached the Windows WASAPI endpoint at 48 kHz stereo with a 10 ms device period. The selected endpoint was Steam Streaming Speakers. The diagnostic run ended with nine reported underruns and two reported lost audio packets. Acoustic output and physical microphone loopback were unmeasured. The native relay PCM-sink test exercised audio and microphone transport with synthetic PCM. [Audio events and queue metrics](/Users/daniel/Desktop/T3_Projects/latch/docs/measurements/2026-11-14-latch6-field/diagnostic-auto-noise-1080-60.err)

The Windows gamepad utility ran for three seconds and reported no connected device. Physical controller buttons and rumble were unverified. Native runtime tests exercised synthetic gamepad state and feedback. The earlier reverse-direction failure is preserved above. Final reverse-direction results after the bounded software resolution fix appear below.

## Reproduction commands

The synthetic hardware host used the following environment and arguments, with `serve` built from the identified frozen Mac source. `tailnet-forward.py` supplied the restricted tailnet-to-loopback path.

```sh
LATCH_TEST_PATTERN=noise LATCH_TEST_VIDEOTOOLBOX=1 LATCH_TEST_AUDIO=clicks \
  RUST_LOG=latch_engine=debug \
  target/debug/examples/serve --bind 127.0.0.1 --port 47965 --telemetry
```

The Windows native renderer was generated from the existing presentation example with `make-viewer.py`, then built in its isolated package. The scheduled interactive-session launch invoked the following PowerShell arguments. Omitting `-Automatic` selects the UDP-only fixture branch. UDP impairment came from the `deny-udp` marker consumed by the private forwarder. No global network setting changed.

```powershell
powershell.exe -NoProfile -File run-viewer.ps1 -Label controller2-auto-noise-1080-60 -Fps 60 -Seconds 120 -Automatic -Audio
```

The 600-second run changed the label and `-Seconds 600`. Final Windows native commands and their exact arguments are retained in `native-checks.ps1` and `final3-native-results.jsonl`. `windows.py` UTF-16LE/Base64-encodes PowerShell for the existing SSH alias. [Viewer generator](/Users/daniel/Desktop/T3_Projects/latch/docs/measurements/2026-11-14-latch6-field/make-viewer.py) · [Renderer launch](/Users/daniel/Desktop/T3_Projects/latch/docs/measurements/2026-11-14-latch6-field/run-viewer.ps1) · [Native commands](/Users/daniel/Desktop/T3_Projects/latch/docs/measurements/2026-11-14-latch6-field/native-checks.ps1) · [SSH wrapper](/Users/daniel/Desktop/T3_Projects/latch/docs/measurements/2026-11-14-latch6-field/windows.py)

## Final field follow-ups

### Scoped relay outage and reattachment

`outage-driver.py` ran a 90-second 1080p60 automatic noise stream with synthetic audio. The deny rule matched only the scratch viewer executable and public TCP 443. Private UDP denial remained confined to the field forwarder. The shared AWS service stayed running. [Driver](/Users/daniel/Desktop/T3_Projects/latch/docs/measurements/2026-11-14-latch6-field/outage-driver.py) · [Deny rule](/Users/daniel/Desktop/T3_Projects/latch/docs/measurements/2026-11-14-latch6-field/deny-relay.ps1) · [Impairment times](/Users/daniel/Desktop/T3_Projects/latch/docs/measurements/2026-11-14-latch6-field/impairment-events.jsonl)

Public attempt 1 became active at 11:40:45.585 UTC. The deliberate TCP denial produced Priority `ConnectionReset` error 10054 at 11:40:56.656, and delivery resumed over permitted UDP. After restoring public TCP and denying fixture UDP again, public attempt 2 became active at 11:41:12.583. The viewer decoded 4,269 frames, submitted 4,185 for rendering, exited zero, and received no `Terminated` event. [Viewer events](/Users/daniel/Desktop/T3_Projects/latch/docs/measurements/2026-11-14-latch6-field/final-outage2-noise-1080-60.out) · [Metrics](/Users/daniel/Desktop/T3_Projects/latch/docs/measurements/2026-11-14-latch6-field/final-outage2-noise-1080-60.err) · [Rule timing](/Users/daniel/Desktop/T3_Projects/latch/docs/measurements/2026-11-14-latch6-field/relay-denial-events.jsonl)

A further close occurred after fixture UDP was restored at 11:41:39. The viewer reported `UnexpectedEof relay Video: relay socket closed` at 11:41:48.749, followed by the host's `TimedOut relay Video: video acknowledgment deadline expired` at 11:41:49.565. UDP continued. This occurred on the adaptive ACK revision and remains a recorded residual transport failure. Exact oldest-ACK age and frame bytes were absent from that close log. The later endpoint polling/control changes received native checks only. The earlier 75-second outage attempt had insufficient observation time after reattachment and is retained as historical evidence. [Host close](/Users/daniel/Desktop/T3_Projects/latch/docs/measurements/2026-11-14-latch6-field/mac-final-serve.err) · [Viewer close](/Users/daniel/Desktop/T3_Projects/latch/docs/measurements/2026-11-14-latch6-field/final-outage2-noise-1080-60.out)

### Natural campus routing

The 60-second automatic noise run applied no impairment. It stayed on UDP, retained a 20,000 kbps wire budget, decoded 3,153 frames and submitted 3,076 for rendering. The final stats reported zero frame loss and drops. This observed a healthy ordinary path. [Metrics](/Users/daniel/Desktop/T3_Projects/latch/docs/measurements/2026-11-14-latch6-field/final-natural-noise-1080-60.err) · [Sockets](/Users/daniel/Desktop/T3_Projects/latch/docs/measurements/2026-11-14-latch6-field/final-natural-noise-1080-60.sockets.jsonl)

### Windows software host to Mac hardware viewer

After refreshing the typed `FrameTooLarge` codec result and bounded software resize recovery, both 720p60 synthetic input runs completed without termination. The request was 10,000 kbps, and the low-complexity sweep pattern consumed substantially less. The public-relay run denied only the scratch host's UDP port 47968. [Software source refresh](/Users/daniel/Desktop/T3_Projects/latch/docs/measurements/2026-11-14-latch6-field/postlong-refresh.json) · [Reverse relay driver](/Users/daniel/Desktop/T3_Projects/latch/docs/measurements/2026-11-14-latch6-field/reverse-relay.py)

| Path | Input responses | Key-to-picture minimum / median / p95 / maximum, ms | Decoded fps |
| --- | ---: | ---: | ---: |
| Ordinary Tailscale UDP | 20/20 | 151.54 / 160.99 / 166.00 / 166.61 | 60.05 |
| Public TLS relay | 20/20 | 172.69 / 192.02 / 194.79 / 197.82 | 59.14 |

The headless fixture observes synthetic picture changes through the real hardware decoder. The public run logged `using Latch TLS relay`. Synthetic audio reached the Mac output queue, with ten reported underruns at the end of the public run. Click-to-audio timing produced zero detected samples. Physical acoustic timing remains unmeasured. [UDP result](/Users/daniel/Desktop/T3_Projects/latch/docs/measurements/2026-11-14-latch6-field/final-reverse-input.json) · [Public result](/Users/daniel/Desktop/T3_Projects/latch/docs/measurements/2026-11-14-latch6-field/final-reverse-relay-input.json) · [Public events](/Users/daniel/Desktop/T3_Projects/latch/docs/measurements/2026-11-14-latch6-field/final-reverse-relay-input.err)

A separate 20-second native Mac renderer run decoded 1,173 frames and submitted 1,166 for rendering. Render preparation took p50 2.820 ms, p95 5.229 ms and p99 8.024 ms. The final stats reported approximately 59.7 fps with the hardware H.264 decoder. [Renderer metrics](/Users/daniel/Desktop/T3_Projects/latch/docs/measurements/2026-11-14-latch6-field/final-reverse-render.err) · [Windows host](/Users/daniel/Desktop/T3_Projects/latch/docs/measurements/2026-11-14-latch6-field/final-reverse-windows-host.err)

## Final source refresh and Windows gates

After field work, the parent supplied further End routing, bounded relay-control inbox, transport-failure code, retry, active-relay polling, software resize-progress, NAT64 filtering, delayed ACK regression, and lifecycle-fixture changes. These nine files were archived with per-file hashes, extracted into the existing Windows scratch tree, touched, and rebuilt. The completed-field manifest remains preserved. [Latest refresh](/Users/daniel/Desktop/T3_Projects/latch/docs/measurements/2026-11-14-latch6-field/final-end-refresh.json) · [Completed-field manifest](/Users/daniel/Desktop/T3_Projects/latch/docs/measurements/2026-11-14-latch6-field/field-completed-effective-source-manifest.json) · [Mac source verification](/Users/daniel/Desktop/T3_Projects/latch/docs/measurements/2026-11-14-latch6-field/mac-source-verification.json) · [Latest native manifest](/Users/daniel/Desktop/T3_Projects/latch/docs/measurements/2026-11-14-latch6-field/effective-source-manifest.json)

The final native manifest SHA-256 is `d7215f838ac66b8cd2a5e187b16f817ce0271ff0b0c9bc544b8074af7c7d6192`. Relay `client.rs` is `899b0d9cb278f706d483cd717a8b3416087d42e5500c77f9e484247113275301`. Windows reverified all 928 source files with zero mismatches. Compiler logs show the refreshed Latch crates rebuilding. The field-only renderer executable retained its earlier binary because no further field run was requested. Its exact generated Windows source and the final Mac renderer/headless fixture sources are preserved with hashes in the fixture manifest. Newly built native examples belong to this final manifest. [Fixture hashes](/Users/daniel/Desktop/T3_Projects/latch/docs/measurements/2026-11-14-latch6-field/fixture-manifest.json) · [Identity](/Users/daniel/Desktop/T3_Projects/latch/docs/measurements/2026-11-14-latch6-field/effective-source-identity.json) · [Audit and binary hashes](/Users/daniel/Desktop/T3_Projects/latch/docs/measurements/2026-11-14-latch6-field/final-native-audit.json) · [Foundation compiler output](/Users/daniel/Desktop/T3_Projects/latch/docs/measurements/2026-11-14-latch6-field/final3-foundation.err) · [Platform compiler output](/Users/daniel/Desktop/T3_Projects/latch/docs/measurements/2026-11-14-latch6-field/final3-platform-libs.err)

| Windows command | Passed | Ignored | Command elapsed, s |
| --- | ---: | ---: | ---: |
| `cargo test --locked -p latch-core -p latch-proto -p latch-relay --all-targets` | 200 | 3 | 14.32 |
| `cargo test --locked -p latch-engine -p latch-stream -p latch-client --lib` | 381 | 33 | 36.31 |
| `cargo test --locked -p latch-engine --test lifecycle -- --test-threads=1` | 6 | 0 | 62.46 |
| `cargo test --locked -p latch-engine --test relay_runtime -- --test-threads=1` | 9 | 0 | 130.27 |
| `cargo test --locked -p latch-codec --lib` | 45 | 0 | 5.47 |

These selected suites total 641 passed, zero failed and 36 ignored. Counts exclude nested child-test result lines that report filtered tests. The ignored cases retain their existing platform/hardware/manual conditions. The 13 real-TLS transport tests passed with default parallelism in 4.96 seconds, including the revised 650/650/900 ms delayed-return regression and permanent ACK blackhole. A separate final sequential transport invocation passed all 13 in 13.92 seconds. The runtime suite completed in 126.70 seconds and includes the actual Stop/takeover/end-code fixture. No final3 output contained Windows socket error 10022. [Exact commands and exit codes](/Users/daniel/Desktop/T3_Projects/latch/docs/measurements/2026-11-14-latch6-field/final3-native-results.jsonl) · [Count extraction](/Users/daniel/Desktop/T3_Projects/latch/docs/measurements/2026-11-14-latch6-field/final-native-test-counts.json) · [Transport output](/Users/daniel/Desktop/T3_Projects/latch/docs/measurements/2026-11-14-latch6-field/final3-foundation.out) · [Runtime output](/Users/daniel/Desktop/T3_Projects/latch/docs/measurements/2026-11-14-latch6-field/final3-relay-runtime.out) · [Sequential transport](/Users/daniel/Desktop/T3_Projects/latch/docs/measurements/2026-11-14-latch6-field/final-transport-sequential.out)

Native examples `serve`, `pipeline_bench`, `present`, `view` and `pads` rebuilt successfully in 13.96 seconds. `cargo check --locked --workspace --all-targets` passed in 4.72 seconds. Installed Rust stable 1.98.0 was retained. Final Windows cleanup again found no owned processes, firewall rules or scheduled task. [Build output](/Users/daniel/Desktop/T3_Projects/latch/docs/measurements/2026-11-14-latch6-field/final3-windows-examples.err) · [Workspace check](/Users/daniel/Desktop/T3_Projects/latch/docs/measurements/2026-11-14-latch6-field/final3-workspace-check.err) · [Final audit](/Users/daniel/Desktop/T3_Projects/latch/docs/measurements/2026-11-14-latch6-field/final-native-audit.json)

## Traffic and cleanup

Integration of the saved viewer application-media samples covers 1,395.68 seconds, approximately 1.803 GB of encoded video and 1.928 GB using the maximum of video and received-byte rates to compensate for the early display-counter omission. Counting two relay legs gives approximately 3.86 GB for those observed samples. These exclude protocol overhead, retransmissions, unlogged startup and some short diagnostic/input attempts. The recorded main viewer run durations total 1,370 seconds including the invalidated 120-second attempt and earlier 75-second outage attempt, each with requests capped at 20 Mbps. Their full-request envelope is 3.425 GB per leg, or 6.85 GB for two legs. Short reverse/input tests requested 10–20 Mbps and ran for seconds. This accounting leaves substantial margin under the 15 GB task limit. Cloud-billing-exact counters were unavailable. [Application integration](/Users/daniel/Desktop/T3_Projects/latch/docs/measurements/2026-11-14-latch6-field/traffic-estimate.json) · [Run metadata](/Users/daniel/Desktop/T3_Projects/latch/docs/measurements/2026-11-14-latch6-field/controller-long-noise-1080-60.meta.json)

The Mac synthetic hosts and private field forwarder were stopped. The Windows audit found zero owned scratch processes, zero field firewall rules, and zero field scheduled tasks. The private port 47964 had no Mac listener after cleanup. Production binaries, backup services and relay deployment stayed with their existing owners. No screenshot, raw media recording, or user keystroke log was collected. [Windows audit](/Users/daniel/Desktop/T3_Projects/latch/docs/measurements/2026-11-14-latch6-field/field-completed-audit.json) · [Mac audit](/Users/daniel/Desktop/T3_Projects/latch/docs/measurements/2026-11-14-latch6-field/mac-cleanup-audit.json)
