//! The CPU path: any source of BGRA pictures, converted on the CPU and
//! encoded by `latch-codec` or by a hardware encoder that takes pictures
//! from memory (VA-API, NVENC on Linux). Linux always takes it; Windows
//! falls back to it.

use crate::input::DisplayRect;
use crate::video::{Codec, EncodedVideo, Pacer, VideoConfig, VideoInfo, VideoPipeline};
use anyhow::ensure;
pub use latch_codec::color::Rect;
use latch_codec::h264::{Encoder, EncoderConfig};
use latch_codec::Yuv420;
use std::time::{Duration, Instant};

/// A BGRA picture: `stride` bytes per row.
#[derive(Debug, Clone, Default)]
pub struct Bgra {
    pub width: u32,
    pub height: u32,
    pub stride: usize,
    pub data: Vec<u8>,
}

/// A screen the CPU can read.
pub trait CpuCapture: Send {
    /// The screen's size now.
    fn size(&self) -> (u32, u32);

    /// Waits up to `timeout` for the screen to change (a source that cannot
    /// tell waits one frame interval), then copies it into `frame` with the
    /// cursor drawn in. Returns false when it did not change.
    fn grab(&mut self, frame: &mut Bgra, timeout: Duration) -> anyhow::Result<bool>;

    fn display(&self) -> DisplayRect;

    /// Where the picture the last `grab` that returned true differs from
    /// the one the source returned before it; `None` when it cannot tell.
    /// Lets the pipeline convert only what changed.
    fn damage(&self) -> Option<Vec<Rect>> {
        None
    }

    /// For [`VideoInfo::encoder`]'s sake: "X11", "test pattern".
    fn name(&self) -> String;

    fn set_cursor_drawn(&mut self, _drawn: bool) -> bool {
        false
    }

    fn take_cursor(&mut self) -> Option<crate::video::CursorUpdate> {
        None
    }
}

/// Time spent in each stage of the last frame. Capture includes waiting
/// for damage; frame pacing happens outside these measurements.
#[derive(Debug, Clone, Copy, Default)]
pub struct SoftwareTimings {
    pub capture: Duration,
    pub convert: Duration,
    pub encode: Duration,
}

/// Turns converted pictures into access units.
pub trait PictureEncoder: Send {
    /// For people: "Latch software H.264", "NVENC HEVC (NVIDIA GeForce …)".
    fn name(&self) -> String;
    fn hardware(&self) -> bool;
    fn codec(&self) -> Codec;
    /// Appends one Annex B access unit for `picture` (parameter sets in
    /// band before every keyframe) to `out`. Returns whether it is a
    /// keyframe, which it is when asked.
    fn encode(
        &mut self,
        picture: &Yuv420,
        keyframe: bool,
        out: &mut Vec<u8>,
    ) -> anyhow::Result<bool>;
    /// [`PictureEncoder::encode`], told where `picture` may differ from
    /// the last picture given (pixel rectangles; `None`: anywhere).
    fn encode_changed(
        &mut self,
        picture: &Yuv420,
        changed: Option<&[Rect]>,
        keyframe: bool,
        out: &mut Vec<u8>,
    ) -> anyhow::Result<bool> {
        let _ = changed;
        self.encode(picture, keyframe, out)
    }
    /// From the next frame.
    fn set_bitrate(&mut self, kbps: u32);
    /// See [`VideoPipeline::recovers`].
    fn recovers(&self) -> bool {
        false
    }
    /// See [`VideoPipeline::recover_from`]: `picture` is a number
    /// [`PictureEncoder::last_picture`] gave.
    fn recover_from(&mut self, picture: u64) -> bool {
        let _ = picture;
        false
    }
    /// The last frame's number (0 where the encoder does not recover) and
    /// the picture it predicts from when it is a recovery frame.
    fn last_picture(&self) -> (u64, Option<u64>) {
        (0, None)
    }
}

/// Opens an encoder for pictures of `width × height` and `config`'s rate.
pub type OpenEncoder =
    Box<dyn FnMut(u32, u32, &VideoConfig) -> anyhow::Result<Box<dyn PictureEncoder>> + Send>;

/// Conservative worker limits by output pixel count. Raise a tier only after
/// the quiet-machine sweep in docs/measurements/2026-11-09-linux-output-size.
const WORKER_LIMITS: [(u64, usize); 3] = [(1920 * 1080, 4), (2560 * 1440, 4), (u64::MAX, 4)];

/// Shared by conversion, encoding and the capacity probe. The default table
/// preserves the four-worker policy; small pictures cannot have more slices
/// than macroblock rows. Larger future limits must leave a CPU for capture,
/// audio and sending. Two-core machines retain their two workers.
pub fn worker_count(available: usize, width: u32, height: u32) -> usize {
    let pixels = u64::from(width) * u64::from(height);
    let cap = WORKER_LIMITS
        .iter()
        .find(|(limit, _)| pixels <= *limit)
        .map_or(4, |(_, cap)| *cap);
    let budget = if available > 4 {
        available - 1
    } else {
        available.max(1)
    };
    let rows = (height as usize).div_ceil(16).max(1);
    // Future limits above four retain at least 128 pixels per slice;
    // the existing small-picture policy stays unchanged.
    cap.min(budget).min((rows / 8).max(4)).min(rows)
}

