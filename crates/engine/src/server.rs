//! Sessions: the control service hands over an authorized, upgraded TCP
//! connection and its request; this runs the session until either side
//! ends it. One session at a time; a new one takes over.
//!
//! Threads per session: the connection's own thread runs the control
//! messages and timers, one thread captures and sends video, one captures
//! and sends audio. One thread for the whole server receives datagrams and
//! applies input as it arrives, so a key press never waits behind a frame.
//! A session with gamepads has one more thread, which sends their rumble.

use crate::audio::{self, AudioCapture};
use crate::gamepad::{self, FeedbackHub, SessionPads};
use crate::input::{self, DisplayRect, InputSink};
use crate::video::{self, Codec, VideoConfig, VideoInfo, VideoPipeline};
use latch_proto::audio::{AudioOut, AudioPacketizer, AudioReceiver};
use latch_proto::control::{self, end, Accept, AudioFormat, End, Message, Notice, StreamRequest};
use latch_proto::input::InputReceiver;
use latch_proto::rtt::RttEstimator;
use latch_proto::video::{KeyframeAnswers, OutFrame, Packetizer, DEFAULT_FEC_PERCENT};
use latch_proto::wire::{self, Datagram};
use latch_stream::session::RelayRoute;
use parking_lot::{Condvar, Mutex};
use std::collections::VecDeque;
use std::io::Write;
use std::net::{IpAddr, Ipv4Addr, Shutdown, SocketAddr, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

#[cfg(test)]
#[path = "../../../tests/pairs/record.rs"]
pub(crate) mod pair_interop;

/// A still screen still gets a nearly empty frame this often, so a viewer
/// notices a lost last frame and the stream never looks dead. It also bounds
/// how long a keyframe request waits while next() waits for the screen to
/// change: on a still screen a 100 ms wait shows nothing new. Measured on a
/// 2-core ARM host at 720p: about 3% of a core; every 25 ms cost a Mac
/// viewer 2.5 times the CPU for nothing it could see.
const IDLE_REPEAT: Duration = Duration::from_millis(100);
const KEEPALIVE: Duration = Duration::from_secs(1);
/// A viewer heard from neither over TCP nor UDP for this long is gone.
const SILENT: Duration = Duration::from_secs(10);
const HELLO_EVERY: Duration = Duration::from_millis(50);
/// Often enough that each viewer report has round trips
/// to tell a filling queue from the path's base; a ping is 16 bytes.
const PING_EVERY: Duration = Duration::from_millis(20);
/// Keyframes for requests at most this often: a burst of requests for one
/// loss must not turn into a burst of keyframes.
const KEYFRAME_GAP: Duration = Duration::from_millis(50);
/// How long a new session waits for the one it replaces to let go of the
/// screen and the encoder.
/// Initial capture can wait 10 seconds; undo commands can take another 15.
const TAKEOVER_WAIT: Duration = Duration::from_secs(30);
/// How long a session's end waits for its media threads. One stuck in a
/// capture or encoder call is left behind (it stops at its next check of
/// the session) so that the session still ends and the screen is let go.
const MEDIA_STOP: Duration = Duration::from_secs(5);
const VIDEO_RECOVERY_WINDOW: Duration = Duration::from_secs(60);
const VIDEO_RECOVERIES: usize = 3;
/// Samples per channel in one audio packet: the capture's 5 ms frame when
/// its worst case fits a datagram (stereo: 965 of 1 182 bytes), else the
/// frame split evenly into parts that do (5.1: 80, 7.1: 60).
/// Lossless sound grows with its channels: the encoder budget keeps 1.2
/// Mbps for stereo, and this keeps as much again for each further pair
/// (centre/LFE, rears, sides), which codes to no more than a stereo pair.
fn surround_kbps(channels: usize) -> u32 {
    (channels.saturating_sub(2) as u32).div_ceil(2) * 1200
}

fn audio_packet_samples(channels: usize) -> usize {
    audio_packet_samples_with_limit(channels, latch_proto::MAX_DATAGRAM)
}

fn audio_packet_samples_with_limit(channels: usize, datagram_size: usize) -> usize {
    let fits = latch_codec::audio::frame_samples_within(channels, datagram_size - 18);
    audio::FRAME / audio::FRAME.div_ceil(fits.max(1))
}

/// Readies the desktop for a session (for example, sizes it to the
/// viewer); returns what undoes that when the session ends.
pub type Prepare =
    Box<dyn Fn(&StreamRequest) -> anyhow::Result<Option<Box<dyn FnOnce() + Send>>> + Send + Sync>;

pub struct Options {
    pub relay: Option<latch_relay::ClientConfig>,
    pub prepare: Option<Prepare>,
    /// Lines for the owner's log (the host window shows them).
    pub log: Box<dyn Fn(String) + Send + Sync>,
    /// Plays a viewer's microphone where the engine has no player of its
    /// own (Windows, macOS). See [`crate::mic`].
    pub mic: Option<crate::mic::MicOpener>,
}

impl Default for Options {
    fn default() -> Self {
        #[cfg(test)]
        std::env::set_var("LATCH_NETENV_PROBE", "0");
        Self {
            relay: latch_stream::session::relay_config(&control::RelaySettings::default()),
            prepare: None,
            log: Box::new(|line| tracing::info!("{line}")),
            mic: None,
        }
    }
}

/// Host-side video progress, independent of the viewer receiving a complete frame.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct VideoDiagnostics {
    pub frames_produced: u64,
    pub frames_sent: u64,
    pub encoded_bytes: u64,
    pub produce_time: Duration,
    pub total_produce_time: Duration,
    pub max_produce_time: Duration,
    pub software: Option<crate::software::SoftwareTimings>,
    pub pacing_waits: u64,
    pub pacing_wait: Duration,
    pub pacing_max_lateness: Duration,
    producing_since: Option<Instant>,
}

/// What `/v1/status` says about streaming.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ServerStatus {
    /// The viewer's machine name while a session runs.
    pub viewer: Option<String>,
    /// The encoder of the running or last session.
    pub encoder: String,
    pub hardware: bool,
    pub captured_display: Option<String>,
    /// Why the running or last session had no sound.
    pub audio_problem: String,
    pub requested_bitrate_kbps: u32,
    pub applied_bitrate_kbps: u32,
    pub pacing_queue_ms: f32,
    pub send_errors: u64,
    pub send_retries: u64,
    pub socket_send_bytes: usize,
    pub socket_receive_bytes: usize,
    pub video: VideoDiagnostics,
}

/// Why a stream request was turned down, as an HTTP status and a sentence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub status: u16,
    pub message: String,
}

