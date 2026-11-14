#!/usr/bin/env python3
"""Reuse the native presentation example with an explicit UDP/automatic A/B switch.

The generated scratch package links the unchanged Latch crates in ROOT.
Usage: make-viewer.py ROOT OUTPUT_DIRECTORY
"""
import pathlib
import sys

root = pathlib.Path(sys.argv[1]).resolve()
out = pathlib.Path(sys.argv[2]).resolve()
out.mkdir(parents=True, exist_ok=True)
source = (root / 'crates/client/examples/present.rs').read_text()
replacements = [
    ('    last_stats: Instant,', '    last_stats: Instant,\n    render_history: Vec<Duration>,'),
    ('        if self.last_stats.elapsed() >= Duration::from_secs(1) {',
     '        if self.last_stats.elapsed() >= Duration::from_secs(1) {\n            self.render_history.extend(self.frames.render_times());\n            self.frames.measure_render();'),
    ('        let mut times: Vec<f64> = self\n            .frames\n            .render_times()\n            .iter()',
     '        let mut times: Vec<f64> = self.render_history.iter().copied().chain(self.frames.render_times())'),
    ('                last_stats: Instant::now(),', '                last_stats: Instant::now(),\n                render_history: Vec::new(),'),
    ('eprintln!("{event:?}");', 'eprintln!("field_event elapsed_s={:.3} {event:?}", self.start.elapsed().as_secs_f64());'),
    ('Duration::from_secs(self.seconds).saturating_sub(self.start.elapsed()),', 'Duration::from_secs(self.seconds).saturating_sub(self.start.elapsed()).min(Duration::from_secs(1)),') ,
    ('Some(Session::start(', 'Some(Session::start_with_relay('),
    ('bitrate_kbps: 50_000,', 'bitrate_kbps: std::env::var("LATCH_FIELD_KBPS").unwrap_or_else(|_| "20000".into()).parse().expect("field bitrate"),'),
    ('audio: false,', 'audio: std::env::var_os("LATCH_FIELD_AUDIO").is_some(),'),
    ('move || ctx.request_repaint(),\n                    )),',
     'move || ctx.request_repaint(),\n                        if std::env::var_os("LATCH_FIELD_UDP_ONLY").is_some() { None } else { latch_stream::session::relay_config(&Default::default()) },\n                    )),'),
    ('eprintln!("stats {:?}", session.stats());',
     'eprintln!("field elapsed_s={:.3} relay={} decoded={} rendered={} stats {:?}", self.start.elapsed().as_secs_f64(), session.relay_active(), self.frames.seq(), self.render_history.len(), session.stats());'),
]
for before, after in replacements:
    assert source.count(before) == 1, before
    source = source.replace(before, after)
(out / 'main.rs').write_text(source)
headless = (root / 'crates/stream/examples/view.rs').read_text()
headless = headless.replace('fn square_is_white(f: &Frame) -> bool {', 'fn square_is_white(f: &Frame, source_width: u32, source_height: u32) -> bool {')
headless = headless.replace('for y_pos in 16..48 {', 'for y_pos in (16 * f.height / source_height) as usize..(48 * f.height / source_height) as usize {')
headless = headless.replace('for x_pos in 16..48 {', 'for x_pos in (16 * f.width / source_width) as usize..(48 * f.width / source_width) as usize {')
headless = headless.replace('let mut sum = 0u64;', 'let mut sum = 0u64;\n        let mut samples = 0u64;')
headless = headless.replace('if idx < y.len() {', 'if idx < y.len() {\n                    samples += 1;')
headless = headless.replace('sum / (32 * 32) > 128', 'sum / samples.max(1) > 128')
assert headless.count('square_is_white(&f)') == 2
headless = headless.replace('square_is_white(&f)', 'square_is_white(&f, width, height)')
assert headless.count('let session = Session::start(') == 1
headless = headless.replace('let session = Session::start(', 'let session = Session::start_with_relay(')
assert headless.count('        || {},\n    );') == 1
headless = headless.replace('        || {},\n    );', '        || {},\n        if std::env::var_os("LATCH_FIELD_UDP_ONLY").is_some() { None } else { latch_stream::session::relay_config(&Default::default()) },\n    );')
(out / 'headless.rs').write_text(headless)
manifest = '''[package]
name = "latch-field-viewer"
version = "0.0.0"
edition = "2021"
[workspace]
[[bin]]
name = "latch-field-viewer"
path = "main.rs"
[[bin]]
name = "latch-field-headless"
path = "headless.rs"
[dependencies]
serde_json = "1"
parking_lot = "0.12"
anyhow = "1"
eframe = { version = "0.31", default-features = false, features = ["wgpu", "default_fonts", "accesskit", "x11", "wayland"] }
egui = "0.31"
egui-wgpu = "0.31"
tracing-subscriber = { version = "0.3", features = ["env-filter", "fmt"] }
'''
for name in ['client', 'stream', 'proto']:
    manifest += 'latch-' + name + ' = { path = "' + (root / 'crates' / name).as_posix() + '" }\n'
manifest += '''[profile.dev]
opt-level = 1
[profile.dev.package."*"]
opt-level = 2
'''
(out / 'Cargo.toml').write_text(manifest)
print(out / 'Cargo.toml')
