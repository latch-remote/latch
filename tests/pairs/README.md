# Pair interoperability fixtures

`scripts/pairs.py` builds test-only recorders and consumers. The producer
runs the production engine server and records its HTTP-negotiated
session output. The recording source supplies numbered YUV test pictures
and six distinct synthetic audio channels. The software encoder is
`LatchEncoder`. macOS additionally uses the production synthetic
IOSurface adapter around VideoToolbox. Windows uses the production
Microsoft H.264 MFT and NV12 memory-sample path. Linux additionally captures
a deterministic X11 window through `X11Capture` and `SoftwarePipeline`.
macOS and Windows open native capture while a disposable fullscreen test
window is open. Both native encoder and capture tapes reopen at frame 12
with a requested size change from 160×98 to 128×74 and assert both sizes.
The software tapes share the same deterministic source across OSes. The
native encoder and capture tapes exercise the OS-specific video paths.
The Mac producer also emits `legacy-mac.session.json` from the committed
pre-fix `mac-vt-6.0.0.h264` recording. All consumers must pass this mode,
including Windows Media Foundation with viewer-side SPS normalization.
The producer also runs its ramp through the production OS audio conversion
boundary. macOS copies planar Float32 buffers and converts to i16, Windows
converts Float32 and PCM24 with the 5.1 side-surround WAVE mask through the
WASAPI converter/resampler, and Linux checks the actual PulseAudio channel
map and S16LE representation. These in-memory sources supply the server's
AudioCapture boundary. Physical loopback devices and OS callback timing
require separate desktop qualification.

## Format version 1

Each producer directory contains UTF-8 JSON files. Byte arrays use JSON
integer arrays to keep the format inspectable with existing dependencies.
The recording is a few megabytes at 160×98 and 128×74 for 24 frames.
Unknown versions and unknown schema fields fail deserialization.

| File | Fields |
|---|---|
| `manifest.json` | Version, producer OS/architecture, Git revision, SHA-256 of each JSON artifact, capability results |
| `MODE.session.json` | Version, producer, mode, original `StreamRequest`, negotiated `Accept`, timed control/datagram events, picture references, audio references |
| `windows.input.json`, `macos.input.json`, `linux.input.json` | Version, source viewer, target host, named input cases with observed and intended concatenated protocol input bytes, fitted clipboard text |
| `gamepad.pad.json` | Version, producer, production gamepad datagram |

Session events contain `us`, a monotonic offset in microseconds from the
recording's accepted connection, a `lane` of `Control` or `Datagram`, and
`bytes`. Control bytes retain protocol 1's length/type framing. Datagram
bytes include their original headers and session identity. The HTTP
request and `Accept` are serialized separately. Replay performs a fresh
HTTP upgrade, validates offered capabilities, sends the recorded `Accept`
and waits for a valid viewer `Hello` before scheduling the events.

Picture references include wire frame identity, keyframe flag, visible
dimensions, tight visible NV12 pixels and their FNV-1a-64 hash. Padding is
excluded. The producer first checks synthetic decoded Y/U/V against the
source with an MSE below 65 per plane and checks its numbered stripe within
four luma levels. Every IDR must carry a Baseline-profile SPS. Software
consumer hashes must match exactly. OS decoder Y and UV planes each require at least 40 dB
PSNR against the software-decoded reference. Dimensions and frame order
must match exactly. Every decode call must return its own picture before
the next access unit is submitted. Results retain each call's output delay
in frames. Empty output, a delayed output, and multiple outputs fail this
zero-delay contract even when final picture counts match. The session's
separate buffered-decoder regression verifies that delayed batches retain
all identities and increment the decoder-held counters.
Every synthetic producer forces IDRs at 0, 6, 12 and
18, changes size at 12 and asserts SPS/PPS/IDR repetition.