impl Refusal {
    fn new(status: u16, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    /// Preserve HTTP retry behavior when preparation follows the upgrade.
    pub fn into_end(self) -> End {
        End {
            code: if matches!(self.status, 408 | 429 | 500..=599) {
                end::TRANSPORT_FAILED
            } else {
                end::CAPTURE_FAILED
            },
            message: self.message,
        }
    }
}

pub struct Server {
    udp: UdpSocket,
    epoch: Instant,
    options: Options,
    current: Mutex<Option<Arc<Session>>>,
    /// Held while a session is being opened, so two requests cannot both
    /// take the screen.
    opening: Arc<Opening>,
    status: Mutex<ServerStatus>,
}

impl Server {
    /// Binds the media socket on every IPv4 address at `port` and starts
    /// receiving.
    pub fn bind(port: u16, options: Options) -> anyhow::Result<Arc<Self>> {
        let udp = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, port)).map_err(|e| {
            anyhow::Error::new(e).context(format!("cannot listen on UDP port {port}"))
        })?;
        latch_proto::transport::configure_socket(&udp, 8 * 1024 * 1024, 8 * 1024 * 1024)?;
        udp.set_read_timeout(Some(Duration::from_millis(200)))?;
        let server = Arc::new(Self {
            udp,
            epoch: Instant::now(),
            options,
            current: Mutex::new(None),
            opening: Arc::new(Opening::default()),
            status: Mutex::new(ServerStatus::default()),
        });
        let weak = Arc::downgrade(&server);
        std::thread::Builder::new()
            .name("stream-udp".into())
            .spawn(move || receive(weak))?;
        Ok(server)
    }

    pub fn port(&self) -> u16 {
        self.udp.local_addr().map(|a| a.port()).unwrap_or(0)
    }

    pub fn status(&self) -> ServerStatus {
        let mut status = self.status.lock().clone();
        status.viewer = self.current.lock().as_ref().map(|s| s.viewer.clone());
        if let Some(session) = self.current.lock().as_ref() {
            status.applied_bitrate_kbps = session.transport.applied.load(Ordering::Relaxed);
            status.pacing_queue_ms =
                session.transport.queue_us.load(Ordering::Relaxed) as f32 / 1000.0;
            status.send_errors = session.transport.errors.load(Ordering::Relaxed);
            status.send_retries = session.transport.retries.load(Ordering::Relaxed);
            status.video = session.video.lock().clone();
            status.video.frames_sent = session.frames.load(Ordering::Relaxed);
            if let Some(since) = status.video.producing_since {
                status.video.produce_time = since.elapsed();
            }
        }
        status
    }

    /// Includes a prepared stream waiting for its HTTP upgrade.
    pub fn busy(&self) -> bool {
        *self.opening.busy.lock() || self.current.lock().is_some()
    }

    /// Ends the running session, if any.
    pub fn stop(&self, code: u32, message: &str) -> bool {
        // Preparation can finish just before run installs the current session.
        // Wait for that reservation to be dropped or installed before stopping.
        if !self.opening.wait_idle(Instant::now() + TAKEOVER_WAIT) {
            return false;
        }
        let current = self.current.lock().clone();
        if let Some(session) = current {
            session.end(code, message);
            if !session.wait_finished(TAKEOVER_WAIT) {
                self.log("stream cleanup did not finish before the stop deadline".into());
                return false;
            }
        }
        !self.busy()
    }

    fn log(&self, line: String) {
        (self.options.log)(line);
    }

    fn now_us(&self) -> u64 {
        self.epoch.elapsed().as_micros() as u64
    }

    /// Opens what a session needs, after ending the one running. A refusal
    /// is for the HTTP reply; nothing is left open after one.
    pub fn prepare(&self, req: StreamRequest, peer: SocketAddr) -> Result<Prepared, Refusal> {
        let display = req.display.clone();
        self.prepare_using(
            req,
            peer,
            || video::encodable_for_display(display.as_deref()),
            video::limits,
            video::open,
        )
    }

    /// Prepare after HTTP upgrade so an approval request can observe the
    /// viewer closing its control connection, including older viewers.
    pub fn prepare_connected(
        &self,
        req: StreamRequest,
        peer: SocketAddr,
        tcp: &TcpStream,
        stopping: Arc<AtomicBool>,
    ) -> Result<Prepared, Refusal> {
        #[cfg(target_os = "linux")]
        {
            // Reserve time for takeover, encoder and media setup. Older
            // viewers advertise no deadline and still stop at 60 seconds.
            let approval = crate::linux::wayland::portal::approval_budget(req.startup_timeout_secs);
            crate::linux::wayland::portal::during_startup(tcp, approval, stopping, || {
                self.prepare(req, peer)
            })
            .map_err(|e| Refusal::new(500, format!("could not track the viewer connection: {e}")))?
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (tcp, stopping);
            self.prepare(req, peer)
        }
    }

    fn prepare_using(
        &self,
        mut req: StreamRequest,
        peer: SocketAddr,
        encodable: impl FnOnce() -> Vec<Codec>,
        limits_for: impl Fn(Codec) -> video::Limits,
        mut open: impl FnMut(&VideoConfig) -> anyhow::Result<Box<dyn VideoPipeline>>,
    ) -> Result<Prepared, Refusal> {
        if req.protocol != latch_proto::PROTOCOL {
            return Err(Refusal::new(
                426,
                format!(
                    "this host speaks stream protocol {}, not {}",
                    latch_proto::PROTOCOL,
                    req.protocol
                ),
            ));
        }
        if req.codecs.is_empty() {
            return Err(Refusal::new(
                406,
                "the viewer offered no supported video codecs",
            ));
        }
        let Some(opening) = self.opening.lock(TAKEOVER_WAIT) else {
            return Err(Refusal::new(
                503,
                "another viewer is still starting a stream here; try again",
            ));
        };
        let encodable = encodable();
        let original = req.clone();
        // Best first, of those this host encodes at the size it would send.
        let mut candidates: Vec<Codec> = req
            .codecs
            .iter()
            .copied()
            .filter(|c| encodable.contains(c))
            .filter(|&c| {
                let (w, h, _) = limits_for(c).fit(
                    even(req.width.clamp(64, control::MAX_SIDE)),
                    even(req.height.clamp(64, control::MAX_SIDE)),
                    1,
                );
                video::takes(c, w, h)
            })
            .collect();
        latch_proto::color::ColorDescription::rank_codecs(
            &mut candidates,
            req.color,
            req.prefer_color,
            cfg!(any(target_os = "macos", windows)),
        );
        let Some(&codec) = candidates.first() else {
            return Err(Refusal::new(
                406,
                format!(
                    "this host encodes {}, which the viewer cannot decode",
                    names(&encodable)
                ),
            ));
        };
        // Refuse incompatible offers before interrupting the active viewer.
        self.release_current(&req, peer)?;
        // What the encoder for that codec takes here, before the desktop is
        // sized for it: a larger request is scaled down in proportion.
        let limits = limits_for(codec);
        let (mut width, mut height, mut fps) = limits.fit(
            even(req.width.clamp(64, control::MAX_SIDE)),
            even(req.height.clamp(64, control::MAX_SIDE)),
            req.fps.clamp(1, control::MAX_FPS),
        );
        // A host encoding in software, as it measured its CPU (or, until
        // then, conservatively): whatever the viewer fitted, or did not.
        if let Some(ceiling) =
            crate::software::ceiling().filter(|_| !video::hardware_encodes(codec))
        {
            (width, height, fps) = ceiling.fit(width, height, fps);
        }
        if (width, height, fps) != (req.width, req.height, req.fps) {
            self.log(format!(
                "{} × {} at {} fps is past what {} encodes here; streaming {width} × {height} at {fps} fps",
                req.width,
                req.height,
                req.fps,
                codec.name()
            ));
        }
        req.width = width;
        req.height = height;
        req.fps = fps;
        req.bitrate_kbps = req.bitrate_kbps.clamp(500, limits.max_kbps);
        let undo = match &self.options.prepare {
            Some(prepare) => prepare(&req)
                .map_err(|e| Refusal::new(500, format!("could not prepare the desktop: {e:#}")))?,
            None => None,
        };
        let mut prepared = Prepared {
            undo,
            pipeline: None,
            audio: None,
            audio_problem: String::new(),
            mic: None,
            mic_problem: String::new(),
            input: None,
            input_problem: None,
            pads: None,
            pads_problem: String::new(),
            feedback: Arc::new(FeedbackHub::default()),
            info: None,
            codec,
            req,
            peer,
            opening: Some(opening),
        };
        let mut config = VideoConfig {
            color: latch_proto::color::ColorDescription::negotiate(
                prepared.req.color,
                codec == video::Codec::Hevc,
                cfg!(any(target_os = "macos", windows)),
            ),
            width: prepared.req.width,
            height: prepared.req.height,
            fps: prepared.req.fps,
            bitrate_kbps: prepared.req.bitrate_kbps,
            codec,
            display: prepared.req.display.clone(),
        };
        let mut opened = open(&config);
        // A codec whose only encoder will not open here gives way to the
        // next the viewer takes, refitted from the original request.
        for &next in &candidates[1..] {
            match &opened {
                Err(e) if video::Unencodable::is(e) => {
                    self.log(format!("{e:#}; trying {}", next.name()));
                    config.codec = next;
                    config.color = latch_proto::color::ColorDescription::negotiate(
                        prepared.req.color,
                        next == video::Codec::Hevc,
                        cfg!(any(target_os = "macos", windows)),
                    );
                    let limits = limits_for(next);
                    (config.width, config.height, config.fps) = limits.fit(
                        even(original.width.clamp(64, control::MAX_SIDE)),
                        even(original.height.clamp(64, control::MAX_SIDE)),
                        original.fps.clamp(1, control::MAX_FPS),
                    );
                    config.bitrate_kbps = original.bitrate_kbps.clamp(500, limits.max_kbps);
                    if let Some(ceiling) =
                        crate::software::ceiling().filter(|_| !video::hardware_encodes(next))
                    {
                        (config.width, config.height, config.fps) =
                            ceiling.fit(config.width, config.height, config.fps);
                    }
                    prepared.req.width = config.width;
                    prepared.req.height = config.height;
                    prepared.req.fps = config.fps;
                    prepared.req.bitrate_kbps = config.bitrate_kbps;
                    if let Some(undo) = prepared.undo.take() {
                        undo();
                    }
                    prepared.undo = match &self.options.prepare {
                        Some(prepare) => prepare(&prepared.req).map_err(|e| {
                            Refusal::new(500, format!("could not prepare the desktop: {e:#}"))
                        })?,
                        None => None,
                    };
                    prepared.codec = next;
                    opened = open(&config);
                }
                _ => break,
            }
        }
        let pipeline = opened.map_err(|e| {
            let status = if e.is::<video::CaptureEnded>() {
                403
            } else {
                500
            };
            Refusal::new(status, format!("could not capture the screen: {e:#}"))
        })?;
        config.color = pipeline.info().color;
        prepared.info = Some(pipeline.info());
        prepared.pipeline = Some((pipeline, config));
        if prepared.req.audio {
            let channels = (prepared.req.audio_channels as usize).clamp(2, audio::MAX_CHANNELS);
            match audio::open_with(audio::Layout::UpTo(channels)) {
                Ok(capture) => prepared.audio = Some(capture),
                Err(e) => prepared.audio_problem = format!("{e:#}"),
            }
        }
        if prepared.req.mic {
            match crate::mic::open(self.options.mic.as_ref()) {
                Ok(mic) => prepared.mic = Some(mic),
                Err(e) => prepared.mic_problem = format!("{e:#}"),
            }
        }
        match input::open() {
            Ok(mut sink) => {
                sink.set_display_selected(prepared.req.display.is_some());
                let info = prepared.info.as_ref().expect("prepared has video info");
                if let Err(e) = sink.start_display(info.display, info.width, info.height) {
                    prepared.input_problem =
                        Some(format!("move pointer into the captured display: {e:#}"));
                }
                sink.set_key_positions(prepared.req.key_positions);
                prepared.input = Some(sink);
            }
            Err(e) => prepared.input_problem = Some(format!("{e:#}")),
        }
        if prepared.req.gamepads {
            match gamepad::open(prepared.feedback.feedback_fn()) {
                Ok(pads) => prepared.pads = Some(pads),
                Err(e) => prepared.pads_problem = format!("{e:#}"),
            }
        }
        Ok(prepared)
    }

    fn release_current(&self, req: &StreamRequest, peer: SocketAddr) -> Result<(), Refusal> {
        let running = self.current.lock().clone();
        if let Some(old) = running {
            self.log(format!(
                "{} takes the stream over from {}",
                display_name(&req.name, peer.ip()),
                old.viewer
            ));
            old.end(end::TAKEN_OVER, "Another device took this stream over.");
            if !old.wait_finished(TAKEOVER_WAIT) {
                return Err(Refusal::new(
                    503,
                    "the previous stream is still letting go of the screen; try again",
                ));
            }
        }
        Ok(())
    }

    /// Runs a prepared session on the upgraded connection until it ends.
    pub fn run(self: &Arc<Self>, mut prepared: Prepared, tcp: TcpStream) {
        let (mut pipeline, config) = prepared.pipeline.take().expect("prepared has a pipeline");
        let cursor_supported = prepared.req.cursor && pipeline.set_cursor_drawn(false);
        if !prepared.req.cursor {
            pipeline.set_cursor_drawn(true);
        }
        let info = prepared.info.take().expect("prepared has video info");
        let recovery = prepared.req.reference_invalidation && pipeline.recovers();
        let token: [u8; 16] = rand::random();
        let id: u32 = rand::random();
        let pads = prepared.pads.take();
        let (pad_slots, pad_kinds) = pads
            .as_ref()
            .map_or((0, Vec::new()), |p| (p.slots, p.kinds.clone()));
        let feedback = prepared.feedback.clone();
        feedback.start(id);
        let relay_enabled = prepared.req.relay && self.options.relay.is_some();
        let initial_budget = if prepared.req.media_path_policy && relay_enabled {
            config.bitrate_kbps.min(8000)
        } else {
            config.bitrate_kbps
        };
        let stop = Arc::new(AtomicBool::new(false));
        if let Some(sink) = prepared.input.as_mut() {
            sink.set_stopping(stop.clone());
        }
        let session = Arc::new(Session {
            relay: RelayRoute::new(
                id,
                latch_relay::Role::Host,
                relay_enabled.then(|| self.options.relay.clone()).flatten(),
            ),
            id,
            token,
            viewer: display_name(&prepared.req.name, prepared.peer.ip()),
            announced: SocketAddr::new(prepared.peer.ip(), prepared.req.udp_port),
            addr: Mutex::new(None),
            ended: Mutex::new(None),
            audio_notice: Mutex::new(None),
            stop,
            finished: Mutex::new(false),
            finished_cv: Condvar::new(),
            keyframe: AtomicBool::new(true),
            input: Mutex::new(InputState {
                receiver: InputReceiver::new(),
                sink: prepared.input.take(),
                display: info.display,
            }),
            pads: Mutex::new(SessionPads {
                receiver: Default::default(),
                sink: pads.map(|p| p.sink),
                slots: pad_slots,
                failing: Default::default(),
            }),
            feedback,
            rtt: Mutex::new(RttEstimator::new()),
            rtt_low: Mutex::new(None),
            train_ping: AtomicU64::new(0),
            train_rtt: Mutex::new(None),
            pace_kbps: AtomicU32::new(initial_budget),
            fec: AtomicU8::new(DEFAULT_FEC_PERCENT),
            fec_burst: AtomicU8::new(0),
            heard_us: AtomicU64::new(self.now_us()),
            frames: AtomicU64::new(0),
            video: Mutex::new(VideoDiagnostics::default()),
            bytes: AtomicU64::new(0),
            transport: TransportState::new(initial_budget),
            keyframe_requests: Mutex::default(),
            recovery: RecoveryState::new(recovery),
            audio_enabled: prepared.audio.is_some(),
            surround_kbps: prepared
                .audio
                .as_ref()
                .map_or(0, |capture| surround_kbps(capture.channels())),
            mic: Mutex::new(prepared.mic.take().map(|out| {
                let format = crate::mic::format();
                (
                    AudioReceiver::with_format(id, format.frame_samples, format.rate),
                    out,
                )
            })),
            cursor_supported,
            probe_trains: prepared.req.probe_trains,
            draw_cursor: AtomicBool::new(!cursor_supported),
            control_writer: Mutex::new(None),
        });
        session.relay.enable_policy(prepared.req.media_path_policy);
        // prepare owns the reservation until installation, so a second
        // prepare must see this session and wait for its undo before opening.
        *self.current.lock() = Some(session.clone());
        prepared.opening.take();
        {
            let mut status = self.status.lock();
            status.encoder = info.encoder.clone();
            status.hardware = info.hardware;
            status.captured_display = info.captured_display.clone();
            status.audio_problem = prepared.audio_problem.clone();
            status.requested_bitrate_kbps = config.bitrate_kbps;
            let buffers = latch_proto::transport::configure_socket(
                &self.udp,
                8 * 1024 * 1024,
                8 * 1024 * 1024,
            )
            .unwrap_or_default();
            status.socket_send_bytes = buffers.send;
            status.socket_receive_bytes = buffers.receive;
            self.log(format!("host UDP buffers: send={} receive={} bytes (requested 8388608/8388608; Linux may cap at net.core.wmem_max/rmem_max)", buffers.send, buffers.receive));
        }
        let audio = prepared.audio.take();
        let accept = Accept {
            media_path_policy: prepared.req.media_path_policy,
            key_positions: prepared.req.key_positions,
            relay: relay_enabled,
            color: info.color,
            session: id,
            token: control::token_hex(&token),
            codec: info.codec,
            width: info.width,
            height: info.height,
            fps: config.fps,
            bitrate_kbps: config.bitrate_kbps,
            encoder: info.encoder.clone(),
            audio: audio.as_ref().map(|capture| AudioFormat {
                rate: audio::RATE,
                channels: capture.channels() as u32,
                frame_samples: if prepared.req.media_path_policy {
                    audio_packet_samples_with_limit(capture.channels(), 1000)
                } else {
                    audio_packet_samples(capture.channels())
                } as u32,
                codec: "lpcm-rice".into(),
            }),
            audio_problem: prepared.audio_problem.clone(),
            cursor: cursor_supported,
            gamepads: pad_slots,
            gamepad_kinds: pad_kinds,
            gamepad_problem: prepared.pads_problem.clone(),
            reference_invalidation: recovery,
            mic: session.mic.lock().as_ref().map(|_| crate::mic::format()),
            mic_problem: prepared.mic_problem.clone(),
        };
        self.log(format!(
            "{} is watching: {}×{} at {} fps, {} kb/s, {}{}{}",
            session.viewer,
            info.width,
            info.height,
            config.fps,
            config.bitrate_kbps,
            info.encoder,
            if audio.is_some() {
                String::new()
            } else if prepared.req.audio {
                format!("; no sound ({})", prepared.audio_problem)
            } else {
                String::new()
            },
            match session.mic.lock().as_ref() {
                Some((_, out)) => format!("; their microphone → {}", out.describe),
                None if prepared.req.mic =>
                    format!("; not their microphone ({})", prepared.mic_problem),
                None => String::new(),
            }
        ));
        if prepared.req.gamepads && pad_slots == 0 {
            self.log(format!("no gamepads: {}", prepared.pads_problem));
        }
        let pad_feedback = (pad_slots > 0).then(|| {
            let server = self.clone();
            let session = session.clone();
            std::thread::Builder::new()
                .name("stream-pads".into())
                .spawn(move || {
                    session.feedback.run(|datagram| {
                        if let Some(addr) = (*session.addr.lock())
                            .or_else(|| session.relay.active().then_some(session.announced))
                        {
                            send(&server.udp, &session, datagram, addr);
                        }
                    })
                })
        });
        let started = Instant::now();
        let reason = self.control(&session, tcp, accept, prepared.input_problem.take(), {
            let server = self.clone();
            let session = session.clone();
            move || {
                let video = {
                    let server = server.clone();
                    let session = session.clone();
                    std::thread::Builder::new()
                        .name("stream-video".into())
                        .spawn(move || send_video(&server, &session, pipeline, config, video::open))
                };
                let audio = audio.map(|capture| {
                    let server = server.clone();
                    let session = session.clone();
                    std::thread::Builder::new()
                        .name("stream-audio".into())
                        .spawn(move || {
                            let layout = audio::Layout::Exactly(capture.channels());
                            send_audio(&server, &session, capture, || audio::open_with(layout))
                        })
                });
                (video.ok(), audio.and_then(Result::ok))
            }
        });
        {
            let mut state = session.input.lock();
            if let Some(mut sink) = state.sink.take() {
                sink.release_all();
            }
        }
        session.unplug_pads();
        session.feedback.stop();
        if let Some(Ok(thread)) = pad_feedback {
            let _ = thread.join();
        }
        if let Some(undo) = prepared.undo.take() {
            undo();
        }
        {
            let mut current = self.current.lock();
            if current.as_ref().is_some_and(|c| Arc::ptr_eq(c, &session)) {
                *current = None;
            }
        }
        self.log(format!(
            "{} stopped watching after {}: {reason}",
            session.viewer,
            human_duration(started.elapsed())
        ));
        session.mark_finished();
    }

    /// The session's control loop on its TCP connection; returns why it
    /// ended once the media threads have stopped.
    fn control(
        &self,
        session: &Arc<Session>,
        tcp: TcpStream,
        accept: Accept,
        input_problem: Option<String>,
        start_media: impl FnOnce() -> (
            Option<std::thread::JoinHandle<()>>,
            Option<std::thread::JoinHandle<()>>,
        ),
    ) -> String {
        // This owner also paces UDP reassessment. Keep it on the same scheduling
        // class as media so a busy machine can produce the requested probe rate.
        if accept.media_path_policy {
            crate::priority::media_thread();
        }
        if let Err(e) = tcp.set_write_timeout(Some(Duration::from_secs(2))) {
            return format!("cannot bound control writes: {e}");
        }
        let _ = tcp.set_nodelay(true);
        let mut writer = match tcp.try_clone() {
            Ok(w) => w,
            Err(e) => return format!("the connection failed: {e}"),
        };
        if let Err(e) = control::write_message(&mut writer, &Message::Accept(accept.clone())) {
            return format!("the viewer left before the stream started ({e})");
        }
        if let Some(problem) = input_problem {
            let _ = control::write_message(
                &mut writer,
                &Message::Notice(Notice {
                    kind: "input".into(),
                    message: problem,
                }),
            );
        }
        let (messages, inbox) = mpsc::sync_channel(128);
        let reader = tcp.try_clone();
        if let Ok(mut reader) = reader {
            let session = session.clone();
            let _ = std::thread::Builder::new()
                .name("stream-control".into())
                .spawn(move || loop {
                    match control::read_message(&mut reader) {
                        Ok(m) => {
                            // Wake input I/O and route maintenance even when the
                            // control owner is waiting for an input transaction.
                            if matches!(m, Message::End(_)) {
                                session.stop.store(true, Ordering::Release);
                            }
                            if messages.send(m).is_err() {
                                return;
                            }
                        }
                        Err(_) => {
                            session.stop.store(true, Ordering::Release);
                            return;
                        }
                    }
                });
        }
        *session.control_writer.lock() = Some(writer);
        let (video, audio) = start_media();
        let mut relay_controller = crate::congestion::RelayController::at_rate(
            accept.bitrate_kbps,
            if accept.media_path_policy {
                8000
            } else {
                accept.bitrate_kbps
            },
            Instant::now(),
        );
        let mut controller =
            crate::congestion::Controller::new(accept.bitrate_kbps, Instant::now());
        controller.enable_media_policy(accept.media_path_policy && accept.relay);
        let requested_bitrate = accept.bitrate_kbps;
        let mut fec_policy = crate::congestion::FecPolicy::default();
        let mut last_keepalive = Instant::now();
        let mut last_hello = Instant::now() - HELLO_EVERY;
        let mut last_ping = Instant::now();
        let mut train = 0u16;
        // Bytes the encoder had produced at the last viewer report.
        let mut sent_mark = (Instant::now(), session.bytes.load(Ordering::Relaxed));
        let mut route_revision = session.relay.revision();
        let mut relay_messages = VecDeque::new();
        let mut mtu = latch_stream::path::MtuProbe::new(Instant::now());
        let mut mtu_settle_until = Instant::now();
        let mut network_status = latch_stream::network::NetworkStatus::default();
        let mut network_notice = None;
        let reason = loop {
            let ended = session.ended.lock().clone();
            if let Some(e) = ended {
                break e.message;
            }
            let audio_notice = session.audio_notice.lock().take();
            if let Some(message) = audio_notice {
                if !session.send_control(&Message::Notice(Notice {
                    kind: "audio".into(),
                    message,
                })) {
                    break "the viewer's connection broke".to_string();
                }
            }
            for _ in 0..64 {
                match session.relay.poll() {
                    Some((revision, latch_relay::Event::Datagram(bytes))) => {
                        if revision != session.relay.revision() {
                            continue;
                        }
                        dispatch_datagram(self, session, &bytes, session.announced, true);
                    }
                    Some((revision, latch_relay::Event::Control(bytes))) => {
                        if revision != session.relay.revision() {
                            continue;
                        }
                        let mut source = bytes.as_slice();
                        match control::read_message(&mut source) {
                            Ok(
                                message @ (Message::Report(_)
                                | Message::Keepalive
                                | Message::DrawCursor(_)),
                            ) if source.is_empty() => {
                                if !session.queue_relay_control(
                                    &mut relay_messages,
                                    revision,
                                    message,
                                ) {
                                    break;
                                }
                            }
                            _ => {
                                session.end(end::TRANSPORT_FAILED, "Invalid relay control packet.")
                            }
                        }
                    }
                    Some(_) => {}
                    None => break,
                }
            }
            if !session
                .update_relay(|relay| relay.maintain(false, |message| session.send_tcp(message)))
            {
                break "the viewer's connection broke".to_string();
            }
            let status = session.relay.network_status();
            if status != network_status {
                for message in &status.messages {
                    self.log(message.clone());
                }
                network_status = status.clone();
            }
            if let Some(notice) = latch_stream::network::changed_host_notice(
                &mut network_notice,
                &status,
                session.relay.failure().as_ref(),
            ) {
                let _ = session.send_tcp(&Message::Notice(notice));
            }
            if route_revision != session.relay.revision() {
                route_revision = session.relay.revision();
                let returned = session
                    .relay
                    .take_return_kbps()
                    .filter(|_| accept.media_path_policy);
                controller = returned.map_or_else(
                    || crate::congestion::Controller::new(requested_bitrate, Instant::now()),
                    |rate| {
                        crate::congestion::Controller::at_rate(
                            requested_bitrate,
                            rate.min(requested_bitrate),
                            Instant::now(),
                        )
                    },
                );
                controller.enable_media_policy(accept.media_path_policy && accept.relay);
                relay_controller = crate::congestion::RelayController::at_rate(
                    requested_bitrate,
                    if accept.media_path_policy {
                        8000
                    } else {
                        requested_bitrate
                    },
                    Instant::now(),
                );
                fec_policy = crate::congestion::FecPolicy::default();
                *session.rtt.lock() = RttEstimator::new();
                *session.keyframe_requests.lock() = KeyframeAnswers::new();
                *session.rtt_low.lock() = None;
                *session.train_rtt.lock() = None;
                session.train_ping.store(0, Ordering::Relaxed);
                session.transport.probe.lock().take();
                session
                    .transport
                    .budget
                    .store(controller.budget(), Ordering::Relaxed);
                session.pace_kbps.store(0, Ordering::Relaxed);
                session.keyframe.store(true, Ordering::Release);
                sent_mark = (Instant::now(), session.bytes.load(Ordering::Relaxed));
            }
            let incoming = match inbox.try_recv() {
                Ok(message) => Ok(message),
                Err(mpsc::TryRecvError::Disconnected) => Err(mpsc::RecvTimeoutError::Disconnected),
                Err(mpsc::TryRecvError::Empty) => relay_messages
                    .pop_front()
                    .and_then(|(revision, message)| {
                        (revision == session.relay.revision()).then_some(message)
                    })
                    .map(Ok)
                    .unwrap_or_else(|| {
                        let wait = if session.relay.pacing_udp_probe() {
                            1
                        } else {
                            5
                        };
                        inbox.recv_timeout(Duration::from_millis(wait))
                    }),
            };
            match incoming {
                Ok(Message::Path(message))
                    if accept.media_path_policy && message.session == session.id =>
                {
                    if let control::PathAction::MtuResult { serial, received } = message.action {
                        if let Some(bytes) = mtu.result(serial, received) {
                            if bytes != session.relay.datagram_size() {
                                session.relay.set_datagram_size(bytes);
                                tracing::debug!(
                                    datagram_size = bytes,
                                    "measured UDP datagram size"
                                );
                                // Loss accounted for the old shard size must
                                // not constrain a newly measured safe path.
                                let now = Instant::now();
                                mtu_settle_until = now
                                    + Duration::from_secs(1)
                                        .max(controller.base_rtt().unwrap_or_default() * 2);
                                if !session.relay.active() {
                                    controller = crate::congestion::Controller::at_rate(
                                        requested_bitrate,
                                        requested_bitrate.min(8000),
                                        now,
                                    );
                                    controller.enable_media_policy(
                                        accept.media_path_policy && accept.relay,
                                    );
                                    fec_policy = crate::congestion::FecPolicy::default();
                                    session
                                        .transport
                                        .budget
                                        .store(controller.budget(), Ordering::Relaxed);
                                    session
                                        .pace_kbps
                                        .store(controller.pace_kbps(), Ordering::Relaxed);
                                    sent_mark = (now, session.bytes.load(Ordering::Relaxed));
                                }
                                if !session.send_tcp(&Message::Path(control::PathControl {
                                    session: session.id,
                                    action: control::PathAction::MtuApplied {
                                        datagram_size: bytes as u16,
                                    },
                                })) {
                                    break "the viewer's connection broke".to_string();
                                }
                            }
                        }
                    }
                }
                Ok(Message::Relay(message)) if accept.relay => {
                    if !session.update_relay(|relay| {
                        relay.handle(message, |message| session.send_tcp(message))
                    }) {
                        break "the viewer's connection broke".to_string();
                    }
                }
                Ok(Message::End(e)) => {
                    let message = if e.message.is_empty() {
                        "the viewer disconnected".to_string()
                    } else {
                        e.message
                    };
                    session.end(end::ENDED, &message);
                    break message;
                }
                Ok(Message::DrawCursor(d)) if session.cursor_supported => {
                    session.heard(self.now_us());
                    session.draw_cursor.store(d.draw, Ordering::Relaxed);
                }
                Ok(Message::Report(r)) => {
                    session.heard(self.now_us());
                    let host_rtt = session
                        .rtt_low
                        .lock()
                        .take()
                        .map_or(0.0, |d| d.as_secs_f32() * 1000.0);
                    let bytes = session.bytes.load(Ordering::Relaxed);
                    let now = Instant::now();
                    let (then, before) = std::mem::replace(&mut sent_mark, (now, bytes));
                    let secs = now.duration_since(then).as_secs_f64();
                    let sent_kbps = (secs >= 0.025).then(|| {
                        (bytes.saturating_sub(before) as f64 * 8.0 / 1000.0 / secs) as u32
                    });
                    if let Some(sent_kbps) = sent_kbps {
                        controller.observe_sent(sent_kbps);
                    }
                    let queue_ms =
                        session.transport.queue_us.swap(0, Ordering::Relaxed) as f32 / 1000.0;
                    let relay_ack_ms = session
                        .relay
                        .take_frame_ack_elapsed()
                        .map(|elapsed| elapsed.as_secs_f32() * 1000.0);
                    let queue_ms = queue_ms
                        .max(session.relay.queued_frame_age().as_secs_f32() * 1000.0)
                        .max(
                            if accept.media_path_policy && session.relay.video_backpressured() {
                                100.0
                            } else {
                                0.0
                            },
                        );
                    if let Some(rtt) = session.train_rtt.lock().take() {
                        controller.observe_train_rtt(rtt.as_secs_f32() * 1000.0, now);
                    }
                    if session.relay.active() {
                        session.relay.observe_relay_delivery(now, r.received_kbps);
                    }
                    let mut observed = r.clone();
                    if now < mtu_settle_until {
                        observed.loss_pct = 0.0;
                        observed.unrecovered_loss_pct = Some(0.0);
                        observed.frames_lost = 0;
                        observed.loss_burst_packets = 0;
                    }
                    let changed = if session.relay.active() {
                        let changed = relay_controller.update(
                            now,
                            queue_ms,
                            relay_ack_ms,
                            sent_kbps.unwrap_or(0),
                            r.received_kbps,
                        );
                        session.transport.budget.store(
                            session.relay.relay_media_budget(relay_controller.budget()),
                            Ordering::Relaxed,
                        );
                        changed
                    } else {
                        controller.update(now, &observed, host_rtt, queue_ms)
                    };
                    session
                        .transport
                        .probe_pressure
                        .store(controller.probe_pressure(), Ordering::Release);
                    if !session.relay.active() {
                        session
                            .transport
                            .budget
                            .store(controller.budget(), Ordering::Relaxed);
                    }
                    if changed {
                        tracing::debug!(
                            requested_bitrate,
                            wire_kbps = session.transport.budget.load(Ordering::Relaxed),
                            applied_kbps = session.transport.applied.load(Ordering::Relaxed),
                            host_rtt,
                            queue_ms,
                            relay_ack_ms,
                            sent_kbps,
                            received_kbps = r.received_kbps,
                            decoder_queue_ms = r.queue_ms,
                            udp_damage = controller.udp_damage(),
                            application_limited = controller.application_limited(),
                            relay = session.relay.active(),
                            loss = r.loss_pct,
                            "bitrate controller decision"
                        );
                    }
                    session.pace_kbps.store(
                        if session.relay.active() {
                            0
                        } else {
                            controller.pace_kbps()
                        },
                        Ordering::Relaxed,
                    );
                    let (fec, fec_burst) = fec_policy.update(&observed, &controller, now - then);
                    session.fec_burst.store(fec_burst, Ordering::Relaxed);
                    session.fec.store(
                        if session.relay.active() { 0 } else { fec },
                        Ordering::Relaxed,
                    );
                    let status = self.status();
                    let report = control::Report {
                        requested_bitrate_kbps: requested_bitrate,
                        wire_budget_kbps: session.transport.budget.load(Ordering::Relaxed),
                        udp_damage: accept.media_path_policy && controller.udp_damage(),
                        application_limited: if session.relay.active() {
                            sent_kbps.is_some_and(|sent| sent < relay_controller.budget() * 4 / 5)
                        } else {
                            controller.application_limited()
                        },
                        applied_bitrate_kbps: session.transport.applied.load(Ordering::Relaxed),
                        pacing_queue_ms: queue_ms,
                        send_errors: status.send_errors,
                        send_retries: status.send_retries,
                        socket_send_bytes: status.socket_send_bytes,
                        socket_receive_bytes: status.socket_receive_bytes,
                        ..Default::default()
                    };
                    if !session.send_control(&Message::Report(report)) {
                        break "the viewer's connection broke".to_string();
                    }
                }
                Ok(_) => session.heard(self.now_us()),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    break "the viewer disconnected".to_string()
                }
            }
            let now = Instant::now();
            if accept.media_path_policy
                && !mtu.send(
                    now,
                    session.id,
                    |action| {
                        session.send_tcp(&Message::Path(control::PathControl {
                            session: session.id,
                            action,
                        }))
                    },
                    |bytes| {
                        let _ = self
                            .udp
                            .send_to(bytes, (*session.addr.lock()).unwrap_or(session.announced));
                    },
                )
            {
                break "the viewer's connection broke".to_string();
            }
            if now - last_keepalive >= KEEPALIVE {
                if !session.send_control(&Message::Keepalive)
                    || (session.relay.active() && !session.send_tcp(&Message::Keepalive))
                {
                    break "the viewer's connection broke".to_string();
                }
                last_keepalive = now;
            }
            let addr = (*session.addr.lock())
                .or_else(|| session.relay.active().then_some(session.announced));
            match addr {
                None if !session.announced.ip().is_loopback()
                    && now - last_hello >= HELLO_EVERY =>
                {
                    let mut buf = Vec::with_capacity(16);
                    wire::write_hello(&mut buf, session.id, None);
                    let _ = self.udp.send_to(&buf, session.announced);
                    last_hello = now;
                }
                Some(addr) if now - last_ping >= PING_EVERY => {
                    let mut buf = Vec::with_capacity(16);
                    wire::write_ping(&mut buf, session.id, self.now_us());
                    send(&self.udp, session, &buf, addr);
                    last_ping = now;
                }
                _ => {}
            }
            let probe_addr = *session.addr.lock();
            if let Some(addr) = probe_addr {
                if !session
                    .relay
                    .reassess_udp(&self.udp, addr, requested_bitrate, |message| {
                        session.send_tcp(message)
                    })
                {
                    break "the viewer's connection broke".to_string();
                }
            }
            if session.relay.active() {
                session.transport.budget.store(
                    session.relay.relay_media_budget(relay_controller.budget()),
                    Ordering::Relaxed,
                );
            }
            // The video sender owns the ordinary idle probe trains.
            if addr.is_some() && session.probe_trains && !session.relay.active() {
                if let Some(count) = controller.probe_train(now) {
                    train = train.wrapping_add(1);
                    *session.transport.probe.lock() = Some((train, count));
                }
            }
            let silent = self
                .now_us()
                .saturating_sub(session.heard_us.load(Ordering::Relaxed));
            if silent >= SILENT.as_micros() as u64 {
                let e = End {
                    code: end::VIEWER_SILENT,
                    message: "The viewer stopped answering.".into(),
                };
                session.end(e.code, &e.message);
                break e.message;
            }
        };
        // Only an explicit End establishes normal completion. In particular,
        // a priority worker can mark the route fatal before it records code 6;
        // cleanup must not win that race with a non-retrying End. Preserve any
        // already-recorded Stop, takeover or media failure.
        session.end(end::TRANSPORT_FAILED, &reason);
        // Terminal lifecycle messages must reach the original control socket
        // before shutdown discards queued relay traffic. Release every state
        // guard before sending or running cleanup.
        let audio_notice = session.audio_notice.lock().take();
        if let Some(message) = audio_notice {
            let _ = session.send_tcp(&Message::Notice(Notice {
                kind: "audio".into(),
                message,
            }));
        }
        let ended = session.ended.lock().clone();
        if let Some(ended) = ended {
            let _ = session.send_tcp(&Message::End(ended));
        }
        session.stop.store(true, Ordering::Release);
        session.relay.shutdown();
        if let Some(mut w) = session.control_writer.lock().take() {
            let _ = w.flush();
        }
        // Close our side first and read until the viewer closes too, for at
        // most two report intervals: a report arriving at a fully closed
        // socket draws a reset, which can discard the End before the viewer
        // has read it (Windows does), and 50 ms reports are often in flight.
        let _ = tcp.shutdown(Shutdown::Write);
        let linger = Instant::now() + Duration::from_millis(100);
        while let Some(left) = linger.checked_duration_since(Instant::now()) {
            if let Err(mpsc::RecvTimeoutError::Disconnected) = inbox.recv_timeout(left) {
                break;
            }
        }
        let _ = tcp.shutdown(Shutdown::Both);
        let deadline = Instant::now() + MEDIA_STOP;
        for (what, thread) in [("video", video), ("audio", audio)] {
            if let Some(thread) = thread {
                if !join_by(thread, deadline) {
                    self.log(format!(
                        "the {what} thread did not stop within {MEDIA_STOP:?}; leaving it behind"
                    ));
                }
            }
        }
        reason
    }
}

