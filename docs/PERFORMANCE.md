# Performance

What Latch has been measured doing, on which machines, and how to repeat
it. Every number here comes from a record under
[measurements/](measurements/) and belongs to that machine, workload and
run; most are single short runs on machines doing other work. Anything
without a record is listed under [Not measured yet](#not-measured-yet).

## The short version

- **Mac host.** An M3 Pro's hardware encoder, with Latch's low-latency
  settings, keeps 240 fps at 1080p, about 195 fps at 1440p and about 97
  fps at 4K in HEVC, encoder alone
  ([record](measurements/2026-11-09-mac-encode/README.md)).
- **Windows host.** A PC with a Radeon RX 6600 delivered 217 decoded
  frames a second to a Mac at 1080p with 240 asked for, 213 at 1440p, and
  96 at 4K with 120 asked for, taking 3 to 10 ms from capture to send
  ([record](measurements/2026-11-03/windows-host/README.md)).
- **Linux host.** A 4-core ARM server with no GPU holds 60 fps at 1080p in
  software H.264, at 1.4 ms a frame on a moving test pattern
  ([record](measurements/2026-11-03-linux-host/README.md)).
- **Bad networks.** Through a simulated 20 Mbps link the viewer showed 296
  of 300 frames in five seconds
  ([record](measurements/2026-11-09-paced-batches/README.md)). Under 12%
  random loss the software encoder's recovery delivered 1,543 of 1,800
  pictures intact
  ([record](measurements/2026-11-03/rfi/review-fixes/README.md)).
- **Limits of the request.** A viewer can ask for up to 16384 pixels a
  side, 500 fps and 1 Gbps. Those are what the protocol can carry, not
  results: no machine here reaches them.

## Hardware

| Machine | Used as |
| --- | --- |
| Apple M3 Pro laptop, 12 cores, macOS | host and viewer: VideoToolbox encode and decode, the software codec, loopback transport and recovery tests |
| Windows 11 desktop, AMD Radeon RX 6600 | host and viewer: AMF and Media Foundation encode, DXVA decode, HDR readback, sound |
| ARM64 Ubuntu virtual server, 4 cores, no GPU | Linux host: X11 capture and software H.264 |

That is every machine Latch has run on. No NVIDIA or Intel GPU, no GPU of
any kind on Linux, no physical HDR display, and no wired network faster
than a few hundred Mbps.

## Encoding

Mac, VideoToolbox, encoder alone on synthetic pictures
([record](measurements/2026-11-09-mac-encode/README.md)):

| | 60 fps asked | 120 | 240 |
| --- | ---: | ---: | ---: |
| HEVC 1920×1080 | 60.6 | 119.5 | 242.2 |
| HEVC 2560×1440 | 59.5 | 119.6 | 194.9 |
| HEVC 3840×2160 | 60.5 | 97.2 | not run |
| H.264 1920×1080 | 60.6 | 119.5 | 194.8 |
| H.264 2560×1440 | 59.9 | 114.3 | not run |
| H.264 3840×2160 | 53.5 | not run | not run |

Windows (RX 6600, HEVC through AMF) and Linux (software H.264):

| Machine and workload | Result | Record |
| --- | --- | --- |
| RX 6600, capture and encode on the PC, 1080p at 240 asked | 202.0 fps; 2.92 ms capture to encoded, p50 | [Windows host](measurements/2026-11-03/windows-host/README.md) |
| same, 1440p at 240 | 157.4 fps; 4.69 ms | same |
| same, 4K at 120 | 90.2 fps; 9.69 ms | same |
| RX 6600 to the Mac over Tailscale, animated desktop, 1080p at 240 | 217.3 decoded fps; 3.00 ms mean capture to send | same |
| same, 1440p at 240 | 213.4 fps; 4.73 ms | same |
| same, 4K at 120 | 96.2 fps; 10.00 ms | same |
| ARM server to the Mac, 1080p sweep pattern, 60 fps | 1.40 ms encode, 29% of one core | [Linux host](measurements/2026-11-03-linux-host/README.md) |
| same, 1080p noise pattern | 6.01 ms encode, 60% of one core | same |

The Windows runs lasted six to eight seconds a mode on a PC in use; the
ones to the Mac crossed a path with a 164 to 172 ms round trip. The Linux
figures are
medians of two short runs. None of these is a game, and none measures the
time from an input to light leaving a screen.

The Windows host does not reach 240 fps, and part of that is Latch. On
that PC, capturing the animated desktop with nothing after it supplied 223
updates a second at 1080p, 191 at 1440p and 107 at 4K, and the AMF encoder
alone, fed noise, reached 413, 233 and 108. Capture and encode together on
the PC gave 202, 157 and 90: between 9% and 18% under the slower of the
two stages. That shortfall is in Latch's own path from capture to encoder
(a GPU copy, the cursor pass, colour conversion and the hand-off), and
closing it is on the [roadmap](ROADMAP.md). The capture figure itself
mixes Windows' compositor with Latch's copy, and those have not been
separated either.

## Decoding and presenting

Decode is the decoder's own time. "To the renderer" ends when the frame has
been handed to the GPU for drawing; it does not include the GPU finishing,
the compositor, or the display
([record](measurements/2026-11-03/viewer/README.md)).

| Machine and stream | Decode | To the renderer | Decoded / shown |
| --- | --- | --- | --- |
| M3 Pro, 1080p with 120 asked, ten seconds (about 61 fps arrived) | 1.17 ms mean | 2.95 ms p50, 7.30 ms p95 | 613 / 556 |
| RX 6600, HEVC 1080p at 120, six seconds | 4.92 ms p50 | 0.12 ms p50 | 725 / 667 |
| RX 6600, HEVC 4K at 120 | 16.23 ms p50 | 12.04 ms p50 | 373 / 352 |
| RX 6600, AV1 4K at 120 | 16.70 ms p50 | 6.92 ms p50 | 417 / 340 |

The Mac row was fed by a software-encoding host on the same loaded
machine, which is why about half the frames asked for arrived; it measures
the time per frame, not the Mac's decode rate. The RX 6600 does not decode
4K at 120 fps: about 60 to 70 frames a second came out. Linux viewers
decode H.264 in software and have not been measured.

## Network and loss recovery

| Harness and conditions | Result | Record |
| --- | --- | --- |
| ARM server and Mac over Tailscale, no codec, 50 Mbps offered | 50.0 Mbps delivered each way | [Transport](measurements/2026-11-03/transport/README.md) |
| Simulated 20 Mbps link, 5% random loss with bursts and jitter | 32.6 frames lost a minute; parity is 34.8% of what is sent | [Transport FEC](measurements/2026-11-04/transport-fec/README.md) |
| Software H.264 recovery, simulated 12% random loss | 1,543 of 1,800 pictures delivered and checked; 114 ms p95 to resume | [Recovery](measurements/2026-11-03/rfi/review-fixes/README.md) |
| RX 6600, AMF HEVC recovery frames | all 112 delivered pictures identical to an uninterrupted decode, across four recoveries | same |
| Mac, whole session through a simulated 20 Mbps, 20 ms link | 296 of 300 frames in five seconds; 16.3 ms p50 and 18.3 ms p95 between frames | [Paced batches](measurements/2026-11-09-paced-batches/README.md) |
| Mac, sender alone over loopback, 1 Gbps asked | 734 Mbps, with frames lost; the loopback relay is the limit | same |

Forward error correction costs bitrate, and it does not remove loss: mixed
loss on a long round trip still damages frames. One known weakness is a
host whose CPU is busy: a delayed ping reads as a queue on the path, and
the controller can cut a lightly loaded stream's bitrate to its floor for
a second or two before restoring it. It shows up on loaded CI runners; how
often it happens on a PC running a game has not been measured. Recovery frames in place
of keyframes are on by default for the software encoder and AMD's AMF, and
opt-in for VideoToolbox and Media Foundation
([record](measurements/2026-11-04/rfi-vt-mf/README.md)).

## Sound

On the RX 6600 PC, from a click in the stream to the virtual speaker
device, the median was 61 to 71 ms over three runs
([record](measurements/2026-11-03-audio/README.md)). That is device
timing, not a microphone in front of a speaker. A simulated 30 minutes
with clocks drifting 300 parts per million and up to 10 ms of jitter held
26 to 27 ms in the buffer with no gaps.

## HDR

Verified by reading pixels back, not by looking at a display
([record](measurements/2026-11-03/hdr/README.md)):

- Mac: ten-bit HEVC encode and decode of a synthetic picture, 64.5 dB luma
  PSNR, and a 1000-nit patch drawn at 3.85 times SDR white by the
  presenter.
- Windows: scRGB to PQ to ten-bit HEVC through both encoders, within 1.08
  code values, with the HDR metadata checked independently.

Nobody has yet seen a Latch HDR stream on an HDR panel.

## Not measured yet

- NVENC and VA-API on a Linux GPU; any hardware decode on a Linux viewer.
- NVIDIA and Intel GPUs on Windows.
- HDR on a physical HDR display: brightness, colour, and a whole HDR
  session to a Windows viewer.
- Input to photon: the time from a key or mouse movement to the change on
  the viewer's screen, measured with a camera.
- What Linux GSO and Windows USO save on a real network card.
- Sessions lasting hours, and real games at high frame rates.
- A clean network path above about 700 Mbps.
- Competing traffic and real Wi-Fi congestion.
- Sound latency through physical speakers, and sound against picture.

If you have the hardware for one of these, the issues labelled
[needs hardware](https://github.com/MrBeldum/latch/labels/needs%20hardware)
say what would help most.

## Repeat it, or add your machine

Everything below runs from a checkout ([building](../CONTRIBUTING.md#building))
on loopback with test patterns. None of it touches an installed Latch.

A host and a headless viewer, in two shells:

```sh
LATCH_TEST_PATTERN=sweep cargo run --release -p latch-engine --example serve -- --bind 127.0.0.1
cargo run --release -p latch-stream --example view -- 127.0.0.1 --width 1920 --height 1080 --fps 120 --kbps 50000 --seconds 10 --hevc
```

`serve` skips the Tailscale identity check, so keep it on loopback.

The stages on their own:

```sh
# Capture-to-packet pipeline with the software encoder
cargo run --release -p latch-engine --example pipeline_bench -- --software --width 1920 --height 1080 --fps 60 --motion sweep
# Decoders
cargo run --release -p latch-stream --example stream_decode_bench
# The Mac's hardware encoder: codec:WIDTHxHEIGHT@fps:kbps, comma separated
LATCH_VT_BENCH=hevc:1920x1080@240:80000 cargo test --release -p latch-engine --lib realtime_benchmark -- --ignored --nocapture
# The sender and a whole session through a rate-limited link (kbps, ms)
PB_CAP=20000 PB_RTT=20 cargo test --release -p latch-engine --lib paced_socket_measurement -- --ignored --nocapture
PB_CAP=20000 PB_RTT=20 cargo test --release -p latch-engine --test bottleneck noise_socket_measurement -- --ignored --nocapture
# Transport and recovery simulations
cargo test --release -p latch-engine --test steady_transport --test transport_scenarios --test recovery -- --nocapture
```

On Windows, `cargo run --release -p latch-engine --example windows_capture
-- 6 NUL 1920 1080 240 50000 hevc` measures capture and encode on the PC
itself. It switches the display mode for the run and puts it back.

To add results, open a pull request with a folder
`docs/measurements/YYYY-MM-DD-topic/` whose `README.md` says what the
machine is (CPU, GPU, OS and driver versions), the commit, the exact
commands, and the raw output, including the runs that went badly. The
existing records show the shape. [ENGINE.md](ENGINE.md) explains what each
timing starts and ends at.
