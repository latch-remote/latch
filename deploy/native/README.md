# Native Linux desktop

This guide installs a headless X11 desktop on a fresh Ubuntu 24.04 machine,
x64 or arm64, with a normal user who can run `sudo`. Releases require glibc
2.39 or later. Run the commands as that user, including over SSH. Do not
run the desktop as root. Wayland capture is not supported yet.

## Install Tailscale

Install the Ubuntu 24.04 repository key and package list, then the daemon
and CLI. These are downloaded files, not a script piped into a root shell:

```sh
sudo apt-get update
sudo apt-get install -y ca-certificates curl gnupg
curl -fsSLo /tmp/tailscale-keyring.gpg https://pkgs.tailscale.com/stable/ubuntu/noble.noarmor.gpg
curl -fsSLo /tmp/tailscale.list https://pkgs.tailscale.com/stable/ubuntu/noble.tailscale-keyring.list
sudo install -m 0644 /tmp/tailscale-keyring.gpg /usr/share/keyrings/tailscale-archive-keyring.gpg
sudo install -m 0644 /tmp/tailscale.list /etc/apt/sources.list.d/tailscale.list
sudo apt-get update
sudo apt-get install -y tailscale
sudo systemctl enable --now tailscaled
sudo tailscale up
tailscale status
```

Follow the printed login URL and sign in with the same account as the
viewing machine. Tailnet policy must allow TCP and UDP 47850 between them.
An untagged host answers only devices signed in to its own Tailscale
account. A tagged host has no account: on a personal tailnet it answers
the tailnet's owner, and on an organisation's tailnet only the logins
listed in `allowed_logins` in `host.toml`
(`allowed_logins = ["you@example.com"]`); with none listed it answers
nobody. Prefer an untagged personal host and restrict reachability with
Tailscale policy. A fresh install uses the native daemon, not a relay
container's socket. Do not run two daemons against the same identity.

## Install runtime packages

```sh
sudo apt-get install -y --no-install-recommends \
  sudo systemd dbus-user-session dbus-x11 \
  xvfb xauth x11-utils util-linux xfce4 xfce4-terminal \
  pulseaudio libasound2t64 \
  libxkbcommon0 libxkbcommon-x11-0 libwayland-client0 libvulkan1 libgl1 libegl1 \
  libxrandr2 libxi6 libxcursor1 libx11-xcb1 \
  libxcb-render0 libxcb-shape0 libxcb-xfixes0
```

Xvfb provides display `:1` at 1920×1080. XFCE supplies the desktop;
PulseAudio runs as a user service. X11 capture and software H.264 need no
GPU. Optional NVENC/VA-API encoding needs the appropriate GPU and drivers
and remains unverified on physical Linux hardware. Linux viewers currently
use software H.264 decode.

## Download and verify