/// An owned reservation, since Prepared crosses the HTTP upgrade callback.
#[derive(Default)]
struct Opening {
    busy: Mutex<bool>,
    cv: Condvar,
}

impl Opening {
    fn wait_idle(&self, deadline: Instant) -> bool {
        let mut busy = self.busy.lock();
        while *busy {
            if self.cv.wait_until(&mut busy, deadline).timed_out() && *busy {
                return false;
            }
        }
        true
    }

    /// The reservation, once the one holding it lets go; None after `limit`.
    fn lock(self: &Arc<Self>, limit: Duration) -> Option<OpeningGuard> {
        let deadline = Instant::now() + limit;
        let mut busy = self.busy.lock();
        while *busy {
            if self.cv.wait_until(&mut busy, deadline).timed_out() && *busy {
                return None;
            }
        }
        *busy = true;
        Some(OpeningGuard(self.clone()))
    }
}

/// Joins `thread` if it ends by `deadline`; otherwise leaves it to end by
/// itself and returns false.
fn join_by(thread: std::thread::JoinHandle<()>, deadline: Instant) -> bool {
    while !thread.is_finished() {
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    let _ = thread.join();
    true
}

struct OpeningGuard(Arc<Opening>);

impl Drop for OpeningGuard {
    fn drop(&mut self) {
        *self.0.busy.lock() = false;
        self.0.cv.notify_all();
    }
}

/// What [`Server::prepare`] opened, for [`Server::run`]. Dropped unused, it
/// undoes the desktop preparation.
pub struct Prepared {
    undo: Option<Box<dyn FnOnce() + Send>>,
    pipeline: Option<(Box<dyn VideoPipeline>, VideoConfig)>,
    audio: Option<Box<dyn AudioCapture>>,
    audio_problem: String,
    mic: Option<crate::mic::MicOut>,
    mic_problem: String,
    input: Option<Box<dyn InputSink>>,
    input_problem: Option<String>,
    pads: Option<gamepad::Opened>,
    pads_problem: String,
    feedback: Arc<FeedbackHub>,
    info: Option<VideoInfo>,
    codec: Codec,
    req: StreamRequest,
    peer: SocketAddr,
    opening: Option<OpeningGuard>,
}

impl Prepared {
    pub fn codec(&self) -> Codec {
        self.codec
    }
}

impl Drop for Prepared {
    fn drop(&mut self) {
        // Release capture before restoring the display, and keep the
        // reservation until both resource cleanup and undo have finished.
        self.pipeline.take();
        self.audio.take();
        self.input.take();
        self.pads.take();
        if let Some(undo) = self.undo.take() {
            undo();
        }
        self.opening.take();
    }
}

struct InputState {
    receiver: InputReceiver,
    sink: Option<Box<dyn InputSink>>,
    display: DisplayRect,
}

struct Session {
    relay: RelayRoute,
    id: u32,
    token: [u8; 16],
    viewer: String,
    /// The viewer's tailnet address and media port, for the host's Hello.
    announced: SocketAddr,
    /// Where media goes: the source of the viewer's valid Hello.
    addr: Mutex<Option<SocketAddr>>,
    /// Set once to end the session, with what to tell the viewer.
    ended: Mutex<Option<End>>,
    audio_notice: Mutex<Option<String>>,
    /// The media threads stop.
    stop: Arc<AtomicBool>,
    finished: Mutex<bool>,
    finished_cv: Condvar,
    keyframe: AtomicBool,
    input: Mutex<InputState>,
    pads: Mutex<SessionPads>,
    feedback: Arc<FeedbackHub>,
    rtt: Mutex<RttEstimator>,
    /// The lowest round trip since the last viewer report.
    rtt_low: Mutex<Option<Duration>>,
    train_ping: AtomicU64,
    train_rtt: Mutex<Option<Duration>>,
    /// How fast to send a frame's datagrams; 0 sends each frame at once.
    pace_kbps: AtomicU32,
    fec: AtomicU8,
    fec_burst: AtomicU8,
    /// When the viewer was last heard from, on the server's clock.
    heard_us: AtomicU64,
    frames: AtomicU64,
    video: Mutex<VideoDiagnostics>,
    bytes: AtomicU64,
    transport: TransportState,
    /// Which keyframe requests ask for a keyframe not already sent.
    keyframe_requests: Mutex<KeyframeAnswers>,
    recovery: RecoveryState,
    audio_enabled: bool,
    /// What surround sound needs beyond the stereo allowance the encoder
    /// budget already keeps for audio.
    surround_kbps: u32,
    /// The viewer's microphone: its packets put in order, and where they play.
    mic: Mutex<Option<(AudioReceiver, crate::mic::MicOut)>>,
    cursor_supported: bool,
    /// The viewer measures probe trains (`docs/PROTOCOL.md`, "Probe").
    probe_trains: bool,
    draw_cursor: AtomicBool,
    control_writer: Mutex<Option<TcpStream>>,
}

/// Reference invalidation (`docs/PROTOCOL.md`, "RecoveryRequest"), when
/// the viewer and the encoder both take it.
#[derive(Default)]
struct RecoveryState {
    enabled: bool,
    /// The newest request, for the encoder thread.
    request: Mutex<Option<wire::RecoveryRequest>>,
    /// The answer to the newest request, repeated to a repeat.
    answer: Mutex<Option<wire::RecoveryAck>>,
    /// The generation and picture a recovery frame was asked from, and the request.
    asked: Mutex<Option<(u64, u64, u32)>>,
    /// Frame numbers sent, pipeline generations and picture numbers,
    /// newest last.
    sent: Mutex<VecDeque<(u32, u64, u64)>>,
    /// Recovery frames sent and requests refused.
    frames: AtomicU64,
    refused: AtomicU64,
}

/// Frame numbers remembered for recovery: more than any encoder holds.
const RECOVERY_SENT: usize = 256;

impl RecoveryState {
    fn new(enabled: bool) -> Self {
        Self {
            enabled,
            ..Default::default()
        }
    }

    /// Answers request `id` (if it is still the newest) and remembers the
    /// answer for repeats.
    fn answer(&self, server: &Server, session: &Session, ack: wire::RecoveryAck) {
        {
            let mut answer = self.answer.lock();
            if answer.is_some_and(|a| a.id != ack.id) {
                return;
            }
            *answer = Some(ack);
        }
        if let Some(addr) =
            (*session.addr.lock()).or_else(|| session.relay.active().then_some(session.announced))
        {
            let mut out = Vec::with_capacity(16);
            wire::write_recovery_ack(&mut out, session.id, &ack);
            send(&server.udp, session, &out, addr);
        }
    }

    /// The encoder thread, before each frame: hands the newest request to
    /// the pipeline, or refuses it and asks for a keyframe.
    fn take(
        &self,
        server: &Server,
        session: &Session,
        pipeline: &mut dyn VideoPipeline,
        generation: u64,
    ) {
        let Some(request) = self.request.lock().take() else {
            return;
        };
        let picture = self
            .sent
            .lock()
            .iter()
            .rev()
            .find(|(frame, g, _)| *frame == request.last_delivered && *g == generation)
            .map(|&(_, _, picture)| picture)
            .filter(|&p| p != 0);
        let keyframe_owed = session.keyframe.load(Ordering::Acquire);
        match picture {
            Some(picture) if !keyframe_owed && pipeline.recover_from(picture) => {
                *self.asked.lock() = Some((generation, picture, request.id));
            }
            _ => {
                tracing::info!(
                    frame = request.last_delivered,
                    "recovery refused ({}); sending a keyframe",
                    if keyframe_owed {
                        "a keyframe is owed"
                    } else if picture.is_none() {
                        "frame not sent recently"
                    } else {
                        "the encoder no longer holds it"
                    }
                );
                self.refused.fetch_add(1, Ordering::Relaxed);
                session.keyframe.store(true, Ordering::Release);
                self.answer(
                    server,
                    session,
                    wire::RecoveryAck {
                        id: request.id,
                        status: wire::RecoveryStatus::Refused,
                        frame: 0,
                    },
                );
            }
        }
    }
}

/// Encoded frames that may be outstanding (being sent, or waiting to be)
/// before a paced session stops taking new pictures.
const PACED_BACKLOG: u32 = 1;

struct TransportState {
    probe: Mutex<Option<(u16, u8)>>,
    probe_pressure: AtomicBool,
    pending: AtomicU32,
    budget: AtomicU32,
    applied: AtomicU32,
    queue_us: AtomicU64,
    errors: AtomicU64,
    retries: AtomicU64,
}
impl TransportState {
    fn new(kbps: u32) -> Self {
        Self {
            probe: Mutex::new(None),
            probe_pressure: AtomicBool::new(false),
            pending: AtomicU32::new(0),
            budget: AtomicU32::new(kbps),
            applied: AtomicU32::new(0),
            queue_us: AtomicU64::new(0),
            errors: AtomicU64::new(0),
            retries: AtomicU64::new(0),
        }
    }
}

impl Session {
    fn update_relay(&self, update: impl FnOnce(&RelayRoute) -> bool) -> bool {
        // Both routes belong to the same authorized session. Keep held keys
        // and virtual devices alive, with input ordered across the transition.
        // Wait for in-flight input before applying and acknowledging the new
        // route. Deferred legacy cleanup could otherwise unplug a gamepad after
        // its replacement snapshot was already acknowledged. Check shutdown
        // between waits so input cleanup cannot hold the control owner forever.
        let mut input = loop {
            if self.stopping() {
                return true;
            }
            if let Some(input) = self.input.try_lock_for(Duration::from_millis(10)) {
                break input;
            }
        };
        let mut pads = self.pads.lock();
        let was_active = self.relay.active();
        let ok = update(&self.relay);
        if !self.relay.policy_enabled() && was_active && !self.relay.active() {
            if let Some(sink) = input.sink.as_mut() {
                sink.release_all();
            }
            if let Some(sink) = pads.sink.as_mut() {
                sink.unplug_all();
            }
        }
        ok
    }

    fn queue_relay_control(
        &self,
        inbox: &mut VecDeque<(u64, Message)>,
        revision: u64,
        message: Message,
    ) -> bool {
        if inbox.len() >= 128 {
            self.end(
                end::TRANSPORT_FAILED,
                "Relay control queue filled. Reconnecting to release input.",
            );
            return false;
        }
        inbox.push_back((revision, message));
        true
    }

    fn send_control(&self, msg: &Message) -> bool {
        match self.relay.send_control(msg) {
            Ok(true) => return true,
            Err(_) => {
                self.end(
                    end::TRANSPORT_FAILED,
                    "Relay priority queue failed. Reconnecting to release input.",
                );
                return false;
            }
            Ok(false) => {}
        }
        self.send_tcp(msg)
    }

    fn send_tcp(&self, msg: &Message) -> bool {
        let mut guard = self.control_writer.lock();
        if let Some(writer) = guard.as_mut() {
            control::write_message(writer, msg).is_ok() && writer.flush().is_ok()
        } else {
            false
        }
    }

    fn end(&self, code: u32, message: &str) {
        {
            let mut ended = self.ended.lock();
            if ended.is_some() {
                return;
            }
            // Let control and capture stop even while input cleanup is pending.
            *ended = Some(End {
                code,
                message: message.into(),
            });
        }
        self.stop.store(true, Ordering::Release);
        // An in-flight apply owns cleanup until it returns. The control owner
        // must be able to publish End and stop capture while that I/O unwinds.
        if let Some(mut input) = self.input.try_lock() {
            if let Some(mut sink) = input.sink.take() {
                sink.release_all();
            }
        }
        self.unplug_pads();
    }

    fn unplug_pads(&self) {
        if let Some(mut sink) = self.pads.lock().sink.take() {
            sink.unplug_all();
        }
    }

    fn heard(&self, now_us: u64) {
        self.heard_us.store(now_us, Ordering::Relaxed);
    }

    fn stopping(&self) -> bool {
        self.stop.load(Ordering::Acquire) || self.ended.lock().is_some()
    }

    fn mark_finished(&self) {
        *self.finished.lock() = true;
        self.finished_cv.notify_all();
    }

    fn wait_finished(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut finished = self.finished.lock();
        while !*finished {
            if self
                .finished_cv
                .wait_until(&mut finished, deadline)
                .timed_out()
            {
                return *finished;
            }
        }
        true
    }
}

/// The server's datagram loop: hellos, input, keyframe requests, pings.
fn receive(server: Weak<Server>) {
    crate::priority::media_thread();
    let mut buf = vec![0u8; latch_proto::MAX_DATAGRAM + 1];
    loop {
        let Some(server) = server.upgrade() else {
            return;
        };
        let (n, from) = match server.udp.recv_from(&mut buf) {
            Ok(r) => r,
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                // A lost microphone packet at the end of speech is given
                // up on by time.
                if let Some(session) = server.current.lock().clone() {
                    play_mic(&session, None);
                }
                continue;
            }
            // A reset from an earlier datagram to a closed port (Windows
            // reports those on the next receive).
            Err(_) => continue,
        };
        if let Some(session) = server.current.lock().clone() {
            dispatch_datagram(&server, &session, &buf[..n], from, false);
        };
    }
}

