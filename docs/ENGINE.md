# The Latch streaming engine

Latch 4 shared a desktop through Sunshine (C++) and viewed it through
moonlight-common-c (C), OpenH264 (C++) and libopus (C). Since version 5, Latch does both
in Rust, inside the one `latch-host` executable that already runs the
control service and the window.

## Boundary

Everything Latch ships is Rust source compiled by `rustc`. Allowed below
that line, because they are the platform rather than a library we chose:

* operating-system and driver APIs called from Rust: ScreenCaptureKit,
  VideoToolbox, CoreGraphics, CoreAudio (macOS); DXGI, Direct3D 11, Media
  Foundation, WASAPI, SendInput (Windows); the X11 protocol, spoken by the
  pure-Rust `x11rb`, and the PulseAudio/PipeWire server (Linux); Metal,
  Vulkan or DX12 through `wgpu`;
* the GPU vendors' encoders and decoders those APIs reach (NVENC, AMF,
  Quick Sync, Apple's media engine);
* the Windows virtual display driver, which is a driver;
* Rust crates from crates.io that compile no C, C++ or assembler files.
  One of them, `graviola` (the cryptography provider for update TLS and the
  public relay), holds
  formally verified assembly written as Rust inline `asm!`: `rustc`
  compiles it, no external assembler runs, and it is the only assembly in
  the build.

No crate in the build may compile C, C++ or assembler with `cc`, `cmake`
or `nasm`. `scripts/audit-native.sh` checks the resolved build graph and
CI runs it.

## Local network diagnosis

`latch_core::netenv` exposes independent network observations through
`NetworkDiagnosis`. `discover(&Options)` is a blocking worker entry point.
UI callers use `NetworkEnvironment::request`, `poll` and `get`, passing a
network generation that they increment after route, DNS or proxy changes
and wake, for changed Tailscale health/peer status, or for an explicit
retry. Changed options also invalidate cached results. `request` returns
`RequestStatus::{Cached, Started, Pending, Unavailable}`. A refresh already
in flight finishes first. The cache remembers the latest requested generation
and options, and `poll` starts that refresh automatically.
The default six-second deadline is clamped to 100 ms–15 s. Partial results
retain unknown states. Each check kind has one process-wide in-flight OS job.
Matching inputs coalesce onto that job. Changed inputs stay pending until
that kind becomes available, keeping another host's credentials and facts
scoped to their original inputs. A stalled resolver leaves the other check
kinds available. Every subprocess exit and output read shares its deadline.
Timed-out children are killed and reaped. A descendant holding a pipe open
can outlive its parent, while the caller still returns at the deadline.

Proxy discovery targets an HTTPS host on port 443. An explicit
`Options::proxy_override`, parsed with `Proxy::parse`, takes precedence over
all system settings and bypass rules. Parse failures must be shown by the
caller. Each ordered candidate carries a `ProxySource`. HTTP CONNECT proxy
credentials have explicit accessors and redacted Debug/Display output.
Unsupported proxy protocols produce warnings. Credential stores such as
macOS Keychain and Windows Credential Manager are outside this module.

- macOS reads CFNetwork system settings, including scoped settings and
  bypass rules, and evaluates PAC with CFNetwork on a private worker run
  loop. If CFNetwork settings are unavailable, a static `scutil --proxy`
  fallback reports its limits. WPAD reads DHCP option 252 for the primary
  service, then tries `http://wpad/wpad.dat`. A successful PAC/WPAD result
  supplies the complete candidate order, including any explicit DIRECT.
  Cancellation invalidates and removes the run-loop source before freeing
  the boxed callback state. A looping-script test verifies the caller's
  deadline. CFNetwork provides no public guarantee that invalidation stops
  its internal JavaScript worker, so continued CPU use by a looping PAC
  script after cancellation remains unverified. Username-only Keychain
  entries produce `keychain_credentials_unread` with no credentials.
- Windows evaluates the calling account's Internet settings with WinHTTP,
  trying DHCP/DNS WPAD before explicit PAC, then static settings. An async
  resolver handle is cancelled at the deadline, including on networks with
  no WPAD. Missing WPAD is an ordinary direct result. An explicit PAC
  failure produces `pac_failed`. Machine
  WinHTTP defaults provide a fallback. Automatic proxy authentication is
  disabled during discovery. A service sees its own account's settings
  and the machine defaults. The signed-in desktop's configuration may
  differ, so a service can need an explicit override.
- Linux checks nonempty `HTTPS_PROXY`, `https_proxy`, `ALL_PROXY`, then
  `all_proxy`, with `NO_PROXY`/`no_proxy` bypasses. Uppercase takes
  precedence. Bypasses support domain label boundaries, optional ports,
  IP literals, IPv6 and CIDR without DNS lookups. It then reads GNOME
  through `gsettings` and KDE's XDG `kioslaverc` layers, preferring the
  current desktop. The commands read the process's desktop session. PAC/WPAD
  configuration is reported as unevaluated with no candidate because
  Linux has no shared system PAC evaluator. Set an explicit proxy to
  resolve that case. Shell and service environments can differ.

`ProbeTarget::new` makes the HTTP destination, expected status and body
explicit. The product configures an anonymous probe on its relay host by default.
Core callers with an empty `Options::probes` use native signals only. Windows
uses its native connectivity profile and Linux uses NetworkManager's cached
connectivity signal when available. macOS has no public, general-purpose
portal-status API, so it needs a probe. Probes use direct cleartext HTTP
with no credentials, cookies, redirects or device identifiers, and bounded
headers/body/time. A redirect, HTTP 511 or replacement HTML indicates a
sign-in portal. Filters and authentication failures have a separate
restricted state. Configured probes run before native hints, allowing a
successful check to clear a stale OS portal signal. Sign-in URLs omit
userinfo. `PortalUrl::as_str` exposes the complete path/query/fragment for
opening a sign-in page. Debug/Display expose only the origin.
All failed direct probes mean those targets were unreachable. A proxy-only
network or a failed endpoint can produce the same observation. An ordered
fallback target can reduce that ambiguity. These checks cannot authenticate
a cleartext response or prove every internet service is reachable.

The product probes `http://CONFIGURED_RELAY_HOST/netenv/probe` on port 80,
expecting HTTP 204, an empty body, `Content-Length: 0` and
`Cache-Control: no-store`. The daemon's optional `--http-listen` serves it
alongside an ACME webroot, without HTTPS redirection or authentication.
Older relays lacking the listener degrade silently. No third-party fallback
is configured. Probe operators see public source IP and timing, and the
probe listener records no request addresses. DNS64 discovery queries only
`ipv4only.arpa` through the system resolver, following RFC 7050 and RFC 6052.
Either well-known IPv4 constant identifies an RFC 6052 prefix length.
Answers matching multiple legal positions or repeating the same constant
are set aside. Usable answers for both constants must agree on prefixes.
Resolver order is preserved.
Interface families describe operational local addresses. They do not prove
internet access. DNS64 absence is inconclusive. A DNS64 prefix together
with IPv6 and only CLAT service-continuity IPv4 addresses (192.0.0.0/29)
is reported as IPv6-only. VPN interfaces can change the apparent family.

Tailscale diagnosis reuses core's status and netcheck parsers. Stable
`TailscaleReason::code()` values accompany short actionable messages,
DERP region and peer-relay details. An idle peer's home DERP never establishes
an active relay path. Only explicit daemon health evidence establishes a
control/DERP reachability failure. UDP failure alone cannot establish it.
Unknown or future health formats stay unknown. Netcheck itself probes
Tailscale infrastructure. No network result is added to the control API or
used to select a stream transport in this module.

`NetworkDiagnosis::warnings` and `ProxyDiscovery::warnings` contain
`NetworkWarning` values. Each provides `code()`, `severity()` and `message()`.
`WarningSeverity` distinguishes `Informational` and `Actionable`. Both enums
are non-exhaustive. Messages are fixed and credential-free. Stable codes are
`checks_pending`, `checks_timed_out`, `invalid_proxy`, `invalid_destination`,
`unsupported_pac_protocol`, `malformed_pac_candidate`,
`pac_evaluator_unavailable`, `incomplete_system_proxy`, `linux_pac_unavailable`,
`pac_failed`, `unsupported_system_protocol`, `mac_static_fallback`,
`mac_settings_unavailable`, `wpad_failed`, `keychain_credentials_unread` and
`windows_service_settings`. The Windows account note is emitted when the
current account's Internet settings cannot be read, a service-account hint.
Real system proxy smoke tests run on macOS and Windows, including WinRT
connectivity and bounded Windows auto-detection.

Run `cargo run -p latch-core --example netenv -j 4` for local facts. An
optional HTTP probe URL and peer ID are positional arguments. `LATCH_PROXY`
is an example-only override. Output describes the current system's proxy
candidates, Tailscale path and local address families. An absent NAT64 prefix
is inconclusive. Portal state can stay unknown without an HTTP target.

## Shape

```
viewer (latch-stream)                         host (latch-engine, in the control service)
---------------------                         ------------------------------------------
POST /v1/stream, Upgrade ─── TCP 47850 ────▶  service.rs authorizes, engine::server takes the socket
                      ◀── 101 + Accept ─────  picks codec/encoder, starts the pipeline
UDP Hello ──────────────────────────────────▶ binds the viewer's address
                      ◀── Video shards + FEC  capture ─▶ convert ─▶ encode ─▶ packetize
reassemble ─▶ decode ─▶ FrameSlot ─▶ wgpu
                      ◀── Audio + parity ───  loopback capture ─▶ lpcm-rice
cpal ◀─ jitter buffer ◀─ decode
Input (redundant until acked) ──────────────▶ inject (SendInput / CGEvent / XTest)
Pad snapshots (newest, until acked) ────────▶ virtual pads (ViGEmBus / uinput)
                      ◀── PadFeedback ──────  the game's rumble and lights
RecoveryRequest on loss ────────────────────▶ next frame predicts from the last delivered
                      ◀── RecoveryAck ──────  (or refused: IDR)
KeyframeRequest (fallback, older hosts) ────▶ next frame is IDR
```

| Crate | What | Platform code |
| --- | --- | --- |
| `latch-proto` | wire format, Reed–Solomon FEC, packetizer and reassembler, input reliability, control framing, an impairment simulator for tests | socket helpers in `transport` |
| `latch-relay` | native two-lane TLS transport, end-to-end record encryption, capabilities and daemon quotas | bounded outbound TLS connections and relay daemon sockets |
| `latch-codec` | H.264 Constrained Baseline encoder and decoder, BGRA→YUV conversion, the `lpcm-rice` audio codec | none |
| `latch-engine` | the host side: capture, encode, audio capture, input injection, display sizing, and the session server | `mac/`, `windows/`, `linux/` |
| `latch-stream` | the viewer side: session, decoders (VideoToolbox, Media Foundation, `latch-codec`), audio playback | `video/` |

