# Viewer responsiveness: implementation and outstanding input measurement

Date: 2026-11-09. Apple Silicon M3 Pro Mac, macOS. No installed service,
remote machine, system settings, or port 47850 was used.

## Input findings (incomplete)

The input defect is **not fixed** in this branch. No OS-to-socket timing
instrumentation or live latency experiment has been completed. These tables
explicitly record missing measurements; they are not performance results.

| Before: presentation request | n | median ms | p95 ms | max ms |
|---|---:|---:|---:|---:|
| 60 fps | 0 | unmeasured | unmeasured | unmeasured |
| 120 fps | 0 | unmeasured | unmeasured | unmeasured |
| 240 fps | 0 | unmeasured | unmeasured | unmeasured |

| After: presentation request | n | median ms | p95 ms | max ms |
|---|---:|---:|---:|---:|
| 60 fps | 0 | unmeasured | unmeasured | unmeasured |
| 120 fps | 0 | unmeasured | unmeasured | unmeasured |
| 240 fps | 0 | unmeasured | unmeasured | unmeasured |

Source trace:

- Mouse, keyboard and text enter winit/egui on the window event-loop thread.
  eframe 0.31's `App::raw_input_hook` runs in `EpiIntegration::update`, after
  `take_egui_input`; it is not a per-OS-event callback. Moving the handler
  there alone would still wait for a frame.
- `client::stream::View::forward` clones the accumulated egui events during
  the UI update. Absolute mouse keeps the latest position, relative mouse
  and wheel accumulate with fractional remainders, and key/text dispatch
  handles physical keys, composition, clipboard operations and capture.
  `Held` releases keys/buttons on capture/focus loss.
- `stream::session::Input::send` takes the link mutex, queues the reliable
  event and flushes it immediately. There is no frame timer after this call.
  `proto::input::InputSender` preserves reliable order and merges adjacent
  unsent relative movement. Retries are independent of UI updates.
- Gamepad state already bypasses UI frames: Mac GameController callbacks
  run on their dispatch queue, Windows polls on its gamepad thread, and
  Linux reads evdev on its gamepad thread. `Pads` invokes the attached
  client sink, then `Input::pad` updates and flushes the session sender.
- CHANGELOG's `MOUSE_MOVE_NOCOALESCE` regression explicitly requires plain
  relative movement on Windows because affected games discard NOCOALESCE
  moves. No mouse coalescing or injection semantics were changed here.

An actual fix needs a callback before eframe accumulates events, plus a
shared capture/shortcut/IME routing state that cannot send duplicates or
leak locally handled input. No negligible-delay claim is supported.

## Reconnect and decoder checks

The reconnect policy retains quick 2/5/10-second retries and then waits
20–30 seconds with jitter, for a five-minute recovery window. Each attempt
uses the existing fresh connection/negotiation path. Discovery offline→online
and an event-loop gap over ten seconds make a pending retry immediately due.
This gap heuristic is not a platform wake notification. There is no new
native network-change listener. Stop cancels retries and in-flight work.
Explicit stream HTTP refusal, host Stop and session end do not trigger retries.
Capture/encoder failures retain their existing retry behavior.

Windows startup, decode reply and hardware-probe caller waits have a
one-second deadline; the existing GPU-copy fence has a two-second bound.
A failed decode reply drops the request channel; a subsequent keyframe
opens a fresh worker. Driver-owned resources remain on the abandoned worker
until the driver returns. After three abandoned workers within ten minutes,
new workers are refused, allowing existing fallback/renegotiation behavior.
The budget is process-wide, including probes, to bound retained resources.

Commands run from the checkout, with the environment below:

```sh
export PATH=$HOME/.cargo/bin:$HOME/.local/bin:$PATH TMPDIR=/tmp
cargo check --target x86_64-pc-windows-msvc -p latch-stream -p latch-client
cargo clippy --target x86_64-pc-windows-msvc -p latch-stream -- -D warnings
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo test -p latch-client snapshots -- --ignored
```

Local results:

- Formatting, workspace clippy and `cargo test --workspace`: passed.
- Windows cross-target check (stream + client) and stream clippy: passed.
- Deadline tests: 2 passed. Reconnect-filtered tests: 14 passed, 1 ignored.
- Initial full ignored client snapshots: 8 passed, 1 failed at the audit's
  existing `snapshots_stream_menus_at_the_smallest_size` (Copy diagnostics).
  After merging the concurrent viewer work from main, all 9 snapshots passed.
  That test's correction came from the concurrent branch, not this change.
- Formatting, workspace clippy/tests and both Windows cross-target checks
  were repeated after merging that base and passed.
- `cargo test -p latch-client snapshots_reconnect -- --ignored`: passed;
  waiting and in-flight PNGs were inspected at minimum and typical sizes.
- Dependencies unchanged; the dependency audit was not required.

Platform-neutral deadline tests use
zero-duration waits and an injected clock for budget expiry. Reconnect tests
use supplied `Instant` values and synthesized discovery/wake-gap signals.
Snapshot/layout tests exercise the recovery controls at existing window sizes.

Unverified: Windows hardware and actual hung-driver recovery, real sleep or
Wi-Fi changes, physical gamepads, live host restart/reconnect, OS-to-socket
input latency and 60/120/240-fps presentation. No input speedup is claimed.
