# Roadmap

## Now

Latch 5 shares and views a desktop over Tailscale with one Rust app per
machine. Mac and Windows viewers probe hardware decode for AV1, HEVC and
H.264; Linux viewers use software H.264. Mac hosts encode H.264/HEVC;
Windows and Linux can offer AV1 where the encoder supports it. HEVC
sessions negotiate ten-bit SDR or HDR from the viewer's capabilities.
Mouse, keyboard, clipboard, audio and microphone forwarding are implemented;
gamepads work from all three viewer platforms into Windows and Linux hosts.
Linux capture is X11-only. Request ceilings are 16384 pixels a side,
500 fps and 1 Gbps, above what current hardware delivers; each session is
fitted to the encoders, decoders and displays both machines report.

## Next

Open work, grouped by what it would change. None of it is promised for a
date, and none of it is a tested capability until it leaves this list.

### Picture

- **Full-chroma (4:4:4) streams.** Every path is 4:2:0 today, which softens
  coloured text and thin lines. Needs a negotiated pixel format and
  encoder, decoder and presenter support on each platform.
- **Display-paced presentation.** The viewer shows each frame as soon as it
  is decoded. A vsync-locked mode, a smoothing mode, and feeding the
  viewer's display phase back to the host's capture clock are not built.
- **Changing size and frame rate during a session.** Only bitrate adapts
  while a stream runs; resolution and frame rate are fixed at connect. A
  governor that trades them under encoder or network pressure is open.
- **A shared audio and video clock.** Sound and picture are each played as
  early as possible instead of against one timeline.
- **Choosing the display.** A host with several monitors shares one that
  Latch picks (the whole X11 root on Linux). Selection belongs in the
  protocol and the stream menu, along with per-machine stream settings.
- Verify HDR end to end on physical HDR panels. Synthetic roundtrips and
  native presenter tests do not establish panel output or photon latency.

### Hosts and viewers

- **Linux viewer hardware decode** (VA-API, then Vulkan Video) with
  synchronized GPU buffer import. Linux viewers decode H.264 in software.
- **Linux host GPU path without the CPU round trip.** NVENC and VA-API
  encode from pictures prepared on the CPU; capture, colour conversion and
  encode should stay on the GPU. Both backends also still need a first run
  on physical GPUs, including driver failure and fallback.
- **Windows host: close the gap between capture and encode.** On an RX
  6600, capture and encode together deliver 9% to 18% fewer frames than
  the slower of the two does alone (157 fps at 1440p, against 191 from
  capture and 233 from the encoder). The copy, cursor and conversion
  passes between them need profiling and overlapping.
- **Windows ten-bit and HDR viewing without a CPU readback.**
- **Mouse and keyboard sent from the OS event.** The viewer forwards them
  when its window next draws, which is at once on input but still waits
  for the frame in progress. Gamepads already go straight from their own
  thread.
- **A separate cursor plane on Mac hosts.** Windows and Linux hosts send
  the pointer outside the video; a Mac host draws it into the picture.
- Apple hardware AV1 encode when the hardware and platform APIs offer it;
  AV1 viewing already works on supported Macs.
- Gamepad emulation on Mac hosts, and motion and touchpad data from pads
  on Mac viewers. Forwarding to Windows and Linux hosts already works.
- Wayland capture and input, with compositor permission handling.
- Phone, TV and browser viewers. Current viewers are desktop apps.

### Network

- **Cheaper loss recovery everywhere.** Long-term-reference recovery is
  qualified on AMD (AMF) and opt-in elsewhere; other encoders repair a
  lost frame with a full keyframe. Per-device qualification, NVENC
  reference invalidation and intra refresh are open.
- **Pacing that costs no frames when the encoder is the slow part.** In
  that case a paced stream still delivers about 5% fewer frames than an
  unpaced one did ([record](measurements/2026-11-09-paced-batches/README.md)).
- **Per-packet delivery feedback.** The receiver reports in 50 ms
  aggregates over the control connection. Sequence-numbered feedback over
  UDP would let the controller tell network queueing from scheduling.
- **ECN and L4S.** No ECN marks are read or sent. This needs marks to
  survive the Tailscale tunnel and a compliant controller, not a socket
  flag.
- Tuning for mixed loss on long round trips, where simulations still show
  damaged frames.

### Around the stream

