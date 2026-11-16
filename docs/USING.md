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
The toolbar shows **Latch relay · TLS** or **Latch relay · WebSocket** when that route is active,
with **via proxy** when a CONNECT tunnel carries it.

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

The toolbar names the current media transport. “Tailscale UDP” uses Tailscale's
reported path, shown as Direct, Via your relay, or Relayed via a DERP city.
“Latch relay” uses the public relay for video, sound and input. Open
Stream performance for its Tailscale route or relay carrier, the reason, and how long that media path has been in
use. A brief notice appears when the path changes, for example “UDP is losing
17% of packets”, “UDP is blocked on this network”, or “UDP recovered”.
The persistent network diagnosis replaces a notice for the same sign-in,
TLS-interception or proxy condition. An unrelated DERP diagnosis keeps the
UDP/relay transition notice. Path changes keep the session and held input alive.

With media path policy negotiated, Latch prepares the relay after 350 ms
without UDP. It lets congestion control adapt to a shared uplink limit.
Persistent loss after reducing the actual UDP rate or queue-free policing
can select the relay. Complete-frame stall and post-relay recovery timers
allow for round-trip latency. A healthy distant path keeps UDP.

After at least five seconds on relay, Latch tests UDP at a rate based on what
the relay is delivering. The paced check shares the media budget, stops on
loss or relay pressure, and uses under 2.9% duty. Further tests wait thirty
and then sixty seconds after successful handoffs. Failed or cancelled checks
wait sixty seconds. Three measured returns per rolling minute bound
flapping. A return begins at the proven rate and adapts upward.

For size-dependent UDP black holes, video and input use a measured smaller
size. Audio shortens packets only when that measurement requires it. Stream
performance reports the selected size. Route notices are coalesced, with
initial classification, size changes, failed setup and grant renewal silent.

A silently stalled relay is detected from video ACK progress. Before the
first video ACK, Latch uses the connection setup timing and a floor of at
least 750 ms. Later ACKs set the estimate with a 350 ms floor. Both deadlines
also allow for the pending frame's size at the current relay send rate.
The complete first-ACK deadline is capped at three seconds.
Fresh authorization or usable UDP restores media. Active failures have a 100 ms retry cooldown, recovery
allows a UDP keyframe round trip, and three consecutive failures wait thirty
seconds. Setup failures wait two seconds. A grant reaching its time or
traffic allowance renews authorization. A session starts at most sixty
relay grant attempts per rolling hour. If neither
path can deliver usable video within the session's eight-second watchdog,
the reconnect overlay appears. Local queue acceptance alone cannot establish
remote delivery.

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
Manifests use a 1 MiB size allowance. The 32 KiB/s floor also applies to
manifests, so a stalled small manifest normally fails the first rate check after
30 seconds. A blocked read can add the 30-second socket timeout. The size-derived
92-second manifest deadline is an upper bound. Redirect hops share cumulative
byte counts and elapsed time for both rate floors and the overall deadline. The underlying TLS transport additionally
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
A restart or health failure restores the previous binary and restarts it. A
failed systemd unit with no MainPID and no queued job triggers rollback after
one second of stable failure, including start-limit-hit. Activating units and
automatic restart cycles continue through the health window. A systemctl error
during the failure check is logged at debug level while probation continues.
The fsynced `linux-update.json` journal records progress, inode identities,
staging location and outcome. On startup, a service detects an unfinished
update whose supervisor died and restores the previous inode deterministically.
A running service also monitors for this failure. Rollback attempts to fsync the
Restoring journal entry and its directory before moving the old binary back.
A journal-write error is logged while the old binary is restored and its service
restarted. A consumed previous link identifies the failed release when the
journal still says Swapping. Missing pre-swap proof is classified conservatively
as a recovery failure. Recovery retains its staging evidence when its terminal
outcome cannot be persisted. A service already running the original inode
continues serving during a persistent journal-write failure. Recovery handles
Restoring with either executable inode and queues a service restart for
interrupted swap/restore states. The original inode and its verified previous
link prove a pre-rename interruption is eligible for retry. CI covers recovery
both at the next service start and while the service keeps running. If
restoration itself fails, the update becomes terminal Failed with the recovery
copy retained and its path reported. The next update can proceed. A journal
flock and phase compare-and-set serialize writers. Unsafe or unparsable journals
are moved aside with at most three diagnostic copies, pruning the oldest
modification times.
Owned regular lock files restored with incorrect modes are repaired to `0600`
using pinned directory and file descriptors. Journal and transaction lock
directories are created explicitly as `0700` under either umask `002` or `022`.
Existing user-owned lock directories are repaired to `0700` through their open
descriptors after ownership and type checks. The journal lock is
`linux-update.json.lock` directly in the Latch data directory. Every journal
access makes that data directory `0700`, including an existing directory named
by `LATCH_DATA_DIR`. Missing parent directories are created as `0700`. This path
is private application storage. Using the existing lock path gives installations
made by 6.0.0 and main a shared lock inode across upgrades. Every resolved
ancestor must belong to root or the user. Group write on user-owned ancestors follows the owner's
group-access policy, including Ubuntu private-user groups. Group members with
that access are trusted for those ancestors. Root-owned ancestors require group
write to be disabled or the sticky bit to be set. Every world-writable ancestor
requires the sticky bit. The rule uses descriptor UID and mode checks. Lock
symlinks, foreign owners, hard links and unsafe ancestors are refused with
repair instructions. A journal read I/O error retains the journal for a later
retry. Unknown fields and missing fields are
tolerated, and unknown phases become terminal failures. Maintenance failures
appear in service setup/status and logs while the service continues starting.