fn dispatch_datagram(
    server: &Server,
    session: &Session,
    bytes: &[u8],
    from: SocketAddr,
    via_relay: bool,
) {
    let mut out = Vec::with_capacity(64);
    let Ok(datagram) = wire::parse(bytes) else {
        return;
    };
    if datagram.session() != session.id {
        return;
    }
    if !via_relay
        && session.relay.active()
        && !matches!(datagram, Datagram::Hello { .. } | Datagram::Ping { .. })
    {
        return;
    }
    if let Datagram::Hello {
        token: Some(token), ..
    } = datagram
    {
        if *token == session.token {
            let mut addr = session.addr.lock();
            if *addr != Some(from) {
                if addr.is_some() {
                    server.log(format!("{} moved to {from}", session.viewer));
                }
                *addr = Some(from);
                session.keyframe.store(true, Ordering::Release);
            }
            drop(addr);
            session.heard(server.now_us());
        }
        return;
    }
    if !via_relay && *session.addr.lock() != Some(from) {
        return;
    }
    session.heard(server.now_us());
    match datagram {
        Datagram::Input { first, events, .. } => {
            if session.stopping() {
                return;
            }
            let next = {
                let mut guard = session.input.lock();
                if session.stopping() {
                    return;
                }
                let state = &mut *guard;
                let display = state.display;
                let sink = &mut state.sink;
                let next = state.receiver.try_receive(first, events.iter(), |event| {
                    if session.stopping() {
                        anyhow::bail!("the stream stopped during input");
                    }
                    let Some(sink) = sink.as_mut() else {
                        return Ok(());
                    };
                    if let Err(e) = sink.apply(&event, display) {
                        tracing::warn!("input {event:?}: {e:#}");
                        return Err(e);
                    }
                    Ok(())
                });
                if session.stopping() {
                    if let Some(mut sink) = state.sink.take() {
                        sink.release_all();
                    }
                }
                next
            };
            wire::write_input_ack(&mut out, session.id, next);
            send(&server.udp, session, &out, from);
        }
        Datagram::Pad { updates, .. } => {
            let mut acks = Vec::with_capacity(updates.count());
            {
                let mut guard = session.pads.lock();
                if session.stopping() {
                    return;
                }
                let pads = &mut *guard;
                for u in updates.iter() {
                    if u.slot >= pads.slots {
                        continue;
                    }
                    if pads.receiver.accept(u.slot, u.seq) {
                        if let Some(sink) = pads.sink.as_mut() {
                            let failing = &mut pads.failing[u.slot as usize];
                            match sink.update(u.slot, u.kind, &u.state) {
                                Ok(()) => *failing = false,
                                Err(e) if !*failing => {
                                    *failing = true;
                                    server.log(format!("gamepad {}: {e:#}", u.slot + 1));
                                }
                                Err(_) => {}
                            }
                        }
                    }
                    acks.extend(pads.receiver.ack(u.slot));
                }
            }
            if !acks.is_empty() {
                wire::write_pad_ack(&mut out, session.id, &acks);
                send(&server.udp, session, &out, from);
            }
        }
        Datagram::PadFeedbackAck { acks, .. } => {
            for ack in acks.iter() {
                session.feedback.ack(ack);
            }
        }
        Datagram::KeyframeRequest { last, .. } => {
            let rtt = {
                let rtt = session.rtt.lock();
                rtt.srtt().map(|srtt| srtt.saturating_sub(rtt.rttvar()))
            };
            let wanted = session
                .keyframe_requests
                .lock()
                .request(last, Instant::now(), rtt);
            if wanted {
                session.keyframe.store(true, Ordering::Release);
            }
        }
        Datagram::RecoveryRequest { request, .. } if session.recovery.enabled => {
            let ack = {
                let mut answer = session.recovery.answer.lock();
                match *answer {
                    Some(a) if a.id == request.id => Some(a),
                    Some(a) if latch_proto::video::before(request.id, a.id) => None,
                    _ => {
                        let a = wire::RecoveryAck {
                            id: request.id,
                            status: wire::RecoveryStatus::Pending,
                            frame: 0,
                        };
                        *answer = Some(a);
                        *session.recovery.request.lock() = Some(request);
                        Some(a)
                    }
                }
            };
            if let Some(ack) = ack {
                wire::write_recovery_ack(&mut out, session.id, &ack);
                send(&server.udp, session, &out, from);
            }
        }
        Datagram::Ping { time, .. } => {
            wire::write_pong(&mut out, session.id, time);
            if via_relay {
                send(&server.udp, session, &out, from);
            } else {
                let _ = server.udp.send_to(&out, from);
            }
        }
        Datagram::Mic { packet, .. } => play_mic(session, Some(&packet)),
        Datagram::Pong { time, .. } => {
            let rtt = Duration::from_micros(server.now_us().saturating_sub(time));
            if session
                .train_ping
                .compare_exchange(time, 0, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                *session.train_rtt.lock() = Some(rtt);
                return;
            }
            session.rtt.lock().sample(rtt);
            let mut low = session.rtt_low.lock();
            *low = Some(low.map_or(rtt, |low| low.min(rtt)));
        }
        _ => {}
    }
}

