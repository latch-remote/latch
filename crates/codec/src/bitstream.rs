//! Low-delay parameter sets for Latch's I/P-only streams. Coding order is
//! display order. Keep reference storage, colour, timing, cropping and all
//! unrelated syntax intact while making that promise explicit to decoders.
use crate::h264::{decode::golomb::Br, decode::nal::rbsp, encode::bits::Bits, CodecError};
use std::{borrow::Cow, ops::Range};

type Result<T> = std::result::Result<T, CodecError>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    H264,
    Hevc,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct H264Sps {
    pub profile: u8,
    pub constraints: u8,
    pub level: u8,
    pub log2_frame_num: u32,
    pub poc_type: u32,
    pub log2_poc_lsb: u32,
    pub reference_frames: u32,
    pub coded_width: u32,
    pub coded_height: u32,
    /// Raw frame_crop_*_offset syntax values.
    pub crop: [u32; 4],
    pub vui: bool,
    pub reorder_frames: Option<u32>,
    pub buffering: Option<u32>,
}

struct H264 {
    info: H264Sps,
    bytes: Vec<u8>,
    replace: Range<usize>,
    /// An absent VUI needs its presence flag and eight optional-field flags.
    insert_vui: bool,
    insert_restriction: bool,
}

fn valid(condition: bool, message: &'static str) -> Result<()> {
    if condition {
        Ok(())
    } else {
        Err(CodecError::Invalid(message))
    }
}
fn bounded(b: &mut Br<'_>, max: u32, message: &'static str) -> Result<u32> {
    let value = b.ue()?;
    valid(value <= max, message)?;
    Ok(value)
}
fn position(b: &Br<'_>, bytes: &[u8]) -> usize {
    bytes.len() * 8 - b.remaining()
}
fn hrd(b: &mut Br<'_>) -> Result<()> {
    let count = bounded(b, 31, "HRD cpb_cnt_minus1")? + 1;
    b.bits(8)?;
    for _ in 0..count {
        b.ue()?;
        b.ue()?;
        b.bit()?;
    }
    b.bits(20)?;
    Ok(())
}
fn parse_h264(nal: &[u8]) -> Result<H264> {
    valid(
        nal.len() > 1 && nal.len() <= 65536 && nal[0] & 0x9f == 7 && nal[0] & 0x60 != 0,
        "H.264 SPS NAL header/size",
    )?;
    let bytes = rbsp(&nal[1..])?;
    let mut b = Br::new(&bytes);
    let profile = b.bits(8)? as u8;
    let constraints = b.bits(8)? as u8;
    valid(constraints & 3 == 0, "SPS reserved constraint bits")?;
    let level = b.bits(8)? as u8;
    bounded(&mut b, 31, "SPS id")?;
    match profile {
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135 => {
            let chroma = bounded(&mut b, 3, "SPS chroma_format_idc")?;
            if chroma == 3 {
                b.bit()?;
            }
            bounded(&mut b, 6, "SPS luma depth")?;
            bounded(&mut b, 6, "SPS chroma depth")?;
            b.bit()?;
            if b.bit()? {
                for i in 0..if chroma == 3 { 12 } else { 8 } {
                    if b.bit()? {
                        let (mut last, mut next) = (8i64, 8i64);
                        for _ in 0..if i < 6 { 16 } else { 64 } {
                            if next != 0 {
                                next = (last + i64::from(b.se()?) + 256).rem_euclid(256);
                            }
                            if next != 0 {
                                last = next;
                            }
                        }
                    }
                }
            }
        }
        66 | 77 | 88 => {}
        _ => return Err(CodecError::Unsupported("SPS profile")),
    }
    let log2_frame_num = bounded(&mut b, 12, "SPS frame_num width")? + 4;
    let poc_type = bounded(&mut b, 2, "SPS POC type")?;
    let mut log2_poc_lsb = 0;
    match poc_type {
        0 => log2_poc_lsb = bounded(&mut b, 12, "SPS POC width")? + 4,
        1 => {
            b.bit()?;
            b.se()?;
            b.se()?;
            let count = bounded(&mut b, 255, "SPS POC cycle")?;
            for _ in 0..count {
                b.se()?;
            }
        }
        _ => {}
    }
    let reference_frames = bounded(&mut b, 16, "SPS reference count")?;
    b.bit()?;
    let coded_width = (bounded(&mut b, 1023, "SPS width")? + 1) * 16;
    let map_height = bounded(&mut b, 1023, "SPS height")? + 1;
    let frame_only = b.bit()?;
    if !frame_only {
        b.bit()?;
    }
    let coded_height = map_height * 16 * if frame_only { 1 } else { 2 };
    b.bit()?;
    let mut crop = [0; 4];
    if b.bit()? {
        for value in &mut crop {
            *value = bounded(&mut b, 16384, "SPS crop")?;
        }
    }
    let vui_flag = position(&b, &bytes);
    let vui = b.bit()?;
    if vui {
        if b.bit()? && b.bits(8)? == 255 {
            b.bits(32)?;
        }
        if b.bit()? {
            b.bit()?;
        }
        if b.bit()? {
            b.bits(4)?;
            if b.bit()? {
                b.bits(24)?;
            }
        }
        if b.bit()? {
            bounded(&mut b, 5, "VUI chroma location")?;
            bounded(&mut b, 5, "VUI chroma location")?;
        }
        if b.bit()? {
            b.bits(32)?;
            b.bits(32)?;
            b.bit()?;
        }
        let nal_hrd = b.bit()?;
        if nal_hrd {
            hrd(&mut b)?;
        }
        let vcl_hrd = b.bit()?;
        if vcl_hrd {
            hrd(&mut b)?;
        }
        if nal_hrd || vcl_hrd {
            b.bit()?;
        }
        b.bit()?;
    }
    let restriction_flag = position(&b, &bytes);
    let restriction = vui && b.bit()?;
    let (mut reorder_frames, mut buffering) = (None, None);
    let mut start = if vui { restriction_flag } else { vui_flag };
    if restriction {
        b.bit()?;
        bounded(&mut b, 16, "VUI max_bytes_per_pic_denom")?;
        bounded(&mut b, 16, "VUI max_bits_per_mb_denom")?;
        bounded(&mut b, 16, "VUI horizontal MV length")?;
        bounded(&mut b, 16, "VUI vertical MV length")?;
        start = position(&b, &bytes);
        reorder_frames = Some(bounded(&mut b, 16, "VUI reorder frames")?);
        buffering = Some(bounded(&mut b, 16, "VUI decoder buffering")?);
        valid(
            reorder_frames <= buffering && buffering >= Some(reference_frames),
            "inconsistent VUI decoder buffering",
        )?;
    }
    let end = position(&b, &bytes);
    b.trailing_bits()?;
    Ok(H264 {
        info: H264Sps {
            profile,
            constraints,
            level,
            log2_frame_num,
            poc_type,
            log2_poc_lsb,
            reference_frames,
            coded_width,
            coded_height,
            crop,
            vui,
            reorder_frames,
            buffering,
        },
        bytes,
        replace: start..end,
        insert_vui: !vui,
        insert_restriction: !restriction,
    })
}

