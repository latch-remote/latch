//! VA-API decoding for Linux viewers, through the system's `libva.so.2`
//! and `libva-drm.so.2` loaded at run time (nothing is linked, so a
//! machine without libva or a GPU runs exactly as before).
//!
//! VA-API decodes slice data; the application parses the bitstream's
//! headers, keeps the reference pictures and hands the driver each
//! picture's parameters. That is done here in Rust: [`h264`] for
//! Constrained Baseline, Main and High, [`hevc`] for Main and Main 10.
//! AV1 is not decoded here yet (it needs its sequence and frame headers
//! parsed the same way).
//!
//! Successful probes are cached per process, with empty results retried after
//! a cooldown. Each render node is checked for profiles, entry points,
//! formats and largest pictures its driver reports, then a real picture of
//! every codec it claims, decoded and checked against its expected pixels; only what
//! passes is offered to hosts. Decoded pictures are exported as DMA-BUF planes when the EGL renderer
//! supports importing them. Otherwise `vaGetImage` (or a derived image)
//! supplies CPU planes for the renderer's NV12/P010 upload path.
//! `LATCH_LINUX_DECODER=software` turns VA-API off; `LATCH_VAAPI_DEVICE`
//! names a render node to try first.

#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

mod bits;
#[cfg(target_os = "linux")]
pub mod dmabuf;
#[cfg(target_os = "linux")]
mod driver;
mod h264;
mod hevc;
mod va;

#[cfg(target_os = "linux")]
pub use driver::{capabilities, open};

use super::{Frame, HardwareFailed, PixelFormat};
use anyhow::Result;
use latch_proto::control::Codec;
use va::Id;

/// The VA profiles this decoder drives.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Profile {
    H264ConstrainedBaseline,
    H264Main,
    H264High,
    HevcMain,
    HevcMain10,
}

impl Profile {
    pub const ALL: [Profile; 5] = [
        Profile::H264ConstrainedBaseline,
        Profile::H264Main,
        Profile::H264High,
        Profile::HevcMain,
        Profile::HevcMain10,
    ];

    pub fn va(self) -> i32 {
        match self {
            Self::H264ConstrainedBaseline => va::PROFILE_H264_CONSTRAINED_BASELINE,
            Self::H264Main => va::PROFILE_H264_MAIN,
            Self::H264High => va::PROFILE_H264_HIGH,
            Self::HevcMain => va::PROFILE_HEVC_MAIN,
            Self::HevcMain10 => va::PROFILE_HEVC_MAIN10,
        }
    }

    pub fn ten_bit(self) -> bool {
        self == Self::HevcMain10
    }

    /// The render target format a picture of this profile decodes to.
    pub fn rt_format(self) -> u32 {
        if self.ten_bit() {
            va::RT_FORMAT_YUV420_10
        } else {
            va::RT_FORMAT_YUV420
        }
    }

    /// Driver profiles that decode a stream of this one, least first.
    pub fn decoders(self) -> &'static [Profile] {
        match self {
            Self::H264ConstrainedBaseline => &[
                Self::H264ConstrainedBaseline,
                Self::H264Main,
                Self::H264High,
            ],
            Self::H264Main => &[Self::H264Main, Self::H264High],
            Self::H264High => &[Self::H264High],
            Self::HevcMain => &[Self::HevcMain],
            Self::HevcMain10 => &[Self::HevcMain10],
        }
    }

    /// The stream profile a negotiated codec is decoded as.
    pub fn for_codec(codec: Codec, ten_bit: bool) -> Option<Self> {
        match (codec, ten_bit) {
            (Codec::H264Baseline, false) => Some(Self::H264ConstrainedBaseline),
            (Codec::H264High, false) => Some(Self::H264High),
            (Codec::Hevc, false) => Some(Self::HevcMain),
            (Codec::Hevc, true) => Some(Self::HevcMain10),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// What a stream's pictures need of the hardware.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Format {
    /// The least profile that decodes the stream (see [`Profile::decoders`]).
    pub profile: Profile,
    /// Coded size.
    pub width: u32,
    pub height: u32,
    pub surfaces: usize,
    pub visible: Rect,
}

/// A decoded picture, still on its surface.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Decoded {
    pub surface: Id,
    pub visible: Rect,
    pub ten_bit: bool,
}