/// Puts a microphone packet in order and plays what is ready.
fn play_mic(session: &Session, packet: Option<&latch_proto::wire::AudioPacket<'_>>) {
    let mut mic = session.mic.lock();
    let Some((receiver, out)) = mic.as_mut() else {
        return;
    };
    let now = Instant::now();
    if let Some(packet) = packet {
        receiver.push(packet, now);
    }
    while let Some(frame) = receiver.pop(now) {
        match frame {
            AudioOut::Frame { payload, .. } => (out.push)(&payload),
            AudioOut::Missing { .. } => (out.push)(&[]),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct CursorWireState {
    visible: bool,
    serial: u32,
}
/// Latest cursor state with pending shapes retained across coalesced moves.
/// The capture worker never waits for a socket write or an encoded picture.
#[derive(Default)]
struct CursorMailbox {
    pending: Mutex<Option<control::Cursor>>,
    ready: Condvar,
    last: Mutex<Option<CursorWireState>>,
}
impl CursorMailbox {
    fn publish(&self, mut cursor: control::Cursor) {
        let mut last = self.last.lock();
        // Compare the shared wire state by serial without retaining pixels.
        let serial = cursor.shape.as_ref().map_or_else(
            || last.as_ref().map_or(0, |state| state.serial),
            |shape| shape.serial,
        );
        let state = CursorWireState {
            visible: cursor.visible,
            serial,
        };
        if *last == Some(state) {
            return;
        }
        *last = Some(state);
        let mut pending = self.pending.lock();
        if cursor.shape.is_none() {
            cursor.shape = pending.as_mut().and_then(|old| old.shape.take());
        }
        *pending = Some(cursor);
        self.ready.notify_one();
    }
}

fn send_video(
    server: &Server,
    session: &Session,
    pipeline: Box<dyn VideoPipeline>,
    config: VideoConfig,
    mut reopen: impl FnMut(&VideoConfig) -> anyhow::Result<Box<dyn VideoPipeline>>,
) {
    let mailbox = Arc::new(CursorMailbox::default());
    let done = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let sink: video::CursorSink = {
            let mailbox = mailbox.clone();
            Arc::new(move |cursor| mailbox.publish(cursor))
        };
        let mailbox = &mailbox;
        let done = &done;
        if session.cursor_supported {
            scope.spawn(move || {
                while !done.load(Ordering::Acquire) && !session.stopping() {
                    let mut pending = mailbox.pending.lock();
                    if pending.is_none() {
                        mailbox
                            .ready
                            .wait_for(&mut pending, Duration::from_millis(100));
                    }
                    let cursor = pending.take();
                    drop(pending);
                    if let Some(cursor) = cursor {
                        if !session.send_control(&Message::Cursor(cursor)) {
                            session.end(end::TRANSPORT_FAILED, "Cursor control connection failed.");
                            break;
                        }
                    }
                }
            });
        }
        let (tx, rx) = mpsc::sync_channel(2);
        scope.spawn(move || paced_video(server, session, rx));
        encode_video(server, session, pipeline, config, &mut reopen, tx, sink);
        done.store(true, Ordering::Release);
        mailbox.ready.notify_one();
    });
}

fn recovery_due(last: Option<Instant>, now: Instant, rtt: Option<Duration>) -> bool {
    let gap = rtt.unwrap_or_default().max(KEYFRAME_GAP);
    last.is_none_or(|at| now.saturating_duration_since(at) >= gap)
}

fn encode_video(
    server: &Server,
    session: &Session,
    mut pipeline: Box<dyn VideoPipeline>,
    mut config: VideoConfig,
    mut reopen: impl FnMut(&VideoConfig) -> anyhow::Result<Box<dyn VideoPipeline>>,
    tx: mpsc::SyncSender<(video::EncodedVideo, Instant)>,
    cursor_sink: video::CursorSink,
) {
    crate::priority::media_thread();
    if session.cursor_supported {
        pipeline.set_cursor_sink(cursor_sink.clone());
    }
    let mut generation = 0;
    let mut last_keyframe: Option<Instant> = None;
    let mut info = pipeline.info();
    let mut recoveries = VecDeque::new();
    let audio = session.audio_enabled;
    let mut applied = 0;
    while !session.stopping() {
        // Once pacing is needed, do not take another picture while one
        // encoded frame is already waiting behind the one being sent: it
        // would only go stale in the queue. One frame may still be encoded
        // while the previous one leaves, so that sending and encoding
        // overlap; waiting for the sender to drain completely made a frame
        // cost its send time plus its encode time, which lowered the frame
        // rate whenever pacing was on.
        while session.pace_kbps.load(Ordering::Relaxed) != 0
            && session.transport.pending.load(Ordering::Acquire) > PACED_BACKLOG
        {
            if session.stopping() {
                return;
            }
            video::sleep(Duration::from_micros(200));
        }
        let protected =
            session.keyframe.load(Ordering::Relaxed) || session.recovery.asked.lock().is_some();
        let wire_budget = session
            .transport
            .budget
            .load(Ordering::Relaxed)
            .saturating_sub(session.surround_kbps);
        let target = if session.relay.policy_enabled() && session.relay.active() {
            let priority = if audio {
                session
                    .relay
                    .priority_budget_kbps(Instant::now())
                    .unwrap_or(1200 + session.surround_kbps)
            } else {
                0
            };
            crate::congestion::relay_video_budget(
                session.transport.budget.load(Ordering::Relaxed),
                priority,
            )
        } else {
            crate::congestion::video_budget_sized(
                wire_budget,
                session.fec.load(Ordering::Relaxed),
                session.fec_burst.load(Ordering::Relaxed),
                config.fps,
                audio,
                protected,
                session.relay.datagram_size(),
            )
        };
        if applied == 0 || target.abs_diff(applied) >= (applied / 20).max(1) {
            pipeline.set_bitrate(target);
            config.bitrate_kbps = target;
            applied = target;
            session.transport.applied.store(target, Ordering::Relaxed);
        }
        pipeline.set_cursor_drawn(session.draw_cursor.load(Ordering::Relaxed));
        if session.recovery.enabled {
            session
                .recovery
                .take(server, session, pipeline.as_mut(), generation);
        }
        let due = recovery_due(last_keyframe, Instant::now(), session.rtt.lock().srtt());
        let keyframe = due && session.keyframe.swap(false, Ordering::AcqRel);
        // Short idle repeats bound how long a request arriving inside next
        // waits. Send every returned frame: discarding an encoded repeat
        // would discard a reference needed by subsequent P-frames.
        let producing = Instant::now();
        session.video.lock().producing_since = Some(producing);
        let produced = pipeline.next(keyframe, IDLE_REPEAT);
        {
            let mut diagnostics = session.video.lock();
            diagnostics.producing_since = None;
            let elapsed = producing.elapsed();
            diagnostics.produce_time = elapsed;
            diagnostics.total_produce_time += elapsed;
            diagnostics.max_produce_time =
                diagnostics.max_produce_time.max(diagnostics.produce_time);
            diagnostics.software = pipeline.software_timings();
            if diagnostics.produce_time >= Duration::from_millis(50) {
                tracing::debug!(
                    produce_ms = diagnostics.produce_time.as_secs_f64() * 1000.0,
                    software = ?diagnostics.software,
                    "slow video frame production"
                );
            }
            if let Ok(frame) = &produced {
                diagnostics.frames_produced += 1;
                diagnostics.encoded_bytes += frame.data.len() as u64;
            }
        }
        let mut frame = match produced {
            Ok(f) => f,
            Err(e) => {
                if session.stopping() {
                    return;
                }
                if e.is::<video::CaptureEnded>() {
                    session.end(end::CAPTURE_FAILED, &format!("Screen sharing ended: {e:#}"));
                    return;
                }
                if video::Unencodable::is(&e) {
                    server.log(format!("video stopped: {e:#}"));
                    session.end(end::ENCODER_FAILED, &unencodable(config.codec, &e));
                    return;
                }
                server.log(format!("video stopped: {e:#}; starting it again"));
                // Some capture APIs allow only one owner of an output.
                drop(pipeline);
                let mut failure = e;
                loop {
                    if session.stopping() {
                        return;
                    }
                    let now = Instant::now();
                    recoveries.retain(|t| now - *t < VIDEO_RECOVERY_WINDOW);
                    if recoveries.len() >= VIDEO_RECOVERIES {
                        session.end(
                            end::CAPTURE_FAILED,
                            &format!("The host could not capture its screen: {failure:#}"),
                        );
                        return;
                    }
                    recoveries.push_back(now);
                    match reopen(&config) {
                        Ok(p) => {
                            pipeline = p;
                            if session.cursor_supported {
                                pipeline.set_cursor_sink(cursor_sink.clone());
                            }
                            // Queued frames can still arrive from the old pipeline.
                            generation += 1;
                            session.recovery.sent.lock().clear();
                            session.recovery.asked.lock().take();
                            break;
                        }
                        Err(e) if video::Unencodable::is(&e) => {
                            server.log(format!("video reopen failed: {e:#}"));
                            session.end(end::ENCODER_FAILED, &unencodable(config.codec, &e));
                            return;
                        }
                        Err(e) => {
                            server.log(format!("video reopen failed: {e:#}"));
                            failure = e;
                            std::thread::sleep(Duration::from_millis(100));
                        }
                    }
                }
                session.keyframe.store(true, Ordering::Release);
                continue;
            }
        };
        let encoded_at = Instant::now();
        if frame.keyframe {
            if let Some(meta) = pipeline.info().color.hdr {
                let mut data = latch_proto::color::hevc_hdr_sei(meta);
                data.extend_from_slice(&frame.data);
                frame.data = data;
            }
        }
        frame.generation = generation;
        if let Some(update) = pipeline.take_cursor().filter(|_| session.cursor_supported) {
            session.send_control(&Message::Cursor(control::Cursor {
                visible: update.visible,
                shape: update.shape,
            }));
        }
        let next_info = pipeline.info();
        if next_info.color != info.color {
            session.end(
                end::ENCODER_FAILED,
                "Host colour format changed; reconnect to negotiate it",
            );
            return;
        }
        if next_info != info {
            {
                let mut input = session.input.lock();
                if let Some(sink) = &mut input.sink {
                    if next_info.display != info.display {
                        if let Err(e) =
                            sink.start_display(next_info.display, next_info.width, next_info.height)
                        {
                            tracing::warn!("move pointer into the captured display: {e:#}");
                        }
                    } else {
                        sink.set_stream_size(next_info.width, next_info.height);
                    }
                }
                input.display = next_info.display;
            }
            if next_info.encoder != info.encoder
                || next_info.hardware != info.hardware
                || next_info.captured_display != info.captured_display
            {
                let mut status = server.status.lock();
                status.encoder = next_info.encoder.clone();
                status.hardware = next_info.hardware;
                status.captured_display = next_info.captured_display.clone();
            }
            info = next_info;
        }
        if keyframe && !frame.keyframe {
            // The pipeline could not make this one a keyframe; ask again.
            session.keyframe.store(true, Ordering::Release);
        }
        if frame.keyframe {
            last_keyframe = Some(Instant::now());
        }
        if session.addr.lock().is_none() && !session.relay.active() {
            continue;
        }
        // Blocking is deliberate: dropping an encoded reference corrupts the GOP.
        session.transport.pending.fetch_add(1, Ordering::Release);
        if tx.send((frame, encoded_at)).is_err() {
            return;
        }
    }
}

fn paced_video(
    server: &Server,
    session: &Session,
    rx: mpsc::Receiver<(video::EncodedVideo, Instant)>,
) {
    crate::priority::media_thread();
    let mut packetizer = Packetizer::new(session.id);
    let mut sender = latch_proto::transport::SendBatch::default();
    let mut datagram_size = latch_proto::MAX_DATAGRAM;
    let mut pacer = crate::pacing::Pacer::new(Instant::now());
    let mut lost_reference = false;
    let mut relay_frame = 0u32;
    let mut route_revision = session.relay.revision();
    #[cfg(test)]
    let drop_stale = std::env::var_os("PB_DROP").is_some();
    while !session.stopping() {
        let probe = session.transport.probe.lock().take();
        let probe_addr = *session.addr.lock();
        if let (Some((train, count)), Some(addr)) = (probe, probe_addr) {
            pacer.begin_frame(Instant::now(), session.pace_kbps.load(Ordering::Relaxed));
            let mut packets = Vec::with_capacity(usize::from(count));
            for index in 0..count {
                let mut buf = Vec::with_capacity(latch_proto::MAX_DATAGRAM);
                wire::write_probe(
                    &mut buf,
                    session.id,
                    train,
                    index,
                    count,
                    latch_proto::MAX_DATAGRAM,
                );
                packets.push(buf);
            }
            if send_paced_batch(
                server,
                session,
                &mut sender,
                &mut pacer,
                &packets,
                addr,
                None,
            ) {
                let time = server.now_us();
                session.train_ping.store(time, Ordering::Relaxed);
                let mut ping = Vec::new();
                wire::write_ping(&mut ping, session.id, time);
                send_paced_batch(
                    server,
                    session,
                    &mut sender,
                    &mut pacer,
                    &[ping],
                    addr,
                    None,
                );
            }
        }
        let (frame, queued) = match rx.recv_timeout(Duration::from_millis(20)) {
            Ok(frame) => frame,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(_) => return,
        };
        // Also releases producer backpressure on every early return.
        struct Finished<'a>(&'a AtomicU32);
        impl Drop for Finished<'_> {
            fn drop(&mut self) {
                if self.0.load(Ordering::Relaxed) > 0 {
                    self.0.fetch_sub(1, Ordering::Release);
                }
            }
        }
        let _finished = Finished(&session.transport.pending);
        let Some(addr) =
            (*session.addr.lock()).or_else(|| session.relay.active().then_some(session.announced))
        else {
            continue;
        };
        #[cfg(test)]
        if drop_stale && queued.elapsed() > Duration::from_secs_f64(1.0 / 60.0) {
            recovery_lost(session);
            lost_reference = true;
            continue;
        }
        if frame.data.len() > latch_proto::video::MAX_FRAME_LEN {
            server.log(format!(
                "video frame ({} bytes) exceeds maximum {}; requesting a keyframe",
                frame.data.len(),
                latch_proto::video::MAX_FRAME_LEN
            ));
            recovery_lost(session);
            lost_reference = true;
            continue;
        }
        if route_revision != session.relay.revision() {
            route_revision = session.relay.revision();
            lost_reference = true;
            recovery_lost(session);
        }
        if lost_reference && !frame.keyframe {
            continue;
        }
        if session.relay.active() {
            relay_frame = relay_frame.wrapping_add(1);
            let complete = latch_proto::video::InFrame {
                frame: relay_frame,
                data: frame.data,
                keyframe: frame.keyframe,
                recovery: false,
                host_delay: (frame.captured.elapsed().as_micros() / 100).min(u16::MAX as u128)
                    as u16,
                assembly: Duration::ZERO,
            };
            session
                .transport
                .queue_us
                .fetch_max(queued.elapsed().as_micros() as u64, Ordering::Relaxed);
            match session.relay.send_frame(&complete) {
                Ok(true) => {
                    session
                        .bytes
                        .fetch_add(complete.data.len() as u64, Ordering::Relaxed);
                    session.frames.fetch_add(1, Ordering::Relaxed);
                    if complete.keyframe {
                        session.keyframe_requests.lock().sent(Instant::now());
                    }
                    lost_reference = false;
                }
                Ok(false) | Err(_) => {
                    lost_reference = true;
                    recovery_lost(session);
                }
            }
            continue;
        }
        pacer.begin_frame(Instant::now(), session.pace_kbps.load(Ordering::Relaxed));
        let measured_size = session.relay.datagram_size();
        if measured_size != datagram_size {
            datagram_size = measured_size;
            packetizer.set_datagram_size(datagram_size);
            // Software tunnels and firewalls can inspect a GSO aggregate
            // before segmentation. Preserve the measured size at that hop too.
            sender = if datagram_size < latch_proto::MAX_DATAGRAM {
                latch_proto::transport::SendBatch::with_offload(false)
            } else {
                latch_proto::transport::SendBatch::default()
            };
        }
        packetizer.set_fec_percent(session.fec.load(Ordering::Relaxed));
        packetizer.set_fec_burst(session.fec_burst.load(Ordering::Relaxed));
        let recovery = &session.recovery;
        let asked = frame.recovery.and_then(|base| {
            let (generation, picture, id) = recovery
                .asked
                .lock()
                .take_if(|(g, p, _)| *g == frame.generation && *p == base)?;
            let sent = recovery.sent.lock();
            let &(number, _, _) = sent
                .iter()
                .rev()
                .find(|(_, g, p)| *g == generation && *p == picture)?;
            packetizer.mark_recovery(number);
            Some(id)
        });
        let delay = (frame.captured.elapsed().as_micros() / 100).min(u16::MAX as u128) as u16;
        let out = OutFrame {
            data: &frame.data,
            keyframe: frame.keyframe,
            host_delay: delay,
        };
        let number = packetizer.next_frame();
        if recovery.enabled {
            let mut sent = recovery.sent.lock();
            if sent.len() >= RECOVERY_SENT {
                sent.pop_front();
            }
            sent.push_back((number, frame.generation, frame.picture));
        }
        if let Some(id) = asked {
            // Before the frame's datagrams, so the viewer hears it is
            // coming even if the frame is lost.
            recovery.frames.fetch_add(1, Ordering::Relaxed);
            recovery.answer(
                server,
                session,
                wire::RecoveryAck {
                    id,
                    status: wire::RecoveryStatus::Sent,
                    frame: number,
                },
            );
        }
        let result = packetizer.packetize_batches(&out, |batch| {
            send_paced_batch(
                server,
                session,
                &mut sender,
                &mut pacer,
                batch,
                addr,
                Some(queued),
            );
        });
        if let Err(error) = result {
            server.log(format!(
                "video frame {} ({} bytes) not sent: {error}; requesting a keyframe",
                number,
                frame.data.len()
            ));
            recovery_lost(session);
            lost_reference = true;
            continue;
        }
        lost_reference = false;
        #[cfg(test)]
        tests::SEND_SAMPLES.with(|s| {
            s.borrow_mut()
                .push((queued.elapsed().as_secs_f64() * 1000.0, frame.keyframe))
        });
        if frame.keyframe {
            session.keyframe_requests.lock().sent(Instant::now());
        }
        session.frames.fetch_add(1, Ordering::Relaxed);
    }
}

