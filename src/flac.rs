//! FLAC as nori's offload sends it: the `fLaC` head with its STREAMINFO, then the file's frames one
//! by one, each decoded to 32-bit stereo (more channels play their first two).

use alloc::vec::Vec;

/// The longest block decoded: FLAC's subset allows 16384 frames.
pub const MAX_BLOCK: usize = 16_384;

/// The samples [`Flac::new`] takes to work in: the first two channels, and one for the rest.
pub const PLANES: usize = 3 * MAX_BLOCK;

/// What a frame's header says.
struct Header {
    block: usize,
    /// 0: from STREAMINFO.
    bits: u32,
    /// 0 to 7: that many channels less one, apart; 8 left/side, 9 side/right, 10 mid/side.
    assignment: u32,
    /// Bytes it takes.
    len: usize,
}

fn header(p: &[u8]) -> Option<Header> {
    if p.len() < 6 || p[0] != 0xFF || p[1] & 0xFE != 0xF8 {
        return None;
    }
    let block_code = p[2] >> 4;
    let rate_code = p[2] & 0xF;
    let assignment = (p[3] >> 4) as u32;
    let bits = match (p[3] >> 1) & 7 {
        0 => 0,
        1 => 8,
        2 => 12,
        4 => 16,
        5 => 20,
        6 => 24,
        7 => 32,
        _ => return None,
    };
    // The frame or sample number, UTF-8 coded.
    let mut at = 4 + match p[4].leading_ones() {
        0 => 1,
        n @ 2..=7 => n as usize,
        _ => return None,
    };
    let block = match block_code {
        1 => 192,
        2..=5 => 576 << (block_code - 2),
        6 => {
            at += 1;
            *p.get(at - 1)? as usize + 1
        }
        7 => {
            at += 2;
            u16::from_be_bytes([*p.get(at - 2)?, *p.get(at - 1)?]) as usize + 1
        }
        8..=15 => 256 << (block_code - 8),
        _ => return None,
    };
    at += match rate_code {
        12 => 1,
        13 | 14 => 2,
        15 => return None,
        _ => 0,
    };
    // The header's CRC-8.
    at += 1;
    if assignment > 10 || at > p.len() {
        return None;
    }
    Some(Header { block, bits, assignment, len: at })
}

/// Whether `p` is the stream's head (`fLaC` and its metadata).
fn is_head(p: &[u8]) -> bool {
    p.starts_with(b"fLaC")
}

/// The frames packet `p` plays: 0 for the head, none if it is no FLAC frame.
pub fn frames(p: &[u8]) -> Option<u32> {
    if is_head(p) {
        return Some(0);
    }
    header(p).map(|h| h.block as u32)
}

/// Reads bits, most significant first.
struct Bits<'a> {
    data: &'a [u8],
    at: usize,
}

impl Bits<'_> {
    fn left(&self) -> usize {
        self.data.len() * 8 - self.at
    }

    /// The 64 bits from the byte the position is in, zeros past the end.
    fn word(&self) -> u64 {
        let i = self.at / 8;
        match self.data.get(i..i + 8) {
            Some(b) => u64::from_be_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]),
            None => {
                let mut b = [0u8; 8];
                let rest = &self.data[i.min(self.data.len())..];
                b[..rest.len()].copy_from_slice(rest);
                u64::from_be_bytes(b)
            }
        }
    }

    fn read(&mut self, n: u32) -> Option<u32> {
        if n == 0 {
            return Some(0);
        }
        if (n as usize) > self.left() {
            return None;
        }
        let v = (self.word() << (self.at % 8)) >> (64 - n);
        self.at += n as usize;
        Some(v as u32)
    }

    fn signed(&mut self, n: u32) -> Option<i32> {
        let v = self.read(n)?;
        Some(if n == 0 { 0 } else { ((v << (32 - n)) as i32) >> (32 - n) })
    }

    /// The zeros before the next one bit, and the one.
    fn unary(&mut self) -> Option<u32> {
        let mut zeros = 0;
        loop {
            let valid = (64 - (self.at % 8)).min(self.left());
            if valid == 0 {
                return None;
            }
            let z = (self.word() << (self.at % 8)).leading_zeros() as usize;
            if z < valid {
                self.at += z + 1;
                return Some(zeros + z as u32);
            }
            zeros += valid as u32;
            self.at += valid;
        }
    }
}

/// The residual after `order` warm-up samples into `out[order..]`.
fn residual(bits: &mut Bits, order: usize, out: &mut [i32]) -> Option<()> {
    let param_bits = match bits.read(2)? {
        0 => 4,
        1 => 5,
        _ => return None,
    };
    let partition_order = bits.read(4)?;
    let per = out.len() >> partition_order;
    if per << partition_order != out.len() || per < order {
        return None;
    }
    let escape = (1 << param_bits) - 1;
    let mut i = order;
    for p in 0..1usize << partition_order {
        let end = (p + 1) * per;
        let k = bits.read(param_bits)?;
        if k == escape {
            let n = bits.read(5)?;
            for s in &mut out[i..end] {
                *s = bits.signed(n)?;
            }
        } else {
            for s in &mut out[i..end] {
                let q = bits.unary()? as u64;
                let v = ((q << k) | bits.read(k)? as u64) as u32;
                *s = (v >> 1) as i32 ^ -((v & 1) as i32);
            }
        }
        i = end;
    }
    Some(())
}

