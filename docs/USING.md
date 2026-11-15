# Using Latch

What happens in a session, and what keeps a machine reachable when nobody
is sitting at it. Platform details are in [MACOS.md](MACOS.md) and
[WINDOWS.md](WINDOWS.md); Linux servers in
[deploy/native/README.md](../deploy/native/README.md) and
[deploy/node/NODE.md](../deploy/node/NODE.md).
The [platform support matrix](PLATFORMS.md) lists capability and verification
limits for every host/viewer direction.

## A session

1. Open Latch. **Machines** lists every machine on your Tailscale
   account, with a dot and a line for what it can do right now. The
   **Sharing** tab sets this machine up so others can Connect here.
2. Click **Connect**. If a Windows PC is asleep, Latch wakes it and waits.
   Connections connect directly using Tailscale authentication without
   pairing PINs.
3. The desktop appears, full screen by default, and nothing else: the
   mouse is captured the moment you click the picture, as in a game, so
   games that read raw mouse movement work. **Ctrl+Alt** (Control-Option
   on a Mac) is the host key, as in a hypervisor: it frees the mouse and
   drops a toolbar over the
   picture with the stream details, whether the path is direct or relayed,
   the mouse mode, a **Keys** menu for Ctrl+Alt+Del and friends and what the
   Command key does, **Stats** (frame rate, bitrate, round trip, loss and
   latency over the picture's corner; the host key with S), full screen,
   the PC's power menu, sound, **Stream settings** (Balanced, the default,
   asks for this screen's own size at its own refresh rate; Competitive for
   1080p at up to 360 fps; Cinematic for up to 4K at 120; the same on every
   path) and **Disconnect**. Click the picture, or press Ctrl+Alt
   again, and the toolbar goes away. The clipboard follows you both ways.
4. Leave the PC on if you want to connect from anywhere. Asleep, Tailscale
   is asleep too: this Mac can wake it only from that PC's own network
   (or if the router forwards UDP 9). Settings can offer sleep after a
   session; that is off by default.

## Keyboard between different operating systems

The stream toolbar's **Keys** menu offers **Send keys as pressed** and
**Map Command ↔ Ctrl**. Mapping is enabled by default.
**Send keys as pressed** is the explicit opt-out and preserves modifiers in every mouse mode, including
clipboard shortcuts. Alt, Option and AltGr keep their physical identities.

The following table describes hosts running this release with default
keyboard settings.
Free includes the **Free, for desktop** setting and a released mouse with
**Captured, for games** selected. Fullscreen has the same mapping as a
window in the corresponding mouse state. Left and right sides are preserved.

| Viewer | Host | Free or capture selected but released | Relative capture actually engaged | Send keys as pressed, all mouse modes |
| --- | --- | --- | --- | --- |
| Windows | Windows | Physical modifiers | Physical modifiers | Physical modifiers |
| Windows | macOS | Editing-chord swap | Command clipboard chords | Physical modifiers |
| Windows | Linux | Physical modifiers | Physical modifiers | Physical modifiers |
| macOS | Windows | Command → Ctrl, Control → Ctrl | Command → Ctrl, Control → Ctrl | Physical modifiers |
| macOS | macOS | Physical modifiers | Physical modifiers | Physical modifiers |
| macOS | Linux | Command → Ctrl, Control → Ctrl | Command → Ctrl, Control → Ctrl | Physical modifiers |
| Linux | Windows | Physical modifiers | Physical modifiers | Physical modifiers |
| Linux | macOS | Editing-chord swap | Command clipboard chords | Physical modifiers |
| Linux | Linux | Physical modifiers | Physical modifiers | Physical modifiers |

The editing-chord swap applies only while A, C, X, V, Z or F is held,
including Shift variants. Ctrl becomes Command and Win/Super becomes
Control during those chords. Other combinations preserve modifiers,
including Ctrl+Backspace, Q, H, M, Tab, Space and arrows. Alt/AltGr disables
this swap. Pressing a key outside the six editing keys and Shift/Ctrl/Win
restores the physical modifiers before that key reaches the host.

Copy, cut and paste use Command on a Mac host in every mouse mode.
In game capture, a Windows or Linux viewer's Ctrl+C, Ctrl+V and Ctrl+X
still reach a Mac host as Command+C, Command+V and Command+X. Other game
bindings keep their physical modifiers, so Ctrl+Z stays Ctrl+Z.
On a Mac viewer, Command maps to Ctrl toward Windows and Linux hosts in
every mouse mode. Control also stays Ctrl by default, preserving
Control+Tab and Control shortcuts in terminals.