fn recovery_lost(session: &Session) {
    session.recovery.sent.lock().clear();
    session.recovery.asked.lock().take();
    session.keyframe.store(true, Ordering::Release);
}

/// None denotes an idle probe; motion and fresh pressure cancel it between batches.
fn send_paced_batch(
    server: &Server,
    session: &Session,
    sender: &mut latch_proto::transport::SendBatch,
    pacer: &mut crate::pacing::Pacer,
    batch: &[Vec<u8>],
    addr: SocketAddr,
    queued: Option<Instant>,
) -> bool {
    let mut sent = 0;
    while sent < batch.len() && !session.stopping() {
        let mut rate = session.pace_kbps.load(Ordering::Relaxed);
        if queued.is_none() {
            if session.transport.pending.load(Ordering::Acquire) != 0
                || session.transport.probe_pressure.load(Ordering::Acquire)
            {
                return false;
            }
            if rate == 0 {
                rate = session
                    .transport
                    .budget
                    .load(Ordering::Relaxed)
                    .saturating_mul(5)
                    / 2;
            }
        }
        let (count, wait) = pacer.batch(Instant::now(), &batch[sent..], rate);
        if count == 0 {
            let deadline = Instant::now() + wait;
            let started = Instant::now();
            video::sleep(deadline.saturating_duration_since(started));
            let now = Instant::now();
            let mut diagnostics = session.video.lock();
            diagnostics.pacing_waits += 1;
            diagnostics.pacing_wait += now.saturating_duration_since(started);
            diagnostics.pacing_max_lateness = diagnostics
                .pacing_max_lateness
                .max(now.saturating_duration_since(deadline));
            continue;
        }
        if let Some(queued) = queued {
            session
                .transport
                .queue_us
                .fetch_max(queued.elapsed().as_micros() as u64, Ordering::Relaxed);
        }
        send_batch(
            sender,
            &server.udp,
            session,
            &batch[sent..sent + count],
            addr,
        );
        sent += count;
    }
    sent == batch.len()
}