/// Rust accounts for affinity and Linux cgroup CPU quotas. On Apple Silicon,
/// prefer the performance-core count rather than treating efficiency cores
/// as interchangeable encoder workers. Never exceed available parallelism.
fn available_workers() -> usize {
    let available = std::thread::available_parallelism().map_or(1, usize::from);
    #[cfg(target_os = "macos")]
    {
        unsafe extern "C" {
            fn sysctlbyname(
                name: *const std::ffi::c_char,
                old: *mut std::ffi::c_void,
                old_len: *mut usize,
                new: *mut std::ffi::c_void,
                new_len: usize,
            ) -> i32;
        }
        let mut cores = 0u32;
        let mut length = std::mem::size_of_val(&cores);
        // SAFETY: the key is NUL-terminated and the output points to `length`
        // writable bytes; there is no new value, so this is read-only.
        let result = unsafe {
            sysctlbyname(
                c"hw.perflevel0.physicalcpu".as_ptr(),
                (&mut cores as *mut u32).cast(),
                &mut length,
                std::ptr::null_mut(),
                0,
            )
        };
        if result == 0 && cores > 0 {
            return available.min(cores as usize);
        }
    }
    available
}

/// `latch-codec`'s H.264 Constrained Baseline encoder.
pub struct LatchEncoder(Encoder, (u64, Option<u64>));

impl LatchEncoder {
    pub fn open(width: u32, height: u32, config: &VideoConfig) -> anyhow::Result<Self> {
        Self::with_workers(
            width,
            height,
            config,
            worker_count(available_workers(), width, height),
        )
    }

    pub fn with_workers(
        width: u32,
        height: u32,
        config: &VideoConfig,
        workers: usize,
    ) -> anyhow::Result<Self> {
        let mut encoder = Encoder::new(EncoderConfig {
            width,
            height,
            fps: config.fps,
            bitrate_kbps: config.bitrate_kbps,
            threads: workers,
        })?;
        encoder.set_recovery_depth(recovery_depth(width, height, config.fps));
        Ok(Self(encoder, (0, None)))
    }

    fn keep(&mut self, info: latch_codec::h264::EncodedInfo) -> bool {
        self.1 = (info.picture, info.recovery);
        info.keyframe
    }
}

/// Pictures the software encoder keeps for recovery frames: what was sent
/// in the last 300 ms (a loss is noticed within about two frames and the
/// request takes half a round trip), at least 4, and no more than 256 MB
/// of pictures (each kept twice: reconstruction and source).
pub fn recovery_depth(width: u32, height: u32, fps: u32) -> usize {
    let picture = (width as usize).div_ceil(16) * (height as usize).div_ceil(16) * 256 * 3 / 2;
    let by_memory = (256 << 20) / (2 * picture.max(1));
    (fps as usize * 3 / 10).clamp(4, 32).min(by_memory.max(2))
}

impl PictureEncoder for LatchEncoder {
    fn name(&self) -> String {
        "Latch software H.264".into()
    }
    fn hardware(&self) -> bool {
        false
    }
    fn codec(&self) -> Codec {
        Codec::H264Baseline
    }
    fn encode(
        &mut self,
        picture: &Yuv420,
        keyframe: bool,
        out: &mut Vec<u8>,
    ) -> anyhow::Result<bool> {
        let info = self.0.encode(picture, keyframe, out)?;
        Ok(self.keep(info))
    }
    fn encode_changed(
        &mut self,
        picture: &Yuv420,
        changed: Option<&[Rect]>,
        keyframe: bool,
        out: &mut Vec<u8>,
    ) -> anyhow::Result<bool> {
        let info = self.0.encode_changed(picture, changed, keyframe, out)?;
        Ok(self.keep(info))
    }
    fn set_bitrate(&mut self, kbps: u32) {
        self.0.set_bitrate(kbps);
    }
    fn recovers(&self) -> bool {
        true
    }
    fn recover_from(&mut self, picture: u64) -> bool {
        self.0.recover_from(picture)
    }
    fn last_picture(&self) -> (u64, Option<u64>) {
        self.1
    }
}

/// [`CpuCapture`] → [`latch_codec::color`] → a [`PictureEncoder`]
/// (`latch-codec` unless [`SoftwarePipeline::with_encoder`] says otherwise).
/// Aspect-fits the source into the negotiated size; repeats the last picture as
/// an all-skip frame when idle; re-creates the encoder if the source's size
/// changes.
pub struct SoftwarePipeline<C: CpuCapture> {
    capture: C,
    config: VideoConfig,
    frame: Bgra,
    picture: Yuv420,
    encoder: Option<Box<dyn PictureEncoder>>,
    /// The size `encoder` was opened for.
    encoder_size: (u32, u32),
    /// Learned encodable pixel budget and the bitrate at which it was learned.
    bitrate_size: Option<(u64, u32)>,
    source_size: (u32, u32),
    scaler: Option<latch_codec::color::scale::Scaler>,
    cursor: Option<crate::video::CursorUpdate>,
    cursor_dirty: bool,
    cursor_serial: u32,
    open: OpenEncoder,
    /// The encoder's name, whether it is hardware, its codec, and whether
    /// it makes recovery frames.
    describe: (String, bool, Codec, bool),
    pacer: Pacer,
    last_sent: Option<Instant>,
    captured: Instant,
    timings: SoftwareTimings,
    threads: usize,
    /// A recovery frame is owed: make the next frame without waiting
    /// for the screen to change.
    recovering: bool,
    /// Picture numbers go on across encoders: the present encoder's
    /// numbers start after this, the last frame's number.
    picture_base: u64,
    last_picture: u64,
}

