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

These are open work areas, not promised dates or tested capabilities:

- Verify Linux NVENC and VA-API encode on physical GPUs, including driver
  failure and fallback behavior.
- Verify HDR end to end on physical HDR panels. Synthetic roundtrips and
  native presenter tests do not establish panel output or photon latency.
- Add Linux viewer hardware decode and synchronized GPU buffer import.
- Add Apple hardware AV1 encode when hardware and platform APIs offer it;
  AV1 viewing already exists on supported Mac hardware.
- Emulate gamepads on Mac hosts; forwarding to Windows/Linux already works.
- Sign Windows builds and sign/notarize Mac builds; integrate publisher
  verification into updates and reduce repeated permission prompts.
- Add an update path for Windows-only users and Linux update notifications.
- Review elevated input consent, tagged-host authorization, local control
  authentication and independently authenticated pushed updates.
- Add Wayland capture and input with compositor permission handling.
- Add phone, TV and browser viewers; current viewers are desktop apps.
- Add explicit multi-monitor selection, per-machine stream settings and
  tray/menu-bar presence.
- Extend sustained-session and clean-install validation across platforms.

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