/// The hardware side of decoding, which the bitstream side drives. Tests
/// stand in a recorder for the driver.
pub trait Backend {
    /// A renderer may still be reading this surface.
    fn available(&self, _surface: Id) -> bool {
        true
    }

    /// Surfaces (and a context) for pictures of `format`, replacing any.
    fn configure(&mut self, format: &Format) -> Result<Vec<Id>>;
    fn h264(
        &mut self,
        target: Id,
        picture: &va::PictureParameterH264,
        iq: &va::IqMatrixH264,
        slices: &[(va::SliceH264, &[u8])],
    ) -> Result<()>;
    fn hevc(
        &mut self,
        target: Id,
        picture: &va::PictureParameterHevc,
        iq: Option<&va::IqMatrixHevc>,
        slices: &[(va::SliceHevc, &[u8])],
        subsets: &[u32],
    ) -> Result<()>;
}

/// A driver error, as the hardware's rather than the stream's.
fn hardware(e: anyhow::Error) -> anyhow::Error {
    if e.is::<HardwareFailed>() {
        e
    } else {
        HardwareFailed(format!("{e:#}")).into()
    }
}

/// The bitstream side of one stream.
pub enum Bitstream {
    H264(Box<h264::H264>),
    Hevc(Box<hevc::Hevc>),
}

impl Bitstream {
    pub fn new(codec: Codec) -> Result<Self> {
        Ok(match codec {
            Codec::H264Baseline | Codec::H264High => Self::H264(Box::new(h264::H264::new())),
            Codec::Hevc => Self::Hevc(Box::new(hevc::Hevc::new())),
            other => anyhow::bail!("VA-API does not decode {} here", other.name()),
        })
    }

    pub fn decode(&mut self, au: &[u8], backend: &mut dyn Backend) -> Result<Option<Decoded>> {
        match self {
            Self::H264(h) => h.decode(au, backend),
            Self::Hevc(h) => h.decode(au, backend),
        }
    }
}

/// What a driver reports for one VA profile.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Report {
    pub profile: i32,
    /// It has the decode (VLD) entry point.
    pub vld: bool,
    /// `VAConfigAttribRTFormat`, when the driver says.
    pub rt_formats: Option<u32>,
    pub max_width: Option<u32>,
    pub max_height: Option<u32>,
}

/// The largest picture a driver decodes in a profile; 0 where it does
/// not say (the context then refuses what it cannot take).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limit {
    pub profile: Profile,
    pub max_width: u32,
    pub max_height: u32,
}

impl Limit {
    pub fn fits(&self, width: u32, height: u32) -> bool {
        (self.max_width == 0 || width <= self.max_width)
            && (self.max_height == 0 || height <= self.max_height)
    }
}

/// The profiles a driver decodes, from what it reports of each.
pub fn limits(reports: &[Report]) -> Vec<Limit> {
    Profile::ALL
        .into_iter()
        .filter_map(|profile| {
            let r = reports.iter().find(|r| r.profile == profile.va())?;
            let formats_ok = r.rt_formats.is_none_or(|f| f & profile.rt_format() != 0);
            (r.vld && formats_ok).then(|| Limit {
                profile,
                max_width: r.max_width.unwrap_or(0),
                max_height: r.max_height.unwrap_or(0),
            })
        })
        .collect()
}

/// The driver profile that decodes a stream of `profile`, if any.
pub fn decoder_for(limits: &[Limit], profile: Profile) -> Option<Limit> {
    profile
        .decoders()
        .iter()
        .find_map(|&p| limits.iter().find(|l| l.profile == p).copied())
}