### Video paths

| Host | Capture | Encode |
| --- | --- | --- |
| macOS | ScreenCaptureKit, NV12 IOSurface at the stream size and up to the display's refresh, cursor included | VideoToolbox (HEVC, H.264 High, or Constrained Baseline; negotiated HEVC Main10 from P010), real time, no frame reordering, two frames in flight ([MACOS.md](MACOS.md#sharing-this-mac-the-video-pipeline)) |
| Windows | DXGI Output5 Desktop Duplication (BGRA or scRGB FP16); separate DXGI cursor plane or GPU cursor/colour pass, and D3D11 video processor to NV12/P010 | AMF HEVC on AMD, or the GPU's Media Foundation HEVC/AV1/H.264 encoder (Microsoft's CPU encoder for H.264 if needed) ([WINDOWS.md](WINDOWS.md#the-video-pipeline)); `latch-codec` if neither works |
| Linux (X11) | MIT-SHM `GetImage` through `x11rb` of only the rectangles X Damage reports, cursor from XFixes | NVENC or VA-API from NV12 the CPU converts, when a probe at start finds one ([below](#linux-hardware-encoding)); else `latch-codec`, threads by slice |

Windows negotiates separate cursor drawing through `cursor` and `DrawCursor`.
DXGI colour shapes retain straight sRGB alpha. Monochrome AND/XOR and masked
colour shapes use transparent/copy pixels directly, with white inverted regions
and a one-pixel dark outline. Arbitrary colour XOR remains in the GPU compositor.
Malformed shapes, unknown visibility and desktop-embedded pointers also keep
composition enabled. If DXGI shape retrieval fails, GetIconInfo/GetDIBits recover
a system cursor for composition, with the shared system arrow as the final
readable-shape fallback. Cursor recovery never delays a usable desktop frame.
If both reads fail, video continues immediately with the last readable bitmap,
or without one if none is available, with one warning per failure episode and
retries every 250 ms. An idle capture tick repaints the retained desktop when a bitmap becomes available.
Hidden/suppressed and off-output cursors stay hidden.

Shapes already contain the host's physical DPI sizing. The shared scaler scales
Windows shapes and hotspots to stream-content pixels, including the 96×96 wire
bound. Windows and Mac use `cursor::Tracker` for shape comparison, cached pixels,
visibility deduplication and a single process-wide serial allocator. Native
Windows shape changes call `set_shape`; ordinary capture ticks use `publish`,
which compares visibility and serial without copying or comparing bitmap bytes.
The GDI fallback uses the shared `cursor::unpremultiply` conversion.

Pointer-only DXGI acquisitions publish shape/visibility through a coalescing
mailbox and a separate control sender before conversion/encoding. They leave
the retained video picture alone while the separate plane is active. The
mailbox retains unsent pixels across visibility changes and deduplicates each
viewer's wire state. Main's shape/visibility protocol is used on all platforms.
Pointer movement uses the viewer's local input and needs no host sampler thread.

The captured output is the viewer-selected monitor, including its physical
desktop origin. Reopening the same output retains the raw bitmap, rescales the
plane after a resolution change and re-sends its shape with a fresh shared
serial. Output changes clear per-output pointer state. A selected monitor
disappearing reopens on the host's default output with fresh cursor state.
Choosing another display reconnects and uses the shared viewer reconnect reset
to discard the cached cursor texture. Windows reports the actual captured
display identity for hardware and software encoding so the chooser reflects
missing-display fallback.

While a focused free-mouse viewer drives the pointer, its local position is
authoritative. If an application warps the host pointer or another mouse moves
it, the drawn cursor stays at the local position until the user next moves the
mouse. Hover effects on the host can diverge while the local pointer is still.
An unfocused viewer keeps its local OS arrow, paints no plane and sends no
`DrawCursor` updates. If focus is lost while the local pointer is over the
stream, the last `DrawCursor(false)` stays in force: the host does not draw
the pointer into the video and the viewer paints none, so pointer motion made
at the host is invisible until the viewer is focused again. Mac hosts behave
the same.

Separate cursor bytes remain sRGB on SDR and HDR desktops/sessions. The viewer
renders them with its SDR UI white, including over native HDR presentation. The
host's SDR white level is used by the existing scRGB tone mapping and composited
HDR cursor pass, without applying that luminance multiplier to the RGBA bytes.
Live HDR cursor appearance and mixed-DPI multi-output transitions still need
interactive hardware qualification.

| Viewer | Decode |
| --- | --- |
| macOS | VideoToolbox (hardware-probed AV1, HEVC Main/Main10 or H.264), IOSurface → Metal; NV12, or P010 for a negotiated ten-bit stream (HDR drawn by the native EDR layer where available) |
| Windows | Media Foundation + D3D11/DXVA for hardware-probed AV1, HEVC Main/Main10 and H.264 High; Baseline with `latch-codec` fallback. Shared NV12 D3D11 texture → D3D12/wgpu; staging readback when unavailable and for ten-bit P010 (drawn by the native HDR10 swapchain where available) |
| Linux | Runtime-probed VA-API H.264 and HEVC Main/Main10, with software Baseline fallback. DMA-BUF planes → EGL/GLES where compatible, NV12/P010 readback otherwise |

The viewer lists what it decodes; the host picks the first it can encode,
then fits the request to that encoder (`video::Limits`, reported to
viewers as `streamer.codecs`; see `docs/PROTOCOL.md`, "Encoder limits").
The request ceilings in `crates/proto/src/control.rs` are 16384 pixels a
side, 500 fps and 1 Gbps, above what current hardware delivers. A session
is fitted to what the two machines' encoders, decoders and displays report.
Other bounds include the codec's top level and the encoder's own limit
where it has one (VideoToolbox, found by trying; NVENC and VA-API, as
their drivers say), and on the software path the frame rate this CPU was
measured to encode (`software::measure_fps`, held by the server through
`software::ceiling`, 30 fps at 1080p until measured; on a Linux host
whose hardware has some codecs, only the others are held, and the rate is
in their `max_pixel_rate` rather than `max_fps`). Sizes are counted in coded 16-pixel blocks.
`latch-codec` decodes Constrained Baseline only, so a host asked for
`h264-baseline` configures its hardware encoder for that profile. A
viewer probes platform codecs before offering AV1 > HEVC > H.264 High >
Baseline. Windows probes actual pictures through the installed synchronous
MFT and D3D11 manager, caching the result; missing codec extensions or drivers
leave that codec out of negotiation. Mac checks `VTIsHardwareDecodeSupported` and requires a decoded hardware
picture too; a capability query alone can succeed on an unusable decoder.
Only Baseline streams have the ten-access-unit `latch-codec` safety net.
High, HEVC and AV1 have the session's instead (`session::HARDWARE_PROBATION`):
a decoder that cannot be created, gives no picture for ten access units, or
fails three times without one ends that attempt, and the session asks the
host again without the codec (down to Baseline), with a notice saying why;
the codec cools down for this host (five minutes for opening failures,
15 seconds for runtime decode failures); Apply and reconnect retries immediately.
`LATCH_TEST_BREAK_DECODER=hevc,h264-high` (or `all`; `hevc=silent` for a
decoder that gives nothing) breaks decoders on purpose to test this.
Unknown codec names in an offer are skipped by protocol-1 hosts.

Normal sessions offer ten-bit colour when the viewer GPU supports it and HDR
when its display reports a peak luminance. HEVC Main10 retains P010 samples
through decode and presentation. Mac uses native EDR presentation; Windows
uses its native HDR10 presenter when available. The wgpu fallback tone maps
HDR to SDR. See the HDR section below for negotiation and metadata.

On Windows, NV12 pictures can use a bounded pool of shared D3D11 textures
opened as D3D12/wgpu plane views on the same adapter. A fence completes the
copy before publication; the renderer retains surfaces through GPU completion.
P010 and unsupported sharing paths use staging readback, retaining ten-bit
samples for the renderer. A failed import disables sharing for later pictures.

The client requests `AutoNoVsync` and maximum frame latency 1, wakes egui on
every published picture, and samples the newest `FrameSlot` in the paint
callback. There is no frame timer or 60 Hz cap. Mac native buffers are retained
until previous GPU submissions complete, rather than for an arbitrary three
frames. This retention is not a presentation queue. The compositor, VRR and
scanout still determine physical display latency; immediate mode may tear.
`cargo run --release -p latch-client --example present -- --port 47882`
measures decode completion to renderer preparation on the viewer clock;
it does not measure GPU completion, OS present or photons.

Linux uses runtime-probed VA-API for H.264 and HEVC Main/Main10, with
software Baseline fallback. Rust parses the picture and slice parameters;
compatible EGL/GLES renderers import synchronized DMA-BUF planes. Other
paths read back NV12/P010. See the viewer negotiation section for limits,
recovery and hardware verification still required.

The decoder's reconstruction kernels were adapted from the MIT-licensed
`pith-h264` (see `crates/codec/src/h264/decode/SOURCE.md`); everything it
refuses (CABAC, B slices, 8×8 transforms, interlace) was removed rather
than left unreachable.

The software encoder is built for desktops, not films: a macroblock whose
source pixels did not change since the last frame costs a skip and no
search; changed macroblocks get a small motion search seeded by their
neighbours and a scroll detector, and intra prediction. Rate control is a
per-frame QP from a one-frame VBV, targeting a frame-sized wire budget;
individual frames can exceed it. Its hot loops (colour conversion, SAD, residual
reconstruction, deblocking) use NEON or SSE/AVX2 through `std::arch`, and
a governor targets 65% of the frame interval by reducing search effort.
This cannot bound capture, memory contention or minimum encoding work.
Software capacity measurement limits requested fps; the pacer resets after
missed deadlines. Received, decoded, drawn and dropped frame counts show
what the session actually achieves. The Linux host hands it what X Damage says
changed: only those rectangles are read from the X server and converted
(split across cores), and outside them macroblocks are skipped without
being compared; the bitstream is the one a full comparison gives.

### Linux hardware encoding

`latch-engine/src/linux/{nvenc,vaapi,hardware}.rs`. Both are loaded with
`libloading` when first needed; nothing is linked, and the structures
mirror the vendors' headers (sizes, offsets, bit positions, GUIDs and
version words checked against nvEncodeAPI.h 12.0 and libva 2.x).

* **NVENC** (`libnvidia-encode.so.1`, `libcuda.so.1`): H.264 Baseline or
  High, HEVC Main, AV1 Main; preset P1 with ultra-low-latency tuning, CBR
  with a one-frame VBV, no B-frames, an infinite GOP with IDRs (and
  parameter sets) on request, BT.709 VUI; bitrate changes by
  reconfiguration.
* **VA-API** (`libva.so.2`, `libva-drm.so.2`, for each codec the first
  render node whose driver opens its encoder, so an Intel iGPU and an AMD
  card each give what they have): H.264 Constrained Baseline or High and
  HEVC Main, the low-power entry point first, driver-written headers, one
  reference (marked `RPS_ST_CURR_BEFORE` for HEVC, as FFmpeg does), CBR
  with a one-frame HRD, the level from size, rate and bitrate (HEVC's High
  tier where Main's bitrate is too low). Without packed headers HEVC has
  no conformance window, so it codes only sizes in whole minimum coding
  blocks (8 pixels or the driver's): 1366 × 768 or 1600 × 900 are not
  offered HEVC. AV1 is not offered: VA-API leaves its headers to the
  application. The host's user must be able to open the node (the
  `render` group on Ubuntu); the log says so when it cannot.

Each backend and codec is probed once per process, on the host's
capacity thread when its service starts (open, an IDR that must carry its
parameter sets, a P-frame), and what it takes is asked of the driver then
(NVENC's maximum width, height, macroblocks and macroblock rate; VA-API's
maximum picture size and HEVC block sizes); anything missing or failing
leaves `latch-codec`. `LATCH_LINUX_ENCODER=auto` (default) `| software |
nvenc | vaapi` chooses; under `auto` a viewer that asks for Constrained
Baseline keeps `latch-codec`, whose output that decoder is tested
against, and hardware Baseline is not probed. A codec is given up for the
process only when an open fails in a way that lasts (no such profile or
entry point, a driver that does not initialize, a node the user may not
open, NVENC's device and version errors); a GeForce card's session limit,
memory or a size the encoder does not take fail only that open. H.264
then encodes in `latch-codec`, for that session or that size; a codec
that will not open when a session starts gives way to the next one the
viewer takes, and one that cannot go on after the desktop is resized
(`video::Unencodable`) ends the session as `ENCODER_FAILED`, which the
viewer reconnects from to negotiate another. Neither backend has run on a
GPU yet: the Linux host used for testing has libva and a virtio render
node its user may not open, so only the fallback has run.

**Wayland** is not captured. It takes the ScreenCast portal (D-Bus, which
pure Rust can speak) and a PipeWire stream, whose Rust bindings are
generated from and compiled against libpipewire's C headers, outside the
boundary above; the portal also asks a person to approve each session,
which an unattended host cannot. Under a Wayland session only Xwayland
windows are seen.

VideoToolbox LTR recovery is runtime-gated (the low-latency mode is opt-in
with `LATCH_MAC_LTR=1`, and a session that stalls or misses its pace reopens
without it). Windows H.264 MFT recovery is opt-in with `LATCH_MF_LTR=1`
(unverified on hardware), and an MFT must then pass a loss/decode
self-test at open before advertising recovery; the engine reuses `latch-stream`'s Media
Foundation decoder for this check. An H.264 refresh is withheld when the
viewer's references would exceed `max_num_ref_frames`, which freezes Apple's
decoder; the Mac viewer also bounds every VideoToolbox decode call. See the
platform documents.

### Audio

48 kHz 16-bit, coded losslessly with `lpcm-rice` (fixed linear prediction
and Rice codes, like FLAC's fixed predictors). Silence costs a few bytes a
frame and a desktop's stereo sound about 0.5–1 Mbps; coding it costs
microseconds. The decoded samples are exactly the captured ones, which
lets tests check audio bit for bit. Opus is gone.

* **5 ms frames, sent as soon as captured.** Captures hand over 5 ms
  frames (240 samples); each is its own packet the moment the capture
  period holds it. WASAPI loopback runs at the endpoint's engine period
  (10 ms on most drivers, and low-latency `IAudioClient3` streams refuse
  the loopback flag), so a Windows host sends two packets every 10 ms.
* **Surround when both ends have it.** A viewer whose output device has
  more than two channels asks for them (`StreamRequest.audio_channels`);
  the host captures the device's own layout up to that (WASAPI channel
  mask, PulseAudio channel map) and sends `lpcm-rice` version 2, 80
  samples a packet for 5.1 and 60 for 7.1 so the worst case always fits
  a datagram. Stereo is still version 1, byte for byte, so older viewers
  are unaffected. The encoder budget keeps 1.2 Mbps more of the wire for
  each pair beyond stereo.
* **Each channel to its own speaker.** Streams carry WAVE order (FL FR FC
  LFE BL BR SL SR); output devices name theirs, and the orders differ
  (CoreAudio's 7.1 is L R C LFE Ls Rs Rls Rrs, sides first; ALSA's 5.1 is
  FL FR RL RR FC LFE). The viewer reads its device's layout (macOS:
  `kAudioDevicePropertyPreferredChannelLayout`, expanded by AudioToolbox,
  or the preferred stereo pair; Windows: the endpoint's mix-format channel
  mask; Linux: the ALSA channel map that PulseAudio's and PipeWire's
  plugins report, else ALSA's default order) and routes every channel to
  the speaker of the same position (`latch_codec::audio::mix_onto`). A
  speaker the device lacks is folded in: back and side surrounds stand in
  for each other (−3 dB each when both land on one), the rest go to the
  front pair at the usual downmix weights (centre −3 dB, LFE −6 dB), and
  a mono device gets the stereo mix's mean. A device that names no
  speakers is taken as Latch's layout of its channel count.
* **Windows output at the engine's smallest period.** cpal's WASAPI
  stream stays at the engine's default 10 ms period. The Windows viewer
  opens the endpoint itself as an `IAudioClient3` shared stream at the
  smallest period the driver offers (2.67–3 ms on drivers that support it;
  others offer only their default), or the period another stream already
  fixed, keeps two periods written ahead (`LATCH_AUDIO_PERIODS`) on an
  MMCSS "Pro Audio" thread, reopens on a lost device or a new default, and
  falls back to cpal on any failure (`LATCH_AUDIO_OUTPUT=cpal` forces
  it).
* **A jitter buffer that follows the path.** The viewer measures arrival
  jitter and capture bursts and steers the least audio left before each
  device pull to a 2 ms margin plus whatever part of a burst the jitter
  does not already spread, trimming playback speed by at most 0.2 % to
  hold it. A real underrun (not the host going quiet: it sends nothing
  for silence) widens the margin by 2 ms, which narrows again by 0.1 ms
  a second. Devices that state their range get a 2.5 ms buffer
  (`LATCH_AUDIO_BUFFER` overrides it, in frames or `default`).
* **The viewer's microphone, opt-in.** With **Send my microphone** on and
  a host that answers with `Accept.mic`, the default input is mixed to
  mono, resampled to 48 kHz and sent as `lpcm-rice` 5 ms frames with the
  same parity as host audio (datagram kind 13). A Linux host makes a
  "Latch Microphone" source itself (a null sink and a source over its
  monitor, through PulseAudio's native protocol or PipeWire's pulse
  server) and removes it after the session. Windows and macOS cannot turn
  sound into a microphone without a driver, so the host plays it into a
  virtual cable's output (VB-CABLE, BlackHole, or the output named by
  `LATCH_MIC_DEVICE`) and programs record the cable's input.
* **Estimated capture-to-output timing.** `LATCH_TEST_AUDIO=clicks[:period_ms]` on a
  host replaces its sound with a click on every wall-clock quarter second,
  delivered a device period at a time; `LATCH_AUDIO_PROBE=1` on the viewer
  estimates when each click reaches output (callback time plus the
  device's reported playback delay, assuming synchronized wall clocks) and `view` reports them as
  `audio_click_ms`. `LATCH_AUDIO_TRACE` on a Windows host logs loopback
  packet sizes and QPC ages.

### Gamepads

A viewer that offers gamepads gets as many slots as the host can emulate
(`Accept.gamepads`) and sends each pad's whole state whenever it changes;
the host plugs a virtual pad in on a slot's first state, applies only the
newest state of each slot, and unplugs every pad when the session ends.
Rumble and light-bar colours come back the same way ([PROTOCOL.md](PROTOCOL.md#gamepads-912)).

| Host | Virtual pads |
| --- | --- |
| Windows | ViGEmBus (installed by setup, [WINDOWS.md](WINDOWS.md#gamepads)), spoken to through `DeviceIoControl`: Xbox 360 pads (XInput) and DualShock 4 pads, up to 8; rumble, the player LED and the light bar come back from the driver's notifications |
| Linux | `/dev/uinput` devices identical to what `xpad` makes for an Xbox 360 pad, up to 16, with `FF_RUMBLE`; needs write access to `/dev/uinput` ([deploy/native/README.md](../deploy/native/README.md#gamepads)) |
| macOS | none: a Mac cannot add a gamepad without a driver, so `Accept` says why in `gamepad_problem` |

| Viewer | Reading pads |
| --- | --- |
| macOS | GameController (Xbox, DualSense, DualShock 4, Switch Pro, MFi), value-changed handlers on Latch's own dispatch queue; rumble through Core Haptics, an engine per handle and trigger motor |
| Windows | Windows.Gaming.Input polled every millisecond: `Gamepad` (with trigger motors), and Sony pads as `RawGameController` in their DirectInput layout |
| Linux | evdev, a thread per pad blocked in `read`; `FF_RUMBLE`; the host's own virtual pads are skipped |

A pad keeps its slot while it is connected, across streams. A test-pattern
host (`LATCH_TEST_PATTERN`) plugs pads into an in-memory bus that rumbles
back each pad's triggers, unless `LATCH_TEST_GAMEPADS=system` sends them to
the platform's own. None of this compiles C: ViGEmBus's IOCTL layouts are
declared in Rust from its MIT-licensed headers, and evdev/uinput are ioctls
through `libc`.

### Viewer negotiation and decoder load

Auto can rank colour before codec order when the host advertises
`streamer.prefer_color` and the viewer opts in. HEVC currently carries ten-bit
SDR and HDR10; other implemented codecs negotiate SDR8. Without opt-in the
viewer's order is unchanged. Each encoder fallback refits width, height, FPS
and bitrate from the original request, including software limits, and undoes
and repeats desktop preparation for that geometry.

Linux viewers load `libva.so.2` and `libva-drm.so.2` at runtime and probe
DRM render nodes for H.264 Constrained Baseline/Main/High and HEVC Main/Main10.
Each profile needs a VLD entry point, a compatible render target format, and
a successfully decoded 256×256 probe with the expected visible pixels.
Driver surface attributes supply maximum width and height per profile, with
config attributes as a fallback. Unknown dimensions remain unknown. The
probe establishes codec availability and reports driver limits, without
claiming a measured maximum frame rate. Every candidate render node retains
its own limits. Requested dimensions filter codec offers, accepted dimensions
are checked again on open, and coded SPS dimensions are checked before surface
allocation. Ten-bit colour is offered only with a matching Main10 probe.
AV1 remains unavailable through this Linux decoder.

On an EGL/GLES wgpu renderer with DMA-BUF import support, VA-API exports
separate Y and UV planes. EGL imports their DRM formats, offsets, pitches and
modifiers. Surface leases prevent reuse while the renderer or submitted GPU
work holds a picture. Export/import failure disables this path for the
process. Vulkan, unsupported EGL layouts and cropped pictures use NV12/P010
readback into reusable CPU buffers and the existing renderer upload path.
The Linux GUI offers ten-bit SDR when the renderer supports normalized
16-bit textures. Native Linux HDR display output requires further work.

Hardware calls run on a decoder worker, with a five-second open deadline and
a one-second access-unit deadline. A timeout disables further VA-API opens
for the process, bounding abandoned workers. The capability probe has its own
ten-second deadline. A Baseline hardware failure switches to `latch-codec`
and requests a keyframe when needed, including after successful hardware
pictures. A fatal High/HEVC device failure immediately enters the existing
codec renegotiation chain, eventually reaching software Baseline. Bitstream
errors invalidate references and request a keyframe. Missing libva, missing
render nodes and failed probes leave software Baseline available.

`LATCH_LINUX_DECODER=software` disables VA-API. The default is `auto` and
`vaapi` enables the same probed path. `LATCH_VAAPI_DEVICE` names the DRM render
node to try first, followed by the other render nodes. Pure-Rust parser,
negotiation, crop/layout and fallback tests run without GPU hardware on all
CI platforms. On Linux, `LATCH_TEST_VAAPI=1 cargo test -p latch-stream
video::vaapi` requires a working VA-API device. Real Intel/AMD decode and
presentation verification remains outstanding.

The queue of encoded frames waiting for the decoder is bounded by age: three
frame intervals, clamped to 12–100 ms. Up to eight frames may wait, so the
burst a network stall releases is decoded rather than discarded. Past either
bound the reference chain is reset and a keyframe requested. A rolling 60-service-time window
reports overload when mean decode time exceeds the negotiated frame interval
by 10%, once per attempt. Stats show decode p95 over each stats window and a
local throughput estimate. This detection does **not** lower the host's frame rate:
the viewer says what this machine manages and the person lowers the frame
rate or size in Stream settings. Sustained overload can still cause repeated
recovery; adapting the session by itself is on the [roadmap](ROADMAP.md).

Failures are scoped by remote address/port, codec, and failure kind: opening
failures cool down for five minutes, runtime decode failures for 15 seconds.
The next connection after expiry attempts the codec again; a running fallback
session does not upgrade itself. A codec is tried only once per connection's
fallback chain. Apply and reconnect clears cooldowns immediately. Baseline
software decoding remains available. Hardware capability probes remain cached.

Stats count UI-observed pictures, not scanout. Modelled delay adds
host processing, half RTT, measured assembly/queue/decode/preparation and half
refresh; GPU completion, compositor, panel response and scanout are unmeasured.

## Decisions

* **No pairing.** The control service's authorization (the same Tailscale
  user ID; for a tagged host, the tailnet's owner or its `allowed_logins`)
  already guards `/v1/update`, which replaces the executable. A Windows
  host also checks a pushed update against the SHA-256 GitHub publishes
  for that release, so the sender cannot choose the bytes; there is no
  publisher signature yet. The session token travels inside WireGuard.
* **UDP + FEC + a recovery frame on loss**, as GeForce NOW does, keyframe
  where the encoder cannot, without RTSP, ENet or a second encryption layer.
  Input is reliable by redundancy, not timers.
* **Newest frame wins** on the viewer (`FrameSlot`), every frame is decoded
  (P-frames depend on each other), and a lost frame stops decoding until a
  recovery frame that predicts from the last frame delivered, or a
  keyframe. The software encoder makes one by rolling back to that picture
  (bit-exact), AMF from a long-term reference; the others answer with an
  IDR. Recovery maps pair pipeline generation with picture identity, so
  queued frames cannot alias a reference after capture reconstruction
  ([PROTOCOL.md](PROTOCOL.md#recoveryrequest-14-and-recoveryack-15)).
* **Frames on a fixed grid.** Every pipeline takes frames on a grid 1%
  faster than the asked-for rate (`video::Pacer`): capture and encode time
  comes out of the interval instead of adding to it, a screen refreshing at
  the stream's rate gives up every refresh, and a late frame moves the
  grid rather than bursting.
* **The bitrate follows the path** (`engine::congestion`). One controller
  owns the wire budget, including audio, FEC and headers. It starts at the
  requested ceiling. New viewers report every 50 ms; display statistics retain 250 ms windows.
  Host pings run at 50 Hz.
  Two pressure windows trigger cuts; one does when under half of what was
  sent arrived and the rest is lost or queued (a capacity drop). Queue cuts
  repeat no faster than max(50 ms, base RTT), loss cuts only after a base RTT
  plus a report window. A smoothed positive delay gradient detects filling
  queues; sustained delay above max(6 ms, 10% of base RTT, 3× measured ping
  jitter) also counts, so 5–15 ms of Wi-Fi jitter is not a queue. Recovered
  random loss alone does not cut bitrate. Moderate queue-free residual damage
  first raises FEC, then tests whether loss improves with reduced offered load. Parity has a base-RTT floor of 15% at 40 ms and 20%
  at 100 ms (30% cap), with fast attack and a two-second decay after bursts.
* **Send immediately on clean paths.** The bounded two-frame sender remains,
  with no token-bucket delay on a clean path. Queue or severe sustained loss
  enables the 2.5×-budget bucket. Radio loss repaired by FEC
  does not enable pacing. Clean feedback releases pacing after at least
  200 ms (two report intervals for slower peers).
* **Data before parity.** A frame's data shards go out before any parity
  is computed, so a frame that loses nothing never waits for FEC.
* **Media threads at high priority** (user-interactive QoS on macOS,
  highest priority on Windows) and media sockets with 8 MB buffers, so a
  keyframe's burst is not dropped by the sender or the receiver.
* **Sound stays bounded, and short.** The fixed 40 ms preroll is gone:
  the jitter buffer above holds what the measured path needs, with the
  same ±0.2 % clock steering and a 120 ms hard cap trimmed back to the
  current target. The 30-minute ±300 ppm simulation still has no gaps and
  no trims, at about 27 ms buffered on its old jittery path and 11–15 ms
  on a quiet LAN-like one. No second controller is added.
* **A Mac viewer never copies a picture**: VideoToolbox decodes into an
  IOSurface that the renderer samples as Metal textures.
* **One viewer at a time.** A new authorized viewer takes the host over;
  the old one gets `End` code 1.
* **Display sizing** stays where it was: a Linux desktop that follows its
  viewer is resized by the engine (`xrandr` semantics through RandR) or by
  the owner's prep command; Windows sets the virtual display's mode; macOS
  captures the main display (or `LATCH_MAC_DISPLAY`) and lets
  ScreenCaptureKit scale it. A viewer can name another display
  (`display::list`, `StreamRequest.display`): Windows then duplicates that
  output and leaves the virtual display and topology alone, macOS captures
  it, and Linux captures that RandR monitor's rectangle of the root without
  resizing. Linux's CPU pipeline also emits the negotiated
  output size when the display stays at its native size, for software, NVENC
  and VA-API alike. It aspect-fits with centred black bars, using the same
  content rectangle for pointer injection as Windows and macOS. Reductions
  use area averaging fused into BGRA-to-BT.709 limited-range YUV420 conversion;
  enlargements use bilinear filtering. Filter footprints map capture damage
  into output damage, including neighbouring chroma pairs. Equal-size frames
  retain the original SIMD conversion path. Separate cursor shapes and
  hotspots use stream-content pixels. Free-mouse viewers place the cursor at
  their local pointer on every host platform.
  Conversion, software encoding, and the capacity probe share a worker
  selection function of available parallelism and output dimensions. Its
  conservative table remains capped at four pending a quiet-machine sweep;
  Linux CPU quotas are respected, and Apple Silicon uses its performance-core
  count. Two-core machines still use two workers. See the
  [measurement record](measurements/2026-11-09-linux-output-size/README.md).

## Compatibility

Latch 5 and 6 viewers and hosts speak protocol 1. The version 6 public
relay extension is negotiated only between peers that advertise support.
A Latch 5 viewer that finds a Latch 4 host (no `streamer.protocol` in
`/v1/status`) sends it the update it already knows how to send
(`/v1/update`, as a Mac does for Windows hosts today) or says which
machine to update. A Latch 4 viewer finds no GameStream port on a Latch 5
host and offers its own update. Pairing data, Sunshine's configuration and
the engine folders of Latch 4 are removed by Latch 5's setup.

Gamepads are negotiated inside protocol 1: a viewer before them never
asks (`StreamRequest.gamepads` defaults to false), and a host before them
never answers with slots (`Accept.gamepads` defaults to 0), so neither side
ever sends datagram kinds 9–12 to a peer that does not know them.

## Evidence

The 5.0 rewrite shipped on 2026-10-29, replacing the external streaming
engine and viewer libraries with protocol 1 and the in-process Rust crates.
The [dated evidence](measurements/2026-10-29/) records conformance,
encoder comparisons and short loopback/tailnet runs. Those results apply
to the recorded revisions, hardware and content, not all desktops or
hours-long sessions. Current codec and native-build checks live in CI.

What shows the engine is right, other than its authors:

* the ITU-T H.264.1 conformance bitstreams: the decoder's output must equal
  each stream's reference YUV byte for byte;
* VideoToolbox and FFmpeg as independent decoders of the encoder's
  output, which must equal the encoder's own reconstruction byte for byte;
* known-pixel test patterns through the whole pipeline, compared after
  decode;
* bit-exact audio through the whole pipeline;
* the impairment simulator (loss, bursts, reordering, duplication, delay)
  for the transport, and measured loss on the real campus Wi-Fi path;
* input-to-decoded-picture timing measured on the viewer's own clock: the viewer
  presses a key, a pattern on the host changes colour, the viewer times
  until it decodes the change;
* CPU, memory and energy of host and viewer, measured against Latch 4 at
  the same size, frame rate and bitrate;
* `scripts/audit-native.sh`: no C, C++ or assembler in the build graph.

## Cadence and transport budgets

All host pipelines use the same start-to-start deadline scheduler. Work time
is inside the interval; a deadline missed by a whole interval resets the grid to the
actual capture time, without catch-up trains (`video::Pacer`). Idle repeats
preserve their 100 ms phase across late wakes, resetting after a whole missed
idle interval.
Encoding feeds a two-frame bounded channel to a dedicated video sender;
backpressure preserves encoded references. When pacing is active, the producer
does not capture the next picture while an encoded frame is already queued
behind the one being sent (`PACED_BACKLOG`), so one frame can be encoded
while the previous one leaves. This bounds
encoded backlog without dropping P references or generating extra recovery IDRs;
a large keyframe still takes its unavoidable wire serialization time. Clean
paths retain the two-frame overlap. Cancellation may discard queued
media. Audio, input acknowledgements and pings bypass the video queue.

Video uses a wire-byte token bucket at its current pacing rate. A new frame
starts with at most two datagrams or 1 ms of credit plus one datagram,
whichever is larger. While a batch has packets left to send, elapsed send
credit survives ordinary late scheduler wakes, capped at 30 ms plus one
datagram. The 30 ms bound is the smallest that preserves the existing
16 ms and 30 ms injected-clock rate checks at 2, 20 and 60 Mbps. Once a
batch completes, new accrual has the ordinary one-ms cap and unspent tokens
remain available. A gap longer than 30 ms restarts with the ordinary burst,
even mid-frame. A rate change also clamps saved credit immediately.

With 1,200-byte datagrams and the 80-byte tunnel allowance, the ordinary
burst ceilings at pacing rates of 2, 20 and 60 Mbps are 2,560, 3,780 and
8,780 bytes. The corresponding ordinary-late-wake ceilings are 8,780,
76,280 and 226,280 bytes. These are wire-byte credit ceilings before
rounding down to whole datagrams. Every gap longer than the credit bound
uses the ordinary ceilings. The Windows sender uses `std::thread::sleep`.
Rust attempts a high-resolution waitable timer on Windows 10 version 1803
and later, with a fallback to `Sleep`.
[Rust's Windows sleep implementation](https://github.com/rust-lang/rust/blob/1.99.0/library/std/src/sys/thread/windows.rs#L117)

The sender spends credit in batches through the normal GSO/USO/batch fallback
and reloads the current rate between batches. Waits request at most 1 ms
using an absolute deadline. The encoder allowance reserves audio separately,
then compounds header overhead with rounded FEC.
For a loaded sender, cuts use the lower of budget and successful send rate,
bounded by received throughput +7%. A sender is application-limited after
four reports below 80% of what the encoder may spend (including zero-byte
windows); a delay cut while idle starts from that allowance, without
mistaking sparse throughput or a CPU sender stall for network capacity.

Idle keeps the budget and the encoder's target. A pause proves nothing about
the path, and a content-limited encoder must not spiral its own target down;
motion after it resumes at once, unpaced on a clean path, as on any frame.
Instead the idle sender (below half its budget, application-limited) measures
the path: after 500 ms, two probe trains 200 ms apart and then one a second,
each 6 to 16 full datagrams, skipped below an 8 Mbps retained budget,
to viewers that time them (`docs/PROTOCOL.md`, "Probe"). Sequence positions
are deduplicated; only ordered contiguous prefixes count. The median
eight-position packet pair estimates capacity, gated by measured pair
variation (dispersion ≥2× variation) and the dominant-stall rejection.
Two fresh confident trains below 3/4 of the budget need corroborating
queue delay or lossy delivery deficit to lower the target to 90% of the
faster train. An ordinary ping immediately behind each burst independently
measures its FIFO delay, relative to baseline RTT and learned ping jitter.
It does not pollute the minimum-RTT samples. Without corroboration only
resume pacing changes: 105% of a measured rate, and not under 125% of the
best rate received in a window of the last second. When no train has been
measured the pace is a guess: the larger of 1/20 of the budget and twice the
best rate the viewer reported receiving in any window of the last second. A
stream can move at under half its budget, and its own frames then cancel the
trains that would have measured the path, so what it delivers is the only
evidence there is. A guess rises with delivery and is replaced, upwards too,
by the first train measured after it. Lossless delivery filling the pace
validates 25% increases once
per feedback horizon. A fresh fast train removes this limit; later fast
trains restore confirmed cuts. An older viewer gets no probes or limit.

Probe trains and their trailing echo run on the video sender's token bucket,
consume modeled wire credit (payload plus 80 bytes per packet), and count in
successful sender throughput. Idle probes use the current pace, or 2.5 times
the current budget on an otherwise unpaced path. Fresh pressure or queued video
cancels a train between batches, so motion takes priority. Controller pressure
also prevents scheduling trains. Pacing may limit measured dispersion: a slow
sender is not evidence of independently corroborated network capacity.
An
underfilled stream that a standing delay cut while it still delivered
everything (someone else's queue, or pings starved on a loaded machine) gets
back the budget it had once the delay has been gone for a second: it can
never fill a probe to prove it, and trains still guard its idle resume. Likewise an
idle stream (or an underfilled one at the floor), a second after any cut's
signal clears, regrows by time but never past the last pair of trains (only
fresh trains act at all); the regrown rate stands in for capacity, so motion
probes up from it in bounded steps. Viewer reports carry the budget as
`wire_budget_kbps`.
Each cut removes 20%, or 40% for queue delay above 20 ms or loss above 10%.
Loss in a window that delivered more than is now sent predates the last cut
and is ignored, as is a queue still falling within three horizons of a cut
(horizon: 2× base RTT plus one report window, two for windows ≥200 ms). Sender age above
20 ms is pressure only for a loaded sender. Moderate queue-free damage with
usable send/receive feedback gets 500 ms for FEC adaptation before a reversible
20% reduction of the rate actually sent (a lower unused budget would change no
load), if at least 4 Mbps is sent: a sparse stream's few datagrams a window
cannot tell a policer from chance, so parity and the severe-loss escape cover
it. After a feedback horizon plus one second, the mean sent rate over the test
must have fallen by 10% while filling at least 85% of the reduced budget (so
the budget, not the content, lowered it), and mean raw loss (against its mean
over the 500 ms before, not the window that started the test) by half, or by more than max(35% of it, 2 points) (a
policer over a random-loss floor), to confirm a policer at the rate implied by
the loss that remained; otherwise the budget returns and no further test runs
for three seconds. A confirmed test also records that remaining loss as the
path's random floor: loss within 2 points of it is not a new overshoot while
the ten-second policer memory lasts (legacy/severe-loss cuts retain the
three-second memory). Uncorrected loss above 10% on a loaded sender, unusable
rate feedback and delay pressure retain conservative cuts. An underfilled
encoder is no exemption: >10% loss with under 90% of the sent rate arriving,
sustained for 500 ms or a feedback horizon, is pressure and may collapse-cut
(a short sparse-window spike is not). One coarse (≥500 ms, legacy) window
that is damaged with a queue or >10% loss cuts at once. A 50 ms window holds only a few frames, so
received/sent ratios are too coarse to be a signal on their own; they gate only
the one-window collapse cut, together with loss above 10% or delay above 20 ms. The
ping-jitter estimate starts at 3 ms and learns only from falling delay outside
congestion. An application-limited sender keeps that 3 ms floor after pacing
starts: pacing freezes the estimate, so the floor is the only model of jitter
the idle sender has not learned. Without it, the first scheduling spike turns
a few milliseconds of path jitter into a standing queue and the next cut
adopts one sparse window's send rate. The wire floor is a fixed 1 Mbps, whatever the ceiling, so a
high-quality session can still fit a 2–5 Mbps link.

A cut remembers the sustainable rate (what got through, raised to whatever the
smoothed received rate later shows). Clean feedback grows the budget at
doubling per 400 ms of actual elapsed time between reports, but only to 97% of
that rate (92% for coarse ≥200 ms windows). Above it, one probe is in flight at
a time: first ~2.5 ms of queue per horizon (1–5%), doubling only while probes
survive a horizon, and stepping up to the rate that last congested before
accelerating past it. A probe whose own feedback shows over 3 ms of queue
drops under the held rate by as much as it went over, so its excess drains as
fast as it built. With no throughput or loss feedback at the cut, there is no
estimate and recovery is purely time-based. Reports more than 2 s apart grant
no growth: the controller waits for fresh feedback. A lossless RTT plateau
that persists for 2 s through a fourfold budget reduction with no local queue
is a route change: the RTT minima are rebased and the estimate dropped; loss or
a falling RTT (a draining FIFO) cancels the evidence. Raw legacy loss under 10%
without lost frames is treated as repaired radio loss. Rate setters run between
frames at a 5% target change.
Parity percentages retain the 30% path cap, but every keyframe/recovery block
has at least 30% and four parity shards. Other frames add up to the measured
burst plus two shards (at most their data count); burst evidence is bounded to
32 and decays over two seconds, rounding to zero. Queue overflow is excluded
from burst learning. The packetizer and the encoder allowance share one layout
(`latch_proto::video::fec_parity_shards`), and the server and the transport
simulations share one parity policy (`congestion::FecPolicy`). The allowance
uses 108 bytes per packet for Latch/IP/tunnel overhead, with 4% headroom, and
reserves the protected floor while a keyframe or recovery frame is owed (below
five minimum datagrams per frame slot, that irreducible layout is the
permitted burst), but never more than half the ordinary encoder target: at
1 Mbps and 240 fps the full floor would leave 3 kbps, so a protected frame
there may be one frame's burst over its slot. Normal frames do not pay it. CPU time computing parity is excluded from
unpaced sender queue age. See [FEC measurements](measurements/2026-11-04/transport-fec/README.md).
This remains a heuristic controller; it is not a full GCC implementation.

Linux sends up to 32 datagrams with `sendmmsg` and receives with `recvmmsg`.
macOS receives up to 32 with `recvmsg_x`; sends retain `send_to`, since Darwin's
batch-send API disallows destination addresses on the shared unconnected socket.
Batch storage is reused, and data flushes before parity computation. Batches
never wait to fill. Unsupported batch receive syscalls fall back to scalar I/O
on the nonblocking media socket. Windows retains a scalar socket
fallback. Windows supports USO/URO and Linux supports UDP GSO/GRO,
with ordinary batch/scalar I/O when unsupported. Set `LATCH_UDP_OFFLOAD=0`
to disable segmentation and receive coalescing. Protocol 1 retains
the 1200-byte datagram limit; route MTU discovery/negotiation remains outstanding.

Host and viewer UDP request 8 MiB in both directions through the one
`latch_proto::transport::configure_socket` helper. Readback is logged once per session, including Linux sysctl caps;
macOS requests fall back by halves if the kernel refuses a large buffer.
Status and viewer stats expose requested/applied bitrate, sender queue age,
send errors/retries and effective socket buffers. Bytes count successful
media sends only. ENOBUFS uses the platform constant (10055 on Windows).

Viewer receive uses mio readiness and bounded 32-datagram syscall batches on
Linux/macOS (64 scalar receives on Windows). Completed
frames on the batch path are handed to decode after each datagram, before
processing the rest of the batch or waiting for readiness: waiting at the
end of a receive batch held each complete picture until the next datagram. Video
shard storage is reused with a 4096-buffer pool cap. Decode queues are
bounded by eight frames and three frame intervals (12–100 ms) of age; exceeding either restarts at an
IDR rather than decoding an increasing backlog. A missing initial IDR
triggers recovery on the first complete P-frame, or after 500 ms without
a delivered keyframe. Requests retry at max(50 ms, RTT), and at once when
another frame is lost meanwhile: that may be the recovery keyframe itself.
The host (`KeyframeAnswers`) ignores a repeat naming the same last-delivered
frame until a keyframe sent since the request it repeats has been out for a
round trip (smoothed RTT less its variation), or 250 ms have passed, so
duplicates cost nothing and a lost recovery keyframe is replaced about a
round trip later instead of after a 250 ms hold. Requested
IDRs are gated at max(50 ms, smoothed RTT), including requests that advance
the last-delivered frame. See [transport measurements](measurements/2026-11-03/transport/README.md)
for codec-free throughput, CPU, FEC and impairment results and remaining targets.

## HDR and ten-bit colour

Protocol 1 negotiates colour separately from codec. Missing capabilities keep
8-bit limited-range BT.709. HEVC can select SDR Main10 or HDR10 (BT.2020 NCL,
PQ, P010); the pipeline reports its actual colour before Accept. A colour
change after Accept ends the session for renegotiation. Every HDR IDR has
ST 2086 and content-light prefix SEI, independent of the encoder API.

Windows retains scRGB from DuplicateOutput1 for Advanced Color desktops.
The cursor/rotation shader converts linear scRGB (80 cd/m² per unit) to a
1000-nit BT.2020 PQ RGB10 target for HDR, or sRGB RGB10 for ten-bit SDR;
the video processor converts that to P010 with explicit DXGI colour spaces.
For an SDR stream from an HDR desktop the same pass maps wide-gamut colours
into BT.709 toward their own luminance and rolls highlights up to the
1000-nit ceiling off above 75% of SDR white on max(R,G,B), keeping hue
(`latch_proto::color::hdr_to_sdr`). A failed Output5 acquisition
does not silently retry the legacy BGRA conversion that can produce black.

Viewers retain P010 through decode and upload. macOS uses a native float
Metal sublayer for HDR video because egui 0.31 fixes its main framebuffer
to BGRA8; ten-bit SDR goes through the wgpu P010 shader. HDR is rendered in extended linear Display P3, with a smooth
highlight shoulder at the display's current EDR headroom. Windows uses a
10-bit child swapchain: G2084/P2020 for an HDR display and PQ stream, G22/709
for SDR Main10. PQ falls back to a GPU SDR tone map only when native HDR
cannot be presented, never because egui draws over the picture: the Mac
layer sits below egui's, which is cleared to transparent over the video,
and Windows cuts the child window around everything egui draws above the
background layer. Native renderers sample chroma at the colour
description's siting (`chroma`, left by default). Eight-bit SDR keeps the
existing rendering path. Which presenter draws is decided per stream (and
again when the window's display changes and the change has held for
300 ms, at most once a second), never per frame. A display that cannot be
read for a moment is no change. A native
presenter that fails hands that stream to wgpu instead of being retried
frame by frame, which alternated surfaces (a flicker).

See [colour measurements](measurements/2026-11-03/hdr/README.md) for tests,
measurements and platform limitations.

The maximum encoded frame is `255 * 64 * MAX_SHARD_LEN` bytes on both sides
(19,127,040 with the protocol-1 layout). Packetization returns an explicit error
rather than silently consuming a frame number. The sender logs an oversized
frame, invalidates its recovery history, requests a keyframe and skips dependent
queued pictures until an intact keyframe. Protocol 1 layouts are unchanged;
older hosts already cannot emit frames beyond this limit.

### Viewer input scheduling and recovery

Mouse and keyboard dispatch runs synchronously in egui-winit's OS event
callbacks on macOS, Windows, X11 and Wayland, after its normal input
translation and before a repaint. A small local patch in
`vendor/egui-winit` exposes those callbacks and the input-snapshot boundary.
The last completed UI frame publishes focus, capture and pointer geometry.
Both dispatch paths share held keys and buttons and call the existing
session sender, which flushes immediately. Reliability retries run
independently of the UI. Gamepads send directly from their platform thread.

Each early event leaves one receipt for its UI frame so the frame can
observe input without sending it again. Captured movement and wheel
fractions carry between both paths. Absolute movement and stable free-mode
clicks also dispatch immediately. A click that changes capture or hides the
toolbar, a clipboard operation, an input-method event, a local host-key
shortcut or a geometry change closes early dispatch until the frame handles
it and republishes policy. Subsequent events wait behind that operation.
Focus loss releases held input immediately and closes dispatch. Text and
clipboard synchronization retain the UI path.

At debug level, `input from the OS event` reports the average and maximum
lead over the UI frame, once per ten seconds of input. This measures the
interval between dispatch and frame receipt. It includes time until redraw. Network-delivery and input-to-photon
measurements, real OS-event-to-datagram timing at 60 and 240 fps, pointer-lock behavior and
IME interaction require live viewer/host hardware.

After an unexpected transport drop, the viewer reconnects with fresh
negotiation until stopped, with 2/5/10-second waits followed by jittered
20–30-second waits. Discovery returning online or an event-loop gap over ten
seconds advances a pending retry. Explicit refusals, takeover and host ends
stop automatic retries. The overlay offers Reconnect now and Stop.

Version 6 can negotiate an additional public native TLS route when UDP
video is missing or persistently damaged. `latch-relay` sends complete
encoded frames on a video lane and sound, microphone, input, gamepads and
small control messages on an independent priority lane. ChaCha20-Poly1305
and HKDF-SHA256 protect records end to end. Session tickets and keys travel
only over the original authorized Tailscale control connection, which
remains required. Frame, queue and acknowledgment deadlines bound stalls.

Relay daemon and client media lanes use level-triggered socket readiness
(`poll` on Linux and macOS). Windows checks readiness with zero-timeout
`WSAPoll` calls between high-resolution Rust sleeps of at most two milliseconds.
This preserves the previous timer precision for control pacing, keepalive,
quota completion and deadline housekeeping without relying on the system tick
or a process-wide timer-resolution request. The engine does not call
`timeBeginPeriod`. Local send queues and cancellation signal connected loopback
UDP sockets. If those sockets are unavailable, one warning is logged and the
lane or daemon uses the two-millisecond polling cap.
Buffered TLS plaintext is processed before waiting, including WebSocket
headers and frames. Each turn retains the bounded socket I/O and per-lane
record budget. Active connections and pending quota reservations retain a
two-millisecond maximum housekeeping wait. An empty daemon waits for new
connections, cancellation or session expiry. The optional HTTP listener and
in-flight probe/challenge requests participate in the same readiness wait.
Their header, response, accept-retry and admission-window deadlines bound
that wait. WebSocket controls retain a
two-millisecond cadence to bound control-flood responses. DNS, address racing and carrier setup
still use short bounded polling waits during connection establishment.

Relay failure restores UDP routing. Recovered-UDP switch-back and grant
lifecycle refinements remain under validation.
[Wire and security details](PROTOCOL.md#public-tcptls-relay)

After acceptance, a first decoded picture must arrive within eight seconds.
An eight-second decoded-media stall also ends a session despite TCP
keepalives. Pending reliable input is bounded at 4,096 events, and five
seconds without input acknowledgment progress ends the session so teardown
can release held keys and buttons. Host injection failures hold the
acknowledgment and retry the event for up to two seconds. Persistently
failing events are then dropped so later input can proceed.

Windows Media Foundation workers answer their caller within a deadline: one
second for an access unit, five to open a decoder, twenty for the
once-per-process probe of every codec. The GPU-copy fence keeps its
two-second bound. A decode that times out abandons that worker, reports an
error (which requests a keyframe), and opens a fresh decoder on the next
keyframe. At most three worker timeouts per ten minutes are allowed
process-wide; reopen failures feed the existing Baseline software fallback or
modern-codec renegotiation. Driver resources stay owned by the abandoned
worker until its call returns. This recovery has not run against a hung
driver on Windows hardware.


## Media path policy

### Audit of 6.0.0 before this change

This audit follows `bf5da0d` in `crates/stream/src/session.rs` and
`crates/engine/src/server.rs`. Both peers must advertise `relay`, and both
must have a configured relay endpoint. The viewer alone requests fallback.
The authorized Tailscale TCP connection remains required on both paths.

- The viewer samples its decoded-picture publication clock every control-loop
  turn, normally within 5 ms. One second after acceptance without a decoded
  picture, or one second after the last decoded picture, requests relay.
  Traffic, control keepalives and complete undecoded frames do not reset this
  particular clock.
- Viewer feedback is published every 50 ms. Any lost frame or more than 1%
  unrecovered packet loss starts a damage timer. A positive-fps window without
  either clears it. Two uninterrupted seconds requests relay. Alternating
  clean and damaged windows can therefore postpone this trigger indefinitely,
  even at sustained 15–18% raw loss.
- A separate five-second timer requests relay when busy video has a positive
  wire budget below 60% of the requested rate, a request of at least 4 Mbps,
  a positive encoder target and receive frame rate, and useful video filling
  at least 75% of that encoder target. Other windows clear this timer.
  Successfully repaired FEC loss alone does not trigger fallback.
- `Request` starts the host's single cancellable grant worker. Fresh grant,
  keys and attachment tickets precede `Offer`, both endpoints' two-lane E2E
  readiness, then `Ready`, `Commit`, `Committed`. The viewer activates on
  `Commit`, the host on `Committed`. The entire application negotiation has a
  six-second deadline. Route revisions discard queued old-route frames and
  request an IDR. Encoding, audio sequencing and the authorized session survive.
- Preparation errors, transport close events, ordinary send failures and a
  six-second negotiation or direct-handoff timeout reset routing to UDP and
  send `Abort`. The first two failures wait two seconds before retry. The
  third and later wait thirty seconds. A relay active for thirty seconds, or
  ending with its finite grant spent, resets the failure history. The host
  answers an early request with `RetryAfter`, clamped to 250–30,000 ms.
  Priority queue exhaustion is fatal and ends the session to release input.
- The host proactively renews at 250 ms before grant expiry, or after delivered
  video and 75% of the advertised combined byte allowance. `Renew` restores
  UDP, resets the failure count to one, and waits two seconds for fresh
  authorization. This is a break-before-make renewal.
- Only the host reassesses UDP. After a minimum sixty-second relay dwell it
  announces `UdpProbe`, waits 100 ms, and sends one second of 1,200-byte UDP
  padding at 125% of requested kbps, with a 2 Mbps minimum. The 50 Mbps
  probe ceiling excludes requests above 40 Mbps. At most 2,048 packets leave
  per control turn. Each probe start doubles the next interval up to 600 seconds, including
  successful attempts.
  Viewer reports arrive after two seconds. Host expires a missing report
  after three seconds. Qualification requires every local send attempted,
  at least 99.5% of unique packet positions received, a receive span of
  800–1,200 ms, and measured wire delivery at least 90% of the probe rate.
  Duplicates and missing tails count correctly. `Direct` makes the viewer
  close relay and reply `DirectReady`, after which the host returns to UDP.
  A successful handoff resets failures and sets a two-second retry cooldown.
- Host transitions out of relay release all held keyboard/mouse input and
  unplug virtual gamepads. The viewer resends pad snapshots. Acknowledged
  held keyboard events have no equivalent replay.
- The viewer toolbar and performance panel display “Latch TLS relay” while
  active. Otherwise they display discovery's Tailscale path label. There is
  no typed path-change event or visible fallback reason.
- Independent watchdogs still end a session after eight seconds without its
  first decoded picture, an eight-second decoded-picture stall, five seconds
  without reliable-input ACK progress, or ten seconds without host traffic.
  Reliable input is bounded at 4,096 events. Explicit Stop and takeover retain
  their original end codes.

The field report recorded approximately 1.81 seconds from UDP denial to
relay selection and 4.96 seconds from a video-ACK close to a fresh active
relay. Video ACK expiry itself belongs to `crates/relay`, whose lane RTT
plus four variations is bounded between its stall deadline and five seconds.
[Field measurements](measurements/2026-11-14-latch6-field/REPORT.md)

### Policy in this revision

Every changed trigger, timer, probe, queue-failure response and held-input
handoff requires `media_path_policy` in both request and accept. A 6.0.0
peer in either role retains the one-second published-picture stall trigger,
two-second continuous damage trigger, five-second busy-rate trigger,
sixty-second relay dwell, 600-second maximum probe backoff, 50 Mbps probe
ceiling, two-second handoff cooldown and held-input cleanup from the audit.

The negotiated viewer requests preparation after 350 ms without any valid
UDP datagram. A working UDP exchange uses zero relay grants. A previously
working stream requests recovery after `max(700 ms, 2 × RTT + two frame
intervals)` without a complete encoded frame. Ongoing UDP video-shard progress defers
this trigger while congestion control drains an oversized frame, unless
independent UDP damage has been proven. Before the first complete
frame the floor is one second. Decoder stalls continue to use the independent
eight-second picture watchdog. Recovery after a relay close waits
`max(100 ms, RTT + 150 ms)` for a fresh UDP picture.

Capacity limiting alone keeps UDP. When the negotiated relay is available,
a previously unmeasured UDP path starts paced at at most 8 Mbps, then grows
under congestion control. A measured UDP return retains its proven start rate.
The host congestion controller first
reduces the actual offered load below demonstrated delivery so a shared FIFO
can drain. Within a trailing period of at least 300 ms, or the feedback
horizon on a distant path, covered receive time of at least three quarters
of that period across at least two reports with 10% time-weighted mean raw
loss at less than 80% of previously delivered throughput establishes residual
UDP damage. Report
intervals bound each sample's credit and partial edge windows are clipped.
Windows with more than
100 ms of local send backlog cannot establish this evidence. Standing delay
above `max(20 ms, 20% of baseline RTT)` suppresses it. Evidence lasts three
seconds while fresh window-mean loss remains at least 3%.

The controller also compares loss before and during explicit reduced-rate
tests. Four reduced-rate windows with at least 10% mean raw loss, little
improvement in loss and an actual offer below 90% of the previous offer
supply evidence. Completed tests remember residual random loss for three
seconds and queue-free policing for ten seconds. A nearly lossless policing
test must retain at least 80% of its previous baseline RTT, excluding a FIFO
whose standing queue drained. Negotiated tests cover offers down to 1 Mbps.
Legacy tests retain their 4 Mbps minimum. A reduced ceiling budget alone
cannot select the relay.

The viewer integrates actual receive windows over a trailing second and
requires at least 800 ms of feedback. Eligibility plus fresh damage in a
quarter of the window, or repeated losses of at least three frames and 10%
of delivered-plus-lost frames with 10% mean raw loss, requests recovery.
Alternatively the busy stream must spend three quarters of that window below
60% of the request, with at least 75% encoder utilization or host telemetry
showing continuous demand. FEC cost alone also requires
10% raw loss. Clean windows count in the denominator. Repaired loss that
preserves picture quality keeps UDP.

The host has one grant worker and a six-second negotiation deadline. If the
trigger clears before the offer, the viewer withdraws preparation. A clean
loss window clears a busy-quality trigger after bitrate and FEC limits
recover or the controller's UDP damage memory expires. Active damage evidence
preserves preparation while a policer backoff still limits the picture.
Loss-based preparation requires clean completed video for `max(100 ms, RTT)` before
clearing, allowing isolated receive/report gaps during continued damage. Its retry
backoff grows through 2, 4, 8, 16 and 30 seconds. The host parks a completed
grant and its host lanes for reuse for at most twenty seconds, bounded by
the grant lifetime. A renewed request gets the cached grant with its current
attempt number. Expiry or session cleanup closes the cached transport.
A fresh negotiated relay starts at at most 8 Mbps and grows against its
requested ceiling. Its complete frames reserve measured outgoing priority
traffic plus 10% and 64 kbps from their wire budget, with a 1.2 Mbps stereo
fallback until the first 100 ms sample. They bypass UDP parity and shard-overhead
reservations. Active failures and grant renewal
use a 100 ms cooldown. Refused video frames feed relay congestion control
even when the bounded queue drains between report samples. Setup failures keep
two seconds and three consecutive failures use thirty seconds.

A host-side relay watchdog tracks real video ACK progress independently of
viewer reports. An outstanding frame without progress for
`max(floor, 2 × ACK RTT + 100 ms) + frame serialization time` resets the
relay and requests fresh authorization or usable UDP. The initial RTT comes
from elapsed TLS, lane attachment and end-to-end key setup. This
conservative seed includes peer setup waits and leaves headroom for
fresh-connection slow start.
Until the first video ACK,
the floor is at least 750 ms and the configured relay stall timeout. Later
ACKs use a 350 ms floor. The complete first-ACK deadline, including
serialization, is capped at three seconds. Serialization uses the largest
pending frame's bytes and the current relay wire budget. The relay crate also
seeds its video ACK estimate from setup. Its ACK watchdog counts recent ordered authenticated
ACK progress as lane liveness even when prior queueing has aged the next
pending frame. Existing frame, write and queue deadlines still apply.
The application watchdog handles silent black holes
before that deadline where RTT permits. Invalid control and bounded inbox
exhaustion still terminate the session.

UDP reassessment starts after a five-second relay dwell. The next intervals
are thirty and then sixty seconds, retained across successful handoffs.
A failed, cancelled or unanswered check waits sixty seconds before retrying.
Each test uses
110% of the relay's mean delivered rate over the previous five seconds,
bounded by a 2 Mbps minimum and 125% of the requested ceiling.
The peak headroom lets the ramp prove 90% of relay delivery even when the
relay reaches the requested ceiling. It never sizes itself
from an unused Auto ceiling. `UdpProbeReady` installs the receiver before the
100 ms settle period. Padding ramps from 25% to 100% over 200 ms and holds
for 400 ms. A delayed sender may finish within another 250 ms. Send
credit follows elapsed time, capped at forty milliseconds of the ramp's
integrated rate, so coarse turns retain their budget and long stalls discard
older credit. The relay encoder subtracts the current UDP reservation from
the same measured budget, retaining a 1 Mbps video floor. During the hold
phase, UDP carries about 110% of the relay's pre-probe mean delivery while relay video retains its 1 Mbps floor. Their combined
wire load briefly exceeds the user's requested bitrate when relay delivery
was near that request. The UDP probe peak is capped at 125% of the request,
with the 1 Mbps relay floor additional. At the 2 Mbps probe minimum the
combined load can reach 3 Mbps.

Loss in completed probe trains cancels the check after a 100 ms receive
grace. Relay ACK growth over `max(100 ms, baseline ACK RTT)` from the pre-probe
snapshot, or outstanding
ACK age beyond `min(baseline RTT, 250 ms) + 150 ms`, also stops padding.
A test must deliver at least 99.5% of all announced positions, including
its tail, over 450–1,000 ms with mean wire delivery at least 90% of the
relay's measured delivered mean before the check. Sixteen-packet train
rounding accounts for the low-rate floor.
A successful handoff starts the UDP controller at 90% of the measured delivered mean,
with its original requested ceiling retained for later congestion-controlled
growth. Three returns per rolling minute cap flapping. Padding occupies at
most 850 ms per thirty seconds, under 2.9% duty, and under 1.5% once the
sixty-second interval applies. Emergency relay failure can always restore UDP.

MTU checks send 1,200, 1,000, 800 and 576-byte datagrams in three interleaved
rounds, 100 ms apart, initially and every ten seconds. The viewer reports
after 600 ms and deduplicates indexes. Early positions for the next serial
are retained for one second. Two deliveries at a smaller size and zero at
every larger size identify a black hole. Video frames describe their shard
sizes and size changes preserve route revision. A size change resets the
UDP budget/FEC evidence and allows `max(1 s, 2 × RTT)` for feedback about
old-size frames to expire before loss-based fallback resumes. Ordinary decoder
recovery requests an IDR when needed. A reduced MTU also disables UDP send
segmentation so software tunnels and firewalls inspecting aggregates before
segmentation see the measured size. MTU identifiers have the high bit set and
`count = 12`. Return probes reserve their entire train range below `0x8000`
and use `count = 16`, including at the maximum protocol bitrate.

Host-to-viewer MTU results also conservatively bound viewer input batches.
The reverse direction has no separate MTU measurement. Microphone datagrams
remain at most 503 bytes. Audio starts with a 1,000-byte worst-case bound,
which retains the previous stereo, 5.1 and 7.1 packet rates. Only a smaller
measured UDP MTU shortens its self-describing PCM frames. Relay audio retains
the 1,000-byte framing independently of that UDP measurement. Audio sequence numbers,
parity and playback survive size changes. The stereo audio reservation grows
from 1,200 kbps by `1000 / datagram_size` below 1,000 bytes, covering the
additional packet overhead alongside the existing surround reservation.

`Session::media_path()` returns `MediaPathState { path, reason, carrier,
via_proxy, since, activity, udp_probe_kbps }`. `MediaPath` identifies
`TailscaleUdp(UdpPath)` or `LatchRelay`. The `via_proxy` flag preserves CONNECT
use independently of the TLS or WebSocket carrier.
`UdpPath` distinguishes Unknown, Direct, PeerRelay and Derp. Non-exhaustive
`Carrier` values Native, Tls, WebSocket and HttpConnect expose stable
`as_str()` identifiers. Non-exhaustive `PathReason` carries typed UDP causes,
`RelayUnavailable(RelayFailure)`, RelayRenewal, CaptivePortal, TlsInterception,
ProxyAuth and ProxyRequired, also with stable identifiers and plain descriptions. Raw
transport errors stay in logs. `set_udp_path` refines discovery independently.

`Event::PathChanged` reports committed media transport changes, coalesced to
at most one notice per twenty seconds. The network diagnosis surface owns
sign-in, TLS-interception and proxy notices. These covered conditions leave
the path-notice window available for the next real transition. An unrelated
DERP diagnosis preserves UDP/relay transition notices. Initial route
classification, MTU changes, failed preparation and grant renewal are silent.
Current diagnostics remain immediately available through `media_path()`.
The toolbar retains the DERP city hint. Relay policy reads its unrefined path
reason. Display refinement applies while a relay route is active or attempted,
and per-route discovery snapshots are cached for 250 ms.

### Relay load and quota

Healthy UDP sessions allocate zero grants or relay lanes. Each new grant
requires a grant request and four TLS media lanes across the two endpoints.
A negotiated authorized session starts at most sixty grant workers in any
rolling hour, including failures and renewals. Reusing a cached grant starts
no worker. This bounds new successful grants to sixty and media attachments
to 240 per session-hour. Carrier retries can create additional TCP/TLS
connection attempts under their existing transport bounds. The cap is
session-scoped, so a new authorized session starts a fresh accounting window.

A session carrying 6 Mbps of combined media transfers about 2.7 GB per hour
to the relay and another 2.7 GB from relay to viewer, about 5.4 GB of combined
relay transfer accounting. Four such sessions account for about 21.6 GB per
hour. The grant-start cap bounds setup load independently of those media bytes.

The daemon admits four simultaneous grants overall and two per source IP.
A cancellation racing between grant allocation and lane attachment can
leave a slot reserved for the daemon's sixty-second attachment TTL. Twenty
seconds of caching and exponential cancellation backoff reduce repeated
orphan allocation. The existing daemon has no authenticated grant-revocation
operation. Immediate release requires a host-authorized cancellation request
bound to the grant capability, atomic removal of that grant, closure of its
attached lanes, and rejection of later attachment replay. That daemon/API
change belongs in a separate PR.

### Loopback validation

Measured on an Apple M3 Pro, macOS arm64, Rust 1.99.0, with the optimized
test profile and four Cargo jobs. TLS uses the existing local test daemon
and test CA. UDP uses the in-process impairment simulator. These runs use
synthetic video, with low-complexity 128×128 H.264 plus filler NAL units for
capacity tests. All 22 runtime tests passed on macOS and Linux arm64,
including the audio and microphone tests. Windows all-target cross-checks
passed. The tests retain the same timing and delivery bounds on every OS.

| Case | Observed | Enforced bound |
| --- | --- | --- |
| UDP blocked from startup | First decoded picture in 559 ms | 2 s |
| UDP denied mid-stream | Fresh decoded relay picture in 867 ms, held key retained | 2 s |
| Auto on a 25 Mbps FIFO | Mean UDP budget 24.06 Mbps, 20.24 Mbps video, zero grants | Mean budget 15–30 Mbps, zero grants |
| Same FIFO plus 17% random loss | Relay selected in 1.15 s | 2 s |
| Mixed random and burst loss | 1.08 s to relay, 187 of 1,069 UDP packets lost, 17.5% | 2 s |
| 3 Mbps UDP policer | 1.35 s to relay | 2 s |
| Return to UDP, 20 Mbps requested | After full-rate relay warmup, 3.14 s, 22.25 Mbps probe, 22.17 Mbps measured hold, largest picture gap 112 ms | 9 s, gap below 250 ms |
| Return to UDP, 60 Mbps requested | After full-rate relay warmup, 2.96 s, 64.85 Mbps probe, 64.93 Mbps measured hold, largest picture gap 64 ms | 9 s, gap below 250 ms |
| Return probe sharing a 25 Mbps uplink | Largest decoded-picture gap 132 ms, one grant | Gap below 250 ms, one grant |
| Silent relay TCP black hole | Fresh relay picture in 702 ms, two grants total | 1 s |
| Clean daemon stop while UDP is blocked | Fresh relay picture in 292 ms | 950 ms after local relay restart |
| Relay close with working UDP audio | Ten PCM frames resumed in 52 ms, with 3 concealed packets | 1 s |
| 850-byte UDP black hole | Selected 800-byte video/input datagrams, no relay grant | First picture within 2 s |
| 576-byte UDP floor | Forty decoded pictures in 1.31 s after settling, zero grants | Forty pictures within 2 s, zero grants |
| Healthy 150/300 ms one-way delay | Zero grants while decoded video progresses | Zero grants |
| Repeated UDP flaps | Three returns to UDP, four grants, held key retained | Three returns per rolling minute |
| Healthy UDP | Zero additional relay grants/connections | Checked during decoded video and ordered input |

The two full-rate return rows were repeated with Rust 1.100.0 and nine CPU
workers on twelve cores, leaving about three cores for the test. The fixture
uses 64×64 real H.264 pictures plus elapsed-time-sized filler. It waits for
five seconds of ceiling-rate relay delivery before restoring UDP, asserts
the announced peak and measures actual UDP datagrams during the hold phase.
Its local relay quota and grant credit cover that full-rate warmup.
Statistics snapshots preserve the independent congestion-feedback byte
counter. The shared TLS pump stages ready plaintext before its bounded
socket write, avoiding an idle polling tick per outgoing chunk.

The audio result includes loss concealment during a forced transport close.
The clean-stop result measures EOF. The silent black-hole test drops bytes
on existing TCP connections while keeping those sockets open, and measures
the application ACK-progress watchdog followed by a fresh relay connection.

The Linux arm64 namespace harness also passed all original strict d1, d2
and h assertions with four CPU cores and one scenario at a time. Their
expected-failure entries are removed. Each row includes decoded video,
real PCM click detection and ordered input round trips.

| Netlab case | Tail decoded fps | Video/request Mbps | Tail stalls | Maximum tail freeze | Audio clicks | Tail path |
| --- | --- | --- | --- | --- | --- | --- |
| d1 burst loss, measured 16.66% across 23,067 UDP packets | 30.00 | 4.53/6 | 0 | 49 ms | 22 | Relay |
| d2 3 Mbps UDP policer, 134 police drops | 30.02 | 4.53/6 | 0 | 68 ms | 21 | Relay |
| h UDP datagrams above 600 bytes dropped | 30.00 | 0.30/6 | 0 | 44 ms | 17 | UDP, 576-byte datagrams |

d1 qualifies its stochastic impairment with a separate forty-second decoded
UDP-only stream through the same qdisc before measuring the fresh recovery
session. The original 1,000-packet minimum, 15–18% loss range and every
recovery/quality assertion remain enforced. Initial shared-machine overload
caused timing and audio failures, so the final measurements were repeated
after that load subsided. Frame-gap, bitrate and audio bounds retain their
original strictness.

Run the same runtime scenarios with the following command. The protocol and
stream unit suites also cover legacy negotiation, exact 15/17/18% feedback,
FEC-preserved quality, batched frame loss, stale damage, low frame rates,
size-probe deduplication, and maximum-rate probe negotiation.

```sh
cargo test -p latch-engine -j 4 --test relay_runtime -- --test-threads=1 --nocapture
cargo test -p latch-stream -p latch-proto -p latch-client -j 4 -- --test-threads=1
cargo test -p latch-engine -j 4 --lib --test impaired --test recovery --test lifecycle --test ceiling -- --test-threads=1
```

## Restrictive-network integration

`latch_stream::session::relay_config` is the common configuration entry point
for the engine, viewer and measurement examples. It attaches a nonblocking
proxy-discovery callback to the relay configuration. App/service startup warms
the discovery cache. The relay re-reads it for fresh connections, including
recovery attempts. Netenv publishes partial observations so an environment
proxy can be used while other checks continue. OS discovery has a six-second
deadline. The eight-host cache samples local routes/environment settings at most
four times a second. Discovery refreshes on network changes and relay connection
failures. A relay failure requests a refresh within two seconds and retains the
previous proxy when PAC/WPAD discovery is incomplete. Only confirmed portals
double their retry backoff from 1 to 4 seconds. Returning to the app or opening
sign-in requests an immediate portal recheck. Healthy
sessions send no periodic HTTP probes. No session/UI
thread waits for PAC/WPAD, DNS or an HTTP probe.

`RelayRoute` retains typed terminal failures and the selected lane's
`ConnectionInfo`. `Session::network_status` combines them with local netenv facts.
`Event::Network` carries changing local facts and an empty message list clears
an earlier diagnosis. The viewer and examples use these events. Host-local details
stay in the host log. Fixed host-labelled portal, CONNECT-refusal and TLS-
interception notices use the existing version-1 Notice and contain no URL or
browser action. The viewer renders a
small shared diagnosis surface and opens a `PortalUrl` only after a button click.
The first-picture watchdog waits for at most three minutes from the first
positive portal observation, resetting that timer after the portal clears. Delivered media clears the portal banner and retains the
normal eight-second mid-stream watchdog. The first-picture watchdog restarts
when the portal opens or a delayed proxy result becomes available. Relay
attempts continue with fresh grants and normal backoff. A successful carrier clears the earlier
relay error. Unavailable checks and an old relay's missing HTTP probe stay silent.

MSS hints live in the cloned relay configuration. Transport stalls with packet-size
evidence step down from 1200 to 512 bytes for 30 seconds. Two silent TLS
handshake deadlines after TCP connects permit the same bounded probe within a
connection attempt. Healthy paths retain their MSS. `ConnectionInfo` reports the
applied value and reason. Each failed socket records its applied MSS so concurrent lanes step
down from their own value. Refusals, resets and policy denials skip recovery.
TCP_MAXSEG is applied before connect on Linux, shrinking the relay certificate
flight. macOS accepts it only after connect, affecting outgoing data, and relies
on the relay's Linux TCP MTU probing for the certificate flight. Windows skips
client MSS recovery rungs and also relies on relay-side TCP MTU probing.

The media-path indicator uses the selected relay carrier and CONNECT proxy
status. Captive-portal, TLS-interception and proxy-authentication diagnoses also
refine the local media-path reason. The network diagnosis surface owns these
notifications and the sign-in action. These additions require no wire changes.