/// Inspect an H.264 SPS NAL, including its NAL header and emulation prevention.
pub fn h264_sps(nal: &[u8]) -> Result<H264Sps> {
    Ok(parse_h264(nal)?.info)
}

fn copy_bits(out: &mut Bits, bytes: &[u8], range: Range<usize>) {
    for bit in range {
        out.bit(bytes[bit / 8] & (0x80 >> (bit % 8)) != 0);
    }
}
fn finish(header: &[u8], mut bits: Bits) -> Vec<u8> {
    bits.bit(true);
    while !bits.len.is_multiple_of(8) {
        bits.bit(false);
    }
    let mut result = header.to_vec();
    let mut zeros = 0;
    for byte in bits.bytes {
        if zeros == 2 && byte <= 3 {
            result.push(3);
            zeros = 0;
        }
        result.push(byte);
        zeros = if byte == 0 { zeros + 1 } else { 0 };
    }
    result
}
fn content_bits(bytes: &[u8]) -> Result<usize> {
    let last = bytes.last().copied().unwrap_or(0);
    valid(last != 0, "missing RBSP trailing bits")?;
    Ok(bytes.len() * 8 - last.trailing_zeros() as usize - 1)
}
fn h264_low_delay(nal: &[u8]) -> Result<Cow<'_, [u8]>> {
    let sps = parse_h264(nal)?;
    valid(
        sps.info.reorder_frames.is_none_or(|count| count == 0),
        "H.264 encoder declares reordered pictures",
    )?;
    let buffering = sps.info.reference_frames.max(1);
    if sps.info.reorder_frames == Some(0) && sps.info.buffering == Some(buffering) {
        return Ok(Cow::Borrowed(nal));
    }
    let mut bits = Bits::default();
    copy_bits(&mut bits, &sps.bytes, 0..sps.replace.start);
    if sps.insert_vui {
        bits.bit(true);
        bits.put(0, 8);
    }
    if sps.insert_restriction {
        bits.bit(true); // bitstream_restriction_flag
        bits.bit(true); // motion_vectors_over_pic_boundaries_flag
        bits.ue(0); // unrestricted bytes/picture and bits/macroblock
        bits.ue(0);
        bits.ue(16);
        bits.ue(16);
    }
    bits.ue(0);
    bits.ue(buffering);
    copy_bits(
        &mut bits,
        &sps.bytes,
        sps.replace.end..content_bits(&sps.bytes)?,
    );
    Ok(Cow::Owned(finish(&nal[..1], bits)))
}