/// A codec a render node was seen to decode.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Capability {
    pub codec: Codec,
    pub ten_bit: bool,
    pub node: std::path::PathBuf,
    pub limit: Limit,
}

/// What to offer hosts, best first: HEVC and H.264 High where a probe
/// decoded them, Constrained Baseline always (in software if need be).
pub fn codecs(caps: &[Capability]) -> Vec<Codec> {
    let mut out: Vec<Codec> = [Codec::Hevc, Codec::H264High]
        .into_iter()
        .filter(|&c| caps.iter().any(|cap| cap.codec == c && !cap.ten_bit))
        .collect();
    out.push(Codec::H264Baseline);
    out
}

/// Whether a stream of `codec` at this size can be decoded: by a probed
/// decoder that takes the size, or in software for Constrained Baseline.
pub fn fits(caps: &[Capability], codec: Codec, width: u32, height: u32) -> bool {
    codec == Codec::H264Baseline
        || caps
            .iter()
            .any(|c| c.codec == codec && !c.ten_bit && c.limit.fits(width, height))
}

/// Restrict display colour capabilities to a probed Main10 decoder that
/// accepts the requested dimensions.
pub fn color(
    caps: &[Capability],
    mut requested: latch_proto::color::ColorCapabilities,
    width: u32,
    height: u32,
) -> latch_proto::color::ColorCapabilities {
    requested.ten_bit &= caps
        .iter()
        .any(|c| c.codec == Codec::Hevc && c.ten_bit && c.limit.fits(width, height));
    requested.hdr &= requested.ten_bit;
    requested
}

/// The capability a stream opens with.
pub fn capability(
    caps: &[Capability],
    codec: Codec,
    ten_bit: bool,
    width: u32,
    height: u32,
) -> Result<&Capability> {
    let mut known = caps
        .iter()
        .filter(|c| c.codec == codec && c.ten_bit == ten_bit)
        .peekable();
    anyhow::ensure!(
        known.peek().is_some(),
        "VA-API cannot decode {}{} here",
        if ten_bit { "10-bit " } else { "" },
        codec.name()
    );
    known.find(|c| c.limit.fits(width, height)).ok_or_else(|| {
        anyhow::anyhow!(
            "VA-API decodes {} only up to {}×{}, not {width}×{height}",
            codec.name(),
            caps.iter()
                .find(|c| c.codec == codec)
                .map_or(0, |c| c.limit.max_width),
            caps.iter()
                .find(|c| c.codec == codec)
                .map_or(0, |c| c.limit.max_height),
        )
    })
}

/// Whether VA-API should be used, from `LATCH_LINUX_DECODER`.
pub fn wanted(setting: Option<&str>) -> bool {
    match setting.map(str::trim) {
        None | Some("") | Some("auto") | Some("vaapi") => true,
        Some("software") => false,
        Some(other) => {
            tracing::warn!(
                "LATCH_LINUX_DECODER={other} is not auto, vaapi or software; using auto"
            );
            true
        }
    }
}

/// The probe pictures are white above and black below: a decoder that
/// gives anything else is not decoding them (a misread header, a driver
/// that returns garbage), and is not offered.
pub fn probe_picture_ok(frame: &Frame, width: u32, height: u32) -> bool {
    if (frame.width, frame.height) != (width, height) {
        return false;
    }
    let bytes = frame.format.bytes_per_sample();
    frame.with_planes(|y, y_stride, uv, uv_stride| {
        let sample = |plane: &[u8], stride: usize, x: usize, row: usize| -> Option<u8> {
            // The high byte of a P010 sample, or the NV12 byte.
            plane.get(row * stride + x * bytes + bytes - 1).copied()
        };
        let (w, h) = (width as usize, height as usize);
        let points = [(w / 4, h / 4), (w / 2, h / 8), (3 * w / 4, h / 3)];
        let top = points
            .iter()
            .all(|&(x, r)| sample(y, y_stride, x, r).is_some_and(|v| v >= 200));
        let bottom = points
            .iter()
            .all(|&(x, r)| sample(y, y_stride, x, h - 1 - r).is_some_and(|v| v <= 40));
        let neutral = points.iter().all(|&(x, r)| {
            let (cx, cr) = (x / 2 * 2, r / 2);
            [0, 1]
                .iter()
                .all(|&c| sample(uv, uv_stride, cx + c, cr).is_some_and(|v| v.abs_diff(128) <= 12))
        });
        top && bottom && neutral
    })
}

