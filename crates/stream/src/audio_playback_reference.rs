//! Frozen playback controller from main at fa8d6f1 (identical to 45ad4b7).
//! Only visibility, unused methods/fields and intact-discard instrumentation differ.
use super::*;

const MAX_TRIM: f64 = 0.002;
const TRIM_FULL_MS: f64 = 4.0;
const MARGIN_MS: f64 = 2.0;
const MAX_MARGIN_MS: f64 = 40.0;
const MARGIN_DECAY_MS_PER_S: f64 = 0.1;
const RELEASE_MS_PER_S: f64 = 0.25;
const DEFAULT_JITTER_MS: f64 = 10.0;
const PAUSE: Duration = Duration::from_millis(100);

pub(super) struct Mixer {
    queue: Arc<Mutex<VecDeque<f32>>>,
    pulled: Arc<AtomicU64>,
    gain: Arc<AtomicU32>,
    link: Arc<Link>,
    src_channels: usize,
    out_channels: usize,
    /// `mix[out][in]`.
    mix: Vec<Vec<f32>>,
    /// Source frames per output frame: 48000 / 44100 ≈ 1.088.
    step: f64,
    out_rate: f64,
    /// Source samples per millisecond.
    per_ms: f64,
    /// How far from `prev` towards `next` the output is; a whole frame or
    /// more means it is time to pull.
    pos: f64,
    prev: [f32; MAX_CHANNELS],
    next: [f32; MAX_CHANNELS],
    /// A fixed cushion (source samples) instead of the measured one, and no
    /// steering; `Some(0)` in tests plays whatever is queued.
    fixed: Option<usize>,
    started: bool,
    /// The least slack seen lately, source samples: dips are taken at once
    /// and forgotten at [`RELEASE_MS_PER_S`].
    low: f64,
    pub(super) margin_ms: f64,
    /// Queued source samples, smoothed, for the stats.
    level: f64,
    last_pull: Option<Instant>,
    /// When the queue ran dry, until sound comes back.
    dry_at: Option<Instant>,
    /// From the device's callback timestamps.
    device_delay: Duration,
    /// Output frames since the last loud one, while probing.
    probe_quiet: Option<usize>,
}