/// HEVC sublayer ordering, in syntax order. Buffering is the decoded-picture
/// capacity (sps/vps_max_dec_pic_buffering_minus1 + 1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HevcOrdering {
    pub buffering: u32,
    pub reorder_pics: u32,
    pub latency_increase_plus1: u32,
}
struct Hevc {
    bytes: Vec<u8>,
    replace: Range<usize>,
    ordering: Vec<HevcOrdering>,
}
fn skip(b: &mut Br<'_>, mut count: usize) -> Result<()> {
    while count > 0 {
        let width = count.min(32);
        b.bits(width)?;
        count -= width;
    }
    Ok(())
}
fn profile_tier_level(b: &mut Br<'_>, sublayers: usize) -> Result<()> {
    skip(b, 96)?;
    let mut flags = Vec::new();
    for _ in 0..sublayers {
        flags.push((b.bit()?, b.bit()?));
    }
    if sublayers > 0 {
        for _ in sublayers..8 {
            valid(b.bits(2)? == 0, "HEVC reserved sublayer bits")?;
        }
    }
    for (profile, level) in flags {
        if profile {
            skip(b, 88)?;
        }
        if level {
            b.bits(8)?;
        }
    }
    Ok(())
}
fn parse_hevc(nal: &[u8]) -> Result<Hevc> {
    valid(
        nal.len() > 2 && nal.len() <= 65536 && nal[0] & 0x80 == 0 && nal[1] & 7 != 0,
        "HEVC parameter set NAL header/size",
    )?;
    let kind = (nal[0] >> 1) & 63;
    valid(matches!(kind, 32 | 33), "HEVC ordering needs VPS or SPS")?;
    let bytes = rbsp(&nal[2..])?;
    let mut b = Br::new(&bytes);
    b.bits(4)?;
    let sublayers = if kind == 32 {
        b.bits(8)?; // base_layer flags and max_layers_minus1
        b.bits(3)? as usize
    } else {
        b.bits(3)? as usize
    };
    valid(sublayers <= 6, "HEVC max sublayers")?;
    b.bit()?;
    if kind == 32 {
        valid(b.bits(16)? == 65535, "HEVC VPS reserved bits")?;
    }
    profile_tier_level(&mut b, sublayers)?;
    if kind == 33 {
        bounded(&mut b, 15, "HEVC SPS id")?;
        if bounded(&mut b, 3, "HEVC chroma format")? == 3 {
            b.bit()?;
        }
        bounded(&mut b, 16384, "HEVC width")?;
        bounded(&mut b, 16384, "HEVC height")?;
        if b.bit()? {
            for _ in 0..4 {
                bounded(&mut b, 16384, "HEVC conformance window")?;
            }
        }
        bounded(&mut b, 8, "HEVC luma depth")?;
        bounded(&mut b, 8, "HEVC chroma depth")?;
        bounded(&mut b, 12, "HEVC POC width")?;
    }
    let all_sublayers = b.bit()?;
    let start = position(&b, &bytes);
    let mut ordering = Vec::new();
    for _ in if all_sublayers { 0 } else { sublayers }..=sublayers {
        let buffering = bounded(&mut b, 15, "HEVC decoder buffering")? + 1;
        let reorder_pics = bounded(&mut b, buffering - 1, "HEVC reorder pictures")?;
        let latency_increase_plus1 = b.ue()?;
        ordering.push(HevcOrdering {
            buffering,
            reorder_pics,
            latency_increase_plus1,
        });
    }
    let end = position(&b, &bytes);
    valid(end < content_bits(&bytes)?, "truncated HEVC parameter set")?;
    Ok(Hevc {
        bytes,
        replace: start..end,
        ordering,
    })
}

/// Inspect the ordering syntax in an HEVC VPS or SPS. When only the highest
/// sublayer is signalled, its values apply to every lower sublayer too.
pub fn hevc_ordering(nal: &[u8]) -> Result<Vec<HevcOrdering>> {
    Ok(parse_hevc(nal)?.ordering)
}
fn hevc_low_delay(nal: &[u8]) -> Result<Cow<'_, [u8]>> {
    let parsed = parse_hevc(nal)?;
    valid(
        parsed.ordering.iter().all(|o| o.reorder_pics == 0),
        "HEVC encoder declares reordered pictures",
    )?;
    if parsed
        .ordering
        .iter()
        .all(|o| o.reorder_pics == 0 && o.latency_increase_plus1 == 1)
    {
        return Ok(Cow::Borrowed(nal));
    }
    let mut bits = Bits::default();
    copy_bits(&mut bits, &parsed.bytes, 0..parsed.replace.start);
    for ordering in parsed.ordering {
        bits.ue(ordering.buffering - 1);
        bits.ue(0);
        bits.ue(1); // max latency = reorder_pics + increase_plus1 - 1 = 0
    }
    copy_bits(
        &mut bits,
        &parsed.bytes,
        parsed.replace.end..content_bits(&parsed.bytes)?,
    );
    Ok(Cow::Owned(finish(&nal[..2], bits)))
}

