//! Desktop sizing and what this host's encoders can do.

use latch_core::api::CodecCaps;
use latch_engine::software::Ceiling;
use latch_engine::video::{Codec, Limits};
use latch_proto::control::HostDisplay;
use parking_lot::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// How long a look at the displays stands before the next status looks again.
const DISPLAYS_FOR: Duration = Duration::from_secs(3);

#[derive(Default)]
struct Displays {
    seen: Option<(Instant, Vec<HostDisplay>)>,
    held: usize,
}

impl Displays {
    fn list(
        &mut self,
        streaming: bool,
        now: Instant,
        read: impl FnOnce() -> anyhow::Result<Vec<HostDisplay>>,
    ) -> Vec<HostDisplay> {
        if let Some((at, list)) = &self.seen {
            if self.held > 0 || streaming || now.duration_since(*at) < DISPLAYS_FOR {
                return list.clone();
            }
        }
        match read() {
            Ok(list) => {
                self.seen = Some((now, list.clone()));
                list
            }
            Err(error) => {
                tracing::debug!("could not list the displays: {error:#}");
                self.seen
                    .as_ref()
                    .map(|(_, list)| list.clone())
                    .unwrap_or_default()
            }
        }
    }

    fn release(&mut self) {
        self.held -= 1;
        if self.held == 0 {
            // The topology has been restored. Refresh at the next poll.
            if let Some((at, _)) = &mut self.seen {
                *at = Instant::now() - DISPLAYS_FOR;
            }
        }
    }
}

static DISPLAYS: Mutex<Displays> = Mutex::new(Displays {
    seen: None,
    held: 0,
});

/// Keep Windows' pre-session list through preparation, capture and restoration.
/// Polls and preparation use the same lock so a late enumeration cannot overwrite it.
pub fn hold_displays() -> Option<HeldDisplays> {
    if !cfg!(windows) {
        return None;
    }
    let mut displays = DISPLAYS.lock();
    displays.list(false, Instant::now(), latch_engine::display::list);
    displays.held += 1;
    Some(HeldDisplays)
}

pub struct HeldDisplays;
impl Drop for HeldDisplays {
    fn drop(&mut self) {
        DISPLAYS.lock().release();
    }
}

/// The displays a viewer can choose from, preserving Windows' topology
/// while preparation temporarily switches physical outputs off.
pub fn displays(streaming: bool) -> Vec<HostDisplay> {
    DISPLAYS.lock().list(
        cfg!(windows) && streaming,
        Instant::now(),
        latch_engine::display::list,
    )
}

#[cfg(target_os = "linux")]
pub fn capture_size() -> Option<[u32; 2]> {
    if display_follows_viewer() {
        return None;
    }
    latch_engine::linux::capture_size().ok()
}

#[cfg(not(target_os = "linux"))]
pub fn capture_size() -> Option<[u32; 2]> {
    None
}

/// The desktop resizes itself to each viewer (`LATCH_DISPLAY_FOLLOWS_VIEWER=1`):
/// the engine's prep command sets the size a viewer asks for, and its undo
/// puts the idle size back when the session ends.
pub fn display_follows_viewer() -> bool {
    std::env::var_os("LATCH_DISPLAY_FOLLOWS_VIEWER").is_some_and(|v| v == "1")
}

/// The likely encoder before the first session opens its pipeline.
pub fn encoder_description() -> &'static str {
    if cfg!(target_os = "macos") {
        "VideoToolbox HEVC / H.264"
    } else if cfg!(windows) {
        "Media Foundation H.264, with Latch software H.264 fallback"
    } else {
        "Latch software H.264"
    }
}

/// What this host's encoders take, as measured: the software path's frame
/// rate when it is the one in use (at the capture size, or 1920 × 1080),
/// and each codec's limits. Looked at again on a thread when the encoder
/// or the capture size changes, never while `streaming`. Until the first
/// look ends the software path is held to [`Ceiling::PROVISIONAL`] and
/// codecs say nothing (the engine still fits every request to them).
/// What is reported here is also what the engine holds a session to, so a
/// viewer that connects without asking, or before the look ends, gets no
/// more than one that waited.
pub fn capacity(encoder: &str, streaming: bool) -> (u32, Vec<CodecCaps>) {
    static LOOKING: AtomicBool = AtomicBool::new(false);
    // A Linux host always has the software path: Constrained Baseline,
    // and H.264 at a size its hardware does not take.
    let software = is_software(encoder) || cfg!(target_os = "linux");
    let size = capture_size().unwrap_or([1920, 1080]);
    let measured = MEASURED.lock().clone();
    let stale = measured
        .as_ref()
        .is_none_or(|m| m.size != size || m.software != software);
    if stale && !streaming && !LOOKING.swap(true, Ordering::AcqRel) {
        let spawned = std::thread::Builder::new()
            .name("latch-capacity".into())
            .spawn(move || {
                let encodable = latch_engine::video::encodable();
                let split = encodable
                    .iter()
                    .any(|&c| latch_engine::video::hardware_encodes(c));
                let ceiling = if software {
                    // The probe uses the session's shared worker policy and
                    // emits this output geometry, even on a larger desktop.
                    let fps = latch_engine::software::measure_fps(size[0], size[1], MEASURE_FOR);
                    Some(Ceiling {
                        width: size[0],
                        height: size[1],
                        fps,
                    })
                    .filter(|c| c.fps > 0)
                } else {
                    None
                };
                // Where some codecs are hardware's (a Linux host with NVENC
                // or VA-API) the software path's rate is its codecs' own,
                // not a frame rate for every stream.
                let codecs: Vec<CodecCaps> = encodable
                    .into_iter()
                    .map(|codec| {
                        let mut limits = latch_engine::video::limits(codec);
                        if let Some(c) = ceiling
                            .filter(|_| split && !latch_engine::video::hardware_encodes(codec))
                        {
                            let rate = u64::from(c.fps) * u64::from(c.width) * u64::from(c.height);
                            limits.max_pixel_rate = limits.max_pixel_rate.min(rate);
                        }
                        caps(codec, limits)
                    })
                    .collect();
                tracing::info!(
                    "encoder capacity: {ceiling:?} in software (None: not measured), {codecs:?}"
                );
                if software {
                    latch_engine::software::set_ceiling(Some(
                        ceiling.unwrap_or(Ceiling::PROVISIONAL),
                    ));
                }
                *MEASURED.lock() = Some(Measured {
                    size,
                    software,
                    split,
                    ceiling,
                    codecs,
                });
                LOOKING.store(false, Ordering::Release);
            });
        if spawned.is_err() {
            LOOKING.store(false, Ordering::Release);
        }
    }
    // The latest measurement still holds at another size: it is pixels a
    // second. One from the hardware encoder's time says nothing about
    // software, and the reverse.
    let ceiling = software.then(|| {
        measured
            .as_ref()
            .filter(|m| m.software)
            .and_then(|m| m.ceiling)
            .unwrap_or(Ceiling::PROVISIONAL)
    });
    latch_engine::software::set_ceiling(ceiling);
    let split = measured.as_ref().is_some_and(|m| m.split);
    let max_fps = ceiling
        .filter(|_| !split)
        .map_or(0, |c| c.fps_at(size[0], size[1]).max(1));
    (max_fps, measured.map(|m| m.codecs).unwrap_or_default())
}