impl Mixer {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        src_rate: u32,
        src_channels: usize,
        out_rate: u32,
        out_channels: usize,
        queue: Arc<Mutex<VecDeque<f32>>>,
        pulled: Arc<AtomicU64>,
        fixed: Option<usize>,
        gain: Arc<AtomicU32>,
    ) -> Self {
        let src_channels = src_channels.clamp(1, MAX_CHANNELS);
        let out_channels = out_channels.max(1);
        let mask = latch_codec::audio::speaker_mask(src_channels);
        Self {
            queue,
            pulled,
            gain,
            link: Arc::default(),
            src_channels,
            out_channels,
            mix: latch_codec::audio::mix_matrix(src_channels, mask, out_channels),
            step: src_rate.max(1) as f64 / out_rate.max(1) as f64,
            out_rate: out_rate.max(1) as f64,
            per_ms: src_rate.max(1) as f64 * src_channels as f64 / 1000.0,
            pos: 1.0,
            prev: [0.0; MAX_CHANNELS],
            next: [0.0; MAX_CHANNELS],
            fixed,
            started: false,
            low: 0.0,
            margin_ms: MARGIN_MS,
            level: 0.0,
            last_pull: None,
            dry_at: None,
            device_delay: Duration::ZERO,
            probe_quiet: std::env::var_os("LATCH_AUDIO_PROBE").map(|_| 0),
        }
    }

    pub(super) fn linked(mut self, link: Arc<Link>) -> Self {
        self.link = link;
        self
    }

    /// The measured jitter, and the slack to steer the low point to: the
    /// margin, plus whatever of a burst the jitter does not already spread
    /// over every phase of the device's pulls. (With less jitter than a
    /// burst, the clocks' drift slides arrivals past pulls now and then,
    /// and each time the low point drops by a burst at once.)
    fn plan_ms(&self) -> (f64, f64) {
        let jitter = match self.link.jitter_us.load(Ordering::Relaxed) {
            0 => DEFAULT_JITTER_MS,
            us => f64::from(us) / 1000.0,
        };
        let burst = f64::from(self.link.burst_us.load(Ordering::Relaxed)) / 1000.0;
        (jitter, self.margin_ms + (burst - jitter).max(0.0))
    }

    /// Source samples to have queued before playing: this pull's need,
    /// the measured jitter and the slack.
    pub(super) fn cushion(&self, need: usize) -> usize {
        if let Some(fixed) = self.fixed {
            return fixed;
        }
        let (jitter, slack) = self.plan_ms();
        let samples = need + ((jitter + slack) * self.per_ms) as usize;
        samples.div_ceil(self.src_channels) * self.src_channels
    }

    pub(super) fn fill_at(&mut self, out: &mut [f32], now: Instant) {
        let frames_out = out.len() / self.out_channels;
        // Source samples this pull takes, the interpolation's one ahead
        // included.
        let need = ((frames_out as f64 * self.step).ceil() as usize + 1) * self.src_channels;
        let dt = self
            .last_pull
            .map_or(0.0, |t| now.saturating_duration_since(t).as_secs_f64());
        self.last_pull = Some(now);
        let steer = self.fixed.is_none();
        if steer {
            self.margin_ms = (self.margin_ms - MARGIN_DECAY_MS_PER_S * dt).max(MARGIN_MS);
        }
        let mut discarded = Vec::new();
        let mut q = self.queue.lock();
        if !self.started {
            if let Some(dry) = self.dry_at.filter(|_| !q.is_empty()) {
                // Sound came back: soon means it was late, not paused.
                let gap = now.saturating_duration_since(dry);
                if steer && gap < PAUSE {
                    self.margin_ms = (self.margin_ms + MARGIN_MS).min(MAX_MARGIN_MS);
                    self.link.underruns.fetch_add(1, Ordering::Relaxed);
                }
                self.dry_at = None;
            }
            let cushion = self.cushion(need);
            if q.len() < cushion {
                drop(q);
                out.fill(0.0);
                self.pulled.fetch_add(frames_out as u64, Ordering::Relaxed);
                return;
            }
            self.started = true;
            if cushion > 0 {
                // Out of silence, start from the newest cushion's worth: a
                // backlog a stall left behind would stay as added delay.
                let excess = (q.len() - cushion) / self.src_channels * self.src_channels;
                discarded.extend(q.drain(..excess));
            }
            self.low = q.len() as f64 - need as f64;
            self.level = q.len() as f64;
        }
        // Steer the low point of the slack to the margin, so clock drift
        // neither grows the delay nor drains it into a gap, and the delay
        // is no more than this path's jitter needs.
        let step = if steer {
            let slack = q.len() as f64 - need as f64;
            self.low = slack.min(self.low + RELEASE_MS_PER_S * dt * self.per_ms);
            let error = (self.low / self.per_ms - self.plan_ms().1) / TRIM_FULL_MS;
            self.level += (q.len() as f64 - self.level) * dt.min(1.0);
            let us = |samples: f64| (samples / self.per_ms * 1000.0).max(0.0) as u32;
            let link = &self.link;
            link.queued_us.store(us(self.level), Ordering::Relaxed);
            link.target_us
                .store(us(self.cushion(need) as f64), Ordering::Relaxed);
            link.margin_us
                .store((self.margin_ms * 1000.0) as u32, Ordering::Relaxed);
            link.device_us
                .store(self.device_delay.as_micros() as u32, Ordering::Relaxed);
            self.step * (1.0 + (MAX_TRIM * error).clamp(-MAX_TRIM, MAX_TRIM))
        } else {
            self.step
        };
        let gain = f32::from_bits(self.gain.load(Ordering::Relaxed));
        let wall = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            + self.device_delay;
        let mut frames = 0u64;
        for frame in out.chunks_mut(self.out_channels) {
            while self.pos >= 1.0 {
                self.pos -= 1.0;
                self.prev = self.next;
                self.next = [0.0; MAX_CHANNELS];
                if q.len() >= self.src_channels {
                    for c in 0..self.src_channels {
                        self.next[c] = q.pop_front().unwrap_or(0.0);
                    }
                } else if self.started {
                    // Ran dry: rebuild the cushion before playing on, or
                    // every later jitter spike would click too.
                    self.started = false;
                    self.dry_at = Some(now);
                }
            }
            let t = self.pos as f32;
            let mut src = [0.0f32; MAX_CHANNELS];
            for (c, s) in src.iter_mut().enumerate().take(self.src_channels) {
                *s = self.prev[c] + (self.next[c] - self.prev[c]) * t;
            }
            for (o, row) in frame.iter_mut().zip(&self.mix) {
                let mixed: f32 = row.iter().zip(&src).map(|(w, s)| w * s).sum();
                *o = mixed * gain;
            }
            if let Some(quiet) = &mut self.probe_quiet {
                let level = src[0].abs();
                if level > 0.2 && *quiet as f64 >= self.out_rate / 10.0 {
                    let at =
                        wall.as_nanos() + frames as u128 * 1_000_000_000 / self.out_rate as u128;
                    PROBE.lock().push((at % PROBE_EVERY_NS) as f32 / 1e6);
                }
                *quiet = if level < 0.05 { *quiet + 1 } else { 0 };
            }
            self.pos += step;
            frames += 1;
        }
        drop(q);
        self.link.discarded_samples.fetch_add(
            discarded.iter().filter(|&&s| s == 0.5).count() as u64,
            Ordering::Relaxed,
        );
        self.pulled.fetch_add(frames, Ordering::Relaxed);
    }
}