/// Normalize parameter sets in one I/P-only Latch access unit. This is also
/// applied before MF decoding to accept pre-fix 6.0.0 hosts. Other NALs,
/// start-code lengths and trailing zero bytes are preserved byte for byte.
/// Already-correct units borrow their input without copying slice data.
/// Rejected syntax and explicitly reordered streams pass through unchanged,
/// with a rate-limited warning. Rewriting is always an optimization.
pub fn low_delay(annexb: &[u8], format: Format) -> Result<Cow<'_, [u8]>> {
    match rewrite(annexb, format) {
        Ok(bytes) => Ok(bytes),
        Err(reason) => {
            // A syntax optimization must never prevent an encoder output or
            // decoder input. Report only the parser's static reason, at most
            // once per five seconds across streams, without logging payloads.
            static WARNED: std::sync::Mutex<Option<std::time::Instant>> =
                std::sync::Mutex::new(None);
            let mut warned = WARNED.lock().unwrap_or_else(|e| e.into_inner());
            if warned.is_none_or(|at| at.elapsed() >= std::time::Duration::from_secs(5)) {
                tracing::warn!(?format, %reason, "keeping original video parameter sets");
                *warned = Some(std::time::Instant::now());
            }
            Ok(Cow::Borrowed(annexb))
        }
    }
}