Audio references include packet sequence, channel count and FNV-1a-64 of
decoded signed i16 samples in little-endian WAVE channel order. Each
source channel has a different ramp offset. Replay requires every audio
reference, in order, with exact channel count and hash. The receiver hook
observes `AudioReceiver` output and invokes the production audio decoder
without opening a physical playback device.

## Replay schedules

The clean schedule preserves recorded event timing. The loss/reorder
schedule removes shard zero of frame 2, removes every datagram of frame
4, reverses the shards of frame 7 and loses the first audio packet of a
complete group selected by its actual recorded timings. Producers require
a group whose first packet to parity interval is below the receiver's
documented 20 ms AUDIO_HOLD. Replay dispatches the surviving
group and its parity together to keep OS sleep granularity out of the
inside-hold recovery assertion. All audio references must survive exactly.
The separate late-audio schedule withholds that group's parity for at least
40 ms and until the real receiver reports exactly that missing packet,
then requires every other audio reference with exact content and ordering.
Replay requires video FEC recovery and the exact surviving frame sequence.
The peer withholds recorded recovery IDR 6 until the viewer requests a
keyframe, failing after a bounded two-second wait. Tape time pauses during
that wait so subsequent media retains its spacing. IDR 6 then enters the
ordinary receive/decode path. This finite tape
cannot generate reference-invalidation responses to arbitrary new
requests. Recordings negotiate keyframe recovery.

The consumer uses strict software or strict OS decoder construction so
an OS decoder cell cannot silently switch backends. Failure to open a
required OS backend fails CI. The hosted capability contract is explicit.

| Runner | Required producer modes | Required decoders | Explicit unavailable capability |
|---|---|---|---|
| macos-15 ARM64 | software, VideoToolbox native, ScreenCaptureKit capture, legacy Mac tape | software, VideoToolbox | none |
| windows-latest x86_64 | software, Media Foundation native | software, Media Foundation | desktop capture, hosted runner has no D3D11 adapter |
| ubuntu-24.04 x86_64 | software, isolated X11 capture | software | native encoder/decoder, no hardware device |
| ubuntu-24.04-arm ARM64 | software, isolated X11 capture | software | native encoder/decoder, no hardware device |

Local macOS capture requires `--capture-window` and Screen Recording access.
Every required backend is checked on the producer and consumer. Failure
after a successful open fails its cell. Each producer also runs the
existing loopback integration tests and a separate `serve` process
against a software-decoding viewer, with key-to-picture and stereo audio
assertions.

## Input and platform boundaries

The viewer recorder drives the production event gate with translated egui
events and the following egui frame in the existing stream-view fixture, recording actual `Sink`
output. Cases include Shift and held Super,
right-Shift intent, AZERTY/QWERTZ physical-key preference, Unicode IME
commit, captured motion, absolute positions/buttons and wheel units.
The clipboard source drives `Event::Paste` through `StreamView::paste` and
the real clipboard push/fitting and JSON encoding, with an in-memory sender.
Native consumers use independent small key-code fixtures and inspect
constructed native flags, coordinates, Unicode and scroll conversion.
Linux observes XTest from another connection to the isolated `:9` display,
including the temporary Unicode key mapping. X11 layout identity uses
physical XKB key names.

Gamepad sources use a local GameController snapshot on macOS, a raw
Windows.Gaming.Input reading on Windows and evdev events on Linux. They
pass through the production normalization and `Input::pad` sender. Host
consumers check exact XUSB, DS4 and evdev report conversion. macOS has no
host virtual-gamepad adapter. ViGEmBus installation, `/dev/uinput` access,
physical pads, rumble motors, physical audio playback, native global
keyboard capture and user-granted Screen Recording remain outside these
artifact assertions.

