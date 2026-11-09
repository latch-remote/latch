# Linux output size and software workers — 2026-11-09

Machine: M3 Pro, 6 performance cores and 6 efficiency cores. Linux verification:
arm64 Ubuntu 24.04 in Docker Desktop, constrained to a two-CPU quota, Xvfb at
3840×2160, an xterm printing a new date every 50 ms. No installed Latch service,
remote machine, or port 47850 was used. Baseline: `69539f01d947326b050633406b0ba782b202cdd4`.

## Geometry result

| Source | Request | Before: decoded picture | After: decoded picture |
|---|---|---|---|
| Xvfb 3840×2160 | 1920×1080 | 3840×2160 | 1920×1080 |
| Xvfb 3840×2160 | 3840×2160 | native geometry | 3840×2160 |
| Synthetic 1280×720 | 640×360 | not rerun | 640×360, decoded in loopback test |

The smaller request now encodes one quarter as many pixels. This is a geometry
result, not a measured fourfold speedup. Capture remains at native size. The
source is aspect-fitted into the requested canvas, matching Windows/macOS and
the existing input mapping; it is not stretched. The separate cursor's shape
and hotspot are converted to output pixels. Reference history and hardware
encoder construction use output dimensions. NVENC/VA-API surface constructors
already use the factory's dimensions; that factory now receives the output size.

The unchanged-size branch calls the original SIMD converter. No quiet-machine
throughput equivalence or two-worker before/after speed claim is made yet.

## Throughput sweep: pending

Concurrent builds and test suites prevented a sustained quiet window. A brief
interval completed the [1080p encode-only rows](partial-timings.md); the full
size/worker/content matrix remains incomplete. Every accepted row checked
one-minute load below 3 both before and after each trial. Loaded samples are
excluded. Initial pipeline rows were also discarded after correcting an
encoder-setup timing mismatch in the explicit-worker benchmark constructor;
its setup now occurs in the same first-frame call as the original constructor. The worker table therefore
**remains capped at four** for all picture tiers; two-CPU machines retain two.
Conversion, software encoding and the existing capacity probe use one selection
function of available parallelism and output dimensions. Rust's available
parallelism accounts for Linux quotas; macOS additionally prefers performance
cores. Future caps above four reserve a CPU and require at least 128 pixels per
slice. The capacity probe still measures the output it will actually encode.
The probe retains its moving-bar/pixel-rate approximation; native capture and
full-motion scaling costs are not certified by that sample. There is no new
runtime governor or unmeasured capacity uplift.

One command builds the benches, waits for load below 3, checks load before and
after each trial, runs every size × worker count × content three times, saves
raw results, and prints median [minimum, maximum] Markdown tables:

```sh
python3 docs/measurements/2026-11-09-linux-output-size/sweep.py --baseline-ref 69539f01d947326b050633406b0ba782b202cdd4 > /tmp/linux-output-size-timings.md
```

The default wait limit is 40 minutes. Exit 2 means pending; rerunning resumes
valid rows for the same binaries (use a new `--results` directory after code changes). Sizes: 1920×1080, 2560×1440, 3840×2160. Workers: 2, 4, 6, 8, 10.
Encode content: moving texture and scrolling text, 30 frames, 60 fps target,
20 Mbps. Pipeline content: noise and moving-bar desktop, one second, 500 fps
request (to avoid a 60 fps pacing ceiling), 20 Mbps. Encoder fps in the table is
1000 / steady encode milliseconds; pipeline fps includes capture and pacing.
The pipeline's `noise` changes a bounded patch rather than the whole desktop;
`motion` in the encode bench is the full-frame stress case. Pipeline rows near
500 fps are pacing-limited and do not establish a conversion/encoder ceiling.

The command also builds a baseline source snapshot in the results directory.
Its only instrumentation replaces the two fixed worker expressions with a
`LATCH_BENCH_THREADS` override; the script sets it to two or four for each
baseline trial. No worktree or installed service is changed. An already-built
baseline can instead be supplied with `--baseline /tmp/latch-baseline/target/release/examples/pipeline_bench`.
Native-size before/after comparisons use identical patterns. Do not infer
scalability from loaded runs.

## Deterministic slice cost

```sh
python3 docs/measurements/2026-11-09-linux-output-size/sweep.py \
  --deterministic --frames 12 --results /tmp/latch-size-deterministic \
  > /tmp/linux-output-size-slice-cost.md
```

This uses the existing encoder's `--budget 0`: time-based adaptation is disabled,
but rate control still chooses QP. It is **not a fixed-QP test**. All three
bitstream SHA-256 values matched for each of the 30 configurations. Timing
fields are deliberately excluded. Full bitrate, PSNR and QP results:
[slice-cost.md](slice-cost.md).

Relative to four slices, six/eight/ten slices changed bitrate by −4.33% to
+0.56% and PSNR by −0.357 to +0.079 dB on these short sequences. At 4K, scrolling
text lost 0.309/0.315/0.357 dB with six/eight/ten slices. More slices are not a
free compression improvement; these observations do not establish a universal
quality bound or justify increasing the default. The production cap is unchanged.

## Linux reproduction and checks

Use Ubuntu 24.04 arm64 with `build-essential pkg-config libasound2-dev xvfb xterm
x11-utils libxkbcommon-dev`, Rust stable, and a scratch source copy. Build:

```sh
cargo build --release -p latch-engine --example serve --example pipeline_bench -p latch-stream --example view
Xvfb :91 -screen 0 3840x2160x24 -nolisten tcp
DISPLAY=:91 xterm -geometry 100x30 -e sh -c 'while true; do date; sleep 0.05; done'
DISPLAY=:91 LATCH_DATA_DIR=/tmp/latch-output-host target/release/examples/serve --bind 0.0.0.0 --port 47983
# Separate process, inside the same container; repeat with 3840 × 2160:
LATCH_DATA_DIR=/tmp/latch-output-view target/release/examples/view 127.0.0.1 --port 47983 --width 1920 --height 1080 --fps 60 --seconds 3
DISPLAY=:91 target/release/examples/pipeline_bench --software --motion x11 --width 1920 --height 1080 --fps 60 --seconds 3
```

Container ports were published with `-p 127.0.0.1:47983:47983/tcp` and
`-p 127.0.0.1:47983:47983/udp`; `docker update --cpus 2` set the quota.
Pipeline output includes encode milliseconds and CPU; view includes delivered
size and fps. Their loaded timing numbers are not published as benchmarks.

Locally checked: formatting, workspace Clippy with warnings denied, workspace
tests; exhaustive scaler damage equivalence (including odd dimensions and
bilinear footprints), exact box averages, cursor/input aspect-fit coordinates,
odd source capture and black borders, output-size encoder construction, recovery identities and a real decoded
synthetic loopback. The two-CPU Linux container also ran the software unit tests,
scaler tests and the scaled loopback integration test.

Unverified: scaled NVENC/VA-API uploads on real GPUs, physical desktop text
appearance, the owner's ARM64 VPS, and quiet-machine fps/encode-time/CPU and
unchanged-size/two-worker performance comparisons. No software-worker throughput
increase is claimed pending the sweep. Temporary container processes are removed
after verification.
