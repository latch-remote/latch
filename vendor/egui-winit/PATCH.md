# egui-winit OS-event callback

Based on egui-winit 0.31.1 from the crates.io source archive, corresponding
to the egui 0.31.1 tag. The original MIT and Apache-2.0 licenses are included.

Upstream eframe 0.31 exposes input to the app at frame time. Latch uses this
local Cargo patch to reach translated input directly from the event loop
without maintaining macOS, Windows, X11 and Wayland hooks.

The changes in `src/lib.rs` add an optional `EventHook` in the egui context's
temporary data. `on_window_event` and `on_mouse_motion` call it synchronously
with only the events just translated. `take_egui_input` calls its frame
callback immediately before taking the accumulated input. Callbacks run
outside the context data lock. Events and upstream event responses stay
available to the normal UI integration.

`Cargo.toml` uses the licenses included in this directory. Other source
files retain their published contents. Keep the callback points when
updating the dependency, or remove this patch when upstream offers an
equivalent event-time API. Latch's callback integration is exercised by
`early::tests` in `latch-client`.