impl<C: CpuCapture> SoftwarePipeline<C> {
    pub fn new(capture: C, config: &VideoConfig) -> anyhow::Result<Self> {
        ensure!(
            config.fps > 0 && config.fps <= 1000,
            "fps must be in 1..=1000"
        );
        ensure!(config.bitrate_kbps > 0, "bitrate must be positive");
        let (width, height) = capture.size();
        validate_capture_size(width, height)?;
        validate_size(config.width, config.height)?;
        Ok(Self {
            capture,
            config: config.clone(),
            frame: Bgra::default(),
            picture: Yuv420::default(),
            encoder: None,
            encoder_size: (0, 0),
            bitrate_size: None,
            source_size: (0, 0),
            scaler: None,
            cursor: None,
            cursor_dirty: false,
            cursor_serial: 0,
            open: Box::new(|width, height, config| {
                Ok(Box::new(LatchEncoder::open(width, height, config)?))
            }),
            describe: (
                "Latch software H.264".into(),
                false,
                Codec::H264Baseline,
                true,
            ),
            pacer: Pacer::new(config.fps),
            last_sent: None,
            captured: Instant::now(),
            timings: SoftwareTimings::default(),
            threads: worker_count(available_workers(), config.width, config.height),
            recovering: false,
            picture_base: 0,
            last_picture: 0,
        })
    }

    /// Encodes with what `open` opens instead: opened here, for the
    /// capture's present size, so that a failure shows before streaming.
    pub fn with_encoder(
        capture: C,
        config: &VideoConfig,
        mut open: OpenEncoder,
    ) -> anyhow::Result<Self> {
        let (width, height) = (config.width, config.height);
        let encoder = open(width, height, config)?;
        let mut this = Self::new(capture, config)?;
        this.describe = (
            encoder.name(),
            encoder.hardware(),
            encoder.codec(),
            encoder.recovers(),
        );
        this.encoder = Some(encoder);
        this.encoder_size = (width, height);
        this.open = open;
        Ok(this)
    }

    /// Explicit worker counts for repeatable software benchmarks. Session
    /// pipelines use the machine policy in `new` and `LatchEncoder::open`.
    pub fn with_workers(capture: C, config: &VideoConfig, workers: usize) -> anyhow::Result<Self> {
        ensure!((1..=64).contains(&workers), "workers must be in 1..=64");
        let mut pipeline = Self::new(capture, config)?;
        pipeline.open =
            Box::new(move |w, h, c| Ok(Box::new(LatchEncoder::with_workers(w, h, c, workers)?)));
        pipeline.threads = workers;
        Ok(pipeline)
    }

    /// Continue a pipeline's picture identities when replacing another backend.
    #[cfg(windows)]
    pub(crate) fn continue_pictures_after(&mut self, picture: u64) {
        self.picture_base = picture;
        self.last_picture = picture;
    }

    pub fn timings(&self) -> SoftwareTimings {
        self.timings
    }

    fn output_size(&self) -> (u32, u32) {
        let requested = (self.config.width, self.config.height);
        let Some((pixels, kbps)) = self.bitrate_size else {
            return requested;
        };
        let budget = pixels * u64::from(self.config.bitrate_kbps) / u64::from(kbps);
        let full = u64::from(requested.0) * u64::from(requested.1);
        if budget >= full {
            return requested;
        }
        let active = u64::from(self.encoder_size.0) * u64::from(self.encoder_size.1);
        // Grow by substantial steps so small congestion probes do not keep reopening the encoder.
        if budget > active && budget < active * 2 {
            return self.encoder_size;
        }
        let scale = (budget as f64 / full as f64).sqrt();
        let even = |side: u32| ((f64::from(side) * scale) as u32 / 2 * 2).max(side.min(64));
        (even(requested.0), even(requested.1))
    }

    fn acquire(&mut self, keyframe: bool, idle: Duration) -> anyhow::Result<bool> {
        let interval = self.pacer.interval();
        self.pacer.wait();
        let deadline = self.last_sent.unwrap_or_else(Instant::now) + idle.max(interval);
        loop {
            let started = Instant::now();
            let changed = self.capture.grab(&mut self.frame, Duration::ZERO)?;
            self.timings.capture = started.elapsed();
            if changed {
                validate_frame(&self.frame)?;
                self.captured = Instant::now();
                self.pacer.taken(started);
                return Ok(true);
            }
            let now = Instant::now();
            if keyframe || now >= deadline {
                ensure!(
                    !self.frame.data.is_empty(),
                    "capture supplied no initial picture"
                );
                self.pacer.taken(now);
                return Ok(false);
            }
            // Polling also lets a source without damage notifications honour the deadline.
            let wait = interval.min(deadline.saturating_duration_since(now));
            let started = Instant::now();
            if self.capture.grab(&mut self.frame, wait)? {
                self.timings.capture = started.elapsed();
                validate_frame(&self.frame)?;
                self.captured = Instant::now();
                self.pacer.taken(self.captured);
                return Ok(true);
            }
            std::thread::sleep(wait.saturating_sub(started.elapsed()));
        }
    }
}