The **Keys** menu also offers **Control key acts as Windows key** on a Mac
viewer connected to a Windows or Linux host. It is off by default. When
it is on and Command/Ctrl mapping is enabled, Control becomes Win on
Windows or Super on Linux while the mouse is free. During game capture,
Control returns to Ctrl. The option is greyed out with an explanation for
older hosts and while **Send keys as pressed** is selected.
A modifier held across a capture change is
re-targeted immediately. The replacement Ctrl press precedes a Win release
to avoid turning the change into a Windows-key tap. Selecting the game
mouse setting alone keeps the free-mouse mapping until capture engages.

Hosts running Latch 6.0.0 or older keep the previous keyboard behaviour in
every mouse mode. Mac Command becomes Ctrl toward a Windows or Linux host,
Mac Control stays Ctrl, and PC modifiers retain their original identities.
Copy, cut and paste use Command toward Mac hosts. The new Control-key option
and editing-chord swaps require hosts running this release. Win, Alt+Tab
and Super stay local against hosts running Latch 6.0.0 or older, even in
fullscreen or game capture. **Send keys as pressed** also applies to these
older hosts.

Keys use physical US positions, interpreted by the **host's** layout.
UK/German/French ISO extra keys, separate numpad keys and Japanese/Korean
language keys retain positions where the host has an equivalent. Mac
viewers recover native ISO/JIS codes omitted by winit and account for
Apple's ISO section/backquote swap. German ISO hardware has yet to be
verified on either end. Ordinary printable keys and AltGr combinations
use the host layout. Configure that layout on the host for the intended
output.

The stream window uses the host's IME for ordinary native typing. Winit
currently disables viewer IME composition in this window. Windows Process
keys stay with the input method. The separate UTF-8 path carries dead-key
results, pasted text and any IME commit supplied by an embedding application,
with modifiers temporarily released and restored. Native Korean/Japanese
composition still requires the appropriate host input source.

Caps Lock toggles on each viewer change, with repeats ignored. Lock state
starts on the host. Num Lock toggles on Windows/X11 and becomes keypad
Clear on macOS. A Mac viewer's Clear stays local, avoiding an unintended
Num Lock toggle on a PC. macOS approximates Korean language and Japanese
Convert/NonConvert positions using Kana/Eisu. Semantic conversion remains
input-source dependent. F21–F24 and media Stop have no corresponding event
in the Mac injector. Volume, mute, play/pause and next/previous injection
use system-defined Mac media events, while viewers leave media controls
local. During an X11 grab, Latch releases the grab and replays a media
press locally through the existing pure-Rust XTest protocol implementation,
then waits for that key's release before recapturing. Replaying a media key
releases remote held input. X11 capture requires Xlib and server-side XTest support. Without them,
focused keys continue through winit.

Viewers and hosts running this release preserve distinct keypad Enter/Equals
and additional Japanese positions. Hosts running Latch 6.0.0 or older keep
the previous text handling, modifier identities and keypad limitations.
Viewers running Latch 6.0.0 or older keep their original capture limits
against a newer host. A stale Windows
input helper negotiates its own v1 pipe protocol and receives legacy VK
forms, including Return for keypad Enter. Holding both Return and keypad
Enter can produce an extra Return repeat with that helper.

Focus loss, opening a local control, disconnect and stream teardown release
remote held keys. Focus-loss release is queued directly from the native
callback, including while a clipboard paste is waiting.

### System shortcuts

| Viewer | Captured in a focused fullscreen or captured-mouse stream | Viewer-owned combinations and limits |
| --- | --- | --- |
| Windows | Sided Win keys, Alt+Tab/F4/Space through a dedicated capture-thread low-level hook | Ctrl+Alt+Delete is secure attention. Win+L is expected to remain reserved by Windows, which Latch does not test. Hook installation failures are logged. |
| macOS | Command, Option, Control and Shift | Command+Q/H and Option+Command+H use the app-menu override whenever a focused connected stream owns the keyboard, including a windowed free mouse. Command+Tab/Space are owned by macOS. |
| Linux X11 | A capture-scoped keyboard grab keeps sided Super and Alt+Tab in the stream | Another client can prevent the grab. Latch logs once and retries on later frames. A watchdog releases a grab after 250 ms without an event-loop heartbeat, checked every 10 ms. |
| Linux Wayland | Compositor-delivered focused keys and sided Super | The compositor owns global shortcuts. winit 0.30 exposes no shortcut-inhibit protocol API, so Latch cannot request inhibition in this stack. |

