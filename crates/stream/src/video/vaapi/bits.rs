//! Reading H.264 and HEVC header syntax: RBSP extraction and Exp-Golomb
//! bit reading. Positions are counted in RBSP bits (emulation prevention
//! bytes removed), which is what VA-API's slice data offsets count too
//! (FFmpeg and GStreamer give the driver the same).

use anyhow::{bail, ensure, Result};

/// The RBSP of a NAL unit: `0x000003` emulation prevention bytes removed.
pub fn rbsp(nal: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nal.len());
    let mut zeros = 0;
    for &b in nal {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

/// Split an Annex B byte stream into NAL units without their start codes.
pub fn nal_units(data: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut out = Vec::with_capacity(starts.len());
    for (k, &s) in starts.iter().enumerate() {
        let mut end = starts.get(k + 1).map(|&n| n - 3).unwrap_or(data.len());
        while end > s && data[end - 1] == 0 {
            end -= 1;
        }
        if end > s {
            out.push(&data[s..end]);
        }
    }
    out
}

/// Emulation prevention bytes in `nal` before its RBSP byte `rbsp_bytes`.
pub fn emulation_prevention_before(nal: &[u8], rbsp_bytes: usize) -> usize {
    let (mut zeros, mut kept, mut skipped) = (0, 0, 0);
    for &b in nal {
        if kept >= rbsp_bytes {
            break;
        }
        if zeros >= 2 && b == 3 {
            zeros = 0;
            skipped += 1;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        kept += 1;
    }
    skipped
}

pub struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Bits<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    /// Bits read so far.
    pub fn position(&self) -> usize {
        self.pos
    }

    pub fn bit(&mut self) -> Result<u32> {
        let byte = *self
            .data
            .get(self.pos / 8)
            .ok_or_else(|| anyhow::anyhow!("header ends early"))?;
        let bit = (byte >> (7 - self.pos % 8)) & 1;
        self.pos += 1;
        Ok(u32::from(bit))
    }

    pub fn flag(&mut self) -> Result<bool> {
        Ok(self.bit()? == 1)
    }

    /// `u(n)`, up to 32 bits.
    pub fn u(&mut self, n: u32) -> Result<u32> {
        ensure!(n <= 32, "field too wide");
        let mut v = 0u64;
        for _ in 0..n {
            v = (v << 1) | u64::from(self.bit()?);
        }
        Ok(v as u32)
    }

    pub fn skip(&mut self, n: usize) -> Result<()> {
        ensure!(self.pos + n <= self.data.len() * 8, "header ends early");
        self.pos += n;
        Ok(())
    }

    /// `ue(v)`, at most 32 leading zeros.
    pub fn ue(&mut self) -> Result<u32> {
        let mut zeros = 0;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros > 31 {
                bail!("Exp-Golomb code too long");
            }
        }
        let rest = self.u(zeros)?;
        Ok(((1u64 << zeros) - 1 + u64::from(rest)) as u32)
    }

    /// `ue(v)` no larger than `max`.
    pub fn ue_max(&mut self, max: u32, what: &str) -> Result<u32> {
        let v = self.ue()?;
        ensure!(v <= max, "{what} {v} is out of range");
        Ok(v)
    }

    pub fn se(&mut self) -> Result<i32> {
        let k = self.ue()?;
        let magnitude = (k as i64 + 1) / 2;
        Ok(if k % 2 == 1 { magnitude } else { -magnitude } as i32)
    }

    pub fn se_range(&mut self, min: i32, max: i32, what: &str) -> Result<i32> {
        let value = self.se()?;
        ensure!(
            (min..=max).contains(&value),
            "{what} {value} is out of range"
        );
        Ok(value)
    }

    pub fn byte_aligned(&self) -> bool {
        self.pos.is_multiple_of(8)
    }

    /// Whether syntax remains before the RBSP's stop bit.
    pub fn more_rbsp_data(&self) -> bool {
        let Some(last) = self.data.iter().rposition(|&b| b != 0) else {
            return false;
        };
        let stop = last * 8 + 7 - self.data[last].trailing_zeros() as usize;
        self.pos < stop
    }
}

/// `Ceil(Log2(n))`, the width of a `u(v)` index into `n` things.
pub fn ceil_log2(n: u32) -> u32 {
    if n <= 1 {
        0
    } else {
        32 - (n - 1).leading_zeros()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exp_golomb_codes_read_as_the_standard_lists() {
        // 1, 010, 011, 00100, 00101 → ue 0..4; then se from 010 (1), 011 (-1).
        let data = [0b1010_0110, 0b0100_0010, 0b1010_0110];
        let mut b = Bits::new(&data);
        assert_eq!(
            (0..5).map(|_| b.ue().unwrap()).collect::<Vec<_>>(),
            [0, 1, 2, 3, 4]
        );
        assert_eq!(b.se().unwrap(), 1);
        assert_eq!(b.se().unwrap(), -1);
        assert_eq!(b.position(), 23);
    }

    #[test]
    fn emulation_prevention_is_removed_and_rbsp_ends_at_the_stop_bit() {
        assert_eq!(rbsp(&[0, 0, 3, 1, 0, 0, 3, 0, 0]), [0, 0, 1, 0, 0, 0, 0]);
        // `00 00 03` only counts after two zeros.
        assert_eq!(rbsp(&[0, 3, 0, 0, 3]), [0, 3, 0, 0]);
        assert_eq!(emulation_prevention_before(&[0, 0, 3, 1, 0, 0, 3, 0], 3), 1);
        assert_eq!(emulation_prevention_before(&[0, 0, 3, 1, 0, 0, 3, 0], 5), 1);
        assert_eq!(emulation_prevention_before(&[0, 0, 3, 1, 0, 0, 3, 0], 6), 2);
        let data = [0b1011_0000];
        let mut b = Bits::new(&data);
        assert!(b.more_rbsp_data());
        b.u(2).unwrap();
        assert!(b.more_rbsp_data());
        b.bit().unwrap();
        assert!(!b.more_rbsp_data(), "only the stop bit is left");
    }

    #[test]
    fn index_widths_round_up() {
        assert_eq!(
            [1, 2, 3, 4, 5, 64, 65].map(ceil_log2),
            [0, 1, 2, 2, 3, 6, 7]
        );
    }
}
