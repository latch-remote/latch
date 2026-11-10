# Using Latch

What happens in a session, and what keeps a machine reachable when nobody
is sitting at it. Platform details are in [MACOS.md](MACOS.md) and
[WINDOWS.md](WINDOWS.md); Linux servers in
[deploy/native/README.md](../deploy/native/README.md) and
[deploy/node/NODE.md](../deploy/node/NODE.md).

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

## When the connection drops

If a stream loses its connection, leave the window open. Latch tries again
after 2, 5 and 10 seconds, then every 20 to 30 seconds for five minutes,
and at once when the machine shows as online again or this one wakes from
sleep. The overlay shows the countdown; **Reconnect now** skips the wait
and **Stop** goes back to the machine list. Each attempt is a new session,
so a display or network that changed in the meantime is taken into account.

Latch does not keep retrying when the other side said no: a refusal,
another viewer taking the machine over, or the stream being ended there.
**Try again** starts over when you are ready.

## Updates

Every few hours Latch asks GitHub for the latest release. Settings has the
switch and a **Check now** button.

- **macOS.** A newer app is downloaded, checked against the digest GitHub
  publishes and its own code signature, swapped into `/Applications` once
  no stream is running, and relaunched. The Mac also sends a newer
  `latch-host.exe` to each of your PCs whose host reports an older version,
  over the same Tailscale-authenticated control API that can put the PC to
  sleep. A PC that is asleep gets it the next time the Mac sees it.
- **Windows.** With the window open, Latch downloads a newer
  `latch-host.exe` itself and installs it when no stream is running.
  Whether an update is pulled or pushed from a Mac, the PC looks the
  release up on GitHub and installs the file only if it matches the
  SHA-256 published there; what the sender says about it is not enough. A
  PC that cannot reach GitHub refuses the update.
- **Linux.** Latch says a newer version exists and links to it. Install the
  tarball as in [deploy/native/README.md](../deploy/native/README.md), or
  rebuild the Docker node.

Public releases need no GitHub login. For a private fork, Latch uses the
GitHub token git has stored for github.com, `LATCH_GITHUB_TOKEN`, or
`github_token` in `client.toml`. Hosts installed before 3.1 need a local or
SSH installation of a current release: a Latch 5 viewer cannot stream the
retired protocol to upgrade them. Mac builds are ad-hoc signed, so macOS may
ask for Screen Recording and Accessibility again after an update
([why](ROADMAP.md#signing-and-notarization)).

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

An older install updates to Latch 5 and keeps its settings. Latch 5
machines stream only with Latch 5 machines: a Latch 5 viewer that finds an
older host sends it the update (or says which machine to update), and an
older viewer finds no stream on a Latch 5 host and offers its own update.
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

Use `LATCH_DATA_DIR` and `LATCH_GITHUB_TOKEN` for environment overrides.
The equivalent names from versions before 4.1.0 still work.