Use **Keys** in the stream toolbar to send the host-specific Alt+Tab,
Command+Tab/Space, Win+L, Windows key or Ctrl+Alt+Delete actions. These actions use host key identities
and bypass the modifier-mapping setting. The toolbar is available after
Ctrl+Alt releases the mouse. Windows secure attention cannot be synthesized
by SendInput or the elevated helper. It requires local trusted input or
another OS-supported administration mechanism.

The Windows hook has its own message loop and a 256-event bounded queue.
Its callback performs fixed-size state updates and nonblocking queue sends.
All sided modifiers share its ordering before Win. Ordinary keys form
queue barriers until the window has translated them, preserving their position
between modifiers. A barrier without a matching window event is dropped
after 8 ms only when the UI keyboard queue is idle. Queued keys retain
their ordering through a UI stall, and lost-key hints release no held input. AltGr's synthetic Control
is filtered using the active layout and the adjacent right-Alt event, with
winit confirming an otherwise lone real Control. Focus loss, capture end,
window destruction, app teardown and worker unwinding uninstall the hook.
The message loop blocks in Windows' message retrieval API so hook callbacks
run as input arrives. A timer checks a 250 ms UI heartbeat lease. While
capture is active, Latch schedules a UI repaint every 83 ms independently
of video arrival. A video freeze therefore preserves held keys and mouse
buttons. A dead UI, full or disconnected queue, or lost foreground ownership
ends capture. Lost capture releases remote held input when the UI resumes.
Queue publication wakes the worker, which posts a Windows paint message
to the viewer window. The hook thread takes no egui context lock.

The X11 watchdog uses winit's thread-enabled Xlib connection because X11
allows only the grabbing client to ungrab its keyboard. Teardown joins the
helper before eframe closes the display. It can release a stalled UI's grab
while the X server and Xlib connection remain responsive. An Xlib display
lock held by a stuck native call triggers a separate server probe after
two seconds. Latch logs and aborts only if the server answers while the
lock remains stuck, letting the server release the grab. A slow or
unresponsive server keeps recovery waiting until it answers. The watchdog leaves the display alone
while nothing is grabbed. Process-wide suspension can prevent timely recovery.
Windows process-wide suspension or a stalled hook thread can still encounter
Windows' `LowLevelHooksTimeout`. These recovery mechanisms have constructed
state-machine coverage. The Linux CI Xvfb step requires exactly one passing
isolated test for local media delivery, real heartbeat renewal, timed grab
release, slow-server recovery and stuck-display recovery. Real-hardware
verification remains open for remappers such as AutoHotkey and PowerToys,
global hotkeys, media-key daemons, non-US layouts, IMEs and Windows hook
latency and timeout recovery. The Win↓ … Ctrl↓ Win↑ capture-click retarget
against Windows awaits hardware verification, with Start expected to stay closed.


## Choosing a display

A machine with more than one display shares the one it picks: a Windows
PC its virtual display (or else its main monitor), a Mac its main display
(or the one named in `LATCH_MAC_DISPLAY`), a Linux machine its whole X
screen, every monitor together. To share another, open the machine's
more menu (**⋯**) in **Machines** and pick it under **Display to share** before
you connect, or pick it in the stream toolbar's PC (or Mac, or Machine)
menu while you watch; the stream starts again on it in a second or two.
**Automatic** (**All displays** on Linux) goes back to the machine's own
choice.

The choice is remembered for that machine in `client.toml`:

```toml
# client.toml
[displays]
"<Tailscale node id>" = "<display id>"
```

If the display is no longer there (a monitor unplugged, a Windows display
renumbered after a restart), the machine shares its own choice again until
you pick another. The menu appears only when the machine lists two or more
displays; a machine running an older Latch lists none and has no choice to
offer.

Choosing a monitor other than a Windows PC's virtual display shares it as
it is: the virtual display is not resized and the other monitors stay on.
On Linux one monitor of several is shared at its own size, and the
desktop is not resized for the viewer. Clicks and the pointer land on the
chosen display.