/// Frames a second this machine's CPU takes through the software path at
/// `width` × `height`: the test pattern's moving bar drawn, converted and
/// encoded as a session would, at no frame-rate limit, for about
/// `budget`. A host that encodes in software reports this as its ceiling,
/// so a viewer asks for what it keeps up with rather than what a core
/// count suggests. 0 when the pipeline cannot be opened.
pub fn measure_fps(width: u32, height: u32, budget: Duration) -> u32 {
    let fps = latch_proto::control::MAX_FPS;
    let pattern = crate::pattern::Pattern::new(width, height, fps, crate::pattern::Motion::Sweep);
    let config = VideoConfig {
        color: Default::default(),
        width,
        height,
        fps,
        bitrate_kbps: 20_000,
        codec: Codec::H264Baseline,
        display: None,
    };
    let Ok(mut pipeline) = SoftwarePipeline::new(pattern, &config) else {
        return 0;
    };
    // The first frame is an IDR and sets up the encoder: not counted.
    if pipeline.next(true, Duration::ZERO).is_err() {
        return 0;
    }
    let (start, mut frames) = (Instant::now(), 0u32);
    while start.elapsed() < budget {
        if pipeline.next(false, Duration::ZERO).is_err() {
            return 0;
        }
        frames += 1;
    }
    (f64::from(frames) / start.elapsed().as_secs_f64()) as u32
}

/// What the software path keeps up with: `fps` frames a second at
/// `width` × `height`, as [`measure_fps`] found it. Encoding time follows
/// the pixel count, so it holds any picture to the same pixels a second.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ceiling {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

impl Ceiling {
    /// Until the host has measured: 30 fps at 1080p, what 5.1 held a
    /// small CPU to. A first connection must not ask a slow CPU for its
    /// display's refresh rate at 4K while the measurement runs.
    pub const PROVISIONAL: Ceiling = Ceiling {
        width: 1920,
        height: 1080,
        fps: 30,
    };

    fn pixel_rate(&self) -> u64 {
        u64::from(self.fps) * u64::from(self.width) * u64::from(self.height)
    }

    /// Frames a second at `width` × `height`.
    pub fn fps_at(&self, width: u32, height: u32) -> u32 {
        let pixels = (u64::from(width) * u64::from(height)).max(1);
        u32::try_from(self.pixel_rate() / pixels).unwrap_or(u32::MAX)
    }

    /// `width` × `height` at `fps` held to this: the frame rate comes down
    /// first, to no less than 30 (or what was asked, if less), then the
    /// picture shrinks in proportion. Sides stay even.
    pub fn fit(&self, width: u32, height: u32, fps: u32) -> (u32, u32, u32) {
        let at = self.fps_at(width, height);
        if fps <= at {
            return (width, height, fps);
        }
        let least = fps.min(SMOOTH);
        if at >= least {
            return (width, height, at);
        }
        let pixels = (u64::from(width) * u64::from(height)).max(1);
        let scale = ((self.pixel_rate() / u64::from(least.max(1))) as f64 / pixels as f64).sqrt();
        let even = |v: u32| ((f64::from(v) * scale) as u32 / 2 * 2).max(64);
        (even(width), even(height), least)
    }
}

/// The least frame rate [`Ceiling::fit`] trades picture size for.
const SMOOTH: u32 = 30;

static CEILING: parking_lot::Mutex<Option<Ceiling>> = parking_lot::Mutex::new(None);

/// What a session is held to while this host encodes in software; None
/// when it does not, and nothing is held. The host sets it (see
/// `set_ceiling`) from what it measured, so a request it did not fit
/// itself cannot ask for more.
pub fn ceiling() -> Option<Ceiling> {
    *CEILING.lock()
}

pub fn set_ceiling(ceiling: Option<Ceiling>) {
    *CEILING.lock() = ceiling.filter(|c| c.fps > 0 && c.width > 0 && c.height > 0);
}

fn validate_capture_size(width: u32, height: u32) -> anyhow::Result<()> {
    ensure!(
        width > 0 && height > 0 && width <= 16384 && height <= 16384,
        "capture dimensions must be in 1..=16384"
    );
    Ok(())
}

fn validate_size(width: u32, height: u32) -> anyhow::Result<()> {
    validate_capture_size(width, height)?;
    ensure!(
        width.is_multiple_of(2) && height.is_multiple_of(2),
        "software H.264 requires even dimensions"
    );
    Ok(())
}

fn validate_frame(frame: &Bgra) -> anyhow::Result<()> {
    validate_capture_size(frame.width, frame.height)?;
    ensure!(
        frame.stride >= frame.width as usize * 4,
        "short BGRA stride"
    );
    let needed = frame
        .stride
        .checked_mul(frame.height as usize)
        .ok_or_else(|| anyhow::anyhow!("BGRA size overflow"))?;
    ensure!(frame.data.len() >= needed, "short BGRA picture");
    Ok(())
}

impl<C: CpuCapture> VideoPipeline for SoftwarePipeline<C> {
    fn info(&self) -> VideoInfo {
        let (width, height) = if self.encoder_size == (0, 0) {
            self.output_size()
        } else {
            self.encoder_size
        };
        let (name, hardware, codec, _) = &self.describe;
        VideoInfo {
            color: Default::default(),
            encoder: format!("{name} ({})", self.capture.name()),
            hardware: *hardware,
            codec: *codec,
            width,
            height,
            display: self.capture.display(),
        }
    }

