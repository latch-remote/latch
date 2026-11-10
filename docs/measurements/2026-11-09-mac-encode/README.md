# Mac hardware encode rates, 2026-11-09

How many frames a second VideoToolbox encodes on one Mac, by codec and
size, with nothing else in the path.

## Machine and method

An Apple M3 Pro laptop (12 cores, macOS), otherwise lightly used: load
average about 3 before and after.

`realtime_benchmark` is an ignored test in `latch-engine`
(`src/mac/encoder/tests.rs`). It submits prefilled IOSurfaces to the
production encoder with the settings a stream uses (real time, no frame
reordering or delay, two frames in flight) on the stream's frame grid for
four seconds, with a keyframe once a second, and times each frame from
submission to the encoded output. There is no capture, network, decode or display, and the
pictures are synthetic, so this is the encoder's ceiling on this machine
and not a stream's frame rate.

```sh
LATCH_VT_SECONDS=4 \
LATCH_VT_BENCH=hevc:1920x1080@60:50000,hevc:1920x1080@120:50000,hevc:1920x1080@240:80000,hevc:2560x1440@60:50000,hevc:2560x1440@120:80000,hevc:2560x1440@240:100000,hevc:3840x2160@60:80000,hevc:3840x2160@120:100000,high:1920x1080@60:50000,high:1920x1080@120:50000,high:1920x1080@240:80000,high:2560x1440@60:50000,high:2560x1440@120:80000,high:3840x2160@60:80000 \
cargo test --release -p latch-engine --lib realtime_benchmark -- --ignored --nocapture
```

Each case is `codec:WIDTHxHEIGHT@fps:kbps`; `hevc` is HEVC Main and `high`
is H.264 High.

## Results

One run, at commit `a2e4d32`; the benchmark's own output is in
[raw.txt](raw.txt). Time is per frame, submission to output, in
milliseconds.

| Codec | Size | Asked fps | Got fps | p50 | p95 | max |
|---|---|---:|---:|---:|---:|---:|
| HEVC | 1920×1080 | 60 | 60.6 | 8.53 | 14.31 | 16.21 |
| HEVC | 1920×1080 | 120 | 119.5 | 8.39 | 17.14 | 20.75 |
| HEVC | 1920×1080 | 240 | 242.2 | 5.83 | 7.13 | 9.63 |
| HEVC | 2560×1440 | 60 | 59.5 | 13.16 | 35.75 | 36.12 |
| HEVC | 2560×1440 | 120 | 119.6 | 8.75 | 17.72 | 22.71 |
| HEVC | 2560×1440 | 240 | 194.9 | 9.81 | 11.66 | 15.37 |
| HEVC | 3840×2160 | 60 | 60.5 | 13.57 | 16.88 | 18.31 |
| HEVC | 3840×2160 | 120 | 97.2 | 20.36 | 21.12 | 23.75 |
| H.264 High | 1920×1080 | 60 | 60.6 | 6.48 | 11.79 | 37.42 |
| H.264 High | 1920×1080 | 120 | 119.5 | 6.47 | 17.03 | 41.00 |
| H.264 High | 1920×1080 | 240 | 194.8 | 9.92 | 10.78 | 34.71 |
| H.264 High | 2560×1440 | 60 | 59.9 | 10.63 | 33.57 | 43.61 |
| H.264 High | 2560×1440 | 120 | 114.3 | 16.87 | 18.09 | 37.27 |
| H.264 High | 3840×2160 | 60 | 53.5 | 36.60 | 38.32 | 64.89 |

## What it says

- HEVC keeps 240 fps at 1080p, about 195 at 1440p and about 97 at 4K. A
  Mac host cannot share 4K at 120 fps, and that is the encoder's limit,
  not the two-frame pipeline's (next section).
- H.264 is slower than HEVC on this chip at every size above 1080p60, and
  does not reach 60 fps at 4K. Auto prefers HEVC when the viewer decodes
  it.
- A frame takes less time at 240 fps (6 ms at 1080p) than at 60 (8.5 ms),
  so the 60 fps rows do not show the encoder working as fast as it can.

## More frames in flight

Latch keeps at most two frames in the encoder at once. To see whether
that holds the rate down, the same benchmark was run with
`LATCH_VT_IN_FLIGHT` set to 2, 3, 4 and 6:

| Case | 2 in flight | 3 | 4 | 6 |
|---|---|---|---|---|
| HEVC 3840×2160, 120 asked | 95.4 fps, 20.6 ms | 96.8, 30.8 | 96.8, 41.1 | 96.8, 61.8 |
| HEVC 2560×1440, 240 asked | 193.8, 9.8 | 203.3, 14.7 | 203.5, 19.6 | 203.5, 29.4 |
| H.264 3840×2160, 60 asked | 53.1, 36.7 | 53.6, 55.0 | 53.6, 73.5 | 53.6, 110.2 |
| H.264 1920×1080, 240 asked | 190.4, 10.0 | 196.2, 15.0 | 197.2, 19.9 | 196.4, 30.0 |

Each cell is frames a second and the p50 frame time. A third frame in
flight gains 1% to 5% and adds half as much delay again; beyond that the
rate does not move and every extra frame is one more frame time of delay.
The encoder hardware sets these rates, and two in flight stays.

## Not measured

Real desktop content, ScreenCaptureKit capture, ten-bit HEVC, other Apple
chips, and anything over the network.