## Linux hardware decoding

The viewer tries VA-API H.264 and HEVC decoders using the installed system
libva and GPU driver. Give your user access to `/dev/dri/renderD*`, usually
through the distribution's `render` group. Stream stats show `VA-API H.264`
or `VA-API HEVC` when hardware decoding is active. A machine without a usable
driver uses software H.264 automatically.

Set `LATCH_LINUX_DECODER=software` to force software decoding, or
`LATCH_VAAPI_DEVICE=/dev/dri/renderD129` to try a particular GPU first.
Driver-reported resolution limits filter codec offers. Hardware failures
request a keyframe or restart the stream with another codec until software
H.264 is available. A stalled driver disables VA-API until the app restarts.

EGL/GLES renderers can import decoded DMA-BUF planes directly. Other
renderers use hardware decode with CPU readback and texture upload. Ten-bit
SDR requires a Main10 decoder and compatible renderer. Native HDR display
output and AV1 decoding on Linux require further work.

## When the connection drops

If a stream loses its connection, leave the window open. Latch tries again
after 2, 5 and 10 seconds, then every 20 to 30 seconds until you stop it,
and at once when the machine shows as online again or this one wakes from
sleep. The overlay shows the countdown; **Reconnect now** skips the wait
and **Stop** goes back to the machine list. Each attempt is a new session,
so a display or network that changed in the meantime is taken into account.

Latch does not keep retrying when the other side said no: a refusal,
another viewer taking the machine over, or the stream being ended there.
**Try again** starts over when you are ready.

## Stream relay

Latch first tries UDP over Tailscale. If both machines support the version
6 relay, missing video for about a second or sustained damaged video for
about two seconds can start a relay attempt. Busy video held below 60% of
the requested rate for five seconds can also trigger recovery, even when
error correction repairs the lost packets. Quiet desktops do not trigger
this bandwidth check. This can help when UDP media
is unusable and the authorized Tailscale control connection still works.
The toolbar shows **Latch TLS relay** when that route is active.

Open **Settings → Stream relay** to change **Recover automatically through
the Latch relay** or **Relay hostname** and its port. The default is
`relay.latch.rune.foo`, port `443`, with recovery enabled. The dedicated
AWS Lightsail relay has public TLS, four concurrent session slots, and a
monthly transfer budget. If it is unavailable or full, Latch keeps trying
the original path within its recovery limits.

Turning off recovery opts this viewer out. For a sharing machine, edit the
existing `host.toml` in Latch's settings folder and restart its background
service to apply the policy. A custom endpoint must match on both machines.
The equivalent viewer configuration is in `client.toml`.

```toml
# host.toml
[relay]
enabled = false
host = "relay.latch.rune.foo"
port = 443
```

```toml
# client.toml
[stream.relay]
enabled = false
host = "relay.latch.rune.foo"
port = 443
```

Edit existing tables in place to avoid duplicate TOML sections. Restart
Latch after editing its files. Host endpoint and policy changes currently
take effect only when the background service starts.

The relay carries complete video frames on one TLS lane, and sound,
microphone, keyboard, mouse, gamepads and small control messages on another.
Fresh session keys and attachment tickets are exchanged through authorized
Tailscale control. Media and input remain end-to-end encrypted. The relay
operator sees public IP addresses, connection timing and traffic volume.

Relay use has finite capacity. The daemon's pilot defaults allow four
sessions globally, two per creator IP, a 60-second attachment window,
15-minute sessions and 512 MiB per session. The CLI's default monthly
ingress-plus-egress reservation budget is 10 GiB. Operators can configure
different bounded limits. These are implementation defaults, with deployed
limits set by the operator. Quota exhaustion or admission rejection prevents
a new relay route. The monthly ledger survives daemon restarts.

Relay setup and frame stalls have deadlines and retries have cooldowns.
A failed relay attempt leaves or returns routing to UDP. If that path
still cannot deliver usable video, the media watchdog ends the session and
the reconnect overlay appears. After a minute on relay, a bounded UDP load
probe can return the stream to a healthy direct path. This probe currently
covers requests up to 40 Mbps. At the relay's time or traffic limit, Latch
returns briefly to UDP and starts a fresh relay connection. On a network
where UDP is blocked, video can pause for several seconds during renewal.
Priority queue exhaustion ends the
session to release held keys and buttons. Local acceptance of a packet
into a queue does not guarantee remote delivery.