fn send_audio(
    server: &Server,
    session: &Session,
    capture: Box<dyn AudioCapture>,
    mut reopen: impl FnMut() -> anyhow::Result<Box<dyn AudioCapture>>,
) {
    crate::priority::media_thread();
    let mut packetizer = AudioPacketizer::new(session.id);
    let channels = capture.channels();
    let mut pcm = Vec::with_capacity(audio::FRAME * channels);
    let mut coded = Vec::with_capacity(2048);
    let mut capture = Some(capture);
    let mut backoff = Duration::from_millis(100);
    let mut problem = false;
    while !session.stopping() {
        if capture.is_none() {
            // Check stop during backoff so takeover never waits for a retry.
            let retry_at = Instant::now() + backoff;
            while !session.stopping() && Instant::now() < retry_at {
                std::thread::sleep(
                    Duration::from_millis(20)
                        .min(retry_at.saturating_duration_since(Instant::now())),
                );
            }
            if session.stopping() {
                return;
            }
            match reopen() {
                Ok(c) => capture = Some(c),
                Err(e) => {
                    audio_problem(server, session, format!("{e:#}"));
                    backoff = (backoff * 2).min(Duration::from_secs(5));
                    continue;
                }
            }
        }
        match capture
            .as_mut()
            .expect("open audio")
            .read_frame(&mut pcm, Duration::from_millis(100))
        {
            Ok(true) => {
                if problem {
                    audio_problem(server, session, String::new());
                    problem = false;
                }
                backoff = Duration::from_millis(100);
            }
            Ok(false) => continue,
            Err(e) => {
                server.log(format!("sound stopped: {e:#}"));
                capture.take();
                audio_problem(server, session, format!("{e:#}"));
                problem = true;
                backoff = (backoff * 2).min(Duration::from_secs(5));
                continue;
            }
        }
        let Some(addr) =
            (*session.addr.lock()).or_else(|| session.relay.active().then_some(session.announced))
        else {
            continue;
        };
        if pcm.len() != audio::FRAME * channels {
            // A capture that changed its layout under a running session.
            continue;
        }
        let pts = server.now_us() as u32;
        let samples = if session.relay.policy_enabled() {
            audio_packet_samples_with_limit(channels, session.relay.audio_datagram_size())
        } else {
            audio_packet_samples(channels)
        };
        let packet_us = (samples as u64 * 1_000_000 / u64::from(audio::RATE)) as u32;
        for (i, part) in pcm.chunks(samples * channels).enumerate() {
            coded.clear();
            latch_codec::audio::encode_channels(part, channels, &mut coded);
            let pts = pts.wrapping_add(i as u32 * packet_us);
            // Packets are sized so lpcm-rice's worst case fits; a refusal
            // would mean the codec changed underneath.
            let fits = packetizer.packetize(pts, &coded, |datagram| {
                send(&server.udp, session, datagram, addr);
            });
            debug_assert!(fits, "a coded audio frame outgrew a datagram");
        }
    }
}

