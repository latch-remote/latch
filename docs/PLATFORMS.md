# Platform support

Audit of version 6.0.0 at `bf5da0d`, 14 November 2026. The requirement is
sharing in all six Windows/macOS/Linux directions. Each platform has a host
and viewer implementation. Complete real-desktop sessions covering every
feature in all six directions remain untested.

## Reading the matrices

- **works** means an implemented path has focused automated or recorded
  verification for the stated scope.
- **partial** means a usable path has a functional restriction or an external
  prerequisite. **missing** means the audited implementation lacks the feature.
- **untested** means code exists but verification of the stated native path
  was not found. An untested path can still work on a particular machine.
- **U** identifies unit/component tests. **S** identifies synthetic same-runner
  session tests. **F** identifies a committed real-machine measurement. **N**
  means no feature-specific execution evidence found. OS suffixes are **W**
  Windows, **M** macOS and **L** Linux. Source footnotes give `file:line` evidence.
- CI configures workspace tests on `windows-latest`, `macos-15`,
  `ubuntu-24.04` x64 and `ubuntu-24.04-arm` arm64. U/S labels describe tests
  included there, subject to platform gates and hardware skips. This audit
  used static reading and existing verification records.[^ci]

Field evidence is narrower than a platform-wide qualification. **F-WM** is
RX 6600 Windows capture/AMF HEVC to an M3 Pro viewer. **F-LM** is ARM64
Ubuntu X11/software H.264 to that Mac. **F-MW** is synthetic Mac
VideoToolbox H.264 to Windows DXVA, including the public relay and synthetic
input. **F-HDR** is numerical GPU/codec readback on Mac and RX 6600.
Physical HDR panel appearance and input-to-photon remain unmeasured.[^field]

## Host