/// A Constrained Baseline probe picture like the fixtures, from
/// `latch-codec`'s encoder (the stream a Baseline viewer gets): 256×256.
pub fn probe_baseline() -> (Vec<u8>, u32, u32) {
    let (width, height) = (256, 256);
    let mut picture = latch_codec::Yuv420::new(width, height);
    let half = (width * height / 2) as usize;
    picture.y[..half].fill(235);
    picture.y[half..].fill(16);
    let mut out = Vec::new();
    if let Ok(mut encoder) = latch_codec::h264::Encoder::new(latch_codec::h264::EncoderConfig {
        width,
        height,
        fps: 30,
        bitrate_kbps: 2_000,
        threads: 1,
    }) {
        let _ = encoder.encode(&picture, true, &mut out);
    }
    (out, width, height)
}

/// Copy a picture's visible part out of mapped image memory.
#[allow(clippy::too_many_arguments)]
pub fn copy_planes(
    bytes: &[u8],
    offsets: [usize; 2],
    pitches: [usize; 2],
    visible: Rect,
    ten_bit: bool,
    spare: Option<Frame>,
) -> Result<Frame> {
    let sample = if ten_bit { 2 } else { 1 };
    let (x, y) = (visible.x as usize & !1, visible.y as usize & !1);
    let (w, h) = (visible.width as usize, visible.height as usize);
    anyhow::ensure!(
        w > 0 && h > 0 && visible.x.is_multiple_of(2) && visible.y.is_multiple_of(2),
        "VA-API image has an empty or unaligned crop"
    );
    let widths = [w * sample, w.div_ceil(2) * 2 * sample];
    let rows = [h, h.div_ceil(2)];
    let origins = [y, y / 2];
    let mut planes = [Vec::new(), Vec::new()];
    let mut frame = spare.unwrap_or_default();
    std::mem::swap(&mut planes[0], &mut frame.y);
    std::mem::swap(&mut planes[1], &mut frame.uv);
    for i in 0..2 {
        let start = origins[i]
            .checked_mul(pitches[i])
            .and_then(|n| n.checked_add(offsets[i]))
            .and_then(|n| n.checked_add(x * sample));
        let end = start.and_then(|n| {
            (rows[i] - 1)
                .checked_mul(pitches[i])
                .and_then(|last| n.checked_add(last))
                .and_then(|last| last.checked_add(widths[i]))
        });
        anyhow::ensure!(
            pitches[i] >= x * sample + widths[i] && end.is_some_and(|end| end <= bytes.len()),
            "VA-API image layout does not hold the picture"
        );
        let start = start.unwrap();
        planes[i].clear();
        for row in 0..rows[i] {
            let at = start + row * pitches[i];
            planes[i].extend_from_slice(&bytes[at..at + widths[i]]);
        }
    }
    frame.width = visible.width;
    frame.height = visible.height;
    frame.y_stride = widths[0];
    frame.uv_stride = widths[1];
    frame.full_range = false;
    frame.color = Default::default();
    frame.format = PixelFormat::Nv12;
    frame.y = std::mem::take(&mut planes[0]);
    frame.uv = std::mem::take(&mut planes[1]);
    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    {
        frame.native = None;
    }
    if ten_bit {
        frame.format = PixelFormat::P010;
        frame.color.bit_depth = 10;
    }
    Ok(frame)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone)]
    pub struct H264Picture {
        pub target: Id,
        pub picture: va::PictureParameterH264,
        pub iq: va::IqMatrixH264,
        pub slices: Vec<(va::SliceH264, Vec<u8>)>,
    }

    #[derive(Clone)]
    pub struct HevcPicture {
        pub target: Id,
        pub picture: va::PictureParameterHevc,
        pub iq: Option<va::IqMatrixHevc>,
        pub slices: Vec<(va::SliceHevc, Vec<u8>)>,
        pub subsets: Vec<u32>,
    }

    /// A backend that keeps what it is given, and fails when told to.
    #[derive(Default)]
    pub struct Recorder {
        pub formats: Vec<Format>,
        pub h264: Vec<H264Picture>,
        pub hevc: Vec<HevcPicture>,
        pub fail: bool,
    }

    impl Backend for Recorder {
        fn configure(&mut self, format: &Format) -> Result<Vec<Id>> {
            anyhow::ensure!(!self.fail, "the device went away");
            self.formats.push(format.clone());
            let base = 1000 * self.formats.len() as u32;
            Ok((0..format.surfaces as u32).map(|i| base + i).collect())
        }

        fn h264(
            &mut self,
            target: Id,
            picture: &va::PictureParameterH264,
            iq: &va::IqMatrixH264,
            slices: &[(va::SliceH264, &[u8])],
        ) -> Result<()> {
            anyhow::ensure!(!self.fail, "the device went away");
            self.h264.push(H264Picture {
                target,
                picture: *picture,
                iq: *iq,
                slices: slices.iter().map(|(s, d)| (*s, d.to_vec())).collect(),
            });
            Ok(())
        }

        fn hevc(
            &mut self,
            target: Id,
            picture: &va::PictureParameterHevc,
            iq: Option<&va::IqMatrixHevc>,
            slices: &[(va::SliceHevc, &[u8])],
            subsets: &[u32],
        ) -> Result<()> {
            anyhow::ensure!(!self.fail, "the device went away");
            self.hevc.push(HevcPicture {
                target,
                picture: *picture,
                iq: iq.copied(),
                subsets: subsets.to_vec(),
                slices: slices.iter().map(|(s, d)| (*s, d.to_vec())).collect(),
            });
            Ok(())
        }
    }

    fn report(profile: Profile, rt: Option<u32>, size: Option<u32>) -> Report {
        Report {
            profile: profile.va(),
            vld: true,
            rt_formats: rt,
            max_width: size,
            max_height: size,
        }
    }

    #[test]
    fn driver_reports_become_profiles_with_their_limits() {
        let reports = [
            report(Profile::H264High, Some(va::RT_FORMAT_YUV420), Some(4096)),
            // A driver that lists Main10 but no 10-bit render target.
            report(Profile::HevcMain10, Some(va::RT_FORMAT_YUV420), Some(8192)),
            report(Profile::HevcMain, None, None),
            // Encode-only (no VLD entry point).
            Report {
                vld: false,
                ..report(Profile::H264Main, None, None)
            },
            Report {
                profile: 99,
                ..Default::default()
            },
        ];
        let limits = limits(&reports);
        assert_eq!(
            limits,
            [
                Limit {
                    profile: Profile::H264High,
                    max_width: 4096,
                    max_height: 4096
                },
                Limit {
                    profile: Profile::HevcMain,
                    max_width: 0,
                    max_height: 0
                },
            ]
        );
        // Constrained Baseline decodes on High when that is all there is.
        assert_eq!(
            decoder_for(&limits, Profile::H264ConstrainedBaseline).map(|l| l.profile),
            Some(Profile::H264High)
        );
        assert_eq!(decoder_for(&limits, Profile::HevcMain10), None);
        assert!(limits[0].fits(3840, 2160));
        assert!(!limits[0].fits(5120, 1440));
        assert!(limits[1].fits(16384, 16384), "no limit said, none assumed");
    }

    fn cap(codec: Codec, ten_bit: bool, size: u32) -> Capability {
        Capability {
            codec,
            ten_bit,
            node: "/dev/dri/renderD128".into(),
            limit: Limit {
                profile: Profile::for_codec(codec, ten_bit).unwrap(),
                max_width: size,
                max_height: size,
            },
        }
    }

    #[test]
    fn negotiation_offers_what_was_probed_at_sizes_it_takes() {
        assert_eq!(codecs(&[]), [Codec::H264Baseline], "no GPU: as before");
        let caps = [
            cap(Codec::H264High, false, 4096),
            cap(Codec::Hevc, false, 8192),
            cap(Codec::Hevc, true, 8192),
        ];
        assert_eq!(
            codecs(&caps),
            [Codec::Hevc, Codec::H264High, Codec::H264Baseline]
        );
        assert!(fits(&caps, Codec::H264High, 3840, 2160));
        assert!(
            !fits(&caps, Codec::H264High, 7680, 4320),
            "too large for H.264"
        );
        assert!(fits(&caps, Codec::Hevc, 7680, 4320));
        assert!(fits(&caps, Codec::H264Baseline, 16384, 16384), "software");
        assert!(!fits(&caps, Codec::Av1, 640, 480));
        assert!(
            capability(&caps, Codec::Hevc, true, 3840, 2160)
                .unwrap()
                .ten_bit
        );
        let error = capability(&caps, Codec::H264High, false, 7680, 4320).unwrap_err();
        assert!(format!("{error}").contains("4096×4096"), "{error}");
        assert!(capability(&caps[..1], Codec::Hevc, false, 64, 64).is_err());
    }

    #[test]
    fn color_negotiation_requires_main10_at_the_requested_size() {
        let requested = latch_proto::color::ColorCapabilities {
            ten_bit: true,
            hdr: true,
            peak_nits: 1000,
        };
        let caps = [cap(Codec::Hevc, false, 8192), cap(Codec::Hevc, true, 4096)];
        assert_eq!(color(&caps, requested, 3840, 2160), requested);
        assert!(!color(&caps, requested, 7680, 4320).ten_bit);
        assert!(!color(&caps, requested, 7680, 4320).hdr);
        assert!(!color(&caps[..1], requested, 1920, 1080).ten_bit);
        assert!(!color(&[], requested, 1920, 1080).hdr);
        let display = latch_proto::color::ColorCapabilities::default();
        assert_eq!(color(&caps, display, 1920, 1080), display);
    }

    #[test]
    fn only_software_turns_vaapi_off() {
        assert!(wanted(None));
        assert!(wanted(Some("auto")));
        assert!(wanted(Some("vaapi")));
        assert!(!wanted(Some("software")));
        assert!(wanted(Some("nonsense")));
    }

    #[test]
    fn hardware_errors_are_marked_once() {
        let e = hardware(anyhow::anyhow!("vaEndPicture: operation failed"));
        assert!(e.is::<HardwareFailed>());
        let again = hardware(e);
        assert!(format!("{again}").starts_with("vaEndPicture"));
    }

    /// An NV12 image the way a driver lays it out: padded rows, the UV
    /// plane at an aligned offset.
    fn image(width: usize, height: usize, pitch: usize, ten_bit: bool) -> (Vec<u8>, [usize; 2]) {
        let s = if ten_bit { 2 } else { 1 };
        let uv_at = pitch * (height + 16);
        let mut bytes = vec![0u8; uv_at + pitch * height / 2];
        for row in 0..height {
            for x in 0..width {
                let v = if row < height / 2 { 235 } else { 16 };
                bytes[row * pitch + x * s + s - 1] = v;
            }
        }
        for row in 0..height / 2 {
            for x in 0..width {
                bytes[uv_at + row * pitch + x * s + s - 1] = 128;
            }
        }
        (bytes, [0, uv_at])
    }

    #[test]
    fn readback_keeps_the_visible_picture_and_checks_the_probe_pattern() {
        let (bytes, offsets) = image(256, 256, 320, false);
        let visible = Rect {
            x: 0,
            y: 0,
            width: 256,
            height: 256,
        };
        let frame = copy_planes(&bytes, offsets, [320, 320], visible, false, None).unwrap();
        assert_eq!(
            (frame.width, frame.y_stride, frame.format),
            (256, 256, PixelFormat::Nv12)
        );
        assert!(probe_picture_ok(&frame, 256, 256));
        assert!(!probe_picture_ok(&frame, 256, 128), "the wrong size");
        // Upside down is not the probe picture.
        let flipped = Frame {
            y: frame.y.chunks(256).rev().flatten().copied().collect(),
            ..copy_planes(&bytes, offsets, [320, 320], visible, false, None).unwrap()
        };
        assert!(!probe_picture_ok(&flipped, 256, 256));
        // Ten bits: P010, two bytes a sample, the depth said.
        let (bytes, offsets) = image(256, 256, 640, true);
        let frame = copy_planes(&bytes, offsets, [640, 640], visible, true, None).unwrap();
        assert_eq!(
            (frame.format, frame.color.bit_depth),
            (PixelFormat::P010, 10)
        );
        assert!(probe_picture_ok(&frame, 256, 256));
        // A crop is honoured, and a layout that cannot hold it refused.
        let crop = Rect {
            x: 0,
            y: 8,
            width: 250,
            height: 240,
        };
        let frame = copy_planes(&bytes, offsets, [640, 640], crop, true, None).unwrap();
        assert_eq!((frame.width, frame.height), (250, 240));
        assert_eq!(frame.y[1], 235);
        assert!(copy_planes(&bytes[..1000], offsets, [640, 640], visible, true, None).is_err());
        assert!(copy_planes(&bytes, offsets, [200, 640], visible, true, None).is_err());
    }

    #[test]
    fn readback_crops_last_rows_without_requiring_trailing_padding() {
        let bytes = [1, 2, 3, 4, 0, 0, 5, 6, 7, 8, 0, 0, 128, 129, 130, 131];
        let visible = Rect {
            x: 2,
            y: 0,
            width: 2,
            height: 2,
        };
        let frame = copy_planes(&bytes, [0, 12], [6, 4], visible, false, None).unwrap();
        assert_eq!(frame.y, [3, 4, 7, 8]);
        assert_eq!(frame.uv, [130, 131]);
        assert_eq!((frame.y_stride, frame.uv_stride), (2, 2));
        assert!(copy_planes(&bytes, [usize::MAX, 12], [6, 4], visible, false, None).is_err());
        assert!(copy_planes(&bytes, [0, 12], [usize::MAX, 4], visible, false, None).is_err());
    }

    #[test]
    fn malformed_and_truncated_bitstreams_return_errors_without_panicking() {
        for (codec, bytes) in [
            (
                Codec::H264High,
                include_bytes!("../../tests/fixtures/decode-probe.h264").as_slice(),
            ),
            (
                Codec::Hevc,
                include_bytes!("../../tests/fixtures/decode-probe.hevc").as_slice(),
            ),
        ] {
            for end in 0..bytes.len() {
                let _ = Bitstream::new(codec)
                    .unwrap()
                    .decode(&bytes[..end], &mut Recorder::default());
            }
            for bit in 0..bytes.len().min(128) * 8 {
                let mut corrupted = bytes.to_vec();
                corrupted[bit / 8] ^= 1 << (bit % 8);
                let _ = Bitstream::new(codec)
                    .unwrap()
                    .decode(&corrupted, &mut Recorder::default());
            }
        }
    }

    #[test]
    fn the_software_decoders_picture_passes_the_probe_check() {
        let (picture, width, height) = probe_baseline();
        let mut decoder = super::super::software::Software::new();
        let frame = super::super::Decoder::decode(&mut decoder, &picture, true, None)
            .unwrap()
            .unwrap();
        assert!(probe_picture_ok(&frame, width, height));
    }
}