fn audio_problem(server: &Server, session: &Session, message: String) {
    server.status.lock().audio_problem = message.clone();
    *session.audio_notice.lock() = Some(message);
}

/// Retry only an unsent suffix: batch syscalls can succeed partially.
fn send_batch(
    sender: &mut latch_proto::transport::SendBatch,
    udp: &UdpSocket,
    session: &Session,
    batch: &[Vec<u8>],
    to: SocketAddr,
) {
    if session.relay.active() {
        return;
    }
    let mut sent = 0;
    let mut failures = 0;
    while sent < batch.len() && !session.stopping() {
        #[cfg(all(test, target_os = "macos"))]
        tests::SEND_CALLS.with(|s| s.set(s.get() + (batch.len() - sent) as u64));
        match sender.send(udp, &batch[sent..], to) {
            Ok(n) if n > 0 => {
                let bytes: usize = batch[sent..sent + n].iter().map(Vec::len).sum();
                session.bytes.fetch_add(bytes as u64, Ordering::Relaxed);
                sent += n;
                failures = 0;
            }
            result => {
                session.transport.errors.fetch_add(1, Ordering::Relaxed);
                failures += 1;
                let retry = result
                    .err()
                    .is_some_and(|e| latch_proto::transport::send_buffer_full(&e));
                if !retry || failures >= 3 {
                    session.keyframe.store(true, Ordering::Release);
                    return;
                }
                session.transport.retries.fetch_add(1, Ordering::Relaxed);
                std::thread::sleep(Duration::from_micros(200));
            }
        }
    }
}

/// One datagram, retried briefly when the socket's buffer is full (macOS
/// says ENOBUFS when a keyframe outruns the interface).
fn send(udp: &UdpSocket, session: &Session, datagram: &[u8], to: SocketAddr) {
    match session.relay.send_datagram(datagram) {
        Ok(true) => return,
        Err(_) => {
            session.end(
                end::TRANSPORT_FAILED,
                "Relay priority queue failed. Reconnecting to release input.",
            );
            return;
        }
        Ok(false) => {}
    }
    for attempt in 0..3 {
        #[cfg(all(test, target_os = "macos"))]
        tests::SEND_CALLS.with(|s| s.set(s.get() + 1));
        match udp.send_to(datagram, to) {
            Ok(n) => {
                session.bytes.fetch_add(n as u64, Ordering::Relaxed);
                return;
            }
            Err(e) => {
                session.transport.errors.fetch_add(1, Ordering::Relaxed);
                if !latch_proto::transport::send_buffer_full(&e) || attempt == 2 {
                    session.keyframe.store(true, Ordering::Release);
                    return;
                }
                session.transport.retries.fetch_add(1, Ordering::Relaxed);
                std::thread::sleep(Duration::from_micros(200));
            }
        }
    }
}

fn even(v: u32) -> u32 {
    v & !1
}

/// Why a session ended for its codec: the viewer reconnects, and the host
/// negotiates another.
fn unencodable(codec: Codec, error: &anyhow::Error) -> String {
    format!(
        "The host's {} encoder cannot go on ({error:#}); reconnecting picks another codec.",
        codec.name()
    )
}

fn names(codecs: &[Codec]) -> String {
    if codecs.is_empty() {
        return "nothing".into();
    }
    codecs
        .iter()
        .map(|c| match c {
            Codec::Hevc => "HEVC",
            Codec::Av1 => "AV1",
            Codec::H264High => "H.264 High",
            Codec::H264Baseline => "H.264 Baseline",
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn display_name(name: &str, ip: IpAddr) -> String {
    let name = name.trim();
    if name.is_empty() {
        ip.to_string()
    } else {
        name.chars().take(64).collect()
    }
}

fn human_duration(d: Duration) -> String {
    let s = d.as_secs();
    if s < 60 {
        format!("{s} s")
    } else if s < 3600 {
        format!("{} min", s / 60)
    } else {
        format!("{} h {} min", s / 3600, s / 60 % 60)
    }
}

#[cfg(test)]
#[path = "../tests/support/lifecycle.rs"]
mod tests;
