<div align="center">

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/brand/logo-dark.svg">
  <img src="docs/brand/logo.svg" alt="" width="72" height="72">
</picture>

# Latch

**Your machines, on any of your machines.**

Low-latency desktop and game streaming between computers you own, over
[Tailscale](https://tailscale.com). One app for macOS, Windows and Linux,
written in Rust.

[![Release](https://img.shields.io/github/v/release/latch-remote/latch-releases?label=release&color=4F8FE8)](https://github.com/latch-remote/latch-releases/releases/latest)
[![Downloads](https://img.shields.io/github/downloads/latch-remote/latch-releases/total?color=4F8FE8)](https://github.com/latch-remote/latch-releases/releases)
[![License: GPL-3.0-or-later](https://img.shields.io/badge/license-GPL--3.0--or--later-4F8FE8)](LICENSE)

[Download](https://github.com/latch-remote/latch-releases/releases/latest) ·
[Docs](docs/README.md) ·
[Roadmap](docs/ROADMAP.md) ·
[Development (access required)](https://github.com/latch-remote/latch)

<br>

<img src="site/img/lobby-1280.png" alt="The Latch window: every machine on your Tailscale account, with a Connect button beside each." width="820">

</div>

Install Latch on your computers, sign them in to the same Tailscale
account, and click **Connect**. The other machine's desktop appears full
screen with its sound, your keyboard, mouse and gamepads, and the
clipboard. Every install can both share and view.

- **Fast.** Hardware HEVC, AV1 and H.264, HDR10, surround sound, at this
  display's own size and refresh rate. [What was measured](docs/PERFORMANCE.md).
- **Your Tailscale identity.** No Latch account or pairing PIN. Tailscale
  authorizes control and protects the normal UDP stream. Relay media has
  additional end-to-end encryption.
- **Holds up on a bad network.** Forward error correction repairs lost
  packets, and the bitrate follows the connection. Version 6 can attempt a
  public TCP/TLS relay when UDP video is missing, persistently damaged, or
  limited far below the requested rate during motion.
- **Made for games.** Raw mouse input, Xbox and DualShock pads with rumble,
  a latency overlay, and competitive and cinematic profiles.
- **Reaches a sleeping PC.** Wake-on-LAN, remote sleep and restart, a
  self-hosted relay kit, and updates that install themselves on Mac and
  Windows.
- **One small binary.** All Rust, with no bundled streaming server.

## Install

Every machine needs [Tailscale](https://tailscale.com/download), signed in
to the same account.

**Windows 10 or 11 (x64).** Download `latch-windows-x64.zip` from the
[latest release](https://github.com/latch-remote/latch-releases/releases/latest), unzip
it and run `latch-host.exe`. To share the PC, open the **Sharing** tab and
click **Set up as administrator**. [More](docs/WINDOWS.md)

**macOS 13 or later (Apple Silicon).** [More](docs/MACOS.md)

Download `latch-macos-arm64.tar.gz` and `SHA256SUMS` from the
[latest release](https://github.com/latch-remote/latch-releases/releases/latest).
Compare `shasum -a 256 latch-macos-arm64.tar.gz` with its line in
`SHA256SUMS`, extract the archive, and run
`codesign --verify --deep --strict Latch.app`. Quit Latch before replacing
`/Applications/Latch.app`. Run
`xattr -dr com.apple.quarantine /Applications/Latch.app`, then open it.
The app uses an ad-hoc signature. Allow Screen Recording and Accessibility
when setting up sharing.

**Linux (x64 and arm64, X11).** Download `latch-host-linux-x64.tar.gz` or
`latch-host-linux-arm64.tar.gz` from the
[latest release](https://github.com/latch-remote/latch-releases/releases/latest)
(needs glibc 2.39 or later) and run `latch-host`. For a headless server
there are [systemd units](deploy/native/README.md) and a
[Docker container](deploy/node/NODE.md).

Then open Latch and click **Connect** beside another machine. The
[user guide](docs/USING.md) covers the rest.

### Package managers

This repository has package definitions for Scoop, the AUR and Nix, in
[packaging/](packaging/README.md). They install the same releases. None is
in a public bucket, the AUR or nixpkgs yet, so for now they install from
this repository.

**Scoop (Windows).**

```powershell
scoop install https://raw.githubusercontent.com/latch-remote/latch/main/packaging/scoop/latch.json
```

Scoop runs the release's `install-host.ps1`, which installs Latch to
`%LOCALAPPDATA%\Latch` as the zip does. Latch then updates itself.
`scoop update` installs a release only if it is newer than that copy.
`scoop uninstall` leaves the copy in place; remove it from Settings > Apps.

**Arch Linux.** `latch-remote-bin` packages the release binary and
`latch-remote` builds the release's source. (`latch` on the AUR is another
program.)

```sh
sudo pacman -S --needed git base-devel tailscale
git clone https://github.com/latch-remote/latch
cd latch/packaging/aur/latch-remote-bin
makepkg -si
```

**Nix (x86_64 and aarch64 Linux).** The flake builds from source. Its
`latch-bin` package is the release binary.

```sh
nix run github:latch-remote/latch
nix profile add github:latch-remote/latch           # older Nix: nix profile install
nix profile add github:latch-remote/latch#latch-bin
```

The packages are also in `overlays.default`. Tailscale comes separately
(`services.tailscale.enable = true` on NixOS). Sharing setup records the
program's `/nix/store` path in `~/.config/systemd/user/latch.service`;
after an upgrade, run setup again from the **Sharing** tab.

Each package installs `latch-host` with a desktop entry. For a headless
server, follow the [native guide](deploy/native/README.md) instead.

### Moving from 5.x

Older binaries can still look for updates in the former repository. If
**Check now** cannot find 6.0.0, install once from the public release page
on each machine. On Windows, unzip the release and run its
`install-host.ps1`. On macOS, replace the app as above. On Linux, replace
the binary and restart the service using the native guide. Keep your
settings folder. A 5.x Windows host may also reject an update pushed by a
new Mac because it verifies against its old release endpoint.

### Relay availability and privacy

The default endpoint, `relay.latch.rune.foo:443`, runs on a dedicated AWS
Lightsail instance with public TLS and a durable transfer budget. The
relay is a capacity-limited pilot. Its
operator can see public IP addresses, connection timing and traffic volume.
Session media and input are end-to-end encrypted. Tailscale-authenticated
control must remain reachable throughout the session.

In **Settings → Stream relay**, turn off **Recover automatically through
the Latch relay** to opt out on the viewer. **Relay hostname** selects the
endpoint, which must also match the host's configuration. Host changes
take effect when its background service restarts. Native TLS uses ALPN
`latch-relay/1`. Some HTTP-only proxies and inspected networks cannot carry
it. Relay CPU requirements, limits and failure behavior are in the
[user guide](docs/USING.md#stream-relay). Synthetic Mac-to-Windows sessions
on the campus connection have exercised the public relay. Sustained
performance and remaining field limits are recorded in the
[performance notes](docs/PERFORMANCE.md#latch-6-network-recovery).

## Platforms

| | Shares its desktop | Views |
|---|---|---|
| **Windows** | HEVC or H.264 on the GPU, AV1 where it has an encoder (not yet run on one); HDR10; virtual gamepads | hardware AV1, HEVC and H.264; HDR |
| **macOS** | HEVC or H.264 on VideoToolbox; HDR10 | hardware AV1 (M3 and later), HEVC and H.264; HDR |
| **Linux** | software H.264; NVENC and VA-API (not yet run on a GPU); virtual gamepads | software H.264 |

Latch is young. HDR has been checked by reading pixels back, not yet by
eye on an HDR panel. Viewers are desktop apps only: for a phone, a TV or a
browser, use [Moonlight](https://moonlight-stream.org) today. What has and
has not met real hardware is listed in
[docs/PERFORMANCE.md](docs/PERFORMANCE.md), and what is planned in the
[roadmap](docs/ROADMAP.md).

<details>
<summary><b>How it compares</b></summary>

| | Latch | Sunshine + Moonlight | Parsec | GeForce NOW |
|---|---|---|---|---|
| Streams | your own machines | your own machines | your own machines | NVIDIA's servers |
| Sign-in | your Tailscale account | PIN pairing per device | Parsec account | NVIDIA account |
| Reaching home from outside | Tailscale; no ports opened | port forwarding or your own VPN | Parsec's servers | n/a |
| Apps to install | one, the same on every machine | a host and a client | one | one |
| Viewers | macOS, Windows, Linux | nearly everything, phones and TVs included | desktops, Android, web | nearly everything |
| Source | GPL-3.0, all Rust | GPL-3.0 | closed | closed |

</details>

<details>
<summary><b>Build from source</b></summary>

Rust stable and the platform's linker are all it takes; on Linux, also the
`-dev` packages listed in [CONTRIBUTING.md](CONTRIBUTING.md).

Development happens in the private
[source repository](https://github.com/latch-remote/latch), which requires
access. Each public release includes a corresponding GPL source snapshot
named `latch-<version>-source.tar.gz`, with build scripts and license
notices. Download and extract that snapshot to build without repository
access. Downloads and the version 6 updater need no GitHub credentials.

```bash
cargo run --release -p latch-host        # the app
cargo test --workspace                   # the tests
./scripts/install-macos.sh               # macOS: build, bundle and install Latch.app
```

</details>

## Documentation

- [Using Latch](docs/USING.md), on [Windows](docs/WINDOWS.md) and [macOS](docs/MACOS.md)
- [How the engine works](docs/ENGINE.md) and the [wire protocol](docs/PROTOCOL.md)
- [Security model](SECURITY.md), and how to report a problem privately
- [Changelog](CHANGELOG.md)

## Contributing

Bug reports and measurements from hardware we do not have are welcome.
Development issues and pull requests require source-repository access.
Start with [CONTRIBUTING.md](CONTRIBUTING.md) and
[SUPPORT.md](SUPPORT.md).
Everyone taking part follows the [code of conduct](CODE_OF_CONDUCT.md).

## License

GPL-3.0-or-later; see [LICENSE](LICENSE) and [NOTICE](NOTICE). Latch is an
independent project, not affiliated with Tailscale, NVIDIA, Parsec or the
Sunshine and Moonlight projects.
