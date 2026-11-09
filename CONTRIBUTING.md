# Contributing to Latch

Thanks for looking. Latch is a young project with a small set of test
machines, so the most useful contributions are often not code: a bug
report with the stats overlay's numbers, or a measurement from a GPU the
project has never run on (any NVIDIA or Intel GPU on Windows, any GPU at
all on Linux, an HDR monitor), moves it further than most patches.

- **Questions and ideas:** [Discussions](https://github.com/MrBeldum/latch/discussions).
- **Bugs:** an [issue](https://github.com/MrBeldum/latch/issues/new/choose).
- **Security problems:** privately, as [SECURITY.md](SECURITY.md) says.
- **Code:** read on.

Everyone taking part follows the [code of conduct](CODE_OF_CONDUCT.md).

## Building

Rust stable (`rust-toolchain.toml` selects it) and the platform's linker.
No C compiler, CMake or system library is compiled in: the engine and the
codecs are Rust, and `scripts/audit-native.sh` fails CI if a dependency
brings C, C++ or assembler into the build.

| OS | Needs |
|----|-------|
| macOS 13+ (Apple Silicon) | Xcode command line tools |
| Windows 10/11 x64 | Visual Studio Build Tools (MSVC) |
| Linux (X11) | `sudo apt-get install pkg-config libasound2-dev libxkbcommon-dev libwayland-dev libx11-dev libxcursor-dev libxrandr-dev libxi-dev libx11-xcb-dev libxcb-render0-dev libxcb-shape0-dev libxcb-xfixes0-dev` |

```bash
cargo run -p latch-host                 # the app, as a debug build
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

`cargo test` and `cargo run` keep settings and logs in `target/latch-data`
(`.cargo/config.toml` sets `LATCH_DATA_DIR`), so a development build never
touches an installed Latch's files. Some engine integration tests stream
through an emulated bad network and take a few minutes.

## Where things live

| Crate | What is in it |
|-------|---------------|
| `crates/proto` | The wire format: datagram kinds, Reed–Solomon FEC, packetizer and reassembler, input reliability, control framing. No I/O. |
| `crates/codec` | The software H.264 encoder and decoder and the `lpcm-rice` audio codec. |
| `crates/engine` | The sharing side: capture, hardware and software encode, display sizing, audio capture, input injection, congestion control, and the session server. One directory per OS (`mac/`, `windows/`, `linux/`). |
| `crates/stream` | The viewing side: session, receive loop, decoders (VideoToolbox, Media Foundation, `latch-codec`), audio playback, gamepads. |
| `crates/client` | The viewer UI. `session.rs` is the path from a listed machine to a live stream, `stream.rs` the toolbar and input while streaming, `video.rs` the wgpu presenter. |
| `crates/host` | The app (`latch-host`): control service, setup and uninstall, updates, and the window that both lists machines and shares this one. Windows administrator work is the generated script in `setup.rs`; macOS and Linux setup is `unix_setup.rs`. |
| `crates/core` | The control API types, a small HTTP client and server, the Tailscale CLI wrapper, wake packets, config files. |
| `crates/ui` | One palette, one typeface, one set of cards, rows, pills and buttons. |

[docs/ENGINE.md](docs/ENGINE.md) explains the design stage by stage and
[docs/PROTOCOL.md](docs/PROTOCOL.md) the wire format.

Two compatibility rules:

- **The control API** (`crates/core/src/api.rs`) is read by older hosts and
  viewers. New JSON fields get `#[serde(default)]`; existing ones keep
  their meaning.
- **The stream protocol** is version 1, and everything added since is
  negotiated inside it. A new datagram kind or field must be ignored by,
  and never sent to, a peer that did not offer it, and PROTOCOL.md says
  what it is.

Names from before the project was called Latch live only in
`crates/core/src/legacy.rs` and `crates/host/src/legacy.rs`, which exist
to migrate old installs.

## Streaming without a second machine

Two example programs are a host and a viewer with no window, no service
and no Tailscale check. Run them on loopback (never on a port other
people can reach: `serve` skips the identity check):

```bash
# a host streaming a synthetic desktop on 127.0.0.1:47860
LATCH_TEST_PATTERN=sweep cargo run --release -p latch-engine --example serve

# a viewer that decodes for ten seconds and prints fps, bitrate and latency as JSON
cargo run --release -p latch-stream --example view -- 127.0.0.1 \
  --width 1920 --height 1080 --fps 120 --kbps 50000 --seconds 10 --hevc
```

Without `LATCH_TEST_PATTERN`, `serve` captures the real screen (macOS
asks for Screen Recording). `view --keys 20` measures input-to-picture
latency against the pattern. Both list their options at the top of
`crates/engine/examples/serve.rs` and `crates/stream/examples/view.rs`.

`cargo run --release -p latch-engine --example pipeline_bench` runs the
host's capture-convert-encode pipeline with no network and prints
per-stage times; `cargo run --release -p latch-stream --example
stream_decode_bench` does the same for a decoder.

## Seeing the UI

```bash
cargo test -p latch-client -p latch-host snapshots -- --ignored
```

renders every screen to `target/ui-snapshots/*.png`: each state of the
machine list, Settings, the Sharing page and the stream toolbar, at the
smallest window (640×420), a typical one and a large one, at 1× and 2×.
It needs a GPU but no second machine and no real configuration. The same
states run in the ordinary test suite as layout checks: nothing may run
past the window and no two controls may overlap.

Every colour, size and distance comes from `crates/ui/src/theme.rs` and
every control from `crates/ui/src/widgets.rs`. A screen that needs
something new adds it there, with an accessible name and a focus ring.

## Tests that need hardware

Tests marked `#[ignore]` need something CI does not have: a GPU, a
specific encoder, a real display. Their names say what
(`hardware_amf`, `benchmark_mac_encoder`, `hdr10_child_window_…`), and

```bash
cargo test -p latch-engine hardware_ -- --ignored --nocapture
```

runs the ones for the machine you are on. If you have hardware the
project has not seen, the output of these is worth an issue on its own.

## Pull requests

- Branch from `main`; keep a pull request to one subject.
- The three commands above pass, on your OS at least. CI runs them on
  macOS, Windows and Linux (x64 and arm64).
- **Say how you checked it**, and what you could not check. "Not run on
  Windows hardware" is a normal and welcome sentence here; a claim nobody
  measured is not.
- **Performance changes come with numbers**: before and after, the
  machine, and the command, so someone else can repeat it.
  [docs/PERFORMANCE.md](docs/PERFORMANCE.md) shows the form.
- Anything a user would notice gets a line under *Unreleased* in
  [CHANGELOG.md](CHANGELOG.md).
- Documentation states what is true of the code in the same pull request.
  Describe what Latch does and what was verified; leave out what it might
  do one day.

By contributing you agree that your work is licensed under
GPL-3.0-or-later, like the rest of Latch.

## Releases

Maintainers cut a release by bumping `version` in `Cargo.toml`, dating the
changelog section, and pushing a `v*` tag. `.github/workflows/release.yml`
tests, builds and publishes the macOS, Windows and Linux archives with
their SHA-256 digests. Installed copies update themselves from there.
