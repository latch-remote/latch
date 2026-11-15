# macOS client (Apple Silicon)

### Viewer decode measurements

`cargo run --release -p latch-stream --example stream_decode_bench` validates
native VideoToolbox pictures with `Frame::with_planes`. Its native mode times
decode without forced CPU materialisation; CPU-materialised mode includes
`ensure_cpu_planes`. Validation and the pixel oracle are outside sample timing
(and access native planes); loop elapsed time includes validation. Repeated
64×64 H.264 IDRs measure neither desktop throughput nor GPU presentation.
Stats label UI observation and modelled delay; present mode is immediate,
with physical scanout unmeasured. Sustained local decode overload reports a
capacity estimate; lower FPS/size and reconnect to act on it.


Latch for the Mac is one native app: every machine on your Tailscale
account, a Connect button each, a power menu, the stream itself, and a
**Sharing** tab so another desktop can open this Mac, within the
[platform support limits](PLATFORMS.md). Latch's stream protocol is compiled in, video is captured with ScreenCaptureKit
and encoded with VideoToolbox (H.264 and HEVC), decoded with VideoToolbox
(including AV1 where hardware supports it), audio is coded losslessly
with `lpcm-rice`, and the picture is drawn in Latch's own window under a toolbar.

## Requirements

- macOS 13+ on Apple Silicon
- [Tailscale](https://tailscale.com/download/mac), signed in with the same
  account as your other machines
- Screen Recording and Accessibility permissions, which macOS asks
  for when setting up sharing (so others can see this Mac's desktop and send
  input)

## Install a prebuilt app

Download `latch-macos-arm64.tar.gz` and `SHA256SUMS` from the public latest
release. Downloads require no GitHub login. Quit Latch before replacing an
existing app, then run these commands in the download folder.
[Latest release](https://github.com/latch-remote/latch-releases/releases/latest)

```bash
shasum -a 256 latch-macos-arm64.tar.gz
```

Compare the result with this archive's line in `SHA256SUMS`, then extract
and verify the bundle.

```bash
tar xzf latch-macos-arm64.tar.gz
codesign --verify --deep --strict Latch.app
```

Move `Latch.app` into `/Applications` with Finder, replacing the old app if
present. Then clear quarantine and open it.

```bash
xattr -dr com.apple.quarantine /Applications/Latch.app
open /Applications/Latch.app
```

The digest comes from GitHub alongside the artifact, not an independent
publisher signature. Ad-hoc signature verification checks bundle consistency.

The corresponding source snapshot on the release page also contains
`scripts/install-macos-release.sh`. From the extracted source, run
`bash scripts/install-macos-release.sh` for the manifest-verified installer.
It uses macOS tools (`curl`, `sed`, `shasum`), checks digest, size and ad-hoc
signature, swaps the app into `/Applications`, then opens it. The script
needs no GitHub CLI or credentials. The development repository requires
access, so its raw script URL is unsuitable for public installation.

The `xattr` step is required for a browser download. The app is ad-hoc
signed rather than signed with an Apple Developer ID, and macOS 15 reports
such apps as damaged instead of offering "Open anyway". Removing the
quarantine flag is how you tell macOS you fetched it deliberately.

Move the app to `/Applications` before turning on sharing: the login item
records its path. If this folder is not writable, ask an administrator to
install it there. After an update, ad-hoc signing can invalidate Screen
Recording and Accessibility grants. Re-grant them in System Settings and
quit and reopen Latch. Viewing needs neither permission. Allow incoming
connections if the macOS application firewall asks when sharing starts.

## Build on a Mac

```bash
./scripts/install-macos.sh
```

builds for `aarch64-apple-darwin` (pure Rust, no C/C++ library compilation),
bundles `dist.noindex/Latch.app`, and installs it to `/Applications`. The `.noindex` folder keeps build copies out of app search.
Install Xcode Command Line Tools for the platform SDK and linker.
A locally built app never carries the quarantine flag.

### Signing

`bundle-macos.sh` signs ad-hoc by default. For a build other people can
download without the quarantine step, sign with a Developer ID and
notarize:

```bash
CODESIGN_IDENTITY="Developer ID Application: Your Name (TEAMID)" ./scripts/bundle-macos.sh
ditto -c -k --keepParent dist.noindex/Latch.app dist.noindex/Latch.zip
xcrun notarytool submit dist.noindex/Latch.zip --keychain-profile "notary" --wait
xcrun stapler staple dist.noindex/Latch.app
```

## First run

1. If Tailscale is missing or signed out, a banner at the top of
   **Machines** says so; **Get Tailscale** opens the download page.
2. **Machines** lists every machine on your Tailscale account, this Mac
   first. A dot and one line say whether each is ready, asleep, or
   missing something on its side.
3. Click **Connect** (**Wake and connect** for a machine that is asleep).
   There is no pairing: a host answers devices signed in to its own
   Tailscale account. A tagged host (a server with no account of its own)
   answers the tailnet's owner, or on an organisation's tailnet the logins
   in its `allowed_logins`.
   If Latch is not sharing on that machine, open Latch there and set up
   sharing on its **Sharing** tab first.
4. The desktop appears.

Command-1 opens **Machines**, Command-2 **Sharing** and Command-comma
**Settings**; Escape goes back to **Machines**.

## While streaming

The picture fills the window and nothing else is drawn over it. With
**Match screen** the picture is this display's own size, so there is no
bar on any side; a window is resized to the stream's proportions when the
session starts, for the same reason. A manual 16:9 size on a display of
another shape leaves a bar above and below.

**Control-Option** (Ctrl+Alt on Windows and Linux) is the host key, as in
VMware or VirtualBox. Press it and the mouse is freed and the toolbar drops
over the top of the picture; press it again, or click the picture, and the
toolbar goes away and the mouse is captured back. The keys are shown when
a stream starts, and on the toolbar itself. Before the first picture
arrives, a **Cancel** button under the progress ends the attempt without
the host key. A menu or a question keeps the toolbar there until it is
answered.

**Stats** (or the host key with **S**, or **F6** as in GeForce NOW:
Control-Option-S) lays a stats overlay over the picture's top-right
corner without taking the mouse or showing the toolbar: resolution, codec
and decoder; frames a second received, decoded and UI-observed against what was
asked for; bitrate now against the host's current target and the ceiling;
round trip; frame jitter; packet loss as it happened and after FEC; frames
lost, skipped before decoding and decoded but never observed; average and p95
decode time against the frame budget; decode-to-UI observation delay and immediate
present mode; and modelled delay (excluding physical scanout)
with its breakdown (host capture to send, half the round trip plus frame
assembly, decoder queue plus decode, waiting to be drawn plus half a
refresh of this display). Values worth a look turn amber.

The mouse is captured by default: the Mac cursor is hidden and held in
place and raw movement is sent, which is what games read for the camera
(a game that ignores the cursor's position, such as Genshin Impact, turns
only with this). The PC draws its own cursor in the picture, so only one
pointer is ever visible. The **Mouse** menu can switch to **Free**, where
the Mac cursor's position is sent 1:1 instead and nothing is captured.

| Toolbar item | What it does |
|--------------|--------------|
| **● Machine name · 1920×1080 · 60 fps · HEVC** | The stream as negotiated (the numbers show on wide windows); the dot turns amber while the connection is poor |
| **Direct · 38 ms / Relayed via Tokyo · 210 ms / Via your relay · 60 ms** | The path Tailscale found and its round trip. Amber means every packet goes through one of Tailscale's shared relays, which adds delay; **Connection details** under the machine list say why and what would give a direct path. What Latch asks for is the same on every path |
| **Mouse** | **Captured, for games** (the default): a click on the picture hides the cursor and sends raw movement, which games read; the host key frees it. **Free, for desktops**: the Mac cursor's position is sent 1:1 and nothing is captured. The choice is remembered, and is also in Settings |
| **Keys** | Ctrl+Alt+Delete, Windows key, Alt+Tab, Escape, Print Screen; and **Command acts as Ctrl** |
| **Stats** | The stats overlay (above). **More → Stream performance** opens the full panel: frame rate and bitrate against what was asked for, round trip, packet loss, encode, assembly, queue and decode times, and the decoder. **Copy diagnostics** puts it all on the clipboard |
| **Full screen** | Toggle; the setting decides how a session starts |
| **PC** (or **Mac**, **Machine**) | Sleep…, Restart…, Shut down…, each asking first, when the machine's owner allows them; Sleep only on a Windows PC, the one kind a wake packet can bring back. |
| **Stream settings** | The same controls as Settings: quality profile, resolution, frame rate, bitrate and codec. **Apply and reconnect** restarts the session with them in a second or two; Escape or the close button leaves them as they were |
| **Disconnect** | End the session |
| **2 gamepads** | Shown while gamepads are connected to this Mac; hover for which, and whether the host takes them (green), or why not (amber) |

On a narrow window everything but **Stream settings** and **Disconnect**
folds into one **More** menu.

Keys delivered to Latch are forwarded in order when its window next draws.
Physical keys use a US-oriented mapping; differing host/viewer layouts and
OS-reserved shortcuts have limits listed in the [platform audit](PLATFORMS.md).
With **Command acts as Ctrl** on (the default), ⌘C, ⌘V, ⌘Z and
the rest do on Windows and Linux what they do on the Mac, and ⌘ can stay
held across several of them; a Mac at the other end always gets ⌘ as
Command. While the stream has the keys, ⌘Q, ⌘H and ⌘+/⌘−/⌘0 go to the PC
too (⌘Q closes the PC's window, not Latch): quit or hide Latch from the menu
bar or the Dock, after **Disconnect**, or while the toolbar or a panel has
the focus. ⌘Tab and ⌘Space stay on the Mac.

### Gamepads

Gamepads connected to the Mac (Xbox Wireless, DualSense, DualShock 4,
Switch Pro and other MFi pads, over USB or Bluetooth) go to the host as
they are: a Windows PC gets an Xbox 360 pad for an Xbox pad and a
DualShock 4 for a PlayStation one, a Linux host an Xbox 360 pad for each.
The game's rumble comes back to the pad, on each handle and, on pads that
have them, the triggers. macOS gives gamepads to the frontmost app only,
so keep the stream's window in front. A Mac host takes no gamepads.

### Clipboard

With Latch Host 3.1 or newer on the PC, the clipboard follows you both
ways: text copied on the PC is in the Mac's clipboard a second later, and
⌘V on the PC pastes the text the Mac has (Latch sends it to the PC first,
then presses Ctrl+V there). Text only, up to 32 KB; an image or a file on
either clipboard is left alone, so ⌘V with a screenshot on the Mac's
clipboard, or after copying a file on the PC, pastes what the PC itself last
copied. Keys typed right after ⌘V wait for the paste to land (up to two
seconds). Short notices under the toolbar say when something crossed. With
an older host, ⌘V pastes what the PC last copied.

A Linux machine gets ⌘C, ⌘X and ⌘V as Ctrl+Insert, Shift+Delete and
Shift+Insert, which GTK and Qt apps, Firefox, Chromium and terminals all
take for copy, cut and paste; Ctrl+C in a terminal would interrupt the
running program. Latch Host there puts pasted text in the selection as
well as the clipboard, because terminals paste the selection on
Shift+Insert. To copy in a terminal, select the text and press ⌘C.

A Linux VPS draws a desktop of one fixed size and encodes on the CPU, so
Latch never asks it for more than that size: a larger resolution setting
gets the desktop's own size, a smaller one (720p, say) is scaled down.
Latch Host there times its software encoder on its own CPU at that size
and reports the frame rate it keeps up with; Latch asks for no more than
that, and more for a smaller picture in proportion to the pixels it saves
(a host measured at 30 fps at 1080p is asked for up to 67 at 720p, 120 at
540p). To get 60 fps from a slow machine, pick 720p (or 540p) and 60 fps.

## Only connecting from this Mac

On a Mac, **Share this Mac** on the **Sharing** page decides whether Latch
runs in the background so other machines can connect here. Turn it off and
this Mac only connects to the others: the background service and the
streaming engine stop, opening Latch no longer starts them, and nothing of
Latch runs once its window closes. **Share this Mac** turns it back on.

### Laggy? Look at the path

Tailscale connects the Mac and the PC directly when it can punch through
both routers. When it cannot, every packet goes through a relay: your own
peer relay if you run one (**Relay** in Settings), otherwise one of
Tailscale's (DERP). A relay adds a detour, and the round trip it adds is a
floor under how quickly the PC answers a click; it changes nothing about
what Latch asks for. Balanced quality is this screen's size at its own
refresh rate on every path. A relay can increase delay and lower quality;
the host's rate control lowers the bitrate only when the network does not
carry it.
Tailscale's own relays are shared, so a stream through one can also
stutter at busy times; a peer relay you run is yours alone.

Latch shows which case you are in next to each PC (**Relayed via Tokyo ·
210 ms**, **Via your relay · 60 ms**) and, when relayed, **Connection
details** under the list says which router is in the way and what would
fix it: UPnP or NAT-PMP turned on in the PC's router, a UDP port forwarded
to the PC, or IPv6 on both networks. Both machines report their own side
(`tailscale netcheck`); the PC's report needs Latch Host 3.1.

## A machine's menu

The **…** button at the end of each row offers what that machine allows:
**Wake** (asleep, and Latch has learned how to wake it; sends the packet
without connecting), **Test waking it from this network** (awake), **Sleep**,
**Restart…** and **Shut down…** (when that machine allows remote power
actions), and **Copy Tailscale address**. Restart and shut down ask first;
both force-close programs there.

**Test waking it from this network** sends the wake packet to the awake
machine and asks Latch there whether it arrived. A pass means waking it
from where you are will work; a failure means the packets are not reaching
it from this network (see below).

## Settings

| Setting | What it changes |
|---------|-----------------|
| Quality | Profiles that follow this display, as GeForce NOW's do. **Balanced** (the default) asks for this screen's own size at its own refresh rate (120 Hz on a ProMotion MacBook Pro, 144–360 on a fast monitor). **Competitive** is 1080p at the display's refresh rate, up to 360 fps. **Cinematic** is this screen's size up to 4K, up to 120 fps, at half again the bitrate. Each profile's bitrate is a ceiling: a computed 40 Mbps at 1080p60 before the 50 Mbps floor, growing as pixels to the ¾ and frame rate to the ½ (50 Mbps at 1080p60, 87 at 1440p120, 113 at 4K60, 160 at 4K120), half again for Cinematic, never under 5.1's 50 Mbps (Cinematic 100); the host comes down from it whenever the network carries less. **Custom** uses the rows below; changing one of them switches to it, starting from what the profile asked for. The request is fitted to what both machines report; larger pictures are scaled down in proportion. Settings saved by 5.1 move Smooth, Balanced and Sharp to Competitive, Balanced and Cinematic once |
| Resolution | **Match screen** is this display's own pixel size (3024×1964 on a 14" MacBook Pro), so the picture fills it exactly. **540p**, **720p**, **900p**, **1080p**, **1440p**, **4K**, **5K** and **8K** are the standard 16:9 sizes, 960×540, 1280×720, 1600×900, 1920×1080, 2560×1440, 3840×2160, 5120×2880 and 7680×4320; on a display of another shape they leave a bar above and below. The sizes below 1080p are for a PC that encodes in software or a slow network: fewer pixels leave more bitrate for each and a higher frame rate. The list shows the pixel size of each. The PC switches its display to the size asked for; a PC with no monitor needs a virtual display that lists that size, which setup on the PC arranges for the Virtual Display Driver |
| Frame rate | 30 to 500, and this display's own rate (marked); the PC's display is switched to match when it can |
| Bitrate | 2 to 1000 Mbps in steps (fine at the low end); arrow keys step once, Shift-arrow ten times. The most the encoder spends; the host's rate control comes down from it when the path drops packets |
| Codec | Auto (the best both ends have: AV1, then HEVC, then H.264), AV1, HEVC, H.264. Negotiation and decoder failure recovery try the next supported codec, ultimately Baseline H.264 |
| Send my microphone | Off by default. On, this Mac's microphone plays on the PC as a microphone: by itself on Linux; on Windows and macOS through a virtual cable installed there (VB-CABLE, BlackHole). macOS asks for the microphone the first time |
| Open streams full screen | How a session starts |
| Capture the mouse | The toolbar's Mouse choice, remembered |
| Command acts as Ctrl | Off makes ⌘ the Windows key on Windows and Linux machines; a Mac always gets ⌘ as Command |
| Offer to sleep a machine after a session | Off by default. Asleep, Tailscale is off there; this Mac can only wake it from its own network |
| Keep Latch up to date | Updates this app and the Windows PCs' Latch Host; **Check now** checks at once |
| Relay | Whether a relay of your own is in use, and the grant it needs |
| About | Version, licence, source, and the open-source notices |

The stream rows are the panel that **Stream settings** opens during a
session, and **Next stream** under them says exactly what the next connect
asks for; the machine list shows the same. A session also reconnects by
itself, at the new size, when this Mac's window moves to a display of a
different size.

Settings are saved in `~/Library/Application Support/io.github.MrBeldum.Latch/client.toml`,
along with the MAC and LAN address of each PC Latch has seen.

## Waking a PC from another network

Asleep, the PC's Tailscale is off, so Latch cannot reach it over the
tailnet. Leave the PC on (Latch keeps it awake while plugged in)
if you want **Connect** from anywhere.

The wake packet is sent to the LAN broadcast, to the PC's LAN address and
to the PC's public address. Over the internet only the last can arrive,
and only if the PC's router forwards UDP 9 to the PC. A [Tailscale subnet
router](https://tailscale.com/kb/1019/subnets) on the PC's network (a NAS,
a Raspberry Pi, or a router that runs Tailscale) is the alternative. Use
**Test wake** while the PC is on to find out which situation you are in.

## Sharing this Mac: the video pipeline

ScreenCaptureKit renders the display straight into an IOSurface at the
stream's size (it scales, with bars where the shapes differ), and
VideoToolbox encodes that surface; only compressed bytes reach Latch.

- **Which display.** The main one, or the one whose CGDirectDisplayID is
  in `LATCH_MAC_DISPLAY` in the host's environment. The stream request
  does not name a display yet.
- **How often.** Every refresh of a display no faster than the stream
  (a ProMotion panel reports no fixed rate and gets every refresh too);
  for a slower stream, every k-th refresh with half a refresh of slack,
  since a gap of exactly 1/fps lets timing jitter skip every other one.
  The frame grid then takes the stream's rate from what arrives.
- **Surfaces.** Five, so the two frames in the encoder and the newest
  picture never leave the window server without one to draw into.
- **Encoder settings.** Hardware required (software only if it fails),
  real time, no frame reordering, `MaxFrameDelayCount` 0, the stream's
  rate as `ExpectedFrameRate` and (macOS 15+)
  `MaximumRealTimeFrameRate`, no periodic keyframes, BT.709 tags.
  Bitrate is whatever the server asks for; VideoToolbox took 1 Gbps.
  For 8-bit hardware HEVC through 248,832,000 pixels/second (1080p120),
  `PrioritizeEncodingSpeedOverQuality` is false; above that budget, and
  for H.264, Main10 and software, it is true. This M3 Pro crossover
  restores HEVC quality while retaining the fast path at 240 fps and
  3024×1964@120. Other chips may have a different crossover.
- **No low-latency rate control.** `EnableLowLatencyRateControl` sounds
  right but is built for calls: on an M3 Pro it made a 1080p frame take
  9.4 ms instead of 4.7, dropped frames (45 of 60 fps at 1080p60), and
  sent a fifth of the bitrate asked for.
- **Two frames in flight.** A frame goes to the encoder without waiting
  for the one before, and leaves the moment it is done, so the media
  engine overlaps them. A stream whose interval is shorter than one
  frame's encode time (240 fps at 1080p, 120 at 3024×1964) keeps its
  rate; a slower one sees no difference.
- **10-bit.** HEVC Main10 from P010 (`x420`) surfaces works and is tested
  with synthetic surfaces. Viewers now negotiate ten-bit SDR or HDR10;
  ScreenCaptureKit selects P010 and canonical HDR capture with a PQ colour
  space for HDR, and VideoToolbox writes matching BT.2020/PQ tags. HDR
  capture requires Apple Silicon and macOS 15; unavailable Main10 capture
  or encoding retries SDR8 before Accept.

A requested keyframe is the next picture returned to the server. Queued
pre-request frames are discarded until the fresh IDR, so they cannot delay
loss recovery or make the server request another IDR. Retries share that
request until the IDR leaves. A bitrate change first completes the
pictures already in the encoder, so every later picture is encoded at the
new rate (on an M3 Pro the first one after a 20 → 1 Mbps cut came out at
half the size). The completed old-rate pictures still leave, in order:
later P-frames reference them, and dropping them would cost an IDR at
every step of a ramp.

### Measured limits (M3 Pro)

`cargo test --release -p latch-engine --lib realtime_benchmark -- --ignored --nocapture`
submits a moving synthetic picture from prefilled IOSurfaces on the
stream's grid, as capture does, and times each frame from submission to
the encoder's output. `LATCH_VT_BENCH=hevc:1920x1080@240:80000,...`
picks the cases, `LATCH_VT_IN_FLIGHT=1` measures one frame at a time.
Two runs on 2026-11-03 on an M3 Pro, busy with other builds (revisions and
source content are recorded in the linked measurements). These are
synthetic encoder samples, not real-desktop frame-rate promises:

| Case | 5.1 p50 / p95 ms | 5.1 fps | Now p50 / p95 ms | Now fps |
| --- | --- | --- | --- | --- |
| HEVC 1080p60, 50 Mbps | 9.8–10.5 / 13–44 | 51–61 | 4.9–5.3 / 8.1–8.3 | 60.7 |
| HEVC 1080p120, 50 Mbps | 9.3–9.7 / 12–14 | 99–105 | 4.7–4.8 / 8.0–8.3 | 121 |
| HEVC 1080p240, 80 Mbps | 8.7–9.8 / 9–20 | 86–114 | 4.2–4.6 / 6.2–6.6 | 241–242 |
| HEVC 3024×1964@120, 100 Mbps | 22.9–23.6 / 24–90 | 31–43 | 11.2–11.8 / 17 | 120 |
| H.264 High 1080p240, 80 Mbps | 8.4–9.6 / 10–92 | 40–116 | 10.0 / 11.2 | 192 |
| H.264 High 3024×1964@120 | 27–33 / 60–65 | 28–32 | 27–28 / 29–32 | 70–72 |
| Baseline 1080p240, 80 Mbps | 8.3–9.3 / 10–43 | 73–115 | 9.7 / 10.6–10.7 | 191–197 |

In these samples H.264 ran out of encoder at about 195 fps at 1080p and
72 at 3024×1964; asked for more, its frames queued behind each other
(hence ~10 ms and ~27 ms). HEVC did better for fast or large streams;
4K60 HEVC took 14 ms a frame.
The table predates HEVC's quality path: up to 1080p120 an HEVC frame
now takes about 7 ms (p95 8–9 ms), still inside a 120 Hz frame.

Picture quality, from the paced synthetic matrix in
[measurements/2026-11-03/mac-host](measurements/2026-11-03/mac-host/README.md)
(medians of three, FFmpeg PSNR against the exact source pictures, a
forced IDR every second; 5.1 → the speed hint alone → now):

| Case | PSNR dB | IDR max kB |
| --- | --- | --- |
| HEVC 1080p60, 20 Mbps, moving | 40.61 → 34.72 → 42.41 | 925 → 830 → 1329 |
| HEVC 1080p60, 20 Mbps, still | 42.32 → 38.69 → 43.32 | 925 → 1018 → 1279 |
| HEVC 1080p120, 50 Mbps, moving | 44.27 → 40.72 → 47.70 | 1021 → 1215 → 1970 |
| HEVC 1080p240, 80 Mbps, moving | 44.17 → 39.36 → 39.36 | 925 → 1066 → 1066 |
| HEVC 3024×1964@120, 100 Mbps, moving | 43.70 → 36.18 → 36.18 | 2524 → 2794 → 2794 |
| H.264 High 1080p60, 20 Mbps, still | 43.18 → 39.33 → 39.33 | 925 → 844 → 844 |
| H.264 High 1080p120, 50 Mbps, still | 45.18 → 42.92 → 42.92 | 1031 → 1254 → 1254 |

5.1's figures at 240 fps and 3024×1964 come from the 26–45 fps it
managed there. H.264 ignores `PrioritizeEncodingSpeedOverQuality` (its
output is bit-identical either way). It trails 5.1 by up to 4.5 dB
on some cases and gains a little on others; the low-latency rate control
gets 1 dB back at 1080p60/20 but doubles the encode time, so H.264 stays
as it is. HEVC's quality path buys its PSNR with larger IDRs.

Keyframes stay large. VideoToolbox holds P-frames to `DataRateLimits`
but not keyframes: an IDR is 20–40× a P-frame at 1080p (0.8–2 MB).
Nothing found bounds the IDR alone. `ConstantBitRate` and the macOS 26
VBV keys pad P-frames and leave the IDR as it was, `MinAllowedFrameQP`
takes effect only at a keyframe and then holds for the whole GOP (every
P-frame capped too), and `BaseFrameQP` works only under the low-latency
rate control above. So a lost frame is best answered without an IDR
(reference invalidation), and the IDR that remains is paced on the wire.

VideoToolbox recovery uses the public macOS 12 LTR API, loaded at runtime.
`EnableLTR` must succeed before recovery is advertised. Only tokens of
pictures the viewer decoded are acknowledged: at or before its delivered
base, and none from pictures an earlier loss took or that were discarded
in flight at an earlier refresh (each recovery records everything after its
base as missing until the next IDR). `ForceLTRRefresh` then makes a P-frame.
Queued pictures before that submission are discarded; a dropped refresh
requests an IDR. Requests cannot cross an IDR, an already acknowledged newer
base, or an encoder replacement.

An H.264 loss whose dropped pictures (lost, or discarded in flight) carried
an LTR token takes an IDR instead. Such a picture marked a long-term slot
(MMCO 6) and VideoToolbox's refresh does not unmark it: later the encoder
marks the slot again as a replacement, while for the viewer, whose
`frame_num` gap (`gaps_in_frame_num_allowed_flag` is 0) filled its other
reference slots with "non-existing" pictures, it is one picture more. Its
references then exceed `max_num_ref_frames`; FFmpeg warns and decodes on,
Apple's decoder blocks forever (the stream of the #60 review froze a viewer
41 pictures after the refresh). The check runs at the request and again
when the refresh comes out, once the frames that were in flight have called
back. In the pipelined simulator at 3% bursty loss it refuses about half of
H.264 recoveries; HEVC, which lists its references in every slice, is not
affected. A precise rule (only slots the viewer has empty matter) would need
the slice headers parsed.

On M3 Pro the ordinary mode rejects LTR. `LATCH_MAC_LTR=1` opts into
`EnableLowLatencyRateControl`, where H.264 High and HEVC Main accept it;
Main10, Baseline and HDR sessions ignore the switch (their LTR streams are
not validated). The cost belongs to the rate-control mode, not to LTR (the
same with `EnableLTR` skipped): it serialises encode at roughly 210–280
Mpx/s on an idle M3 Pro, so 1440p120, 3024×1964, 4K60 and 1080p120/240 miss
their rate; it sends ⅕–½ of the requested bitrate; under CPU load even
1080p60 falls to 40–58 fps; and at H.264 3024×1964@120 its first encode
fails (the old fallback, a software session, then gave no frame back at
all: the stream froze). HEVC 1080p240 drops from 242.2 to 101.6 fps. So the
switch stays opt-in, with no size-based default
([full measurements](measurements/2026-11-04/rfi-vt-mf/README.md)).

Every Mac host session, opted in or not, has a **stall watchdog**: a
session that holds frames and gives none back for 500 ms (5 s in software)
is replaced by an ordinary hardware session, without LTR, which starts at
an IDR (software only if that fails). The stopped session is not waited
for: it is invalidated on a thread of its own. Under full GPU load plus
twelve CPU hogs the slowest ordinary hardware encode measured was 60 ms
(4K60 H.264), so the watchdog has about 8× headroom.

An opted-in session also protects itself:

- **Failure fallback.** A low-latency session whose encode fails reopens
  as an ordinary hardware session (IDR, no LTR), never in software.
- **Pace fallback.** Over 2-second windows, if more than one frame in
  twenty took longer than the two frames in flight overlap (2 frame
  intervals), or one took over 250 ms, the session reopens in the ordinary
  mode for the rest of the stream; recovery requests then take IDRs.

`ltr_pipeline_with_losses_matches_uninterrupted_decoding` runs the real
submit/collect pipeline (two frames in flight, discards at each boundary)
against a viewer that loses bursts, asks late, re-asks when a refresh is
lost, and meets IDR requests and bitrate changes: every accepted picture
must equal an uninterrupted decode of everything VideoToolbox produced,
discarded frames included, in VideoToolbox and in FFmpeg, with FFmpeg
silent. `ltr_recovery_matches_uninterrupted_decoding` is the older
synchronous check. No Screen Recording permission is needed. Unsupported
sessions retain IDR recovery.

For a permission-free live host, add `LATCH_TEST_VIDEOTOOLBOX=1` to
`LATCH_TEST_PATTERN=sweep` when running `serve`. It feeds synthetic NV12
IOSurfaces to VideoToolbox through the existing CPU picture pipeline. This
measures conversion plus encode, rather than ScreenCaptureKit or the
asynchronous capture pipeline. `LATCH_TEST_VIDEO_LOSS=1:8` or `3:8` on
`view` drops datagram bursts; always use a private test port.

## Permissions

- **Local network**: macOS 15 may ask; allow it so the wake broadcast can
  go out.
- **Incoming connections**: if the macOS firewall is on it may ask when
  this Mac starts sharing. Allow Latch to receive connections.
- Only connecting, nothing else: Latch captures no screen and reads the
  keyboard and mouse only in its own window.
- **Microphone**: asked for only when **Send my microphone** is on, the
  first time a stream starts with it. Until it is allowed (System
  Settings > Privacy & Security > Microphone), macOS hands Latch silence,
  so the PC hears nothing.
- Sharing this Mac with a viewer that sends its microphone needs a virtual
  cable such as [BlackHole](https://existential.audio/blackhole/): Latch
  plays the viewer's microphone into its output, and apps on this Mac pick
  BlackHole as their microphone. `LATCH_MIC_DEVICE` names another output.
- Sharing this Mac also needs **Screen Recording** (the picture and the
  sound, through ScreenCaptureKit) and **Accessibility** (the viewer's
  keyboard and mouse). macOS asks once, when you set up sharing; after
  that the switches are in System Settings > Privacy & Security, and the
  **Sharing** tab has a button that opens each pane. macOS applies a
  newly allowed permission only after Latch restarts, so once a pane has
  been opened the tab offers **Restart Latch**, which restarts the
  background service that records and moves the pointer.

## Uninstalling

`/Applications/Latch.app/Contents/MacOS/Latch --uninstall` stops the
background service, removes its login item (under the names earlier
versions used too), removes Latch from the macOS firewall's list if setup
added it (macOS asks for an administrator), and resets Latch's Screen
Recording and Accessibility permissions. Running it again is harmless.
Then drag `Latch.app` to the Trash; settings stay in
`~/Library/Application Support/io.github.MrBeldum.Latch` until you delete
that folder.

## Connected, but the picture is black

A connection can carry valid video that contains only black pixels.
Latch now detects this after five seconds and shows a persistent message
instead of presenting the stream as healthy. **Restart stream** reconnects
and, for Desktop, resets the PC's capture session. If no video arrives or
decoding fails, the message distinguishes those problems too.

The notice then asks the PC what it can see and says which cause it is.

On a PC with no monitor attached, it is usually the colour mode. Windows
carries on composing the desktop in HDR or wide colour, as the monitor
that is now gone once asked for, on a placeholder display that reports no
luminance at all. Anything converting that desktop to an ordinary picture
turns every frame black, even though the PC's own desktop draws perfectly
normally — which is why the stream looks broken and the PC does not.

Where that mode is one the display supports, the notice offers **Turn off
HDR on the PC**, which does exactly that over the control API and starts a
fresh capture. Where Windows is enforcing it on a placeholder display it
refuses every switch away from it: the mode is a consequence of having no
display rather than a setting, and only giving the PC a display clears it.
Attach a monitor, plug in an HDMI/DisplayPort dummy plug, or install a
virtual display driver on the PC, and select that display for streaming.

The notice names the other causes too — a desktop that is genuinely black
because the display is asleep, a lock screen the PC cannot capture, or a
capture failing while the desktop plainly has a picture.

## Updates

The app checks GitHub for a new release about every six hours and twenty
seconds after it starts. A newer version is downloaded to
`~/Library/Application Support/io.github.MrBeldum.Latch/updates/<tag>/`, checked against the
SHA-256 GitHub publishes for the asset and against its own code signature,
and moved over `/Applications/Latch.app` once no stream is running; the
app then relaunches. The Mac also sends the new `latch-host.exe` to each
PC whose Latch Host is older and already speaks `/v1/update` (3.1+).

Hosts on 3.0 must be upgraded locally or over SSH using the current
Windows release and installer. A Latch 5 viewer cannot stream the retired
GameStream protocol to perform an upgrade through the desktop.
A Windows PC also updates itself while its window is open
([WINDOWS.md](WINDOWS.md#updates)), and checks whatever it is sent against
GitHub's digest for the release. A Linux host shows the new version;
install the new tarball or rebuild the Docker node.

Version 6 fetches public `release.json` manifests with exact version-tagged
URLs, SHA-256 digests and byte sizes. It ignores legacy `github_token`,
`LATCH_GITHUB_TOKEN` and stored Git credentials for distribution. Settings
has the switch and a **Check now** button. The line under
it says what happened last. A copy that is not running from an app bundle
(a development build) reports the new version but does not replace itself.

Some 5.x apps still check the former repository and require a one-time
installation from the public release page. Keep the existing settings
folder. Older Windows hosts may need the same local installer migration
before they can accept updates from this Mac.
[Migration steps](USING.md#moving-from-5x-to-600)

## Staying reachable

- Every PC in the list, and this Mac, shows a warning while its Tailscale
  node key expires. Open the machine in the
  [admin console](https://login.tailscale.com/admin/machines) and choose
  **Disable key expiry**; the warning goes away at the next scan.
- PCs are remembered in `client.toml` with their Tailscale address and
  MAC. While Tailscale on this Mac is off they stay
  listed with when they were last seen, and **Wake** still works over the
  LAN or the public address.

## Viewer decode and high refresh

The viewer offers hardware-supported codecs in AV1 > HEVC > H.264 High >
Baseline order. A cached probe requires both VideoToolbox's hardware query
and a decoded hardware picture, including AV1 on M3 or newer hardware; older hosts skip the new name and negotiate HEVC/H.264.
AV1 sized OBUs are submitted with an `av1C` format description. All codecs
retain the IOSurface → Metal path. HEVC Main10 callers can select P010
output without losing ten-bit samples. Normal sessions negotiate colour
automatically, retaining P010 for ten-bit SDR or native HDR presentation,
with tone mapping on the renderer fallback path.

Decode calls run on a worker thread of the session and are waited for at
most 1 s. Apple's H.264 decoder can block forever inside
`VTDecompressionSessionDecodeFrame` on a stream whose references exceed
`max_num_ref_frames` after a loss (FFmpeg decodes the same stream
bit-exactly; the recorded stream is `tests/fixtures/vt-ltr-overflow.h264`).
A session that misses the deadline is abandoned to its thread (invalidating
it blocks too) and replaced from the same format; the error makes the
viewer ask for a keyframe, so the picture freezes for a second, not for
good. Nothing else calls into an abandoned session; its thread invalidates
and releases it if the call ever returns. Each abandoned session keeps one
thread and its pictures (about 0.4 MB at 960×540; at 4K perhaps a couple of
hundred MB of IOSurfaces, not measured). So each codec (H.264 Baseline and
High count as one) may abandon three sessions per 10 minutes: once it has,
its decoders fail every access unit until the oldest stall is 10 minutes
old, the session's three-error rule renegotiates without that codec alone
(HEVC and AV1 keep decoding after H.264 stalls), and the next stream may use
it again once the window has passed.

Frame arrival wakes egui immediately. `AutoNoVsync` and a maximum frame
latency of one avoid an application 60 Hz cap on ProMotion. The newest
picture is prepared for rendering without an extra frame queue; previous
IOSurfaces live until their GPU submissions complete. Physical scanout and
compositor delay remain display-dependent. See the [measurements and
native viewer command](measurements/2026-11-03/viewer/README.md).

### Transport measurement (2026-11-03)

The viewer drains UDP in reusable 32-datagram `recvmsg_x` batches and hands
completed video to decode within the batch. Unsupported batch receive falls
back on the nonblocking socket. Host sends retain the shared unconnected UDP
socket and scalar sends. See [transport measurements](measurements/2026-11-03/transport/README.md)
for measured headroom and outstanding MTU/offload work.

### HDR viewing

P010 decoder output remains ten-bit through the Metal texture cache. A
separate RGBA16Float CAMetalLayer opts into extended dynamic range and uses
extended linear Display P3. PQ is decoded to absolute luminance, converted
from BT.2020 to P3, and mapped into NSScreen's current EDR headroom. The
viewer advertises HDR only when the native path initializes on an EDR
screen; moving to an SDR screen compresses highlights to that screen.
Only PQ streams use the float layer. Ten-bit SDR (an SDR desktop streamed
in Main10) is drawn by the wgpu P010 shader in egui's own surface, like
eight-bit SDR.

The presenter (EDR layer or wgpu) is chosen when a stream starts and when
the window moves to another screen, never per frame: two surfaces taking
turns is a flicker. The layer is attached to the NSView egui's wgpu surface
draws in, taken from the window handle, not to `NSApplication.mainWindow`
(nil while another app or window is key, or a space is changing, which made
the first HDR build, PR #55, alternate paths several times a second). A frame with
no free drawable is dropped, as a late frame would be; a real EDR failure
gives the rest of the stream to wgpu and is logged once. A window on no
screen for a moment (a space changing) keeps the screen it had, so that is
no change; a new screen counts once it has held for 300 ms, and screen
changes switch the presenter at most once a second. Every switch is
logged (`video presenter -> …`, with its rate), and with
`RUST_LOG=info,latch_client::video=debug` a line a second counts the
frames each presenter drew.

To check this in the real window, start Latch.app's executable with
`LATCH_TEST_SERVE=<ip>:<port>` naming a test host (the engine's `serve`
example) and `LATCH_DATA_DIR` set to a scratch folder: the window connects to
it at once, without any control-service request, and sends it no input,
sound or gamepads. `LATCH_TEST_EXERCISE=<seconds>` then steps through the
stats overlay, the toolbar and full screen and logs focus changes, so a
presenter switch shows in `panel.log` beside its cause.

The float layer is inserted *below* wgpu's Metal layer. While it shows,
wgpu's layer is made non-opaque and the video callback writes transparent
pixels over the picture, so CoreAnimation composites the toolbar, stats,
settings and toasts over HDR video at SDR white; the picture's own values
never change with the controls. Chroma is sampled at its signalled siting
(left: a quarter chroma texel right of the centred position).

The native shader has offscreen Metal numerical tests (headroom, left
chroma siting), and an egui test toggles an overlay over HDR patches and
checks the EDR values and egui's transparency stay put; the full HDR
encode/decode/wgpu SDR tone-map test compares against source pixels. Live
ScreenCaptureKit HDR capture still needs a Screen Recording authorized
process. See [measurements](measurements/2026-11-03/hdr/README.md).

### FEC-first transport (2026-11-04)

Radio loss raises parity before reducing the wire budget. Keyframes and
reference-recovery frames carry at least 30% and four parity shards per block;
normal frames adapt to erasure runs too. Idle encoders keep their target and
resume at once on a known clean path. Idle probe trains reject jitter,
interior loss and reordering; a lower encoder target also requires independent
queue or delivery evidence. Unknown capacity only constrains resume pacing.
Probe padding is outside the wire budget and skipped below an 8 Mbps retained
budget. Queue growth still cuts rate; moderate loss without queue
growth requires a rate-correlation test.
See [measurements and parity costs](measurements/2026-11-04/transport-fec/README.md).