fn rewrite(annexb: &[u8], format: Format) -> Result<Cow<'_, [u8]>> {
    let mut result = Vec::new();
    let mut copied = 0;
    for (_, begin, end) in crate::h264::nal::spans(annexb) {
        let nal = &annexb[begin..end];
        let Some(&header) = nal.first() else { continue };
        let rewritten = match format {
            Format::H264 if header & 31 == 7 => h264_low_delay(nal)?,
            Format::Hevc if matches!((header >> 1) & 63, 32 | 33) => hevc_low_delay(nal)?,
            _ => continue,
        };
        if let Cow::Owned(bytes) = rewritten {
            result.extend_from_slice(&annexb[copied..begin]);
            result.extend_from_slice(&bytes);
            copied = end;
        }
    }
    if copied == 0 {
        Ok(Cow::Borrowed(annexb))
    } else {
        result.extend_from_slice(&annexb[copied..]);
        Ok(Cow::Owned(result))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn hex(value: &str) -> Vec<u8> {
        value
            .as_bytes()
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect()
    }
    fn unit(nal: &[u8]) -> Vec<u8> {
        [&[0, 0, 0, 1][..], nal].concat()
    }
    #[test]
    fn recorded_sps_comparison_and_exact_preservation() {
        for (raw, width, height, crop) in [
            ("2742c015ab4187f08c44d404040408", 192, 112, [0, 16, 0, 7]),
            ("2742c015ab4186f043193501010102", 192, 96, [0, 32, 0, 11]),
        ] {
            let nal = hex(raw);
            let before = h264_sps(&nal).unwrap();
            assert_eq!(
                (before.profile, before.constraints, before.level),
                (66, 0xc0, 21)
            );
            assert_eq!(
                (before.poc_type, before.log2_frame_num, before.log2_poc_lsb),
                (0, 5, 6)
            );
            assert_eq!(
                (
                    before.reference_frames,
                    before.reorder_frames,
                    before.buffering
                ),
                (1, None, None)
            );
            assert_eq!(
                (before.coded_width, before.coded_height, before.crop),
                (width, height, crop)
            );
            let fixed = h264_low_delay(&nal).unwrap();
            let after = h264_sps(&fixed).unwrap();
            assert_eq!(
                after,
                H264Sps {
                    reorder_frames: Some(0),
                    buffering: Some(1),
                    ..before
                }
            );
            assert!(matches!(h264_low_delay(&fixed).unwrap(), Cow::Borrowed(_)));
            // Only the restriction flag and new restriction syntax were changed.
            let old = parse_h264(&nal).unwrap();
            let new = parse_h264(&fixed).unwrap();
            let mut a = Bits::default();
            let mut b = Bits::default();
            copy_bits(&mut a, &old.bytes, 0..old.replace.start);
            copy_bits(&mut b, &new.bytes, 0..old.replace.start);
            assert_eq!(a.bytes, b.bytes);
            eprintln!(
                "VT SPS {raw} -> {}",
                fixed.iter().map(|b| format!("{b:02x}")).collect::<String>()
            );
        }
        for (name, raw) in [
            ("MF", "6742c01595a0a3fe23011000000300100000030320da08846a"),
            (
                "software",
                "6742c01e8d6828ff889a808080a0000003002000000651e1108d40",
            ),
        ] {
            let nal = hex(raw);
            let fields = h264_sps(&nal).unwrap();
            assert_eq!(
                (
                    fields.poc_type,
                    fields.reference_frames,
                    fields.reorder_frames,
                    fields.buffering
                ),
                (2, 1, Some(0), Some(1)),
                "{name}"
            );
            assert!(matches!(h264_low_delay(&nal).unwrap(), Cow::Borrowed(_)));
        }
    }
    fn synthetic(vui: bool, restriction: bool, high: bool) -> Vec<u8> {
        let mut b = Bits::default();
        b.put(if high { 100 } else { 66 }, 8);
        b.put(0, 8);
        b.put(31, 8);
        b.ue(0);
        if high {
            b.ue(1); // High profile, 4:2:0 with eight scaling lists
            b.ue(0);
            b.ue(0);
            b.bit(false);
            b.bit(true);
            for i in 0..8 {
                b.bit(true);
                if i == 0 {
                    b.se(-8);
                } else {
                    for _ in 0..if i < 6 { 16 } else { 64 } {
                        b.se(0);
                    }
                }
            }
        }
        b.ue(0);
        b.ue(1); // POC type 1 with signed offsets
        b.bit(false);
        b.se(-1);
        b.se(2);
        b.ue(2);
        b.se(-3);
        b.se(4);
        b.ue(2);
        b.bit(false);
        b.ue(9);
        b.ue(5);
        b.bit(true);
        b.bit(true);
        b.bit(false);
        b.bit(vui);
        if vui {
            b.bit(true);
            b.put(255, 8);
            b.put(1, 16);
            b.put(1, 16);
            b.bit(true);
            b.bit(false);
            b.bit(true);
            b.put(5, 3);
            b.bit(false);
            b.bit(true);
            b.put(0x010101, 24);
            b.bit(true);
            b.ue(0);
            b.ue(0);
            b.bit(true);
            b.put(1, 32);
            b.put(120, 32);
            b.bit(true);
            for _ in 0..2 {
                // both HRD parameter sets, including boundary escapes
                b.bit(true);
                b.ue(0);
                b.put(0, 8);
                b.ue(0);
                b.ue(0);
                b.bit(false);
                b.put(0, 20);
            }
            b.bit(false);
            b.bit(true);
            b.bit(restriction);
            if restriction {
                b.bit(false);
                b.ue(2);
                b.ue(1);
                b.ue(12);
                b.ue(13);
                b.ue(0);
                b.ue(4);
            }
        }
        finish(&[0x67], b)
    }
    #[test]
    fn inserts_vui_and_restriction_preserves_scaling_hrd_and_other_vui_bits() {
        for high in [false, true] {
            for (vui, restriction) in [(false, false), (true, false), (true, true)] {
                let nal = synthetic(vui, restriction, high);
                let before = parse_h264(&nal).unwrap();
                let fixed = h264_low_delay(&nal).unwrap();
                let after = parse_h264(&fixed).unwrap();
                assert_eq!(
                    after.info,
                    H264Sps {
                        vui: true,
                        reorder_frames: Some(0),
                        buffering: Some(2),
                        ..before.info
                    }
                );
                let mut prefix = Bits::default();
                copy_bits(&mut prefix, &after.bytes, 0..before.replace.start);
                let mut original = Bits::default();
                copy_bits(&mut original, &before.bytes, 0..before.replace.start);
                assert_eq!(prefix.bytes, original.bytes);
                assert!(matches!(h264_low_delay(&fixed).unwrap(), Cow::Borrowed(_)));
                for cut in 0..nal.len() {
                    assert!(
                        h264_sps(&nal[..cut]).is_err(),
                        "truncation {cut}/{}",
                        nal.len()
                    );
                    let input = unit(&nal[..cut]);
                    assert_eq!(low_delay(&input, Format::H264).unwrap().as_ref(), input);
                }
            }
        }
    }
    #[test]
    fn annex_b_preserves_every_other_byte_and_escape_boundaries() {
        let nal = synthetic(true, false, false);
        assert!(nal.windows(3).any(|w| w == [0, 0, 3]));
        let input = [
            &[0, 0, 0, 0, 1][..],
            &nal,
            &[
                0, 0, 0, 0, 1, 0x68, 0x80, 0, 0, 1, 0x65, 0, 0, 3, 0, 0x80, 0, 0,
            ],
        ]
        .concat();
        let expected = [
            &[0, 0, 0, 0, 1][..],
            &h264_low_delay(&nal).unwrap(),
            &[
                0, 0, 0, 0, 1, 0x68, 0x80, 0, 0, 1, 0x65, 0, 0, 3, 0, 0x80, 0, 0,
            ],
        ]
        .concat();
        assert_eq!(*low_delay(&input, Format::H264).unwrap(), expected);
        for tail in [0, 1, 2, 3] {
            let bytes = [0, 0, tail, 0, 0, 0, 0, 1, 0, 0, 2, 0, 0, 3];
            let mut b = Bits::default();
            for byte in bytes {
                b.put(u32::from(byte), 8);
            }
            let escaped = finish(&[0x67], b);
            let restored = rbsp(&escaped[1..]).unwrap();
            assert_eq!(&restored[..bytes.len()], bytes);
            assert_eq!(restored.last(), Some(&0x80));
        }
    }
    #[test]
    fn malformed_parameter_sets_are_rejected_by_inspection() {
        for raw in [
            "",
            "67",
            "6742001f000000",
            "6742001f000003",
            "6742001f00000304",
            "e742c015ab4187f08c44d404040408",
            "6743c015ab4187f08c44d404040408",
        ] {
            assert!(h264_sps(&hex(raw)).is_err());
        }
    }

    #[test]
    fn recorded_parameter_set_mutations_are_bounded_and_reparse() {
        let mut seeds = vec![
            (Format::H264, hex("2742c015ab4187f08c44d404040408")),
            (Format::H264, synthetic(true, true, true)),
        ];
        for stream in [
            include_bytes!("../../stream/tests/fixtures/decode-probe.hevc").as_slice(),
            include_bytes!("../../stream/tests/fixtures/decode-probe-main10.hevc").as_slice(),
        ] {
            for nal in crate::h264::nal::nal_units(stream) {
                if matches!((nal[0] >> 1) & 63, 32 | 33) {
                    assert!(hevc_ordering(nal).is_ok());
                    seeds.push((Format::Hevc, nal.to_vec()));
                }
            }
        }
        let mut state = 17u32;
        for (format, seed) in seeds {
            for iteration in 0..1024 {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                let mut nal = seed.clone();
                let at = state as usize % nal.len();
                match iteration % 6 {
                    0 => nal[at] ^= 1 << (state % 8),
                    1 => nal.insert(at, state as u8),
                    2 => {
                        nal.remove(at);
                    }
                    3 => nal.truncate(at),
                    // Field-targeted extremes at every byte, including PTL,
                    // VUI and HRD. Long zero prefixes stress Exp-Golomb caps.
                    4 => nal.splice(at..at + 1, [0, 0, 0, 0, 0x80]).for_each(drop),
                    _ => nal[at] = 0xff,
                }
                let input = unit(&nal);
                let output = low_delay(&input, format).unwrap();
                assert!(output.len() <= input.len() * 2 + 128);
                if output.as_ref() != input {
                    for nal in crate::h264::nal::nal_units(&output) {
                        match format {
                            Format::H264 if nal[0] & 31 == 7 => {
                                h264_sps(nal).unwrap();
                            }
                            Format::Hevc if matches!((nal[0] >> 1) & 63, 32 | 33) => {
                                hevc_ordering(nal).unwrap();
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
    }

    fn restriction(nal: &[u8], reorder: u32, buffering: u32) -> Vec<u8> {
        let parsed = parse_h264(nal).unwrap();
        assert!(!parsed.insert_restriction);
        let mut bits = Bits::default();
        copy_bits(&mut bits, &parsed.bytes, 0..parsed.replace.start);
        bits.ue(reorder);
        bits.ue(buffering);
        finish(&nal[..1], bits)
    }

    #[test]
    fn rejected_parameter_sets_pass_through_byte_identical() {
        let base = hex("6742c01e8d6828ff889a808080a0000003002000000651e1108d40");
        let mut unknown = base.clone();
        unknown[1] = 144;
        let mut reserved = base.clone();
        reserved[2] |= 1;
        let mut oversized = base.clone();
        oversized.resize(65537, 0xff);
        for nal in [
            restriction(&base, 0, 0),
            restriction(&base, 2, 1),
            unknown,
            reserved,
            oversized,
        ] {
            assert!(h264_sps(&nal).is_err());
            let input = unit(&nal);
            assert_eq!(low_delay(&input, Format::H264).unwrap().as_ref(), input);
        }
        for (kind, field) in [(32, 0), (33, 0), (33, 1), (33, 2), (33, 3)] {
            let mut bits = Bits::default();
            bits.put(0, 4);
            if kind == 32 {
                bits.put(0, 8);
            }
            bits.put(0, 3);
            bits.bit(true);
            if kind == 32 {
                bits.put(0, 16);
            } // reserved != 0xffff
            for _ in 0..3 {
                bits.put(0, 32);
            } // PTL
            if kind == 33 {
                bits.ue(0); // id
                bits.ue(1); // chroma
                bits.ue(if field == 0 { 16385 } else { 160 });
                bits.ue(if field == 1 { 16385 } else { 98 });
                bits.bit(false);
                bits.ue(if field == 2 { 9 } else { 0 });
                bits.ue(if field == 3 { 9 } else { 0 });
                bits.ue(4);
            }
            bits.bit(false);
            bits.ue(4);
            bits.ue(0);
            bits.ue(0);
            bits.bit(false);
            let nal = finish(&[kind << 1, 1], bits);
            assert!(hevc_ordering(&nal).is_err());
            let input = unit(&nal);
            assert_eq!(low_delay(&input, Format::Hevc).unwrap().as_ref(), input);
        }
    }

    #[test]
    fn explicitly_reordered_parameter_sets_pass_through() {
        let base = hex("6742c01e8d6828ff889a808080a0000003002000000651e1108d40");
        let nal = restriction(&base, 1, 2);
        assert_eq!(h264_sps(&nal).unwrap().reorder_frames, Some(1));
        assert!(h264_low_delay(&nal).is_err());
        let input = unit(&nal);
        assert_eq!(low_delay(&input, Format::H264).unwrap().as_ref(), input);
    }

    #[test]
    fn explicitly_reordered_hevc_parameter_sets_pass_through() {
        let stream = include_bytes!("../../stream/tests/fixtures/decode-probe.hevc");
        let mut checked = 0;
        for nal in crate::h264::nal::nal_units(stream) {
            if !matches!((nal[0] >> 1) & 63, 32 | 33) {
                continue;
            }
            let parsed = parse_hevc(nal).unwrap();
            let mut bits = Bits::default();
            copy_bits(&mut bits, &parsed.bytes, 0..parsed.replace.start);
            for ordering in &parsed.ordering {
                bits.ue(ordering.buffering.max(2) - 1);
                bits.ue(1);
                bits.ue(ordering.latency_increase_plus1);
            }
            copy_bits(
                &mut bits,
                &parsed.bytes,
                parsed.replace.end..content_bits(&parsed.bytes).unwrap(),
            );
            let reordered = finish(&nal[..2], bits);
            assert!(hevc_ordering(&reordered)
                .unwrap()
                .iter()
                .all(|o| o.reorder_pics == 1));
            assert!(hevc_low_delay(&reordered).is_err());
            let input = unit(&reordered);
            assert_eq!(low_delay(&input, Format::Hevc).unwrap().as_ref(), input);
            checked += 1;
        }
        assert_eq!(checked, 2, "exercise both VPS and SPS ordering");
    }

    #[test]
    fn rejected_active_sps_still_decodes_recorded_pictures() {
        let stream = include_bytes!("../../../tests/pairs/mac-vt-6.0.0.h264");
        for variant in 0..3 {
            let mut decoder = crate::h264::Decoder::new();
            let mut count = 0;
            let mut rejected_stream = Vec::new();
            for input in crate::h264::nal::access_units(stream) {
                let mut modified = Vec::new();
                for nal in crate::h264::nal::nal_units(input) {
                    let nal = if nal[0] & 31 == 7 {
                        let fixed = h264_low_delay(nal).unwrap();
                        match variant {
                            0 => restriction(&fixed, 0, 0), // buffering below references
                            1 => restriction(&fixed, 2, 1), // reordering above buffering
                            _ => {
                                let mut oversized = fixed.into_owned();
                                oversized.resize(65537, 0xff);
                                oversized
                            }
                        }
                    } else {
                        nal.to_vec()
                    };
                    modified.extend(unit(&nal));
                }
                let output = low_delay(&modified, Format::H264).unwrap();
                assert_eq!(output.as_ref(), modified);
                assert!(decoder.decode(&output).unwrap().is_some());
                rejected_stream.extend_from_slice(&modified);
                count += 1;
            }
            assert_eq!(count, 24);
            if variant == 0 {
                assert_eq!(
                    rejected_stream,
                    include_bytes!("../../../tests/pairs/mac-vt-rejected-sps.h264")
                );
            }
        }
    }
    #[test]
    fn ffmpeg_preserves_acceptance_of_rejected_parameter_sets() {
        use std::io::Write;
        use std::process::{Command, Stdio};
        if !Command::new("ffmpeg")
            .arg("-version")
            .output()
            .is_ok_and(|o| o.status.success())
        {
            eprintln!(
                "FFmpeg unavailable; recorded rejection decode qualification requires FFmpeg"
            );
            return;
        }
        let decode = |bytes: &[u8], format| {
            let input_format = match format {
                Format::H264 => "h264",
                Format::Hevc => "hevc",
            };
            let mut decoder = Command::new("ffmpeg")
                .args(["-f", input_format])
                .args([
                    "-v", "error", "-threads", "1", "-i", "pipe:0", "-pix_fmt", "gray", "-f",
                    "rawvideo", "pipe:1",
                ])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            decoder.stdin.take().unwrap().write_all(bytes).unwrap();
            decoder.wait_with_output().unwrap()
        };
        let mut reserved_h264 = Vec::new();
        let baseline = crate::h264::nal::access_units(include_bytes!(
            "../../../tests/pairs/mac-vt-6.0.0.h264"
        ));
        for nal in crate::h264::nal::nal_units(baseline[0]) {
            let mut nal = nal.to_vec();
            if nal[0] & 31 == 7 {
                nal[2] |= 1;
            }
            reserved_h264.extend(unit(&nal));
        }
        let mut reserved_hevc = Vec::new();
        for nal in crate::h264::nal::nal_units(include_bytes!(
            "../../stream/tests/fixtures/decode-probe.hevc"
        )) {
            let mut nal = nal.to_vec();
            if (nal[0] >> 1) & 63 == 32 {
                assert_eq!(&nal[4..6], &[0xff, 0xff]);
                nal[5] = 0xfe;
            }
            reserved_hevc.extend(unit(&nal));
        }
        for (format, input, pixels) in [
            (Format::H264, reserved_h264.as_slice(), Some(160 * 98)),
            (Format::Hevc, reserved_hevc.as_slice(), None),
            (
                Format::H264,
                include_bytes!("../../stream/tests/fixtures/optimizer-profile-144.h264").as_slice(),
                Some(64 * 64),
            ),
            (
                Format::Hevc,
                include_bytes!("../../stream/tests/fixtures/optimizer-wide.hevc").as_slice(),
                Some(16400 * 64),
            ),
            (
                Format::Hevc,
                include_bytes!("../../stream/tests/fixtures/optimizer-tall.hevc").as_slice(),
                Some(64 * 16400),
            ),
        ] {
            assert!(rewrite(input, format).is_err());
            let output = low_delay(input, format).unwrap();
            assert_eq!(output.as_ref(), input);
            let decoded = decode(&output, format);
            match pixels {
                Some(pixels) => {
                    assert!(
                        decoded.status.success(),
                        "{}",
                        String::from_utf8_lossy(&decoded.stderr)
                    );
                    assert_eq!(decoded.stdout.len(), pixels);
                }
                None => {
                    // Preserve the decoder's own acceptance decision for this
                    // non-conformant reserved value, including implementations
                    // that tolerate it. Available FFmpeg rejects it.
                    let original = decode(input, format);
                    assert_eq!(decoded.status.code(), original.status.code());
                    assert_eq!(decoded.stdout, original.stdout);
                }
            }
        }
    }

    #[test]
    fn hevc_real_fixtures_keep_reference_capacity_and_suffix() {
        for stream in [
            include_bytes!("../../stream/tests/fixtures/decode-probe.hevc").as_slice(),
            include_bytes!("../../stream/tests/fixtures/decode-probe-main10.hevc").as_slice(),
            include_bytes!("../../stream/tests/fixtures/gray-bars.hevc").as_slice(),
        ] {
            for nal in crate::h264::nal::nal_units(stream)
                .into_iter()
                .filter(|n| matches!((n[0] >> 1) & 63, 32 | 33))
            {
                let before = parse_hevc(nal).unwrap();
                if before.ordering.iter().any(|o| o.reorder_pics > 0) {
                    assert!(hevc_low_delay(nal).is_err());
                    let input = unit(nal);
                    assert_eq!(low_delay(&input, Format::Hevc).unwrap().as_ref(), input);
                    continue;
                }
                let fixed = hevc_low_delay(nal).unwrap();
                let after = parse_hevc(&fixed).unwrap();
                assert_eq!(before.ordering.len(), after.ordering.len());
                for (a, b) in before.ordering.iter().zip(&after.ordering) {
                    assert_eq!(
                        (b.buffering, b.reorder_pics, b.latency_increase_plus1),
                        (a.buffering, 0, 1)
                    );
                }
                let mut a = Bits::default();
                let mut b = Bits::default();
                copy_bits(
                    &mut a,
                    &before.bytes,
                    before.replace.end..content_bits(&before.bytes).unwrap(),
                );
                copy_bits(
                    &mut b,
                    &after.bytes,
                    after.replace.end..content_bits(&after.bytes).unwrap(),
                );
                assert_eq!((a.len, a.bytes), (b.len, b.bytes));
                assert!(matches!(hevc_low_delay(&fixed).unwrap(), Cow::Borrowed(_)));
                for cut in 0..=2 {
                    assert!(hevc_ordering(&nal[..cut]).is_err());
                }
            }
        }
    }
}
