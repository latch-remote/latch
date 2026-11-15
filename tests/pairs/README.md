# Pair interoperability fixtures

`scripts/pairs.py` builds test-only recorders and consumers. The producer
runs the production engine server and records its HTTP-negotiated
session output. The recording source supplies numbered YUV test pictures
and six distinct synthetic audio channels. The software encoder is
`LatchEncoder`. macOS additionally uses the production synthetic
IOSurface adapter around VideoToolbox. Windows uses the production
Microsoft H.264 MFT and NV12 memory-sample path. Linux additionally captures
a deterministic X11 window through `X11Capture` and `SoftwarePipeline`.
macOS and Windows attempt native capture while a disposable fullscreen
test window is open. API/window failures appear in the producer results.

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
must match exactly. Every synthetic producer forces IDRs at 0, 6, 12 and
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
4, reverses the shards of frame 7 and removes audio packet 4 while
retaining parity. Replay requires video FEC recovery, audio recovery,
keyframe requests and the exact surviving frame sequence. Recorded IDR 6
answers recovery through the ordinary receive path. This finite tape
cannot generate reference-invalidation responses to arbitrary new
requests. Recordings negotiate keyframe recovery.

The consumer uses strict software or strict OS decoder construction so
an OS decoder cell cannot silently switch backends. Failure to open an
optional OS backend produces an explicit unavailable result. Failure
after a successful open fails its cell. Each producer also runs the
existing loopback integration tests and a separate `serve` process
against a software-decoding viewer, with key-to-picture and stereo audio
assertions.

## Input and platform boundaries

The viewer recorder drives the production OS-event callback and following
egui frame in the existing stream-view fixture, recording actual `Sink`
output. Cases include Shift and held Super,
right-Shift intent, AZERTY/QWERTZ physical-key preference, Unicode IME
commit, captured motion, absolute positions/buttons and wheel units.
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
reuse their OS's producer-built test binaries.