Choose a published tag from [Releases](https://github.com/latch-remote/latch-releases/releases).
Replace `vX.Y.Z` below. The archive includes the binary and this native kit.

```sh
tag=vX.Y.Z
case "$(uname -m)" in
  x86_64) arch=x64 ;;
  aarch64) arch=arm64 ;;
  *) echo 'Unsupported architecture'; exit 1 ;;
esac
base="https://github.com/latch-remote/latch-releases/releases/download/$tag"
name="latch-host-linux-$arch"
curl -fLO "$base/$name.tar.gz"
curl -fLO "$base/$name.tar.gz.sha256"
sha256sum -c "$name.tar.gz.sha256"
tar xzf "$name.tar.gz"
cd "$name"
```

Stop if checksum verification fails. The checksum comes from the same
GitHub release as the binary; it checks integrity, not independent
publisher provenance.

## Headless server

The installer prints its actions, installs the binary and user units,
enables lingering so they survive logout, and starts the stack:

```sh
bash native/install.sh "$PWD/latch-host"
systemctl --user --no-pager status latch-display.service latch-desktop.service latch.service
journalctl --user -u latch -u latch-desktop -u latch-display -n 80
```

It is safe to repeat; it replaces the shipped headless units and drop-in.
Save local unit customizations before upgrading. `--install-only` installs
files without enabling lingering or touching running services. If
`systemctl --user` cannot connect to the bus, log out and back in through
SSH after installing `dbus-user-session`; do not substitute `sudo systemctl`.
Check lingering with `loginctl show-user "$USER" -p Linger`.

The display disables TCP and uses a cookie at `%t/latch.Xauthority`
(`%t` is the user's runtime directory). Input uses XTest. The host runs
capture and encoding in-process; no separate streaming engine is installed.

On another machine, install Latch, sign in to Tailscale with the same
account, open **Machines**, select this server and click **Connect**. Check
that XFCE appears and that pointer and keyboard input work, then disconnect.
A healthy unit alone does not prove a stream. If the server is missing,
check `tailscale status`, account identity, tailnet policy and the journal.
Do not expose 47850 publicly to diagnose it.

## Existing X11 desktop

For a signed-in X11 desktop, install the runtime packages, then install
only the binary and run it in that desktop's terminal:

```sh
sudo install -m 0755 latch-host /usr/local/bin/latch-host
latch-host
```

Use the **Sharing** tab for setup. Do not install the headless units: they
capture their own virtual desktop rather than your existing session.
Wayland sessions are not supported, even if Xwayland is present.

## Updates and old installations

This kit installs a root-owned binary at `/usr/local/bin/latch-host`. Latch
shows an installation-owner update notice for that path. Download and verify
the next release, then repeat the headless installer. The installer restarts
the headless host, ending any active stream, so arrange that after disconnecting.

A release binary installed in a user-owned writable directory, such as
`~/.local/bin`, can update from the Latch window or with `latch-host --self-update`
while its local service runs. The updater waits for streams to end, verifies
the matching public manifest/archive, replaces the resolved binary atomically
and restarts its service with health-check rollback. A registered user unit
uses a separate transient systemd user supervisor. A definitive systemd failure
rolls back promptly. A supervisor lost before the swap retries with bounded
back-off. Manifest outages have a separate retry counter, and verified downloads
reset archive failures. Owned regular updater locks restored with incorrect
modes are repaired to `0600` through verified descriptors. Reopen the window after
installation. AUR/Nix and container installs use their deployment tools.
See [Linux update ownership and recovery](../../docs/USING.md#linux-installation-ownership-and-recovery).

Updater tests use Docker locally and native x64/arm64 Linux CI. No Linux machine outside CI and containers is available for this work. CI covers systemd user-session restart, rollback and killed-supervisor recovery with a fixture service; an installed desktop update remains unverified.
It also covers a replacement that crashes on start until the start limit, and
recovery while the service keeps running. The fixture bypasses supervisor argv parsing, self-pinning, installation
ownership checks and real GitHub manifest fetching. The follow-up adds bounded
assertion diagnostics, an assertion requiring rollback within 15 seconds and a real
`latch-host --background` startup/status smoke check under systemd with isolated
settings. The changed systemd cases await CI verification. A full verified update
of the production executable under systemd remains untested. Real GUI reopening
and stream continuity remain untested.

`migrate-legacy.sh` is only for upgrades from pre-4.1 BroLink unit names or
Latch 4's separate engine service. Install the runtime prerequisites and
new binary first, then run it as the desktop user. It preserves existing
display units and imports legacy prep commands. Settings and Tailscale
identity can survive an upgrade; Latch 5 does not use the old streaming
pairing credentials.

## Display sizing

Xvfb's desktop stays at 1920×1080. A stream of another size is scaled from
it, with black bars when the shape differs.
With a resizable Xorg display, `host.toml` can set `prep_do` and `prep_undo`
to a display script. It receives `LATCH_CLIENT_WIDTH`, `LATCH_CLIENT_HEIGHT`
and `LATCH_CLIENT_FPS`. Set `LATCH_DISPLAY_FOLLOWS_VIEWER=1` in the service
environment only when that script can actually resize the display.
Configuration lives in `~/.local/share/latch/host.toml`.

## Gamepads

Linux gamepads need write access to `/dev/uinput`. Without it, streaming
still works but the toolbar explains why the host takes no gamepads.
A dedicated group avoids granting access to all keyboard devices:

```sh
sudo groupadd -f latch-pads
sudo usermod -aG latch-pads "$USER"
echo 'KERNEL=="uinput", SUBSYSTEM=="misc", GROUP="latch-pads", MODE="0660", OPTIONS+="static_node=uinput"' |
  sudo tee /etc/udev/rules.d/60-latch-uinput.rules
sudo modprobe uinput
sudo udevadm control --reload
sudo udevadm trigger --name-match=uinput
```

Sign out and back in for group membership. A lingering user manager may
retain its old groups until it is restarted; arrange this outside a stream.
The viewer's pads appear as Xbox 360 pads, with rumble sent back.