    fn next(&mut self, keyframe: bool, idle: Duration) -> anyhow::Result<EncodedVideo> {
        let changed = self.acquire(keyframe || self.recovering, idle)?;
        self.recovering = false;
        self.timings.convert = Duration::ZERO;
        self.timings.encode = Duration::ZERO;
        loop {
            let size = self.output_size();
            let source = (self.frame.width, self.frame.height);
            let resized =
                (self.picture.width, self.picture.height) != size || self.source_size != source;
            if self.encoder.is_none()
                || self.encoder_size != size
                || (self.source_size != (0, 0) && self.source_size != source)
            {
                self.encoder = None;
                // A new encoder holds none of the old one's pictures.
                self.picture_base = self.last_picture;
                let encoder = (self.open)(size.0, size.1, &self.config)?;
                // At another size the encoder may be another (software where
                // hardware does not take it).
                self.describe = (
                    encoder.name(),
                    encoder.hardware(),
                    encoder.codec(),
                    encoder.recovers(),
                );
                self.encoder = Some(encoder);
                self.encoder_size = size;
            }
            if resized {
                self.picture = Yuv420::new(size.0, size.1);
                self.source_size = source;
                self.cursor_dirty = true;
                self.scaler = if source == size {
                    None
                } else {
                    let fit = DisplayRect {
                        width: source.0,
                        height: source.1,
                        ..Default::default()
                    }
                    .fitted(size.0, size.1);
                    Some(latch_codec::color::scale::Scaler::new(
                        source,
                        size,
                        Rect {
                            x: fit.x as u32,
                            y: fit.y as u32,
                            width: fit.width,
                            height: fit.height,
                        },
                    ))
                };
            }
            let started = Instant::now();
            // What differs from the picture encoded last: a repeat changes
            // nothing.
            let mut damage = Some(Vec::new());
            if changed || resized {
                let whole = [Rect {
                    x: 0,
                    y: 0,
                    width: self.frame.width,
                    height: self.frame.height,
                }];
                damage = if resized { None } else { self.capture.damage() };
                if let Some(scaler) = &self.scaler {
                    damage = damage.map(|rects| scaler.damage(&rects));
                    let whole = [Rect {
                        x: 0,
                        y: 0,
                        width: size.0,
                        height: size.1,
                    }];
                    scaler.convert(
                        &self.frame.data,
                        self.frame.stride,
                        damage.as_deref().unwrap_or(&whole),
                        self.threads,
                        &mut self.picture,
                    );
                } else {
                    latch_codec::color::bgra_to_yuv420_rects(
                        &self.frame.data,
                        self.frame.width,
                        self.frame.height,
                        self.frame.stride,
                        damage.as_deref().unwrap_or(&whole),
                        self.threads,
                        &mut self.picture,
                    );
                }
            }
            self.timings.convert += started.elapsed();
            // A repeat shows the old picture, but it was taken up now: what the
            // viewer learns about the host's delay is about this frame.
            if !changed && !resized {
                self.captured = started;
            }
            let mut data = Vec::new();
            let started = Instant::now();
            let encoded = self
                .encoder
                .as_mut()
                .ok_or_else(|| anyhow::anyhow!("encoder unavailable"))?
                .encode_changed(
                    &self.picture,
                    damage.as_deref(),
                    keyframe || resized,
                    &mut data,
                );
            self.timings.encode += started.elapsed();
            let sent_keyframe = match encoded {
                Ok(keyframe) => keyframe,
                Err(error)
                    if error.downcast_ref::<latch_codec::h264::CodecError>()
                        == Some(&latch_codec::h264::CodecError::FrameTooLarge)
                        && (size.0 > self.config.width.min(64)
                            || size.1 > self.config.height.min(64)) =>
                {
                    self.bitrate_size = Some((
                        u64::from(size.0) * u64::from(size.1) / 4,
                        self.config.bitrate_kbps,
                    ));
                    if self.output_size() == size {
                        return Err(error);
                    }
                    continue;
                }
                Err(error) => return Err(error),
            };
            self.last_sent = Some(self.captured);
            let (picture, recovery) = self
                .encoder
                .as_ref()
                .map_or((0, None), |e| e.last_picture());
            let base = self.picture_base;
            let picture = if picture > 0 { base + picture } else { 0 };
            self.last_picture = self.last_picture.max(picture);
            return Ok(EncodedVideo {
                generation: 0,
                data,
                keyframe: sent_keyframe,
                captured: self.captured,
                picture,
                recovery: recovery
                    .filter(|_| !sent_keyframe)
                    .map(|local| base + local),
            });
        }
    }

    fn recovers(&self) -> bool {
        self.describe.3
    }

    fn recover_from(&mut self, picture: u64) -> bool {
        let done = picture > self.picture_base
            && self
                .encoder
                .as_mut()
                .is_some_and(|e| e.recover_from(picture - self.picture_base));
        self.recovering |= done;
        done
    }

    fn set_bitrate(&mut self, kbps: u32) {
        self.config.bitrate_kbps = kbps.max(1);
        if let Some(encoder) = &mut self.encoder {
            encoder.set_bitrate(self.config.bitrate_kbps);
        }
    }

    fn set_cursor_drawn(&mut self, drawn: bool) -> bool {
        self.capture.set_cursor_drawn(drawn)
    }