The network must allow native TLS with ALPN `latch-relay/1`. HTTP-only
proxies, TLS inspection and captive portals may prevent it. This path
requires AES, PCLMULQDQ, BMI1, ADX, AVX and AVX2 on x86-64, or NEON, AES,
PMULL and SHA2 on AArch64. Unsupported CPUs disable relay support. Codec,
GPU, capture and display limits still determine picture size and speed.
Synthetic campus sessions exercise this path; see the
[performance notes](PERFORMANCE.md#latch-6-network-recovery) for their limits.
Large-concurrency qualification remains outstanding.
[Protocol and security details](PROTOCOL.md#public-tcptls-relay)

## Updates

Every few hours Latch fetches the public `release.json` manifest. Settings has the
switch and a **Check now** button.

- **macOS.** A newer app is downloaded, checked against the digest GitHub
  publishes and its own code signature, swapped into `/Applications` once
  no stream is running, and relaunched. The Mac also sends a newer
  `latch-host.exe` to each of your PCs whose host reports an older version,
  over the same Tailscale-authenticated control API that can put the PC to
  sleep. A PC that is asleep gets it the next time the Mac sees it.
- **Windows.** With the window open, Latch downloads a newer
  `latch-host.exe` itself and installs it when no stream is running.
  Whether an update is pulled or pushed from a Mac, the PC fetches the
  version-specific public manifest and installs the file only if its exact
  version, URL, size and SHA-256 match. A PC that cannot reach GitHub refuses
  the update.
- **Linux.** A release binary owned by your user in a writable directory
  downloads the matching x64 or arm64 tarball when the window is open. It
  waits for viewing and hosting streams to end, then asks the local service
  to install it. Reopen the window after the service restarts. A headless
  user-owned install can request the same update with `latch-host --self-update`
  while its local service is running. Settings' automatic-update switch
  controls window checks. The CLI is an explicit update request and refuses
  installation during a hosted stream. Run it again after disconnecting.

Version 6 downloads and update checks need no GitHub login. Legacy
`github_token` and `LATCH_GITHUB_TOKEN` settings are ignored by this public
distribution path. Development source requires repository access, or you
can build the corresponding source snapshot attached to each public release.
Hosts installed before 3.1 need a local or
SSH installation of a current release: a Latch 5 viewer cannot stream the
retired protocol to upgrade them. Mac builds are ad-hoc signed, so macOS may
ask for Screen Recording and Accessibility again after an update
([why](ROADMAP.md#signing-and-notarization)).

### Linux installation ownership and recovery

The updater resolves installation symlinks and replaces their target. It
leaves the symlink in place. These installations show a notice with the
appropriate update instructions.

- Pacman-owned files, including AUR packages, and paths under `/usr/bin` or
  `/usr/lib` use the package manager. Pacman ownership is queried with `pacman -Qo`.
- Resolved paths under `/nix/store` use the Nix package or flake rebuild.
- Docker/Podman markers (`/.dockerenv`, `/run/.containerenv`), `container` or
  `LATCH_CONTAINER_INSTALL` environment markers, and OverlayFS, squashfs or
  EROFS filesystems use an image rebuild and container recreation. The
  headless container kit sets `LATCH_CONTAINER_INSTALL=1`.
- Read-only mounts use the installation's deployment tool.
- Root-owned installs, files or parent directories owned by another user,
  unwritable files/directories, group/world-writable install directories or
  files, privileged executables and unsafe ancestor directories require the
  installation owner to replace the binary. Ancestors must belong to root
  or the user and prevent other users from renaming entries, allowing sticky
  directories such as `/tmp`. Running as root also selects this notice.

For a user-owned release, the service authenticates the archive using its
own version-specific public manifest and the shared version, size and SHA-256
checks. Its separate supervisor fetches that manifest independently, copies the
private upload into a `0700` sibling directory and re-hashes it against the
manifest. The archive is downloaded once. Linux self-update archive and manifest
transfers have a size-derived deadline of expected bytes / 32 KiB/s + 60 seconds,
a 30-second socket timeout, and a 32 KiB/s minimum average rate after 30 seconds.
Manifests use a 1 MiB size allowance. The underlying TLS transport additionally
checks a 128 B/s floor. Other downloads retain their existing transport behavior. Only the selected binary member is
written, initially `0600`. After digest and ELF architecture verification,
it becomes `0755` and must report the exact release version with `--version`.
The updater holds an exclusive file lock, hard-links the old executable for
recovery and atomically renames the new executable over the install path.

The supervisor starts in its own transient systemd user unit, without a
`systemd-run --wait` client inside `latch.service`. The service waits for a
fsynced supervisor-start acknowledgement before accepting the request. The
unit has a 240-second runtime limit. Linux 5.3+ (`pidfd_open`) is checked before
offering or downloading an update. Systemd-supervised services also require a
working user manager. An unsupervised service uses a separate supervisor
process and a pinned process handle for its handover, preserving the service's
session environment, without requiring a systemd user manager.

The new service must report the expected version continuously for 15 seconds
within 60 seconds. Its systemd MainPID, executable inode and ownership of the
listening control socket are checked on both sides of each health request.
A restart or health failure restores the previous binary and restarts it.
The fsynced `linux-update.json` journal records progress, inode identities,
staging location and outcome. On startup, a service detects an unfinished
update whose supervisor died and restores the previous inode deterministically.
A running service also monitors for this failure. The fixture exercises both
startup recovery and recovery while the service keeps running. If restoration
itself fails, the update becomes terminal Failed with the recovery copy retained
and its path reported. The next update can proceed. A journal flock and phase
compare-and-set serialize writers. Unsafe or unparsable journals are moved aside
with at most three diagnostic copies. Unknown fields and missing fields are
tolerated, and unknown phases become terminal failures. Maintenance failures
appear in service setup/status and logs while the service continues starting.

The window reads the local service version before downloading. Supervisor
installation and recovery failures persist with their reason and suppress only
the exact failed version. A newer release remains eligible. Transport failures
retain the 15-minute retry back-off without persistent release suppression. **Check now** or `latch-host --self-update`
explicitly retries it. The window, service status and CLI report the outcome.
Reopen the window after a successful update. Locks live in the data directory.
At startup, cleanup checks directory names, ownership, private permissions,
installation markers and age before removing abandoned staging directories
and private upload directories older than a day that no live journal references.
Legacy private upload files use their filename, ownership, permissions and age.
Recovery copies remain until the journal proves they can be discarded. Old
empty updater lock files beside the binary are removed only when owned,
private, at least one day old and unlocked.

Streams accepted during the health window can be cut off by a rollback, as on
Windows. This remains a follow-up. Toolbox/distrobox installs remain refused
through their container markers. Snap uses `snap refresh`, the native kit
requires rerunning its installer, and AppImages are refused even with
`--appimage-extract-and-run` when `APPIMAGE` is set and the executable lives
under `APPDIR`. Snap environment detection likewise requires the executable
under `SNAP`, so a native install launched from a packaged terminal remains eligible.

The authenticated `/v1/update` endpoint accepts a Linux host's matching
release tarball with the existing version and digest headers. Automatic
cross-machine Linux pushes await an architecture capability in discovery.
Current hosts advertise their OS, so automatic Mac pushes continue to select
Windows executables. No streaming protocol or manifest format changes are
required for local Linux updates.

Linux updater validation uses local HTTP manifest/artifact fixtures and
compiled ELF programs on native x64 and arm64 CI. Local development uses
Docker with Ubuntu 24.04/glibc 2.39. No Linux machine outside CI and containers is available for
this work. The `Linux updater systemd` CI jobs pass on x64 and arm64 Ubuntu 24.04 runners. They install a fixture `latch.service`, launch the real supervisor from its control group, and check successful probation, wrong-version rollback and rollback after the supervisor is killed following the swap.
They also exercise a replacement that crashes on start, including the start
limit and `reset-failed`, and monitor recovery while the service keeps running.
The test is ignored during ordinary test runs and requires explicit opt-in
through the dedicated script, which refuses an existing `latch.service`.
The fixture bypasses supervisor argv parsing, self-pinning, installation ownership
checks and real GitHub manifest fetching. The real `latch-host` binary under
systemd is untested. Real GUI reopening and stream continuity across an update
remain untested because no Linux desktop machine was available.

### Moving from 5.x to 6.0.0

1. Download the matching archive and `SHA256SUMS` from the public releases
   page and compare the archive's SHA-256 with its listed value.
2. On Windows, extract the zip and run `install-host.ps1` from that folder.
   Open **Sharing** and run **Set up as administrator** if setup is needed.
   On macOS, quit Latch and replace `/Applications/Latch.app` using the
   [Mac installation steps](MACOS.md#install-a-prebuilt-app).
   On Linux, replace the binary and restart the service using the
   [native guide](../deploy/native/README.md#download-and-verify).
3. Keep your existing settings folder. Open Latch and check the version
   in **Settings**. Use **Check now** for subsequent release checks.
4. Update both ends to use the new relay path. Older protocol 1 peers keep
   UDP streaming and do not negotiate this extension.

Some compiled 5.x versions check the former private release endpoint, so
they may never offer 6.0.0 automatically. They can also reject an update
pushed by a new Mac because their independent verification still uses the
old endpoint. A one-time installer or local/SSH handover gets those machines
onto the public manifest updater.
[Public releases](https://github.com/latch-remote/latch-releases/releases/latest)

## Staying reachable

A PC nobody can get to in person stays reachable when three things hold.
Latch starts when someone signs in to Windows, by default, and turns this
back on at every start unless the owner switches it off on the Sharing tab.
It runs (and streams) in the signed-in session, so after a restart the PC
answers once someone has signed in: a PC nobody sits at needs Windows'
automatic sign-in. And the PC's Tailscale node key must not expire:
Tailscale keys expire after 180 days unless key expiry is disabled for
that machine in the [admin console](https://login.tailscale.com/admin/machines),
and an expired key needs a sign-in at the PC. Latch shows the expiry of
every machine it lists, this Mac included, until expiry is disabled. The
PC's addresses are saved on the Mac, so PCs stay listed even while
Tailscale on the Mac is off.

## Waking the PC

A wake packet has to reach the PC's network card on the PC's own network.
Tailscale cannot deliver it, because the sleeping PC's Tailscale is asleep
too. Latch sends the packet to the LAN broadcast, to the PC's LAN address
and to the PC's public address.

| Where the Mac is | Asleep | Shut down |
|------------------|--------|-----------|
| Same network as the PC | works | works if the board's firmware allows wake from power off |
| Elsewhere | needs the PC's router to forward UDP 9 to the PC, or a Tailscale subnet router on that network | same, and the firmware condition |

**Test waking it from this network** in a machine's **…** menu settles it for the network you are
on: it sends the packet while the PC is awake and asks Latch there whether
it arrived. Setup on the PC takes care of Windows' side (Fast Startup off,
the adapter's wake keywords, wake allowed in power management). Wired
Ethernet is strongly preferred; most Wi-Fi adapters cannot wake a PC.

## Upgrading from Latch 4 or BroLink

Install the current release locally or over SSH and keep your settings.
Version 5 introduced protocol 1, shared by versions 5 and 6. Versions 4
and earlier used the retired GameStream protocol and require an upgrade
before streaming with current peers. An update pushed to an older Windows
host can still fail verification against its compiled release endpoint.
Setup removes the old engine, its pairing data and its services.

- **Mac:** a BroLink app moves to `Latch.app` the first time it runs, and
  its login item moves with it.
- **Windows:** the host updates in place and keeps working. Run
  **Set up as administrator** once on the **Sharing** tab: it removes the
  `LatchStream` service, the `LatchEngineUser` task and
  `%ProgramFiles%\Latch\engine`. `install-host.ps1` from the new zip also
  moves a BroLink install to `%LOCALAPPDATA%\Latch`.
- **Linux:** the first start registers `latch.service` and retires the old
  service. For a native desktop, see [deploy/native/README.md](../deploy/native/README.md).

Use `LATCH_DATA_DIR` to override the settings directory. Version 6 public
updates ignore the legacy GitHub token settings.

Windows IME injection uses the documented `VK_HANGUL`/`VK_HANJA`, Kana,
Convert and NonConvert virtual keys with their scan codes and without
`KEYEVENTF_SCANCODE`, which would ignore the virtual key. Korean Windows
IME acceptance has yet to be verified on hardware. Microsoft's definitions
identify Hangul and Hanja as IME keys and document the injection flag behavior.
[Virtual-key codes](https://learn.microsoft.com/en-us/windows/win32/inputdev/virtual-key-codes),
[KEYBDINPUT](https://learn.microsoft.com/en-us/windows/win32/api/winuser/ns-winuser-keybdinput).