The window reads the local service version before downloading. Supervisor
installation and post-swap recovery failures persist with their reason and suppress
only the exact failed version. A supervisor that dies before swapping keeps the
release eligible for bounded automatic retry. A newer release remains eligible.
Archive and installation retries use persisted per-release exponential delays
of 15 minutes, 30 minutes, 1 hour, 2 hours, 4 hours, 8 hours, 16 hours and then
a 24-hour ceiling. Successful
installation, a newly offered release and **Check now** reset the delay. Manifest
failures have their own counter and never advance archive or installation delays.
A verified download resets archive failures while installation retries retain
their own counter. The first successful manifest check after migrating older
shared retry state clears that legacy delay once and persists the reset
immediately. The window rechecks manifests at least every six hours while
archive retries wait.
Verified cached archives are rechecked and reused. Transient errors keep the
release eligible. **Check now** or `latch-host --self-update`
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
this work. The `Linux updater systemd` CI jobs passed at `18a36e9` on x64 and arm64 Ubuntu 24.04 runners. They install a fixture `latch.service`, launch the real supervisor from its control group, and check successful probation, wrong-version rollback and rollback after the supervisor is killed following the swap.
The fixture adds a replacement that crashes on start, including the start
limit and `reset-failed`, and monitor recovery while the service keeps running.
Both cases passed at `18a36e9` on x64 and arm64. The fixture gates its crash
on a new MainPID with ActiveState=active and SubState=running. The start-limit
check requires a failed unit, at least three fixture starts, a stable restart
counter and exit status 7. It accepts exit-code and start-limit-hit results.
Any failing fixture assertion prints bounded service/supervisor state, recent
journals and fixture history. Unit environments are excluded from diagnostics.
At `18a36e9`, both x64 and arm64 jobs also passed the assertion that rollback
completes within 15 seconds and the production `latch-host --background`
systemd startup/status smoke check with isolated settings. Local Linux tests
exercise both umasks `002` and `022`, five forward/rollback crash boundaries,
fake-systemd restart requests and injected ENOSPC/EIO journal-write failures.
The test is ignored during ordinary test runs and requires explicit opt-in
through the dedicated script, which refuses an existing `latch.service`.
The fixture bypasses supervisor argv parsing, self-pinning, installation ownership
checks and real GitHub manifest fetching. A full verified update of the real
`latch-host` binary under systemd remains untested. Real GUI reopening and stream
continuity across an update remain untested because no Linux desktop machine was available.

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

Latch 4.x and 5.x check the source repository's releases rather than the
public releases page. From 6.1.0, each release is also published there,
while that repository is public, so those versions offer it and update
themselves, and a 5.x PC can check an update pushed by a new Mac. Older
installs that were set up while the source repository was private, or that
miss this window, need the one-time manual install above, or a local/SSH
handover, to reach the manifest updater.
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