/// One channel's subframe of `bits` per sample into `out` (the block's length).
fn subframe(b: &mut Bits, bits: u32, out: &mut [i32]) -> Option<()> {
    if b.read(1)? != 0 {
        return None;
    }
    let kind = b.read(6)? as usize;
    let wasted = if b.read(1)? == 1 { b.unary()? + 1 } else { 0 };
    let bits = bits.checked_sub(wasted)?;
    match kind {
        0 => {
            let v = b.signed(bits)?;
            out.fill(v);
        }
        1 => {
            for s in out.iter_mut() {
                *s = b.signed(bits)?;
            }
        }
        8..=12 => {
            let order = kind - 8;
            for s in out.iter_mut().take(order) {
                *s = b.signed(bits)?;
            }
            residual(b, order, out)?;
            for i in order..out.len() {
                let s = |back: usize| out[i - back] as i64;
                let p = match order {
                    0 => 0,
                    1 => s(1),
                    2 => 2 * s(1) - s(2),
                    3 => 3 * s(1) - 3 * s(2) + s(3),
                    _ => 4 * s(1) - 6 * s(2) + 4 * s(3) - s(4),
                };
                out[i] = out[i].wrapping_add(p as i32);
            }
        }
        32..=63 => {
            let order = kind - 31;
            if order > out.len() {
                return None;
            }
            for s in out.iter_mut().take(order) {
                *s = b.signed(bits)?;
            }
            let precision = b.read(4)? + 1;
            if precision == 16 {
                return None;
            }
            let shift = b.signed(5)?;
            if shift < 0 {
                return None;
            }
            let mut coefs = [0i64; 32];
            for c in &mut coefs[..order] {
                *c = b.signed(precision)? as i64;
            }
            residual(b, order, out)?;
            for i in order..out.len() {
                let sum: i64 = coefs[..order].iter().zip(out[i - order..i].iter().rev()).map(|(c, s)| c.wrapping_mul(*s as i64)).fold(0, i64::wrapping_add);
                out[i] = out[i].wrapping_add((sum >> shift) as i32);
            }
        }
        _ => return None,
    }
    if wasted > 0 {
        for s in out.iter_mut() {
            *s <<= wasted;
        }
    }
    Some(())
}

pub struct Flac {
    /// STREAMINFO's sample width, for frames that leave it to it.
    bits: u32,
    planes: Vec<i32>,
}

impl Flac {
    /// `planes` holds [`PLANES`] samples.
    pub fn new(planes: Vec<i32>) -> Flac {
        Flac { bits: 16, planes }
    }

    /// Decodes packet `p` into `out` (interleaved stereo, 32-bit); the frames it made.
    pub fn decode(&mut self, p: &[u8], out: &mut [i32]) -> Result<usize, &'static str> {
        if is_head(p) {
            // STREAMINFO is the first metadata block, its sample width at bytes 12 and 13.
            let info = p.get(8..8 + 18).ok_or("a short STREAMINFO")?;
            self.bits = (((info[12] & 1) << 4) | (info[13] >> 4)) as u32 + 1;
            return Ok(0);
        }
        let h = header(p).ok_or("not a FLAC frame")?;
        let bits = if h.bits == 0 { self.bits } else { h.bits };
        if bits > 24 {
            return Err("over 24 bits");
        }
        if h.block > MAX_BLOCK || out.len() < 2 * h.block {
            return Err("a block too long");
        }
        let channels = if h.assignment < 8 { h.assignment as usize + 1 } else { 2 };
        let (a, rest) = self.planes.split_at_mut(MAX_BLOCK);
        let (b, other) = rest.split_at_mut(MAX_BLOCK);
        let (a, b, other) = (&mut a[..h.block], &mut b[..h.block], &mut other[..h.block]);
        let mut reader = Bits { data: p, at: h.len * 8 };
        for ch in 0..channels {
            // A side channel takes a bit more.
            let side = matches!((h.assignment, ch), (8, 1) | (9, 0) | (10, 1));
            let plane = match ch {
                0 => &mut *a,
                1 => &mut *b,
                _ => &mut *other,
            };
            subframe(&mut reader, bits + side as u32, plane).ok_or("a corrupt subframe")?;
        }
        let shift = 32 - bits;
        for i in 0..h.block {
            let (l, r) = match h.assignment {
                0 => (a[i], a[i]),
                8 => (a[i], a[i].wrapping_sub(b[i])),
                9 => (a[i].wrapping_add(b[i]), b[i]),
                10 => {
                    let mid = (a[i] << 1) | (b[i] & 1);
                    (mid.wrapping_add(b[i]) >> 1, mid.wrapping_sub(b[i]) >> 1)
                }
                _ => (a[i], b[i]),
            };
            out[2 * i] = l << shift;
            out[2 * i + 1] = r << shift;
        }
        Ok(h.block)
    }
}