`KNOWN-GAPS.md` names the currently failing input contracts. Their exact
observed values are asserted, and an unexpected pass fails so a behavior
fix updates the list. CI summaries contain observed and expected native
values for every case. Every required producer, media schedule, input
case, gamepad conversion and clipboard case must produce a result row.
The workflow has read-only repository permissions, SHA-pinned actions
and 19-minute producer plus 6-minute consumer timeouts. Consumer jobs
reuse their OS's producer-built test binaries. The driver restores execute
permissions for every executable, including the X11 helper, and fails with
the helper path if restoration fails. Every media cell runs in its own child
process. Input and clipboard assertions are isolated per case. All cells
run after a failure, and the final summary distinguishes passed, known-gap,
failed, not-run and unavailable, with a run count. Any required failed or
not-run cell fails the job.

## Observed Mac audio repair latency

Run 43693057052's macOS capture tape sends data seq 4 at 61.120 ms and its
parity at 101.657 ms, a 40.537 ms delay. Seq 5 exposes the gap 40.522 ms
before parity. The receiver's 20 ms hold expires first. Sender PTS advances
40.572 ms between the first data and parity, so this is visible at the
production sender as well as at the recording socket. Of 72 complete groups,
37 have more than 20 ms between seq group+1 and parity. The maximum
first-data-to-parity interval is 46.062 ms.

The same run's software and native-encoder tapes have 47/91 and 48/84 such
groups, with maxima 45.910 and 46.634 ms. All three use the same synthetic
5 ms AudioCapture implementation. These numbers establish a real Mac
server pacing/FEC vulnerability under this fixture's scheduling conditions.
Physical ScreenCaptureKit audio callback cadence requires separate
measurement. ScreenCaptureKit independently queues converted 5 ms frames,
and send_audio emits parity only after every fourth coded packet. A callback
or scheduling stall exceeding the hold can therefore waste redundancy in
a live session too. The pacing/capture-cadence investigation needs its own
product follow-up. This PR preserves product timing and tests both timely
repair and exact late-packet concealment through the real receiver.


## Pre-fix Mac recording

`mac-vt-6.0.0.h264` contains the 24 original access units extracted from
`native.session.json` in `pairs-recording-macos-aarch64` of GitHub Actions
run 43697534260. The recording includes IDRs at 0/6/12/18 and a resize at
12. It retains VideoToolbox's original SPS bytes, with POC type 0, one
reference and no bitstream restriction. The recorder wraps these bytes in
a fresh protocol-1 session with deterministic audio and reference pictures.
The host normalizer is bypassed only for this committed test tape.

The two SPS NALs are `2742c015ab4187f08c44d404040408` and
`2742c015ab4186f043193501010102`. The fixed forms are
`2742c015ab4187f08c44d40404041e1108d4` and
`2742c015ab4186f043193501010107844235`. The codec unit tests retain the
original bytes independently of this tape. The Windows decoder tests
compare raw playback, host-normalized playback and viewer-normalized
playback, and isolate the reordering limit from DPB storage and POC type.

## Rejected SPS regression recording

`mac-vt-rejected-sps.h264` derives from the synthetic pre-fix Mac tape above.
Every SPS gains a VUI restriction with zero reorder frames and zero decoder
buffer slots, below its one declared reference frame. The optimizer rejects
that restriction. The software decoder tolerates it and returns all 24
pictures, including the resize. Codec, host-output and Windows viewer-input
regressions assert byte-identical pass-through and successful decoding.

The optimizer compatibility fixtures in `crates/stream/tests/fixtures` are
one-picture gray images. `optimizer-profile-144.h264` is a 64×64 x264
High 4:4:4 stream with the legacy profile id 144. The x265 HEVC fixtures
are 16400×64 and 64×16400, beyond the optimizer's dimension cap. FFmpeg
qualifies decoding after byte-identical pass-through, alongside reserved
H.264 constraint bits derived from the existing synthetic recording.
An HEVC VPS reserved value of `0xfffe` passes through byte-identically,
but FFmpeg rejects it and the available MF software decoder returns no
picture. Its test compares the decoder's own result before and after normalization. HEVC depths
above 16 bits have byte-preservation coverage. Successful decode of those
two rejection classes remains unverified with the available decoders.