    fn take_cursor(&mut self) -> Option<crate::video::CursorUpdate> {
        let update = self.capture.take_cursor();
        if let Some(mut update) = update {
            if update.shape.is_none() {
                update.shape = self.cursor.as_ref().and_then(|c| c.shape.clone());
            }
            self.cursor = Some(update);
            self.cursor_dirty = true;
        }
        if !std::mem::take(&mut self.cursor_dirty) {
            return None;
        }
        let mut update = self.cursor.clone()?;
        if let Some(shape) = update.shape.as_mut() {
            let source = self.capture.size();
            let size = if self.encoder_size == (0, 0) {
                (self.config.width, self.config.height)
            } else {
                self.encoder_size
            };
            let fit = DisplayRect {
                width: source.0,
                height: source.1,
                ..Default::default()
            }
            .fitted(size.0, size.1);
            if (fit.width, fit.height) != source {
                scale_cursor(shape, source, (fit.width, fit.height));
            }
            // The viewer caches textures by serial; a source resize changes
            // the scaled shape even when XFixes' original serial is unchanged.
            self.cursor_serial = self.cursor_serial.wrapping_add(1);
            shape.serial = self.cursor_serial;
        }
        Some(update)
    }
}

/// The separate plane's shape and hotspot are in output pixels. Position is
/// the viewer's local pointer, whose events map through DisplayRect::map;
/// no absolute X11 cursor position is sent by this protocol.
fn scale_cursor(
    shape: &mut latch_proto::control::CursorShape,
    source: (u32, u32),
    content: (u32, u32),
) {
    let Ok(rgba) = shape.decode_rgba() else {
        return;
    };
    if shape.width == 0
        || shape.height == 0
        || rgba.len() != (shape.width * shape.height * 4) as usize
    {
        return;
    }
    let mut w = (u64::from(shape.width) * u64::from(content.0) / u64::from(source.0)).max(1) as u32;
    let mut h =
        (u64::from(shape.height) * u64::from(content.1) / u64::from(source.1)).max(1) as u32;
    let largest = w.max(h);
    if largest > 96 {
        w = (w * 96 / largest).max(1);
        h = (h * 96 / largest).max(1);
    }
    let mut scaled = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            let at = ((y * shape.height / h) * shape.width + x * shape.width / w) as usize * 4;
            scaled.extend_from_slice(&rgba[at..at + 4]);
        }
    }
    shape.hot_x = shape.hot_x * w / shape.width;
    shape.hot_y = shape.hot_y * h / shape.height;
    shape.width = w;
    shape.height = h;
    shape.rgba = latch_proto::base64::encode(&scaled);
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fake {
        shade: u8,
        changed: bool,
        size: (u32, u32),
        waits: Vec<Duration>,
    }
    impl CpuCapture for Fake {
        fn size(&self) -> (u32, u32) {
            self.size
        }
        fn grab(&mut self, frame: &mut Bgra, timeout: Duration) -> anyhow::Result<bool> {
            self.waits.push(timeout);
            if !std::mem::take(&mut self.changed) {
                return Ok(false);
            }
            *frame = Bgra {
                width: self.size.0,
                height: self.size.1,
                stride: self.size.0 as usize * 4,
                data: vec![self.shade; (self.size.0 * self.size.1 * 4) as usize],
            };
            Ok(true)
        }
        fn display(&self) -> DisplayRect {
            DisplayRect {
                width: self.size.0,
                height: self.size.1,
                ..Default::default()
            }
        }
        fn name(&self) -> String {
            "fake".into()
        }
    }
    fn pipeline() -> SoftwarePipeline<Fake> {
        SoftwarePipeline::new(
            Fake {
                shade: 128,
                changed: true,
                size: (16, 16),
                waits: Vec::new(),
            },
            &VideoConfig {
                color: Default::default(),
                width: 16,
                height: 16,
                fps: 100,
                bitrate_kbps: 1000,
                codec: Codec::H264Baseline,
                display: None,
            },
        )
        .unwrap()
    }
    #[test]
    fn low_bitrate_resizes_visible_input_and_recovers_requested_size() {
        let mut base = pipeline();
        base.capture.size = (1280, 720);
        base.config.width = 1280;
        base.config.height = 720;
        base.config.fps = 60;
        base.config.bitrate_kbps = 10_000;
        let mut p = SoftwarePipeline::with_workers(base.capture, &base.config, 1).unwrap();
        let mut decoder = latch_codec::h264::Decoder::new();
        let first = p.next(true, Duration::ZERO).unwrap();
        let dark = decoder.decode(&first.data).unwrap().unwrap().y[0];
        assert_eq!((p.info().width, p.info().height), (1280, 720));

        p.set_bitrate(94);
        p.capture.shade = 255;
        p.capture.changed = true;
        let low = p.next(false, Duration::ZERO).unwrap();
        assert!(low.keyframe);
        assert!(low.data.len() as u64 * 60 <= 94_000);
        assert!(p.info().width < 1280 && p.info().height < 720);
        let decoded = decoder.decode(&low.data).unwrap().unwrap();
        assert_eq!(
            (decoded.width, decoded.height),
            (p.info().width, p.info().height)
        );
        assert!(
            decoded.y[0] > dark + 50,
            "synthetic input reached the decoded picture"
        );
        let small = p.encoder_size;
        p.set_bitrate(99);
        let probe = p.next(false, Duration::ZERO).unwrap();
        decoder.decode(&probe.data).unwrap().unwrap();
        assert_eq!(
            p.encoder_size, small,
            "small probes retain the active encoder size"
        );

        p.set_bitrate(10_000);
        p.capture.shade = 128;
        p.capture.changed = true;
        let restored = p.next(false, Duration::ZERO).unwrap();
        assert!(restored.keyframe);
        let decoded = decoder.decode(&restored.data).unwrap().unwrap();
        assert_eq!((decoded.width, decoded.height), (1280, 720));
        assert!(decoded.y[0].abs_diff(dark) < 5);
        assert_eq!(p.config.width, 1280);
        assert_eq!(p.config.height, 720);
    }

    #[test]
    fn an_impossible_minimum_size_retains_the_explicit_codec_error() {
        let mut p = pipeline();
        p.config.width = 64;
        p.config.height = 64;
        p.set_bitrate(1);
        let error = p.next(true, Duration::ZERO).unwrap_err();
        assert_eq!(
            error.downcast_ref::<latch_codec::h264::CodecError>(),
            Some(&latch_codec::h264::CodecError::FrameTooLarge)
        );
        assert_eq!(p.encoder_size, (64, 64));
    }

    /// Pictures are numbered across encoders: after a resize the old
    /// numbers name nothing, so a stale request cannot reach a picture
    /// of the new encoder that happens to share its number.
    #[test]
    fn recovery_numbers_outlive_the_encoder() {
        let mut p = pipeline();
        assert!(p.recovers());
        let first: Vec<_> = (0..4)
            .map(|_| p.next(false, Duration::ZERO).unwrap().picture)
            .collect();
        assert_eq!(first, [1, 2, 3, 4]);
        assert!(p.recover_from(2));
        let frame = p.next(false, Duration::ZERO).unwrap();
        assert_eq!((frame.picture, frame.recovery), (5, Some(2)));
        p.capture.size = (32, 16);
        p.capture.changed = true;
        let frame = p.next(false, Duration::ZERO).unwrap();
        assert!(frame.keyframe);
        assert_eq!(frame.picture, 6);
        assert!(!p.recover_from(1), "a picture of the old encoder");
        assert!(p.recover_from(6));
        assert_eq!(p.next(false, Duration::ZERO).unwrap().recovery, Some(6));
    }

    #[test]
    fn capture_idle_keyframe_pacing_and_info() {
        let mut p = pipeline();
        assert_eq!(p.info().encoder, "Latch software H.264 (fake)");
        assert_eq!(p.info().width, 16);
        assert!(!p.info().hardware);
        assert!(p.acquire(false, Duration::ZERO).unwrap());
        p.last_sent = Some(Instant::now());
        p.capture.waits.clear();
        assert!(!p.acquire(true, Duration::from_secs(10)).unwrap());
        // A keyframe polls once and ignores the idle wait. Cadence's
        // fake-clock test verifies pacing without relying on VM scheduling.
        assert_eq!(p.capture.waits, [Duration::ZERO]);
        p.capture.waits.clear();
        let last_sent = Instant::now();
        p.last_sent = Some(last_sent);
        assert!(!p.acquire(false, Duration::from_millis(25)).unwrap());
        assert!(last_sent.elapsed() >= Duration::from_millis(25));
        assert!(p
            .capture
            .waits
            .iter()
            .all(|&wait| wait <= Duration::from_millis(10)));
        p.capture.size = (32, 16);
        p.capture.changed = true;
        assert!(p.acquire(false, Duration::ZERO).unwrap());
        assert_eq!(p.info().width, 16);
        p.set_bitrate(2000);
        assert_eq!(p.config.bitrate_kbps, 2000);
    }
    #[test]
    fn the_software_ceiling_is_measured() {
        let fps = measure_fps(320, 240, Duration::from_millis(200));
        assert!(fps > 0, "a small picture encodes at some rate");
        // Paced at MAX_FPS on a grid 1% fast; a 200 ms sample on a fast
        // machine lands a little above it (CI's x64 runners measured 511).
        let max = latch_proto::control::MAX_FPS;
        assert!(fps <= max + max / 10, "{fps} fps");
        assert_eq!(
            measure_fps(3, 3, Duration::from_millis(10)),
            0,
            "odd sizes cannot open"
        );
    }

    #[test]
    fn a_software_ceiling_holds_the_pixels_a_second() {
        let measured = Ceiling {
            width: 1920,
            height: 1080,
            fps: 120,
        };
        assert_eq!(measured.fit(1920, 1080, 60), (1920, 1080, 60));
        assert_eq!(measured.fit(1920, 1080, 240), (1920, 1080, 120));
        assert_eq!(measured.fit(1280, 720, 240), (1280, 720, 240));
        assert_eq!(measured.fit(3840, 2160, 120), (3840, 2160, 30));
        // Until measured, a cold 4K request at a display's refresh rate
        // becomes 1080p at 30.
        let (w, h, fps) = Ceiling::PROVISIONAL.fit(3840, 2160, 120);
        assert_eq!((fps, w % 2, h % 2), (30, 0, 0));
        assert!(
            u64::from(w) * u64::from(h) <= 1920 * 1080 && w >= 1900,
            "{w} × {h}"
        );
        assert_eq!(Ceiling::PROVISIONAL.fit(1920, 1080, 24), (1920, 1080, 24));
    }

    #[test]
    fn odd_capture_is_fitted_into_the_even_output_without_stretching() {
        let mut pipeline = pipeline();
        pipeline.capture.size = (17, 15);
        pipeline.next(true, Duration::ZERO).unwrap();
        assert_eq!((pipeline.info().width, pipeline.info().height), (16, 16));
        assert_eq!(
            (
                pipeline.info().display.width,
                pipeline.info().display.height
            ),
            (17, 15)
        );
        assert!(pipeline.scaler.is_some());
        let picture = &pipeline.picture;
        // 17:15 fits into 16×14 at y=1, leaving a black row at each edge.
        assert_eq!(&picture.y[..16], &[16; 16]);
        assert_eq!(
            &picture.y[15 * picture.y_stride..15 * picture.y_stride + 16],
            &[16; 16]
        );
        assert_eq!(picture.y[picture.y_stride], 126);
    }

    #[test]
    fn worker_policy_preserves_the_default_cap_and_small_machines() {
        for (w, h) in [(1920, 1080), (2560, 1440), (3840, 2160)] {
            for available in [1, 2, 4, 6, 8, 16, 64] {
                assert_eq!(worker_count(available, w, h), available.min(4));
            }
        }
        assert_eq!(worker_count(2, 1920, 1080), 2);
        assert_eq!(worker_count(64, 16, 16), 1);
        assert_eq!(worker_count(0, 0, 0), 1);
    }

    #[test]
    fn cursor_shape_and_hotspot_use_output_pixels() {
        let mut shape =
            latch_proto::control::CursorShape::new(7, 32, 24, 8, 6, &vec![255; 32 * 24 * 4]);
        scale_cursor(&mut shape, (3840, 2160), (1920, 1080));
        assert_eq!(
            (shape.width, shape.height, shape.hot_x, shape.hot_y),
            (16, 12, 4, 3)
        );
        assert_eq!(shape.decode_rgba().unwrap(), vec![255; 16 * 12 * 4]);
        let desktop = DisplayRect {
            width: 3840,
            height: 2160,
            ..Default::default()
        };
        assert_eq!(desktop.map(960, 540, 1920, 1080), (1920, 1080));
        // Same rounding as capture when the requested aspect differs.
        assert_eq!(
            desktop.fitted(1920, 1200),
            DisplayRect {
                x: 0,
                y: 60,
                width: 1920,
                height: 1080
            }
        );
        assert_eq!(desktop.map(960, 600, 1920, 1200), (1920, 1080));
    }

    #[test]
    fn rejects_bad_frames_and_config() {
        let mut p = pipeline();
        p.config.fps = 0;
        assert!(SoftwarePipeline::new(p.capture, &p.config).is_err());
        assert!(validate_frame(&Bgra {
            width: 16,
            height: 16,
            stride: 64,
            data: vec![0; 3]
        })
        .is_err());
        assert!(validate_size(15, 16).is_err());
    }
    #[test]
    fn encodes_repeats_keyframes_and_resizes() {
        let mut p = pipeline();
        assert!(p.next(false, Duration::ZERO).unwrap().keyframe);
        assert!(!p.next(false, Duration::ZERO).unwrap().keyframe);
        assert!(p.next(true, Duration::from_secs(10)).unwrap().keyframe);
        p.set_bitrate(2000);
        p.capture.size = (32, 16);
        p.capture.changed = true;
        assert!(p.next(false, Duration::ZERO).unwrap().keyframe);
        assert_eq!(p.info().width, 16);
    }

    struct Counting {
        kbps: u32,
    }
    impl PictureEncoder for Counting {
        fn name(&self) -> String {
            "Counting HEVC".into()
        }
        fn hardware(&self) -> bool {
            true
        }
        fn codec(&self) -> Codec {
            Codec::Hevc
        }
        fn encode(
            &mut self,
            p: &Yuv420,
            keyframe: bool,
            out: &mut Vec<u8>,
        ) -> anyhow::Result<bool> {
            out.extend_from_slice(&[0, 0, 0, 1, p.width as u8, self.kbps as u8]);
            Ok(keyframe)
        }
        fn set_bitrate(&mut self, kbps: u32) {
            self.kbps = kbps;
        }
    }
    #[test]
    fn a_hardware_encoder_opens_up_front_and_again_on_resize() {
        let opened = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = opened.clone();
        let p = pipeline();
        let mut p = SoftwarePipeline::with_encoder(
            p.capture,
            &p.config,
            Box::new(move |w, h, config| {
                log.lock().unwrap().push((w, h));
                Ok(Box::new(Counting {
                    kbps: config.bitrate_kbps,
                }))
            }),
        )
        .unwrap();
        let info = p.info();
        assert_eq!(info.encoder, "Counting HEVC (fake)");
        assert!(info.hardware);
        assert_eq!(info.codec, Codec::Hevc);
        assert_eq!(*opened.lock().unwrap(), [(16, 16)]);
        let first = p.next(true, Duration::ZERO).unwrap();
        assert!(first.keyframe);
        assert_eq!(first.data, [0, 0, 0, 1, 16, 232]);
        p.set_bitrate(7);
        assert!(!p.next(false, Duration::ZERO).unwrap().keyframe);
        p.capture.size = (32, 16);
        p.capture.changed = true;
        let resized = p.next(false, Duration::ZERO).unwrap();
        assert!(resized.keyframe, "a new size starts with a keyframe");
        assert_eq!(*opened.lock().unwrap(), [(16, 16), (16, 16)]);
    }
}
