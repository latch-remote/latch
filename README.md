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

[![CI](https://github.com/MrBeldum/latch/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/MrBeldum/latch/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/MrBeldum/latch?label=release&color=4F8FE8)](https://github.com/MrBeldum/latch/releases/latest)
[![Downloads](https://img.shields.io/github/downloads/MrBeldum/latch/total?color=4F8FE8)](https://github.com/MrBeldum/latch/releases)
[![License: GPL-3.0-or-later](https://img.shields.io/badge/license-GPL--3.0--or--later-4F8FE8)](LICENSE)

[Website](https://mrbeldum.github.io/latch/) ·
[Download](https://github.com/MrBeldum/latch/releases/latest) ·
[Docs](docs/README.md) ·
[Roadmap](docs/ROADMAP.md) ·
[Discussions](https://github.com/MrBeldum/latch/discussions)

<br>

<img src="site/img/lobby-1280.png" alt="The Latch window: every machine on your Tailscale account, with a Connect button beside each." width="820">

</div>

Install Latch on your computers, sign them in to the same Tailscale
account, and click **Connect**. The other machine's desktop appears full
screen with its sound, your keyboard, mouse and gamepads, and the
clipboard. Every install can both share and view.

- **Fast.** Hardware HEVC, AV1 and H.264, HDR10, surround sound, at this
  display's own size and refresh rate. [What was measured](docs/PERFORMANCE.md).
- **Nothing to configure.** No accounts, no pairing PINs, no port
  forwarding. Tailscale is the sign-in and the encryption.
- **Holds up on a bad network.** Forward error correction repairs lost
  packets, and the bitrate follows the connection.
- **Made for games.** Raw mouse input, Xbox and DualShock pads with rumble,
  a latency overlay, and competitive and cinematic profiles.
- **Reaches a sleeping PC.** Wake-on-LAN, remote sleep and restart, a
  self-hosted relay kit, and updates that install themselves.
- **One small binary.** All Rust; no bundled streaming server and no
  drivers to hunt for.

## Install

Every machine needs [Tailscale](https://tailscale.com/download), signed in
to the same account.

**Windows 10 or 11 (x64).** Download `latch-windows-x64.zip` from the
[latest release](https://github.com/MrBeldum/latch/releases/latest), unzip
it and run `latch-host.exe`. To share the PC, open the **Sharing** tab and
click **Set up as administrator**. [More](docs/WINDOWS.md)

**macOS 13 or later (Apple Silicon).** [More](docs/MACOS.md)

```bash
curl -fsSL https://raw.githubusercontent.com/MrBeldum/latch/main/scripts/install-macos-release.sh | bash
```

**Linux (x64 and arm64, X11).** Download `latch-host-linux-x64.tar.gz` or
`latch-host-linux-arm64.tar.gz` from the
[latest release](https://github.com/MrBeldum/latch/releases/latest)
(needs glibc 2.39 or later) and run `latch-host`. For a headless server
there are [systemd units](deploy/native/README.md) and a
[Docker container](deploy/node/NODE.md).

Then open Latch and click **Connect** beside another machine. The
[user guide](docs/USING.md) covers the rest.

## Platforms

| | Shares its desktop | Views |
|---|---|---|
| **Windows** | HEVC, AV1 or H.264 on the GPU; HDR10; virtual gamepads | hardware AV1, HEVC and H.264; HDR |
| **macOS** | HEVC or H.264 on VideoToolbox; HDR10 | hardware AV1 (M3 and later), HEVC and H.264; HDR |
| **Linux** | software H.264; NVENC and VA-API (not yet run on a GPU); virtual gamepads | software H.264 |

Latch is young. Viewers are desktop apps only: for a phone, a TV or a
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

Bug reports, measurements from hardware we do not have, and pull requests
are all welcome. Start with [CONTRIBUTING.md](CONTRIBUTING.md), pick
something labelled
[good first issue](https://github.com/MrBeldum/latch/labels/good%20first%20issue)
or [help wanted](https://github.com/MrBeldum/latch/labels/help%20wanted),
or ask in [Discussions](https://github.com/MrBeldum/latch/discussions).
Everyone taking part follows the [code of conduct](CODE_OF_CONDUCT.md).

## License

GPL-3.0-or-later; see [LICENSE](LICENSE) and [NOTICE](NOTICE). Latch is an
independent project, not affiliated with Tailscale, NVIDIA, Parsec or the
Sunshine and Moonlight projects.