| Capability | Windows | macOS | Linux |
|---|---|---|---|
| Screen capture | **works** · DXGI Desktop Duplication, D3D11 conversion. F-WM, U-W.[^wcap] | **partial** · ScreenCaptureKit IOSurfaces, Screen Recording grant. U-M configuration, native screen-to-viewer rate unmeasured.[^mcap] | **works** · X11 root, MIT-SHM with image fallback, Damage. F-LM, U-L. Wayland **missing**, in flight (issue #80, `t3/wayland-capture-input`).[^lcap][^flight] |
| Display selection | **partial** · automatic virtual-output-first, then primary ranking. U-W. Viewer chooser in flight (issue #78, `t3/choose-shared-display`).[^wdisplay][^flight] | **partial** · main display or `LATCH_MAC_DISPLAY` override. N for multi-display sessions. Chooser in flight (issue #78, `t3/choose-shared-display`).[^mcap][^flight] | **partial** · whole root of `$DISPLAY`, including its monitors. U-L geometry. Chooser in flight (issue #78, `t3/choose-shared-display`).[^lcap][^flight] |
| Virtual display | **partial** · uses an existing virtual display driver and seeds its modes. Setup does not install that display driver. U-W setup, F-WM existing VDD.[^vdd] | **missing** · no virtual-display creation path. N.[^resize] | **partial** · supplied Xvfb/systemd kit, separate from desktop setup. Script selftest on Linux x64 checks cookies, F-LM Xvfb.[^ldisplay] |
| Resolution/refresh change and restore | **works** · existing virtual output only, topology restored. U-W selection/restoration logic, F-WM VDD. Physical screens retain their mode.[^wdisplay] | **missing** · host mode change is a no-op, capture scales/aspect-fits. U-M fit geometry.[^resize][^mcap] | **partial** · opt-in `LATCH_DISPLAY_FOLLOWS_VIEWER=1`, RandR, restore afterward. U-L mode math, F-LM resize.[^ldisplay] |
| Hardware H.264 8-bit | **works** · GPU Media Foundation MFT, High/Baseline. U-W probes, RX 6600 field evidence. NVIDIA/Intel untested.[^wencode] | **works** · VideoToolbox High/Baseline. U-M IOSurface roundtrip, F-MW H.264 synthetic source.[^mencode] | **untested** · NVENC or VA-API High/Baseline, driver-probed. U-L capability/framing logic, N real GPU.[^lencode] |
| Hardware HEVC 8-bit | **works** · AMD AMF first, MF fallback. F-WM RX 6600, U-W. NVIDIA/Intel MFTs untested.[^wencode] | **works** · VideoToolbox Main. U-M + independent FFmpeg decode, F encoder benchmarks.[^mencode] | **untested** · NVENC or VA-API Main, CPU capture/upload. U-L parameters/framing, N real GPU.[^lencode] |
| Hardware AV1 8-bit | **untested** · MF hardware MFT only after encode probe. U-W framing, RX 6600 has no encoder.[^wencode] | **missing** · VideoToolbox encoder rejects AV1. U-M codec handling.[^mencode] | **untested** · NVENC code/probe. VA-API AV1 encode is **missing**. U-L parameters/framing, N real GPU.[^lencode] |
| Software H.264 8-bit | **works** · MF software, then Rust Constrained Baseline. U-W software roundtrip, S-all, F reverse synthetic Windows→Mac.[^wencode][^sessiontests] | **works** · VideoToolbox software fallback, High/Baseline. U-M forced software IOSurface roundtrips. Rust software test-pattern path also exists.[^mencode][^sessiontests] | **works** · Rust Constrained Baseline, automatic for Baseline requests. U-all, S-all, F-LM.[^lencode][^sessiontests] |
| Software HEVC / AV1 | **missing** · product fallback encodes H.264. N.[^wencode] | **partial** · VideoToolbox HEVC software retry exists, U-M forced software roundtrips. AV1 **missing**.[^mencode] | **missing** · product software encoder is H.264. N.[^lencode] |
| 10-bit / HDR encode | **partial** · HEVC Main10 SDR or HDR10, scRGB→PQ/P010, AMF/MF, downgrade on open failure. F-HDR, interactive tests ignored by default. H.264/AV1 10-bit **missing**.[^wencode][^colour] | **partial** · HEVC Main10 SDR/HDR10, P010/PQ. HDR capture requires Apple Silicon + macOS 15, SDR8 retry. U-M + F-HDR synthetic. H.264/AV1 10-bit **missing**.[^mencode][^mcap][^colour] | **missing** · production capture/encode returns SDR8, including HEVC. N HDR.[^lencode][^colour] |
| Cursor | **partial** · DXGI colour, monochrome and masked/XOR shapes composited into video. Separate plane **missing**. U-W shader/shape tests, field desktop path.[^wcursor] | **partial** · SCK always draws cursor into video. Separate plane **missing**. U-M config, N shape/Retina pair qualification.[^mcap] | **works** · XFixes shape/visibility, composited or separate RGBA plane, scaled hotspot. U-L cursor + U-all scaling, S-all protocol, F-LM capture. Wayland in flight (issue #80, `t3/wayland-capture-input`).[^lcursor][^sessiontests][^flight] |
| Desktop audio capture | **works** · WASAPI default-output loopback, converts rate/format, stereo/5.1/7.1. U-W resampling, F Windows loopback software timing.[^audio] | **partial** · SCK 48 kHz stereo capture, converted to i16. U-M sample handling, N full desktop-audio pair qualification.[^audio] | **partial** · PulseAudio protocol default-sink monitor, including PipeWire Pulse server, stereo/5.1/7.1. U-L frame/channel maps, N acoustic pair verification.[^audio] |
| Microphone playback as a host input | **partial** · external VB-CABLE or named virtual output required. U-all PCM/sink pipeline, N physical cable session.[^mic] | **partial** · external BlackHole or named virtual output required. U-all PCM/sink pipeline, N physical cable session.[^mic] | **partial** · session null sink + remapped Pulse source created/removed by engine. U-all transport, N real recording-app pair verification.[^mic] |
| Keyboard injection | **works** · US scan table → SendInput, Unicode text, elevated helper for higher-integrity apps. U-W constructed events and helper trust, S synthetic input.[^keys] | **partial** · VK→ANSI Carbon key codes, Unicode CGEvent text, Accessibility required. U-M builds events without posting. Layout/pair coverage incomplete.[^keys] | **partial** · VK→XKB physical names→current server keycodes, XTest, temporary unused key for Unicode. U-L key maps, isolated `linux_input --keys` manual harness. Wayland in flight (issue #80, `t3/wayland-capture-input`).[^keys][^flight] |
| Mouse absolute/relative injection | **works** · virtual-desktop normalized physical pixels / relative SendInput. U-W coordinate/raw-flag tests. OS acceleration settings can affect desktop motion.[^mouse] | **partial** · CGEvent positions in global points, relative deltas scaled/clamped to captured display. U-M event/fit tests, N cross-DPI games.[^mouse] | **partial** · XTest absolute root pixels / relative motion. U-L geometry, manual isolated X11 harness. N acceleration parity, Wayland in flight (issue #80, `t3/wayland-capture-input`).[^mouse][^flight] |
| Scroll injection | **works** · vertical/horizontal wheel, 120 wire units per notch. U-W constructed events.[^scroll] | **partial** · 120 units become 40 CGEvent pixels, horizontal sign conversion. U-M event tests.[^scroll] | **partial** · accumulates 120 units into X buttons 4/5/6/7, smooth sub-notch motion becomes discrete steps. U-L remainder tests. Wayland in flight (issue #80, `t3/wayland-capture-input`).[^scroll][^flight] |
| Touch / pen injection | **missing** · wire has no touch/pen events. N.[^wireinput] | **missing** · same protocol limit. N.[^wireinput] | **missing** · same protocol limit. N.[^wireinput] |
| Gamepad injection | **partial** · ViGEmBus driver, 8 protocol slots, Xbox 360 / DS4, XInput has 4 slots. U-W report/ABI, S fake bus. N physical-controller qualification.[^pads] | **missing** · native opener returns an error, zero usable virtual pads. S uses fake bus only.[^pads] | **partial** · writable `/dev/uinput`, up to 16 Xbox 360-model devices, other models translated. U-L report mapping, S fake bus. N real uinput game session.[^pads] |
| Gamepad feedback / rumble | **partial** · ViGEm notifications for motors/LED/light bar. U-W reports, S fake feedback. N physical pair.[^pads] | **missing** · no host virtual-pad bus. N.[^pads] | **partial** · uinput force-feedback upload/play/erase → two motors. U-L mapping, S fake feedback. N physical pair.[^pads] |
| Clipboard | **partial** · text ≤32 KiB, CF_UNICODETEXT, CRLF↔LF conversion. U-all normalization/state, N six-pair OS-clipboard roundtrip.[^clipboard] | **partial** · text via retained arboard handle, content hash sequence. U-all state, N six-pair OS clipboard.[^clipboard] | **partial** · retained X11 arboard handle, CLIPBOARD + PRIMARY writes, dead-X-connection restart handling. U-all state/restart, N six-pair OS clipboard.[^clipboard] |
| Wake as a target | **partial** · WOL probe/setup/listener and NIC power settings. U-W generated scripts, U-all packets, sleeping-machine delivery depends on LAN/forwarder/NIC. N new physical wake test.[^wake] | **missing** · no target probe/setup, remote sleep refused because Latch cannot wake it. U-all power policy.[^wake] | **missing** · no target probe/setup, core can send packets for a saved target. U-all power policy. N Linux target provisioning.[^wake] |
| Background/login service | **partial** · signed-in user Run key + elevated scheduled input task. U-W setup/trust, Windows CI script execution. Pre-login desktop **missing**.[^lifecycle] | **partial** · launchd LaunchAgent in logged-in user session. U-M generated plist/supervision plans, N fresh-login field qualification.[^lifecycle] | **partial** · systemd user unit, native headless deployment kit. U-L plans + Linux x64 deployment selftests. Requires DISPLAY/XAUTHORITY/Pulse environment.[^lifecycle] |
| Permissions flow | **partial** · UAC setup for firewall, helper, power and ViGEm, interactive desktop required. U-W parser/protection/uninstall CI, secure desktop unqualified.[^permissions] | **partial** · setup requests Screen Recording/Accessibility, engine preflights both, microphone privacy via capture API/bundle usage text. U-M configuration, user grants/restart required.[^permissions] | **partial** · X authentication, Pulse access, render-node and uinput permissions supplied externally. No Wayland portal flow, in flight (issue #80, `t3/wayland-capture-input`). U-L bounded X errors, N desktop permission walkthrough.[^permissions][^flight] |
| Installer / packaging | **partial** · x64 zip + PowerShell install, shortcuts/Apps registration. U-W scripts, CI packages executable. Scoop manifest merged in PR #93 (issue #81, `t3/add-package-manager-support`), publication pending.[^release][^flight] | **partial** · arm64 tar/app, ad-hoc codesign, manual quarantine removal, build/install scripts. U-M bundle/manifest, CI bundle verification. Intel package **missing**.[^release] | **partial** · x64/arm64 glibc ≥2.39 tarballs, native/systemd and container kits, desktop binary also views. Linux CI deploy selftests. AUR/Nix definitions merged in PR #93 (issue #81, `t3/add-package-manager-support`), publication pending.[^release][^flight] |
| Updater | **partial** · window checks/downloads digest-verified exe, authenticated Windows push and rollback. U-all manifest/stage/handover, Windows CI. CPU restrictions below.[^update] | **partial** · bundled app download, digest/codesign/version verification, swap/relaunch. U-M bundle/update tests, CPU restrictions below.[^update] | **partial** · version notice/manual release URL. Self-installing update **missing**. U-all offer tests.[^update] |
| Uninstall | **works** · removes owned registration/tasks/rules/drivers and restores recorded power values, leaves program/settings. Windows CI runs twice and checks restoration.[^uninstall] | **partial** · removes agent/firewall registration, stops service, resets ScreenCapture/Accessibility grants, program/settings remain. U-M script/plans, N full machine uninstall.[^uninstall] | **partial** · desktop `--uninstall` removes user unit/stops service, program/settings and separately provisioned headless kit require manual removal. U-L plans, N full kit uninstall.[^uninstall] |
| Public relay support | **partial** · native TLS + encrypted media, CPU feature gate. U/S-W, F-WM/MW synthetic field.[^relay] | **partial** · same lanes/crypto. U/S-M, F-WM/MW synthetic field.[^relay] | **partial** · same lanes/crypto. U/S-L on both arches, N Linux cross-OS public-relay field session.[^relay] |

## Viewer

The release's `latch-host` executable embeds the `latch-client` GUI and
`latch-stream` viewer.[^binary]

| Capability | Windows | macOS | Linux |
|---|---|---|---|
| Hardware H.264 8-bit decode | **works** · MF/DXVA High/Baseline probe, D3D11 surfaces. U-W installed-decoder pixels, F-MW.[^decode] | **works** · VideoToolbox High/Baseline probe, IOSurface. U-M pixels, F-WM/F-LM.[^decode] | **missing** · software Baseline only, VA-API in flight (issue #79, `t3/linux-vaapi-decode`). N native decode.[^decode][^flight] |
| Hardware HEVC 8-bit decode | **works** · MF/DXVA probe, installed decoder required. U-W hardware-conditional fixture, F RX 6600 replay.[^decode] | **works** · VideoToolbox Main. U-M hardware-conditional fixtures, F-WM.[^decode] | **missing** · VA-API in flight (issue #79, `t3/linux-vaapi-decode`). N.[^decode][^flight] |
| Hardware AV1 8-bit decode | **partial** · MF/DXVA probe, installed codec/GPU required. U-W conditional fixture, F RX 6600 replay, no AV1 host session.[^decode] | **partial** · VideoToolbox hardware probe, supported chips such as M3+. U-M conditional fixture, N AV1 host session.[^decode] | **missing** · no AV1 backend. VA-API work in flight (issue #79, `t3/linux-vaapi-decode`), exact codec scope unverified.[^decode][^flight] |
| Software H.264 8-bit decode | **works** · MF CPU or Rust Baseline fallback, higher profiles offered only after hardware probe. U-W fallback/visible-pixel tests, S-all.[^decode] | **works** · VT software can be used, Rust Baseline fallback. Higher profiles offered after hardware probe. U-M fallback + S-all.[^decode] | **works** · Rust Constrained Baseline only. U-all codec/fallback, S-L, N Linux-viewer cross-machine measurement.[^decode] |
| Software HEVC / AV1 decode | **partial** · MF transform may use CPU after a successful capability probe, no portable Rust fallback. U-W backend logic, N forced CPU HEVC/AV1 qualification.[^decode] | **partial** · VT reports hardware/software session mode, advertising HEVC/AV1 requires a real hardware picture. No portable Rust fallback. U-M probes, N forced CPU HEVC/AV1 qualification.[^decode] | **missing** · software decoder accepts Baseline H.264 only. N.[^decode] |
| 10-bit / HDR decode | **partial** · negotiated HEVC Main10/P010, CPU readback path. F-HDR numerical probes, hardware-dependent tests ignored. H.264/AV1 10-bit **missing**.[^decode][^colour] | **partial** · HEVC Main10/P010 IOSurface. U-M ten-bit fixtures, F-HDR. H.264/AV1 10-bit **missing**.[^decode][^colour] | **missing** · rejects 10-bit, reports SDR-only output. N. In flight (issue #79, `t3/linux-vaapi-decode`) may alter backend capability.[^decode][^colour][^flight] |
| SDR presentation / latency | **works** · wgpu DX12, same-adapter D3D11 shared textures for NV12, newest-frame slot. U-W import/layout, F renderer preparation.[^present] | **works** · wgpu Metal imports IOSurface planes. U-M shader/import, F renderer preparation.[^present] | **partial** · wgpu uploads CPU NV12, native-GPU output unmeasured. U-all shader math, N Linux window session measurement.[^present] |
| HDR presentation | **partial** · native HDR10 child swapchain, SDR tone-map fallback, display-change stabilization. U-W state/shader, F-HDR readback, physical HDR panel untested.[^present] | **partial** · native RGBA16Float Metal EDR layer, tone-map fallback. U-M state/shader, F-HDR readback, physical HDR panel untested.[^present] | **missing** · no native HDR output/capability. N.[^present][^colour] |
| Vsync / low-latency mode | **partial** · `AutoNoVsync`, requested frame latency 1, compositor/driver may choose fallback. F preparation timings, scanout N.[^present] | **partial** · same wgpu policy, native EDR layer for HDR. F preparation timings, scanout N.[^present] | **partial** · same wgpu policy, N real display latency.[^present] |
| Keyboard capture / layouts | **partial** · egui events forwarded at redraw, physical-key/US VK map + IME commits. Win/Meta held modifier **missing**. U-all recorder, OS-event forwarding in flight (issue #77, `t3/send-input-on-os-events`).[^viewerinput][^flight] | **partial** · egui + AppKit menu-key overrides, Cmd maps to Ctrl by default for W/L, Cmd for Mac. Cmd-Tab/Space stay local. U-M menu + U-all recorder, in flight (issue #77, `t3/send-input-on-os-events`).[^viewerinput][^flight] | **partial** · egui physical-key/US VK map + IME commits, Super/Meta held modifier **missing**, compositor shortcuts stay local. U-all recorder, in flight (issue #77, `t3/send-input-on-os-events`).[^viewerinput][^flight] |
| Mouse relative / pointer lock | **partial** · egui raw `MouseMoved`, Locked/Confined grab, scaled/accumulated before send at redraw. U-all recorder/fit, N pair acceleration parity. In flight (issue #77, `t3/send-input-on-os-events`).[^viewerinput][^flight] | **partial** · same pipeline + native pointer hide, display/stream scaling. U-all recorder, N Retina/raw-game pair qualification. In flight (issue #77, `t3/send-input-on-os-events`).[^viewerinput][^flight] | **partial** · egui Locked/Confined depends on window system/compositor. U-all recorder, N X11/Wayland lock qualification. In flight (issue #77, `t3/send-input-on-os-events`).[^viewerinput][^flight] |
| Scroll capture | **works** · egui point/line/page conversion to signed wheel units. U-all unit/sign tests, N physical-device pair feel. Dispatch in flight (issue #77, `t3/send-input-on-os-events`).[^scroll][^flight] | **works** · same conversion, point deltas retained as fractional remainders. U-all unit/sign tests, N trackpad pair feel. Dispatch in flight (issue #77, `t3/send-input-on-os-events`).[^scroll][^flight] | **works** · same conversion. U-all unit/sign tests, N physical-device pair feel. Dispatch in flight (issue #77, `t3/send-input-on-os-events`).[^scroll][^flight] |
| Touch / pen capture | **missing** · no dedicated protocol events. N.[^wireinput] | **missing** · same. N.[^wireinput] | **missing** · same. N.[^wireinput] |
| Gamepads / rumble | **partial** · Windows.Gaming.Input, Xbox + raw Sony layouts, motors/trigger motors for Gamepad objects. Raw Sony feedback absent. U-W mapping, S fake bus, N physical pair.[^viewerpads] | **partial** · GameController Xbox/Sony/Switch/MFi, Core Haptics motors, motion/touchpad forwarding absent. U-M mapping, S fake bus, N physical pair.[^viewerpads] | **partial** · evdev input, FF_RUMBLE output, device access required. U-L mapping, S fake bus, N physical pair.[^viewerpads] |
| Audio playback | **works** · low-period IAudioClient3, cpal fallback, device format/rate/layout conversion. U-W/output mixer, F Windows software click timing on virtual speakers.[^playback] | **partial** · cpal/CoreAudio device format/rate/layout conversion. U-M layout + U-all jitter/mixer, N audible pair measurement.[^playback] | **partial** · cpal/ALSA device format/rate/layout conversion. U-L layout + U-all jitter/mixer, N audible pair measurement.[^playback] |
| Microphone capture | **partial** · cpal default-input format conversion/downmix/resample to mono. U-all synthetic capture→receiver, N device pair.[^mic] | **partial** · same, macOS microphone privacy prompt/usage text. U-all synthetic capture→receiver, N device pair.[^mic] | **partial** · same, input-device/session access required. U-all synthetic capture→receiver, N device pair.[^mic] |
| Clipboard | **partial** · client syncs bounded text through HTTP and egui clipboard/paste events. U-all ordering/echo/paste tests, N six-pair native clipboard.[^clipboard] | **partial** · same + NSPasteboard change count avoids stale echoes. U-M counter + U-all ordering, N six-pair native clipboard.[^clipboard] | **partial** · same client path, host may expose PRIMARY-aware paste shortcuts. U-all ordering, N Wayland/native six-pair clipboard.[^clipboard] |
| Fullscreen / multi-monitor / DPI | **partial** · fullscreen viewport, local display size/refresh, aspect-fit clicks, native HDR display tracking. U-all geometry/state, N mixed-DPI screen transitions. One stream at a time.[^fullscreen] | **partial** · same, global host points vs Retina output pixels handled separately. U-M geometry/state, N mixed-Retina pair transitions. One stream at a time.[^fullscreen] | **partial** · same fullscreen/viewport geometry, N mixed-scale/Wayland monitors. One root capture/session, host chooser in flight (issue #78, `t3/choose-shared-display`).[^fullscreen][^flight] |
| Installer / packaging | **partial** · same unified x64 zip/install as host, Scoop manifest merged in PR #93 (issue #81, `t3/add-package-manager-support`), publication pending. Windows CI packaging/scripts.[^release][^flight] | **partial** · same arm64 app/archive as host. Mac CI bundle/codesign, Intel artifact **missing**.[^release] | **partial** · same x64/arm64 unified binary despite `host` archive name, no dedicated viewer package. Linux CI builds, AUR/Nix definitions merged in PR #93 (issue #81, `t3/add-package-manager-support`), publication pending.[^release][^flight] |
| Updater | **partial** · local exe offer/install, digest + version checks. U-W/all update tests, N fresh installed GUI update measurement.[^update] | **partial** · bundled app replacement/relaunch. U-M installer/update scripts, ad-hoc signing can require privacy reapproval.[^update] | **partial** · manual-update notice only. U-all offer tests.[^update] |
| Wake sender / relay | **partial** · common WOL packet sender + CPU-gated relay. U/S-W, F-MW relay.[^wake][^relay] | **partial** · common WOL sender + CPU-gated relay. U/S-M, F-WM/MW relay.[^wake][^relay] | **partial** · common WOL sender + CPU-gated relay. U/S-L, N cross-OS public-relay field session.[^wake][^relay] |

## Six host → viewer directions

Every row uses the normal codec-intersection, colour and reconnect logic in
`engine::server` and `stream::session`. No row has a complete physical
keyboard/mouse/gamepad/clipboard/audio/HDR acceptance record.[^negotiation]

| Direction | Known session evidence | Unverified | Broken, missing or restricted |
|---|---|---|---|
| Windows → Mac | **partial** · F-WM actual DXGI/AMF HEVC desktop delivery, F reverse Windows-software H.264 synthetic input/relay. Numerical Windows HDR→Mac decode/render probes.[^field] | NVIDIA/Intel, AV1 host, real gamepads/rumble, mic cable, clipboard/layout permutations, mixed DPI, physical HDR and scanout. | Windows cursor remains in video. System shortcuts remain viewer-OS dependent.[^wcursor][^viewerinput] |
| Mac → Windows | **partial** · F-MW synthetic IOSurface→VT H.264→DXVA/client renderer, public relay, synthetic key-to-picture and software audio delivery.[^field] | Full SCK desktop session, HEVC whole session, actual host CGEvent injection, mic, native clipboard, physical HDR and controllers. | Mac host gamepad injection and virtual-display/mode change **missing**. A Windows viewer's Media Foundation decoder withholds and mislabels pictures from Mac VideoToolbox H.264: stale pictures in the pair replay, expected as added delay in a session (issue #118). Windows held Win modifier does not become Mac Command, clipboard edit chords can synthesize Command.[^pads][^resize][^viewerinput] |
| Windows → Linux | **untested** cross-machine pair, S-L establishes local real receive/decode of synthetic Baseline sessions.[^sessiontests] | Windows-generated Baseline against Linux decoder, all desktop I/O, distro/window-system/DPI combinations. | Linux viewer only advertises H.264 Baseline/SDR8. HEVC/AV1/Main10/HDR viewing **missing**. Windows host must choose H.264/MF or software. Super capture and separate Windows cursor **missing**.[^negotiation][^decode][^wcursor] |
| Linux → Windows | **untested** cross-machine pair, F-LM validates Linux X11/software source separately, S-W validates viewer receive separately.[^field][^sessiontests] | Linux hardware codecs/Windows decoder combinations, native input/pad/clipboard/audio/mic pair. | Linux Wayland hosting **missing**, X11 required. Linux host is SDR8 and emulates Xbox pads. Held Windows Meta absent at viewer.[^lcap][^lencode][^pads][^viewerinput] |
| Mac → Linux | **untested** cross-machine pair, U-M proves VT Baseline output with independent decode, S-L proves Rust viewer path separately.[^mencode][^sessiontests] | VT Baseline→Rust interoperability session, native injection, clipboard/audio/mic, raw movement and Retina scaling. | Viewer must negotiate Baseline/SDR8, HEVC/Main10/HDR unavailable. Mac gamepad injection, host mode change and separate cursor **missing**. Linux Super→Command held mapping absent.[^decode][^pads][^resize][^viewerinput] |
| Linux → Mac | **partial** · F-LM X11/software Baseline→Mac, motion/resize, synthetic key-to-picture. F-LM does not certify real keyboard injection.[^field] | Linux GPU encode, actual pads/rumble, acoustic audio/mic, native clipboard/layouts and mixed Retina displays. | X11 host only, SDR8. Sony pads become Xbox on Linux. Smooth scroll becomes X wheel steps. Mac Cmd defaults to Ctrl for this host.[^lcap][^pads][^scroll][^viewerinput] |

### Pair-specific contracts and caveats

1. **Keys and modifiers.** The wire sends Windows VK values and a modifier
   mask, plus separate UTF-8 text events. Viewer physical-key preference maps
   through a US-oriented egui table. Windows hosts use US scan codes, Mac
   hosts map to ANSI Carbon key positions, X11 hosts map to XKB physical
   names in the current server keymap. Physical positions and typed characters
   can diverge when layouts differ. Unicode/IME commits have a separate path,
   with temporary X11 key remapping and 12 ms per character. Left/right
   modifiers and general scan-code identity are lost at the viewer. Mac
   Option maps to Alt, Control to Ctrl, and Command maps to Ctrl by default
   on W/L hosts or to Meta/Command on Mac hosts. W/L viewers expose Ctrl/Alt/
   Shift but held Meta uses only egui's `mac_cmd`, so Win/Super→Mac Command
   is incomplete. Clipboard Copy/Cut/Paste can synthesize Command for Mac
   hosts, other Ctrl shortcuts retain Ctrl. OS-reserved combinations and
   secure-attention sequences need separate qualification.[^keys][^viewerinput]
2. **Mouse and scroll.** All pairs send relative deltas or an absolute point
   in the encoded picture. Aspect-fit bars and display origin are accounted
   for. W hosts use DPI-aware physical virtual-desktop pixels, M hosts global
   points, L hosts X root pixels. Viewer deltas are scaled from viewport to
   stream/desktop geometry. Acceleration equivalence across SendInput,
   CGEvent and XTest is unverified. Wheel conversion uses 3 units/egui point,
   120/line and 1200/page, with horizontal sign inversion. W injects wheel
   units, M converts each notch to 40 pixels, L accumulates discrete wheel
   buttons. This changes fine-scroll feel for W/M viewers controlling L.
   Cursor shapes from L are bounded RGBA plus hotspot/visibility at the local
   pointer, W preserves DXGI XOR shapes through video composition, M sends
   the SCK-composited cursor. Pair tests for custom shapes, scale changes and
   captured/free transitions remain absent.[^mouse][^scroll][^lcursor][^wcursor]
3. **Clipboard.** Every pair exchanges bounded text, with a 32 KiB UTF-8
   boundary. Windows host reads UTF-16 CF_UNICODETEXT and exports LF, writes
   canonicalize CRLF. M/L host arboard paths keep text as supplied. W viewer
   clipboard encoding/newline conversion is delegated to egui's OS backend.
   Linux host writes PRIMARY too and advertises it, allowing Shift+Insert
   paste in terminals. Images, rich text and files have no transfer path.
   Host polling, echoes and paste-before-key ordering have unit tests,
   real native clipboard roundtrips in all six pairs remain untested.[^clipboard]
4. **Audio and microphone.** Desktop wire audio is lossless `lpcm-rice`,
   signed i16, 48 kHz, interleaved WAVE speaker order. M capture is stereo,
   W/L may supply stereo, 5.1 or 7.1 up to the viewer's offer. Explicit
   Pulse maps, WAVE masks and CoreAudio/ALSA output layouts feed conversion
   and downmixing. Playback follows the device's sample rate/format, with
   jitter/drift control. Microphone is 48 kHz mono, normally 240 samples
   per frame, downmixed/resampled by cpal capture. W/M targets need an
   external virtual cable, L creates a Pulse virtual source. Tests cover
   conversion, speaker ordering, jitter and synthetic PCM delivery, including
   relay routing. They leave actual endpoint permissions, acoustic output
   and recording-app selection unverified across pairs.[^audio][^playback][^mic]
5. **Codec and colour.** Auto intersects the viewer's probed list with host
   encoders. Joint colour preference can rank HEVC Main10/HDR above SDR8 AV1,
   explicit AV1 retains its priority. Main10 is HEVC-only. Default colour is
   limited-range BT.709 SDR8, HDR10 is BT.2020/PQ with metadata. Host pipeline
   output determines the accepted format after fallback. Windows and Mac
   can retry SDR8 if Main10 open fails. Linux pipelines return SDR8 even
   when HEVC is selected. W/M→L therefore asks for H.264 Baseline, including
   VT→Rust or Windows MF→Rust. Linux auto hosting deliberately uses Rust
   Baseline for software-only viewers, forcing NVENC/VA-API Baseline needs
   cross-decoder qualification. Windows NVIDIA/Intel use MF vendor MFTs,
   with direct AMF only for AMD HEVC. HEVC/AV1 failures have no Rust decoder
   or same-codec portable software encoder fallback, failure cooldown and
   reconnect renegotiate. H.264 has software fallback. Linux GPU↔W/M and
   both W/M→Linux native-encoder pair fixtures remain unverified.[^negotiation][^decode][^colour][^lencode]
6. **Controllers and display geometry.** Windows hosts emulate Xbox 360 or
   DS4 according to the advertised pad kind, Linux hosts translate all
   models to Xbox 360, Mac hosts reject pads. Viewer backends normalize
   sticks/buttons/triggers, model-specific motion/touchpad data are incomplete
   and rumble depends on backend/device support. Session tests use an
   in-memory bus. Display resize/restore belongs to W virtual outputs or
   opt-in L RandR, M scales capture. A viewer can fullscreen and move its
   window between local monitors, while host selection and mixed-DPI
   acceptance remain incomplete. Concurrent multi-display streams are
   absent.[^pads][^viewerpads][^fullscreen]

## Delivery, Tailscale and relay constraints

| OS / architecture | Shipped artifact and release runner | Verification scope |
|---|---|---|
| macOS arm64 | `latch-macos-arm64.tar.gz`, containing `Latch.app`, `macos-15` builds `aarch64-apple-darwin`. Compatibility `brolink-macos-arm64.tar.gz` also ships. | Workspace tests, independent FFmpeg encoder tests, bundle version/codesign and manifest checks. Hardware-dependent tests may skip. Intel macOS artifact **missing**.[^release] |
| Windows x64 | `latch-windows-x64.zip` + bare `latch-host.exe`, `windows-latest`. Compatibility `brolink-windows-x64.zip` also ships. | Workspace tests, setup/parser/protection/uninstall CI. Native ARM64 Windows artifact **missing**.[^release] |
| Linux x64 / arm64 | `latch-host-linux-x64.tar.gz` / `latch-host-linux-arm64.tar.gz` + individual `.sha256` files, Ubuntu 24.04 x64 / arm64. | Both native architectures build/test the unified executable. Tarballs include the native kit. glibc ≥2.39 baseline, GPU and compositor sessions unqualified.[^release] |

The publisher also assembles the versioned source archive, `LICENSE`, `NOTICE`,
`SHA256SUMS` and `release.json`, with provenance when configured. Workflow
configuration and publisher allowlist establish the intended asset set.[^release]

Core shells out to an installed Tailscale CLI. Windows checks both Program
Files paths, macOS checks the app CLI, `/usr/local/bin/tailscale` and
`/opt/homebrew/bin/tailscale`, all
platforms can use `tailscale` on PATH. Commands set `SHLVL=1` for the Mac app
CLI, hide Windows consoles and enforce timeouts. Status/whois/netcheck/peer
relay parsing and trust decisions have unit tests on the workspace runners.
Fresh installed CLI/authentication/UI flows across all pairs have no dedicated
CI job.[^tailscale]

Public-relay TLS and HTTPS updates require AES/PCLMULQDQ/BMI1/ADX/AVX/AVX2 on
x86-64, or NEON/AES/PMULL/SHA2 on ARM64. Missing features return errors before
the crypto provider is entered. Ordinary Tailscale UDP uses its existing
transport path. Relay tests exercise real local TLS, encrypted records,
audio/mic PCM sinks, input ordering, fallback and grant renewal on each
runner, unsupported-feature checks have unit coverage. Public relay field
coverage is synthetic W↔M. L↔W/M relay field behavior remains untested.[^relay][^update]

## In-flight and recently merged work

Issues #77–#81 track the work below. GitHub issue and PR state was checked on
15 November 2026. Packaging PR #93 merged during this update and closed issue
#81. The capability audit above remains scoped to `bf5da0d`, with packaging
status updated to current main. Verify branch heads and PR state at merge time.
In-flight markers describe proposed changes and confer no implemented support.

| Issue | Branch | Current PR and state | Cells affected |
|---|---|---|---|
| [#77](https://github.com/latch-remote/latch/issues/77) | `t3/send-input-on-os-events` | [#101](https://github.com/latch-remote/latch/pull/101), open | Viewer keyboard/mouse/scroll dispatch latency on W/M/L |
| [#78](https://github.com/latch-remote/latch/issues/78) | `t3/choose-shared-display` | [#103](https://github.com/latch-remote/latch/pull/103), open | Host selection and viewer chooser, DPI/capture/input geometry |
| [#79](https://github.com/latch-remote/latch/issues/79) | `t3/linux-vaapi-decode` | [#104](https://github.com/latch-remote/latch/pull/104), open | VA-API H.264/HEVC Main/Main10 and EGL DMA-BUF presentation implemented in the PR, CPU readback fallback and SDR output, Intel/AMD hardware verification pending |
| [#80](https://github.com/latch-remote/latch/issues/80) | `t3/wayland-capture-input` | [#102](https://github.com/latch-remote/latch/pull/102), open | Linux host capture/input/permissions/display/cursor |
| [#81](https://github.com/latch-remote/latch/issues/81), closed | `t3/add-package-manager-support` | [#93](https://github.com/latch-remote/latch/pull/93), merged | Windows Scoop, Linux AUR/Nix definitions, external publication pending |
| — | `t3/port-major-dependency-updates` | [#100](https://github.com/latch-remote/latch/pull/100), open | macOS API bindings, X11 bindings, dynamic GPU library loading and build compatibility |

## Verification boundaries and reusable harnesses

`cargo test --workspace --all-targets` compiles examples but does not execute
their `main` functions. Hardware probes may return an empty supported-codec
list, a green job therefore gives no guarantee of GPU exercise. Mac encoder
roundtrips use synthetic IOSurfaces with independent FFmpeg decoding. Windows
installed-decoder tests iterate only successful probes. Interactive GPU/HDR
and performance tests use `#[ignore]`. The Linux `native-display-selftest`
checks Xauthority-cookie replacement without starting Xvfb.[^ci][^mencode][^decode][^ldisplay]

Existing reusable session tests include `engine/tests/loopback.rs`
(pixels, input-to-picture, cursor protocol, fake gamepad rumble and scaled
output), `lifecycle.rs` (reservation/takeover/cleanup), `impaired.rs`,
`recovery.rs`, `steady_transport.rs`, `transport_scenarios.rs`, `bottleneck.rs`
and `relay_runtime.rs`. They exercise same-runner sessions or modeled paths,
there is no CI artifact replay matrix for six native OS pairs.[^sessiontests]

`engine/examples/serve` and `stream/examples/view` use the production HTTP
upgrade and receive/decode path with configurable synthetic patterns, inputs,
telemetry and audio probes. `engine/examples/mac_capture`, `windows_capture`,
`linux_capture`, `linux_audio` and `linux_input` exercise platform entry points,
`linux_input` requires isolated `DISPLAY=:9`. `client/examples/present` can
replay elementary streams through the real presenter, `stream_decode_bench`
uses native decode fixtures, and `stream/examples/pads` exercises controller
capture. These provide components for pair CI. Recording/replaying complete
control+media sessions and dry-running every native injector still need a
harness. Test-pattern mode bypasses native screen capture and real input
injection, its passing results cover that narrower scope.[^examples]

## Source and measurement notes

[^ci]: [.github/workflows/ci.yml:22](../.github/workflows/ci.yml#L22), [ci.yml:158](../.github/workflows/ci.yml#L158), [ci.yml:198](../.github/workflows/ci.yml#L198). Windows script execution includes VDD rollback, protection and uninstall restoration. Linux x64 deployment jobs start at [ci.yml:246](../.github/workflows/ci.yml#L246). Linux runner variables can override defaults.
[^field]: [Windows host record:1](measurements/2026-11-03/windows-host/README.md#L1), [Linux host record:1](measurements/2026-11-03-linux-host/README.md#L1), [viewer record:1](measurements/2026-11-03/viewer/README.md#L1), [HDR record:1](measurements/2026-11-03/hdr/README.md#L1), [Mac encoder record:1](measurements/2026-11-09-mac-encode/README.md#L1), [Linux output-size record:1](measurements/2026-11-09-linux-output-size/README.md#L1), [Latch 6 field report:1](measurements/2026-11-14-latch6-field/REPORT.md#L1). Field fixtures use example/test-mode hosts and bypass some installed-app/Tailscale authorization and native injection paths.
[^wcap]: [windows/capture.rs:467](../crates/engine/src/windows/capture.rs#L467), [windows/mod.rs:551](../crates/engine/src/windows/mod.rs#L551). [windows/capture.rs:1182](../crates/engine/src/windows/capture.rs#L1182) starts cursor-shader tests, interactive capture probes are hardware-dependent.
[^mcap]: [mac/mod.rs:26](../crates/engine/src/mac/mod.rs#L26), [mac/capture.rs:26](../crates/engine/src/mac/capture.rs#L26), [mac/capture.rs:143](../crates/engine/src/mac/capture.rs#L143), [mac/capture.rs:228](../crates/engine/src/mac/capture.rs#L228), [mac/capture.rs:551](../crates/engine/src/mac/capture.rs#L551). Capture fit/geometry tests start at [mac/capture.rs:506](../crates/engine/src/mac/capture.rs#L506).
[^lcap]: [linux/capture.rs:60](../crates/engine/src/linux/capture.rs#L60), [linux/mod.rs:64](../crates/engine/src/linux/mod.rs#L64), [linux/mod.rs:129](../crates/engine/src/linux/mod.rs#L129). X11 capture tests live in the capture module, bounded initialization tests in [linux/x11.rs:474](../crates/engine/src/linux/x11.rs#L474).
[^wdisplay]: [windows/display.rs:234](../crates/engine/src/windows/display.rs#L234), [windows/display.rs:299](../crates/engine/src/windows/display.rs#L299), [windows/display.rs:367](../crates/engine/src/windows/display.rs#L367), [windows/display.rs:398](../crates/engine/src/windows/display.rs#L398) tests. Existing VDD measured in the Windows host record.
[^vdd]: [host/virtual_display.rs:85](../crates/host/src/virtual_display.rs#L85), [host/virtual_display.rs:279](../crates/host/src/virtual_display.rs#L279) tests, [scripts/test-vdd-setup.ps1:1](../scripts/test-vdd-setup.ps1#L1). Driver rollback evidence in [windows-setup-vdd/README.md:1](measurements/2026-11-03/windows-setup-vdd/README.md#L1).
[^resize]: [engine/display.rs:18](../crates/engine/src/display.rs#L18), [engine/input.rs:18](../crates/engine/src/input.rs#L18) aspect-fit mapping and tests.
[^ldisplay]: [linux/display.rs:107](../crates/engine/src/linux/display.rs#L107), [linux/display.rs:292](../crates/engine/src/linux/display.rs#L292), [host/streamer.rs:26](../crates/host/src/streamer.rs#L26), [deploy/native/latch-display.service:1](../deploy/native/latch-display.service#L1), [deploy/native/selftest.sh:1](../deploy/native/selftest.sh#L1). Mode/rotation tests at [linux/display.rs:312](../crates/engine/src/linux/display.rs#L312).
[^wencode]: [windows/mod.rs:42](../crates/engine/src/windows/mod.rs#L42), [windows/mod.rs:316](../crates/engine/src/windows/mod.rs#L316), [windows/mod.rs:551](../crates/engine/src/windows/mod.rs#L551), [windows/mod.rs:658](../crates/engine/src/windows/mod.rs#L658), [windows/encode.rs:314](../crates/engine/src/windows/encode.rs#L314), [windows/amf.rs:305](../crates/engine/src/windows/amf.rs#L305). Software MF roundtrip at [windows/mod.rs:784](../crates/engine/src/windows/mod.rs#L784), AV1 framing tests in [engine/bitstream.rs:582](../crates/engine/src/bitstream.rs#L582). Runtime HEVC/AV1 failure ends or renegotiates the session, capabilities are probed per installation.
[^mencode]: [mac/encoder.rs:122](../crates/engine/src/mac/encoder.rs#L122), [mac/encoder.rs:293](../crates/engine/src/mac/encoder.rs#L293), [mac/encoder.rs:507](../crates/engine/src/mac/encoder.rs#L507), [mac/mod.rs:141](../crates/engine/src/mac/mod.rs#L141). Independent roundtrips at [mac/encoder/tests.rs:119](../crates/engine/src/mac/encoder/tests.rs#L119), software cases at [tests.rs:613](../crates/engine/src/mac/encoder/tests.rs#L613), Main10 at [tests.rs:912](../crates/engine/src/mac/encoder/tests.rs#L912). The forced software helper covers H.264 and HEVC when the platform opens them.
[^lencode]: [linux/hardware.rs:175](../crates/engine/src/linux/hardware.rs#L175), [hardware.rs:236](../crates/engine/src/linux/hardware.rs#L236), [linux/nvenc.rs:712](../crates/engine/src/linux/nvenc.rs#L712), [linux/vaapi.rs:710](../crates/engine/src/linux/vaapi.rs#L710), [linux/mod.rs:64](../crates/engine/src/linux/mod.rs#L64), [engine/software.rs:578](../crates/engine/src/software.rs#L578). Capability/framing tests at [hardware.rs:458](../crates/engine/src/linux/hardware.rs#L458), software/scaler/cursor tests follow the software implementation. VA-API implements H.264 and HEVC Main, NVENC includes AV1. All use 8-bit CPU pictures.
[^colour]: [proto/color.rs:65](../crates/proto/src/color.rs#L65), [color.rs:79](../crates/proto/src/color.rs#L79), [color.rs:261](../crates/proto/src/color.rs#L261) codec/colour matrix tests, [client/video.rs:1278](../crates/client/src/video.rs#L1278) viewer capabilities, [engine/server.rs:339](../crates/engine/src/server.rs#L339) negotiation and [server.rs:398](../crates/engine/src/server.rs#L398) pipeline output adoption. Linux software-wrapper output colour at [software.rs:578](../crates/engine/src/software.rs#L578).
[^wcursor]: [windows/capture.rs:365](../crates/engine/src/windows/capture.rs#L365), [windows/capture.rs:370](../crates/engine/src/windows/capture.rs#L370), [windows/mod.rs:512](../crates/engine/src/windows/mod.rs#L512). VideoProxy implements no cursor-plane overrides and uses [video.rs:93](../crates/engine/src/video.rs#L93) defaults. Windows software capture also supplies no separate cursor override.
[^lcursor]: [linux/capture.rs:396](../crates/engine/src/linux/capture.rs#L396), [linux/capture.rs:513](../crates/engine/src/linux/capture.rs#L513), [engine/software.rs:755](../crates/engine/src/software.rs#L755) cursor handling/scaling, [client/stream.rs:2263](../crates/client/src/stream.rs#L2263) local shape positioning. Shape/scaler unit tests and synthetic cursor negotiation do not qualify every desktop shape.
[^audio]: [engine/audio.rs:6](../crates/engine/src/audio.rs#L6), [windows/audio.rs:102](../crates/engine/src/windows/audio.rs#L102), [mac/mod.rs:344](../crates/engine/src/mac/mod.rs#L344), [mac/capture.rs:384](../crates/engine/src/mac/capture.rs#L384), [linux/audio.rs:99](../crates/engine/src/linux/audio.rs#L99), [linux/audio.rs:158](../crates/engine/src/linux/audio.rs#L158). Component tests in those modules and [codec/audio.rs:964](../crates/codec/src/audio.rs#L964). [Audio measurement:1](measurements/2026-11-03-audio/README.md#L1) uses synthetic clicks/virtual endpoint and simulated jitter/drift.
[^mic]: [engine/mic.rs:35](../crates/engine/src/mic.rs#L35), [host/service.rs:1495](../crates/host/src/service.rs#L1495), [linux/mic.rs:133](../crates/engine/src/linux/mic.rs#L133), [stream/mic.rs:178](../crates/stream/src/mic.rs#L178), [stream/mic.rs:303](../crates/stream/src/mic.rs#L303) synthetic capture test, [stream/audio.rs:486](../crates/stream/src/audio.rs#L486) mic sink, [relay_runtime.rs:475](../crates/engine/tests/relay_runtime.rs#L475) PCM sink test. Virtual cables are external installations.
[^keys]: [client/input.rs:27](../crates/client/src/input.rs#L27), [client/stream.rs:2025](../crates/client/src/stream.rs#L2025), [engine/input.rs:140](../crates/engine/src/input.rs#L140), [windows/input.rs:136](../crates/engine/src/windows/input.rs#L136), [mac/input.rs:157](../crates/engine/src/mac/input.rs#L157), [linux/input.rs:169](../crates/engine/src/linux/input.rs#L169), [linux/input.rs:113](../crates/engine/src/linux/input.rs#L113) text remapping. Input construction/key-map tests are in these modules, [linux_input.rs:187](../crates/engine/examples/linux_input.rs#L187) is a manual native XKB harness.
[^mouse]: [engine/input.rs:6](../crates/engine/src/input.rs#L6), [windows/input.rs:130](../crates/engine/src/windows/input.rs#L130), [mac/input.rs:165](../crates/engine/src/mac/input.rs#L165), [linux/input.rs:211](../crates/engine/src/linux/input.rs#L211), [client/stream.rs:1744](../crates/client/src/stream.rs#L1744). Windows construct-only raw/coordinate tests at [windows/input.rs:367](../crates/engine/src/windows/input.rs#L367), Mac tests build events without posting. Geometry tests cover aspect ratio, origins and scaling.
[^scroll]: [client/stream.rs:2248](../crates/client/src/stream.rs#L2248), [stream.rs:4287](../crates/client/src/stream.rs#L4287) tests, [windows/input.rs:213](../crates/engine/src/windows/input.rs#L213), [mac/input.rs:226](../crates/engine/src/mac/input.rs#L226), [linux/input.rs:162](../crates/engine/src/linux/input.rs#L162). Host unit tests check constructed events/remainders, physical scroll equivalence has no measurement record.
[^wireinput]: [proto/input.rs:32](../crates/proto/src/input.rs#L32) enumerates all event kinds. Protocol serialization/reliability unit tests cover those kinds.
[^pads]: [engine/gamepad.rs:34](../crates/engine/src/gamepad.rs#L34), [gamepad.rs:348](../crates/engine/src/gamepad.rs#L348) report/fake bus tests, [windows/vigem.rs:261](../crates/engine/src/windows/vigem.rs#L261), [linux/uinput.rs:99](../crates/engine/src/linux/uinput.rs#L99), [uinput.rs:162](../crates/engine/src/linux/uinput.rs#L162). [loopback.rs:442](../crates/engine/tests/loopback.rs#L442) uses test-mode virtual pads and feedback.
[^viewerpads]: [stream/gamepad/windows.rs:122](../crates/stream/src/gamepad/windows.rs#L122), [windows.rs:32](../crates/stream/src/gamepad/windows.rs#L32), [stream/gamepad/mac.rs:49](../crates/stream/src/gamepad/mac.rs#L49), [stream/gamepad/linux.rs:47](../crates/stream/src/gamepad/linux.rs#L47), [client/gamepads.rs:24](../crates/client/src/gamepads.rs#L24). Backend mapping tests run only on their target OS, fake-bus loopback does not enumerate physical controllers. Windows rumble map contains `Gamepad` objects, excluding raw-only Sony devices, as shown by [windows.rs:155](../crates/stream/src/gamepad/windows.rs#L155) and [windows.rs:33](../crates/stream/src/gamepad/windows.rs#L33).
[^clipboard]: [core/api.rs:47](../crates/core/src/api.rs#L47) bounds and [api.rs:72](../crates/core/src/api.rs#L72) `Clipboard::fit`, [host/clipboard.rs:13](../crates/host/src/clipboard.rs#L13), [clipboard.rs:180](../crates/host/src/clipboard.rs#L180), [clipboard.rs:234](../crates/host/src/clipboard.rs#L234) normalization/restart tests, [client/clipboard.rs:142](../crates/client/src/clipboard.rs#L142), [client/stream.rs:2123](../crates/client/src/stream.rs#L2123), [client/macos.rs:98](../crates/client/src/macos.rs#L98). Unit tests cover echo suppression, ordering and text bounds, with OS clipboards outside the recorder.
[^wake]: [core/wake.rs:79](../crates/core/src/wake.rs#L79) sender/packet tests, [host/wake.rs:46](../crates/host/src/wake.rs#L46) Windows-only target probe, [host/power.rs:153](../crates/host/src/power.rs#L153), [power.rs:203](../crates/host/src/power.rs#L203) policy/tests. [host/setup.rs:183](../crates/host/src/setup.rs#L183) generates Windows wake/power setup.
[^lifecycle]: [host/setup.rs:859](../crates/host/src/setup.rs#L859), [setup.rs:343](../crates/host/src/setup.rs#L343) elevated task, [host/unix_setup.rs:249](../crates/host/src/unix_setup.rs#L249), [unix_setup.rs:356](../crates/host/src/unix_setup.rs#L356). Tests cover generated registration and supervision decisions. [deploy/native/README.md:1](../deploy/native/README.md#L1) gives headless provisioning, CI uses its selftests.
[^permissions]: [host/setup.rs:84](../crates/host/src/setup.rs#L84), [host/unix_setup.rs:1238](../crates/host/src/unix_setup.rs#L1238), [mac/input.rs:50](../crates/engine/src/mac/input.rs#L50), [linux/uinput.rs:30](../crates/engine/src/linux/uinput.rs#L30), [linux/hardware.rs:175](../crates/engine/src/linux/hardware.rs#L175), [scripts/bundle-macos.sh:49](../scripts/bundle-macos.sh#L49). Tests inspect generated script/plist/permission text and bounded connection failures, interactive approval remains OS-managed.
[^release]: [.github/workflows/release.yml:29](../.github/workflows/release.yml#L29), [release.yml:96](../.github/workflows/release.yml#L96), [release.yml:156](../.github/workflows/release.yml#L156), [scripts/publish-release.py:24](../scripts/publish-release.py#L24), [scripts/bundle-macos.sh:36](../scripts/bundle-macos.sh#L36), [scripts/install-host.ps1:1](../scripts/install-host.ps1#L1), [scripts/install-macos-release.sh:1](../scripts/install-macos-release.sh#L1). Publisher Python tests and shell manifest tests check bounded assets/digests/version selection, they do not stream from an installed package.
[^update]: [client/update.rs:76](../crates/client/src/update.rs#L76), [update.rs:390](../crates/client/src/update.rs#L390), [update.rs:631](../crates/client/src/update.rs#L631), [host/update.rs:92](../crates/host/src/update.rs#L92), [core/update.rs:475](../crates/core/src/update.rs#L475). Unit tests in these modules cover malformed/digest/version rejection, platform offers, process handover/rollback and Mac install commands. Ad-hoc identity changes can require renewed macOS privacy grants.
[^uninstall]: [host/setup.rs:629](../crates/host/src/setup.rs#L629), [setup.rs:772](../crates/host/src/setup.rs#L772), [host/unix_setup.rs:499](../crates/host/src/unix_setup.rs#L499), [ci.yml:117](../.github/workflows/ci.yml#L117). Windows CI exercises actual power/firewall cleanup twice, Mac/Linux unit tests inspect generated actions.
[^relay]: [relay/crypto.rs:11](../crates/relay/src/crypto.rs#L11), [crypto.rs:221](../crates/relay/src/crypto.rs#L221) CPU rejection tests, [engine/tests/relay_runtime.rs:289](../crates/engine/tests/relay_runtime.rs#L289), [relay_runtime.rs:475](../crates/engine/tests/relay_runtime.rs#L475). TLS transport/record tests are in [relay/src/client.rs:983](../crates/relay/src/client.rs#L983) and [relay/src/wire.rs:375](../crates/relay/src/wire.rs#L375). Linux CI also runs [deploy/relay-tcp/selftest.py:1](../deploy/relay-tcp/selftest.py#L1).
[^binary]: [host/main.rs:120](../crates/host/src/main.rs#L120), [host/product.rs:34](../crates/host/src/product.rs#L34).
[^decode]: [stream/video.rs:459](../crates/stream/src/video.rs#L459), [video.rs:514](../crates/stream/src/video.rs#L514), [video.rs:562](../crates/stream/src/video.rs#L562), [video/software.rs:21](../crates/stream/src/video/software.rs#L21), [video/mediafoundation.rs:218](../crates/stream/src/video/mediafoundation.rs#L218), [mediafoundation.rs:1126](../crates/stream/src/video/mediafoundation.rs#L1126), [video/videotoolbox.rs:353](../crates/stream/src/video/videotoolbox.rs#L353), [videotoolbox.rs:916](../crates/stream/src/video/videotoolbox.rs#L916). Tests use committed H.264/HEVC/AV1/Main10 fixtures, conditionally on successful probes. Rust fallback accepts Constrained Baseline only. Windows negotiated Main10 uses `new_decoder_for_color`, the older format-only constructor has stricter platform checks.
[^present]: [client/lib.rs:23](../crates/client/src/lib.rs#L23), [stream/video.rs:356](../crates/stream/src/video.rs#L356), [client/video.rs:367](../crates/client/src/video.rs#L367), [video/edr.rs:117](../crates/client/src/video/edr.rs#L117), [video/hdr10.rs:86](../crates/client/src/video/hdr10.rs#L86), [video/presenter.rs:60](../crates/client/src/video/presenter.rs#L60). Shader numerical tests, import/crop logic and presenter fake-time tests accompany these implementations. W/M renderer-preparation and HDR readback records are in the field notes, compositor/scanout are outside their timing boundary.
[^viewerinput]: [client/stream.rs:1696](../crates/client/src/stream.rs#L1696), [stream.rs:1958](../crates/client/src/stream.rs#L1958), [stream.rs:2025](../crates/client/src/stream.rs#L2025), [stream.rs:2194](../crates/client/src/stream.rs#L2194), [client/input.rs:27](../crates/client/src/input.rs#L27), [input.rs:359](../crates/client/src/input.rs#L359), [client/macos.rs:67](../crates/client/src/macos.rs#L67), [stream.rs:456](../crates/client/src/stream.rs#L456). Recorder tests start at [stream.rs:4078](../crates/client/src/stream.rs#L4078), including modifiers, focus loss, IME and paste ordering. macOS menu override tests at [macos.rs:195](../crates/client/src/macos.rs#L195). OS-global key capture is absent.
[^playback]: [stream/audio.rs:131](../crates/stream/src/audio.rs#L131), [audio.rs:519](../crates/stream/src/audio.rs#L519), [stream/audio_wasapi.rs:68](../crates/stream/src/audio_wasapi.rs#L68), [stream/speakers.rs:12](../crates/stream/src/speakers.rs#L12), [speakers.rs:420](../crates/stream/src/speakers.rs#L420). Audio unit tests at [audio.rs:1125](../crates/stream/src/audio.rs#L1125) include long simulated jitter/drift and mixer tests, physical playback is outside those simulations.
[^fullscreen]: [client/app.rs:420](../crates/client/src/app.rs#L420), [client/refresh.rs:86](../crates/client/src/refresh.rs#L86), [client/video/presenter.rs:60](../crates/client/src/video/presenter.rs#L60), [engine/input.rs:6](../crates/engine/src/input.rs#L6), [engine/server.rs:293](../crates/engine/src/server.rs#L293) single-session takeover. Fullscreen uses the shared viewport code, native presenter tests simulate display changes.
[^negotiation]: [engine/server.rs:232](../crates/engine/src/server.rs#L232), [server.rs:339](../crates/engine/src/server.rs#L339), [stream/video.rs:430](../crates/stream/src/video.rs#L430), [stream/session.rs:1577](../crates/stream/src/session.rs#L1577). Server negotiation tests inject fake pipelines, colour matrix tests are [proto/color.rs:261](../crates/proto/src/color.rs#L261). These establish selection logic independently of each hardware encoder's output.
[^sessiontests]: [engine/tests/loopback.rs:243](../crates/engine/tests/loopback.rs#L243), [loopback.rs:327](../crates/engine/tests/loopback.rs#L327), [loopback.rs:442](../crates/engine/tests/loopback.rs#L442), [loopback.rs:499](../crates/engine/tests/loopback.rs#L499), [engine/tests/lifecycle.rs:36](../crates/engine/tests/lifecycle.rs#L36), [engine/tests/impaired.rs:73](../crates/engine/tests/impaired.rs#L73), [engine/tests/recovery.rs:397](../crates/engine/tests/recovery.rs#L397), [engine/tests/relay_runtime.rs:475](../crates/engine/tests/relay_runtime.rs#L475). [engine/testmode.rs:43](../crates/engine/src/testmode.rs#L43) selects synthetic capture and a fake input/pad bus.
[^examples]: [engine/examples/serve.rs:18](../crates/engine/examples/serve.rs#L18), [stream/examples/view.rs:114](../crates/stream/examples/view.rs#L114), [client/examples/present.rs:141](../crates/client/examples/present.rs#L141), [stream/examples/stream_decode_bench.rs:158](../crates/stream/examples/stream_decode_bench.rs#L158), [stream/examples/pads.rs:19](../crates/stream/examples/pads.rs#L19), [engine/examples/linux_input.rs:5](../crates/engine/examples/linux_input.rs#L5), [engine/examples/linux_capture.rs:5](../crates/engine/examples/linux_capture.rs#L5), [engine/examples/mac_capture.rs:4](../crates/engine/examples/mac_capture.rs#L4), [engine/examples/windows_capture.rs:3](../crates/engine/examples/windows_capture.rs#L3), [engine/testmode.rs:48](../crates/engine/src/testmode.rs#L48). Test-pattern host and example authorizers differ from the installed service's authentication and permissions.
[^tailscale]: [core/tailscale.rs:22](../crates/core/src/tailscale.rs#L22), [tailscale.rs:321](../crates/core/src/tailscale.rs#L321), [tailscale.rs:358](../crates/core/src/tailscale.rs#L358), [tailscale.rs:559](../crates/core/src/tailscale.rs#L559), [tailscale.rs:635](../crates/core/src/tailscale.rs#L635) parser/trust tests.
[^flight]: Issues #77–#81 track input, display selection, VA-API, Wayland and packaging work. The table maps each issue to its branch and current PR, including merged packaging PR #93. Verify branch heads and PR state at merge time.
