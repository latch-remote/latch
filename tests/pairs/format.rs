//! Shared by test binaries only. See README.md for the on-disk contract.
#![allow(dead_code)]

use serde::{Deserialize, Serialize};
use std::path::Path;

#[path = "../../crates/engine/tests/support/ports.rs"]
pub mod ports;

pub const VERSION: u32 = 1;
pub const FRAMES: usize = 24;

// Flip each gate in the fixing PR listed in KNOWN-GAPS.md. Intent stays exact.
pub const HELD_SUPER_GAP: bool = true; // PR #109, native capture fixture required.
pub const RIGHT_SHIFT_GAP: bool = true; // PR #109, native capture fixture required.
pub const CLIPBOARD_CRLF_GAP: bool = true; // Follow-up PR remains unassigned.
pub const MAC_VT_MF_GAP: bool = true; // Follow-up PR remains unassigned.

pub fn input_gap(case: &str, producer: &str) -> bool {
    (RIGHT_SHIFT_GAP && case == "right-shift-identity")
        || (HELD_SUPER_GAP && case == "held-super-z" && !producer.starts_with("macos-"))
}

pub fn platform() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

/// FNV-1a over bytes, independent of Rust's randomized Hash implementation.
pub fn hash(bytes: &[u8]) -> String {
    let value = bytes.iter().fold(0xcbf29ce484222325u64, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x100000001b3)
    });
    format!("{value:016x}")
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Recording {
    pub version: u32,
    pub producer: String,
    pub mode: String,
    pub request: latch_proto::control::StreamRequest,
    pub accept: latch_proto::control::Accept,
    pub events: Vec<Timed>,
    pub pictures: Vec<Picture>,
    pub audio: Vec<Sound>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Timed {
    pub us: u64,
    pub lane: Lane,
    pub bytes: Vec<u8>,
}

#[derive(Serialize, Deserialize, PartialEq, Eq)]
pub enum Lane {
    Control,
    Datagram,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Picture {
    pub frame: u32,
    pub keyframe: bool,
    pub width: u32,
    pub height: u32,
    /// Tight NV12 planes. Visible rows only, excluding codec padding.
    pub pixels: Vec<u8>,
    pub hash: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sound {
    pub seq: u32,
    pub channels: usize,
    pub hash: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InputRecording {
    pub version: u32,
    pub producer: String,
    pub target: String,
    pub cases: Vec<InputCase>,
    pub clipboard: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InputCase {
    pub name: String,
    /// Concatenated production Event::write records.
    pub bytes: Vec<u8>,
    /// The protocol events needed for the intended action.
    pub expected: Vec<u8>,
}

pub fn read<T: serde::de::DeserializeOwned>(path: &Path) -> T {
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

pub fn write(path: &Path, value: &impl Serialize) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, serde_json::to_vec(value).unwrap()).unwrap();
}

pub fn inputs(bytes: &[u8]) -> Vec<latch_proto::input::Event> {
    let mut rest = bytes;
    let mut events = Vec::new();
    while !rest.is_empty() {
        let (event, len) = latch_proto::input::Event::read(rest).expect("invalid recorded input");
        events.push(event);
        rest = &rest[len..];
    }
    events
}

pub fn pcm_hash(pcm: &[i16]) -> String {
    hash(&pcm.iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<_>>())
}

pub fn files(root: &Path, suffix: &str) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(root).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            out.extend(files(&path, suffix));
        } else if path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .ends_with(suffix)
        {
            out.push(path);
        }
    }
    out.sort();
    out
}

pub fn result(file: &str, row: serde_json::Value) {
    println!("PAIR {row}");
    if let Some(dir) = std::env::var_os("LATCH_PAIR_RESULTS") {
        use std::io::Write;
        let dir = std::path::PathBuf::from(dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut output = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join(file))
            .unwrap();
        writeln!(output, "{row}").unwrap();
    }
}

/// Keep running independent cells, retaining each assertion's failure evidence.
pub fn cell(file: &str, mut row: serde_json::Value, check: impl FnOnce()) -> bool {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(check)) {
        Ok(()) => true,
        Err(error) => {
            row["status"] = "failed".into();
            row["error"] = error
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| error.downcast_ref::<&str>().map(|s| (*s).to_string()))
                .unwrap_or_else(|| "non-string panic".into())
                .into();
            result(file, row);
            false
        }
    }
}

/// Choose a complete, non-initial audio group by its actual repair latency.
/// Keeping the fastest group leaves headroom inside the receiver's hold.
pub fn recovery_group(recording: &Recording) -> (u32, u64) {
    use latch_proto::{
        audio::{AUDIO_HOLD, GROUP},
        wire::{parse, Datagram},
    };
    recording
        .events
        .iter()
        .filter_map(|event| {
            let Datagram::Audio { packet, .. } = parse(&event.bytes).ok()? else {
                return None;
            };
            if event.lane != Lane::Datagram || !packet.parity || packet.seq < GROUP {
                return None;
            }
            let mut first = None;
            for seq in packet.seq..packet.seq + GROUP {
                let data = recording.events.iter().find(|data| {
                    data.lane == Lane::Datagram
                        && matches!(parse(&data.bytes),
                    Ok(Datagram::Audio { packet: p, .. }) if !p.parity && p.seq == seq)
                })?;
                first.get_or_insert(data.us);
                if data.us > event.us {
                    return None;
                }
            }
            Some((packet.seq, event.us - first?))
        })
        .filter(|(_, delay)| *delay < AUDIO_HOLD.as_micros() as u64)
        .min_by_key(|(_, delay)| *delay)
        .expect("tape needs a complete audio repair group arriving within AUDIO_HOLD (20 ms)")
}