- Clipboard contents other than text, and file transfer.
- Microphone capture off the audio callback thread.
- Sign Windows builds and sign and notarize Mac builds
  ([below](#signing-and-notarization)); hold updates to the publisher's
  signature and stop macOS forgetting permissions after an update.
- Self-installing updates on Linux. Linux hosts show a notice today.
- Tray and menu-bar presence.
- Sustained-session and clean-install validation on more machines. The
  hardware Latch has run on is listed in [PERFORMANCE.md](PERFORMANCE.md).

## Not planned

A Latch account service, separate pairing/password system and streaming
without Tailscale are outside the current design. Tailscale supplies the
network and identity boundary. Latch streams desktops; it has no current
game catalogue or application launch/resume API.

## Version history

| Version | What it introduced |
| --- | --- |
| 1.x (retired) | Custom H.264 transport and rendezvous experiment |
| 2.x (retired) | Sunshine/Moonlight orchestration over Tailscale |
| 3.x (retired) | Embedded GameStream viewer, wake and updates |
| 4.x (retired) | Unified sharing/viewing app and Linux deployment kits |
| 5.x | In-process Rust engine, protocol 1, platform codecs and colour negotiation |

The [changelog](../CHANGELOG.md) records individual releases.

## Signing and notarization

Latch's releases are ad-hoc signed on the Mac and unsigned on Windows.
The maintainer must provide signing credentials belonging to a person or
company. The build scripts support a signing identity; CI integration and
publisher verification still need work.

### macOS: Developer ID and notarization

Needed:

- An Apple Developer Program membership (individual or organisation),
  and in it a **Developer ID Application** certificate. Its private key
  lives in the signing Mac's keychain; for CI, export certificate and key
  as a password-protected `.p12`.
- Credentials for the notary service: an App Store Connect API key (the
  `.p8` file, its key ID and the issuer ID), or an Apple ID with an
  app-specific password and the team ID.
- For CI, as repository secrets: the `.p12` (base64) and its password, the
  identity name (`Developer ID Application: Name (TEAMID)`), and the notary
  credentials.

Then:

- `scripts/bundle-macos.sh` already signs with `CODESIGN_IDENTITY`, with
  the hardened runtime, a secure timestamp and the Apple Events
  entitlement a shared Mac needs to restart and shut down. `docs/MACOS.md`
  (Signing) has the local commands.
- `.github/workflows/release.yml`'s macOS job would import the `.p12` into
  a temporary keychain, set `CODESIGN_IDENTITY`, run `xcrun notarytool
  submit --wait` on a zip of `Latch.app`, `xcrun stapler staple` the app,
  and only then make `latch-macos-arm64.tar.gz` (and the 4.0 updaters'
  copy).
- The `xattr -dr com.apple.quarantine` step then leaves `README.md`,
  `docs/MACOS.md` and `scripts/install-macos-release.sh`.
- macOS keys Screen Recording and Accessibility grants to the app's code
  signature. Ad-hoc, that is the hash of each build, so every update can
  make macOS forget them and the user has to allow Latch again; under a
  Developer ID they survive updates. The updater
  (`crates/client/src/update.rs`) could then also require the team ID
  in the new app's signature, not only GitHub's digest.

### Windows: Authenticode

Needed:

- A code-signing certificate from a certificate authority (OV or EV), or
  an Azure Trusted Signing account. Since 2023 the private key has to sit
  in hardware, so signing in CI goes through the CA's cloud signing
  service (Azure Trusted Signing, DigiCert KeyLocker, SSL.com eSigner and
  the like) and its credentials as repository secrets.
- An RFC 3161 timestamp server URL (every CA has one), so signatures stay
  valid after the certificate expires.

Then:

- The release workflow's Windows job would sign `latch-host.exe` (`signtool sign
  /fd sha256 /tr <timestamp url> /td sha256`, or the service's action)
  before zipping it.
- Signing can reduce SmartScreen warnings as the certificate gains reputation, and the
  UAC prompt names a verified publisher.
- Setup's elevated step (`stage_script` in `crates/host/src/setup.rs`)
  today runs only a copy whose SHA-256 matches the file the window was
  started from. With a signature it should also require
  `Get-AuthenticodeSignature` to be `Valid` for the expected publisher
  before running the copy, and could elevate `latch-host.exe` itself
  (whose prompt would show Latch) instead of Windows PowerShell. The
  update a Mac pushes to `/v1/update` can be held to the same signer.