/// The latest look at the encoders.
static MEASURED: Mutex<Option<Measured>> = Mutex::new(None);

/// How long the software path is timed for.
const MEASURE_FOR: Duration = Duration::from_millis(1500);

#[derive(Debug, Clone)]
struct Measured {
    size: [u32; 2],
    software: bool,
    /// Some codecs are encoded in hardware: the software ceiling is in
    /// the others' limits, and no frame rate holds every stream.
    split: bool,
    ceiling: Option<Ceiling>,
    codecs: Vec<CodecCaps>,
}

/// The engine's software H.264, as the running or last session named it
/// or as [`encoder_description`] expects it.
fn is_software(encoder: &str) -> bool {
    encoder.starts_with("Latch software")
}

fn caps(codec: Codec, limits: Limits) -> CodecCaps {
    CodecCaps {
        codec: serde_json::to_value(codec)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_default(),
        max_side: limits.max_side,
        max_pixels: limits.max_pixels,
        max_pixel_rate: limits.max_pixel_rate,
        max_kbps: limits.max_kbps,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preparation_keeps_physical_displays_through_expired_status_polls() {
        let now = Instant::now();
        let physical = vec![HostDisplay {
            id: "physical".into(),
            ..Default::default()
        }];
        let virtual_only = vec![HostDisplay {
            id: "virtual".into(),
            ..Default::default()
        }];
        let mut cache = Displays::default();
        assert_eq!(cache.list(false, now, || Ok(physical.clone())), physical);
        cache.held += 1;
        let later = now + DISPLAYS_FOR * 2;
        assert_eq!(
            cache.list(false, later, || Ok(virtual_only.clone())),
            physical
        );
        assert_eq!(
            cache.list(true, later, || Ok(virtual_only.clone())),
            physical
        );
        cache.release();
        assert_eq!(
            cache.list(false, later, || Ok(virtual_only.clone())),
            virtual_only
        );
    }

    #[test]
    fn only_the_software_encoder_is_measured() {
        assert!(is_software("Latch software H.264"));
        assert!(is_software("Latch software H.264 (X11)"));
        assert!(!is_software(
            "Media Foundation H.264, with Latch software H.264 fallback"
        ));
        assert!(!is_software("VideoToolbox HEVC"));
    }

    /// Before anything is measured a software host reports, and the engine
    /// holds a session to, the provisional ceiling; a hardware one neither.
    #[test]
    fn software_is_held_to_a_provisional_ceiling_until_measured() {
        // As before the first look (another test may have looked).
        *MEASURED.lock() = None;
        let size = capture_size().unwrap_or([1920, 1080]);
        // `streaming` keeps the measurement from starting.
        let (fps, _) = capacity("Latch software H.264", true);
        assert_eq!(fps, Ceiling::PROVISIONAL.fps_at(size[0], size[1]).max(1));
        assert_eq!(
            latch_engine::software::ceiling(),
            Some(Ceiling::PROVISIONAL)
        );
        let (fps, _) = capacity("VideoToolbox HEVC", true);
        if cfg!(target_os = "linux") {
            // A Linux host's software path is always there to hold.
            assert_eq!(fps, Ceiling::PROVISIONAL.fps_at(size[0], size[1]).max(1));
            assert_eq!(
                latch_engine::software::ceiling(),
                Some(Ceiling::PROVISIONAL)
            );
        } else {
            assert_eq!(fps, 0);
            assert_eq!(latch_engine::software::ceiling(), None);
        }
    }

    #[test]
    fn codec_caps_carry_the_wire_name() {
        let c = caps(Codec::H264High, Limits::level(Codec::H264High));
        assert_eq!(c.codec, "h264-high");
        assert_eq!(c.max_kbps, 1_000_000);
        assert_eq!(caps(Codec::Av1, Limits::level(Codec::Av1)).codec, "av1");
    }
}
