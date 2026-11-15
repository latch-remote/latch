# Contributing to Latch

Thanks for looking. Latch is a young project with a small set of test
machines, so the most useful contributions are often not code: a bug
report with the stats overlay's numbers, or a measurement from a GPU the
project has never run on (any NVIDIA or Intel GPU on Windows, any GPU at
all on Linux, an HDR monitor), moves it further than most patches.

- **Questions and ideas:** [Discussions](https://github.com/latch-remote/latch/discussions).
- **Bugs:** an [issue](https://github.com/latch-remote/latch/issues/new/choose).
- **Security problems:** privately, as [SECURITY.md](SECURITY.md) says.
- **Code:** read on.

Development issues, discussions and pull requests require access to the
private source repository. Public releases contain the corresponding GPL
source snapshot `latch-<version>-source.tar.gz`, including build inputs and
license notices. Private measurement dumps and account-specific deployment
records are excluded; deployment scripts remain included. Download and extract that archive to build without source
repository access. The public distribution repository holds releases.
[Public releases](https://github.com/latch-remote/latch-releases/releases)

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
(`.cargo/config.toml` sets `LATCH_DATA_DIR`), apart from an installed
Latch's. That separates the files, not the machine: `cargo run -p
latch-host` is still the real app. It starts a background service on port
47850 if none is running, and its Sharing tab can register a login item or
change system settings when you ask it to. To exercise streaming without
any of that, use the loopback tools below. Some engine integration tests
stream through an emulated bad network and take a few minutes; a few
codec tests compare against FFmpeg and the ITU conformance streams
(`scripts/fetch-h264-conformance.sh`) and skip quietly when those are not
there.

## Where things live

| Crate | What is in it |
|-------|---------------|
| `crates/proto` | The wire format, Reed–Solomon FEC, packetizer and reassembler, input reliability, control framing, and shared socket helpers. |
| `crates/relay` | Public native TLS transport, end-to-end record encryption, lane capabilities, bounded admission and durable transfer quota. |
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
- `cargo fmt`, `cargo clippy` and `cargo test`, as written under
  [Building](#building), pass on your OS at least. CI runs them on macOS,
  Windows and Linux (x64 and arm64).
- **Say how you checked it**, and what you could not check. "Not run on
  Windows hardware" is a normal and welcome sentence here; a claim nobody
  measured is not.
- **Performance changes come with numbers**: before and after, the
  machine, and the command, so someone else can repeat it.
  [docs/PERFORMANCE.md](docs/PERFORMANCE.md) shows the form.
- Anything a user would notice gets a line under `## Unreleased` at the
  top of [CHANGELOG.md](CHANGELOG.md); add the heading if the last release
  closed it.
- Documentation states what is true of the code in the same pull request.
  Describe what Latch does and what was verified; leave out what it might
  do one day.

By contributing you agree that your work is licensed under
GPL-3.0-or-later, like the rest of Latch.

## Releases

Maintainers align the workspace version, local path package requirements
and `latch-*` package entries in `Cargo.lock`, then date the matching
changelog section. `cargo metadata --locked` verifies the manifest and lock
before a `v*` tag is pushed. The release workflow tests and builds macOS,
Windows and Linux assets, exports corresponding source from the tag, and
prepares an allowlisted public asset set. A distribution-scoped App token
creates a draft, verifies uploaded bytes, and publishes it only after the
asset set passes validation. The public distribution tag belongs to that
repository's history.

Every release includes `release.json`, `SHA256SUMS`, `LICENSE`, `NOTICE`
and `latch-<version>-source.tar.gz`. The manifest binds exact version-tagged
URLs, byte sizes and SHA-256 digests. Source exports preserve workspace
build inputs, the release packaging workflow, and third-party notices while
excluding other repository automation, site assets and private operational
records. Consumer update requests are
anonymous. Installed Mac and Windows copies update from this manifest.
Linux hosts show a notice.

The publisher uses `vars.RELEASE_APP_ID` and
`secrets.RELEASE_APP_PRIVATE_KEY`, with the App installed only on
`latch-remote/latch-releases`. Keep that installation scoped to distribution.
Linux jobs default to GitHub-hosted runners. Optional `LINUX_X64_RUNNER`
and `LINUX_ARM64_RUNNER` variables select verified alternative runners.
Blacksmith dashboard verification and capacity remain unconfirmed.
`PAGES_ENABLED` and `RELEASE_ATTESTATIONS_ENABLED` are opt-in after plan
entitlement and configuration are verified. Private attestations are
unavailable by default. Release signing and notarization remain open work.

```bash
python3 -m unittest discover -s scripts -p 'test_publish_release.py' -v
bash scripts/test-release-manifest.sh
python3 scripts/release-notes.py 6.0.0
cargo metadata --locked --format-version 1 --no-deps
```
