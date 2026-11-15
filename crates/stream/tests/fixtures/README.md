`gray-bars.h264` is the existing 64×64 white-top/black-bottom reference.
The viewer hardware-probe fixtures were transcoded from it with FFmpeg 9.0.2
(Homebrew), which is a test tool and is never linked or shipped by Latch:

```
ffmpeg -i gray-bars.h264 -frames:v 1 -c:v libx265 -x265-params pools=1:log-level=error -f hevc gray-bars.hevc
ffmpeg -i gray-bars.h264 -frames:v 1 -pix_fmt yuv420p10le -c:v libx265 -x265-params pools=1:bframes=0:log-level=error -f hevc gray-bars-main10.hevc
ffmpeg -i gray-bars.h264 -frames:v 1 -c:v libsvtav1 -preset 12 -svtav1-params lp=1 -f obu gray-bars.av1
```

AV1 is a low-overhead OBU temporal unit, including its sequence header.
The HEVC and H.264 files use Annex B. The tests check decoded visible
pixels as well as native storage and hardware selection.

`decode-probe.h264`, `.hevc` and `.av1` use the same pattern scaled to
256×256 (`-vf scale=256:256`), avoiding driver minimum surface sizes for
tiny pictures. H.264 uses `-tune zerolatency -bf 0`, HEVC `bframes=0`, and
AV1 `-svtav1-params lp=1:pred-struct=1`. These are capability probes, not
throughput benchmarks or evidence of maximum resolution/rate support.

`decode-probe-main10.hevc` is the 256×256 pattern encoded as 10-bit HEVC
with FFmpeg/libx265 (`-vf scale=256:256 -pix_fmt yuv420p10le`,
`-x265-params pools=1:bframes=0:log-level=error`).

`vaapi-b-frames.h264` and `.hevc` are 12-picture, 128×64 FFmpeg test-pattern
streams encoded with libx264/libx265 with B pictures enabled. They exercise
slice headers and reference lists in decoding order through a recording
backend on every platform. Latch's streaming encoders use low-delay output.
`vt-ltr-overflow.h264` also exercises retained long-term references across
frame-number gaps and bounded reference storage.

`mac-long-gop-tiny.h264` and `mac-long-gop-1080.h264` are unnormalized
VideoToolbox Constrained Baseline output captured from Latch's production
synthetic IOSurface encoder on Apple Silicon on 2026-11-15. They contain
32 pictures at 160×98/25 fps/4 Mbps/level 2.1 and 16 pictures at
1920×1080/60 fps/10 Mbps/level 4.2, with only the first picture forced to
IDR. Input luma is `32 + 2 * frame_index`, chroma is 128. SPS declares
one reference, POC type 0 and no bitstream restriction. Windows MF tests
measure raw steady-state withholding and require each patched access unit
to return its own sample time immediately, through software and optional
real DXVA. The original six-frame-IDR pair tape is retained separately in
`tests/pairs/mac-vt-6.0.0.h264`.