## Restrictive networks

Latch discovers this network's OS, PAC/WPAD and environment proxy settings in
background. A cached result is used immediately. Slow discovery continues while
the connection tries direct access. Changing networks or signing in to a portal
invalidates the cached result. Relay connection failures refresh discovery within
two seconds. A missing or empty incomplete PAC/WPAD result retains the prior
proxy. After 60 seconds without a successful connection through that retained
proxy, new connections try direct access first and keep the proxy as fallback.
Successful proxy connections restart this timer, including grant renewals and
reconnects. Healthy sessions reuse discovery, so one failed lookup can stay
cached. A completed result with proxy or Direct candidates replaces the prior
settings immediately, including platform WPAD fallback candidates. System
sleep can change the attempt order while retaining the proxy fallback.

A confirmed portal retries after 1 second with exponential backoff up to 4
seconds. The first portal observation and the first fresh open result take
effect immediately. After an open result on the same network, the banner
returns only after two consecutive redirect observations in separate discovery
rounds. A returning portal therefore needs one additional discovery round.
Alternating redirect/open results clear the banner once and preserve the retry
back-off. Delivered media clears the banner immediately. Opening sign-in or
returning to Latch rechecks promptly. Healthy sessions send no periodic HTTP
probes. An in-flight check can take up to six seconds.

- **This network needs a sign-in** appears when a portal redirects the anonymous
  connectivity check. Choose **Open network sign-in**, complete the browser's
  sign-in, and leave Latch open. The pending stream resumes automatically.
  Portal waiting ends three minutes after detection with a reachability message
  that also covers a completed sign-in. Media
  arriving clears the banner and keeps the normal eight-second stall watchdog.
- **This network inspects encrypted traffic** means the relay presented an
  untrusted certificate. Latch refuses the substituted certificate and shows its
  issuer when available. Ask the network administrator to permit the relay's
  genuine TLS connection, or use another network.
- **Proxy required** means Latch is using this network's HTTP CONNECT proxy.
  **Proxy authentication required** means its supplied credentials are absent
  or were rejected. Sign in through your OS's proxy configuration or contact the
  network administrator. Latch never prompts for or guesses proxy credentials.
- **Latch could not verify the relay's encrypted connection** means TLS
  certificate verification failed. Check the machine's clock and the network's
  certificate policy, then ask the administrator to allow the genuine relay
  certificate or use another network.
- **Automatic proxy settings could not be resolved** means OS PAC/WPAD settings
  could not produce a usable proxy. Check the OS proxy configuration and ask the
  network administrator for working automatic settings or a CONNECT proxy.
- **Proxy refused the CONNECT tunnel** means the proxy explicitly denied access.
  Ask the network administrator to allow CONNECT to the configured relay host
  on port 443.
- **Using the WebSocket carrier** means native relay TLS was blocked. The path
  indicator says **Latch relay · WebSocket**, with **via proxy** when applicable.
- **Tailscale is blocked** or **Tailscale is using DERP only** describes the
  network evidence reported by Tailscale. Check that Tailscale is signed in and
  allowed by this network. DERP adds latency.

A notice starting **The host's network** describes a problem on the sharing
machine. Complete sign-in or check the proxy/certificate policy on that machine.
These notices have fixed text and carry no sign-in URL or button.

Healthy networks show no network warning. The relay can retry a stalled TCP
path with 1200-byte and then 512-byte MSS on Linux, applied before connect to
shrink the relay certificate flight. On macOS TCP_MAXSEG is accepted only after
connect, so client hints affect outgoing data but cannot shrink that flight.
macOS and Windows rely on the relay's Linux TCP MTU probing for its sending
path. Windows skips client MSS recovery rungs. The hint expires after 30 seconds.

The portal check sends an anonymous plain-HTTP `GET /netenv/probe` to the
configured relay hostname on port 80. It includes Host, Connection and cache
control headers. It sends no cookies, authorization, grant material, device or
account identifier, or User-Agent, and follows no redirects. The relay sees the
source address as with any connection and does not log probe addresses. A missing
endpoint on an older relay degrades silently. Set `LATCH_NETENV_PROBE=0` to
suppress active HTTP checks and use OS portal signals only.
