//! Toontrack 24-bit 512-frame 3rd-order linear-predictive + Golomb-Rice lossless
//! block codec (`TPC` / `TA_REVIR` family).
//!
//! Block bitstream credit & layout reference:
//! Documented from Toontrack's shared 24-bit lossless audio compressor as
//! analyzed in `Echooff3/ez-er-mix` (MIT), extended here with:
//!   - multi-channel (1..=32 channels: stereo mid-side pairs + optional mono tail)
//!   - sample-rate header extension (`TPCX`) alongside classic `(channels, frames, peak)`
//!   - lossless block encoder for synthetic `SL-*` / `.obw` test generation and
//!     bit-exact round-trip verification.

use crate::wav::AudioBuffer;
use crate::{Result, Sd3Error};

pub const BLOCK_FRAMES: usize = 512;
const FIXED_ONE: f32 = (1 << 20) as f32;
const INV_FIXED_ONE: f32 = 1.0 / FIXED_ONE;

struct BitReader<'a> {
    words: &'a [u32],
    pos: usize,
    buf: u64,
    bits: u32,
}

impl<'a> BitReader<'a> {
    fn new(words: &'a [u32], pos: usize) -> Self {
        Self {
            words,
            pos,
            buf: 0,
            bits: 0,
        }
    }

    #[inline]
    fn fill(&mut self, need: u32) -> Option<()> {
        while self.bits < need {
            let w = *self.words.get(self.pos)? as u64;
            self.buf |= w << self.bits;
            self.pos += 1;
            self.bits += 32;
        }
        Some(())
    }

    #[inline]
    fn read(&mut self, k: u32) -> Option<u32> {
        if k == 0 {
            return Some(0);
        }
        self.fill(k)?;
        let mask = if k == 32 { u64::MAX } else { (1u64 << k) - 1 };
        let val = (self.buf & mask) as u32;
        self.buf >>= k;
        self.bits -= k;
        Some(val)
    }

    #[inline]
    fn read_quotient(&mut self) -> Option<i32> {
        if self.fill(8).is_none() {
            self.fill(1)?;
        }
        let first = self.buf & 1;
        let mut run = 1u32;
        while run < 8 && ((self.buf >> run) & 1) == first {
            run += 1;
        }
        let (q, consumed) = match (first == 1, run) {
            (true, 8) => (7, 8),
            (true, r) => (r as i32 - 1, r + 1),
            (false, 8) => (-8, 8),
            (false, r) => (-(r as i32), r + 1),
        };
        self.buf >>= consumed;
        self.bits = self.bits.saturating_sub(consumed);
        Some(q)
    }
}

struct BitWriter {
    words: Vec<u32>,
    buf: u64,
    bits: u32,
}

impl BitWriter {
    fn new() -> Self {
        Self {
            words: Vec::with_capacity(256),
            buf: 0,
            bits: 0,
        }
    }

    #[inline]
    fn write(&mut self, val: u32, k: u32) {
        if k == 0 {
            return;
        }
        let mask = if k == 32 { u64::MAX } else { (1u64 << k) - 1 };
        self.buf |= ((val as u64) & mask) << self.bits;
        self.bits += k;
        while self.bits >= 32 {
            self.words.push(self.buf as u32);
            self.buf >>= 32;
            self.bits -= 32;
        }
    }

    #[inline]
    fn write_quotient(&mut self, q: i32) {
        debug_assert!((-8..=7).contains(&q));
        if q >= 0 {
            let run = (q + 1) as u32;
            if run == 8 {
                self.write(0xFF, 8);
            } else {
                let pattern = (1u32 << run) - 1;
                self.write(pattern, run + 1); // trailing 0 at bit `run`
            }
        } else {
            let run = (-q) as u32;
            if run == 8 {
                self.write(0x00, 8);
            } else {
                let pattern = 1u32 << run; // `run` low zeros followed by 1
                self.write(pattern, run + 1);
            }
        }
    }

    fn finish(mut self) -> Vec<u32> {
        if self.bits > 0 {
            self.words.push(self.buf as u32);
        }
        self.words
    }
}

#[inline]
fn sign_extend_24(v: u32) -> i32 {
    ((v << 8) as i32) >> 8
}

/// Decode a single 512-sample mono/channel LPC+Rice block starting at `words[start]`.
/// Returns number of 32-bit words consumed.
pub fn decode_block(words: &[u32], start: usize, out: &mut [i32; BLOCK_FRAMES]) -> Option<usize> {
    let header_word = *words.get(start)?;
    let len_words = ((header_word & 0x1ff) as usize) + 1;
    if start + len_words > words.len() {
        return None;
    }
    let mut br = BitReader::new(&words[..start + len_words], start);
    let _ = br.read(9)?;
    let a = br.read(6)? as i32;
    let b = br.read(7)? as i32;
    let c3 = a - 64;
    let c2 = b - 63;
    let c1 = 47 - (b + c3);

    out[0] = sign_extend_24(br.read(24)?);
    out[1] = sign_extend_24(br.read(24)?);
    out[2] = sign_extend_24(br.read(24)?);

    let mut res = [0i32; BLOCK_FRAMES];
    for group in res.chunks_mut(32) {
        let k = br.read(5)?;
        if k > 26 {
            return None;
        }
        for r in group.iter_mut() {
            let q = br.read_quotient()?;
            let low = br.read(k)? as i32;
            *r = q.wrapping_shl(k).wrapping_add(low);
        }
    }

    for n in 3..BLOCK_FRAMES {
        let pred = c1
            .wrapping_mul(out[n - 3])
            .wrapping_add(c2.wrapping_mul(out[n - 2]))
            .wrapping_add(c3.wrapping_mul(out[n - 1]));
        out[n] = res[n].wrapping_sub(pred >> 4);
    }
    Some(len_words)
}

/// Encode a single 512-sample channel block using a 3rd-order predictor.
pub fn encode_block(samples: &[i32; BLOCK_FRAMES]) -> Result<Vec<u32>> {
    // Choose a gentle default predictor (first-difference: c3 = -16, c2 = 0, c1 = 0 =>
    // a = 48, b = 63, c1 = 47 - (63 - 16) = 0; pred >> 4 = -out[n-1], so out[n] = res[n] + out[n-1])
    // vs second-difference (a = 32 => c3 = -32, b = 79 => c2 = +16, c1 = 47 - (79 - 32) = 0),
    // picking whichever yields smaller total residual magnitude.
    let candidates: [(u32, u32); 3] = [
        (63, 63), // near-flat: c3=-1, c2=0, c1=-15
        (48, 63), // 1st-order delta: c3=-16, c2=0, c1=0
        (32, 79), // 2nd-order linear: c3=-32, c2=16, c1=0
    ];

    let mut best_ab = candidates[1];
    let mut best_cost = u64::MAX;
    for &(a, b) in &candidates {
        let c3 = (a as i32) - 64;
        let c2 = (b as i32) - 63;
        let c1 = 47 - ((b as i32) + c3);
        let mut cost = 0u64;
        for n in 3..BLOCK_FRAMES {
            let pred = c1
                .wrapping_mul(samples[n - 3])
                .wrapping_add(c2.wrapping_mul(samples[n - 2]))
                .wrapping_add(c3.wrapping_mul(samples[n - 1]));
            let r = samples[n].wrapping_add(pred >> 4);
            cost = cost.saturating_add(r.unsigned_abs() as u64);
        }
        if cost < best_cost {
            best_cost = cost;
            best_ab = (a, b);
        }
    }

    let (a, b) = best_ab;
    let c3 = (a as i32) - 64;
    let c2 = (b as i32) - 63;
    let c1 = 47 - ((b as i32) + c3);

    let mut res = [0i32; BLOCK_FRAMES];
    for n in 3..BLOCK_FRAMES {
        let pred = c1
            .wrapping_mul(samples[n - 3])
            .wrapping_add(c2.wrapping_mul(samples[n - 2]))
            .wrapping_add(c3.wrapping_mul(samples[n - 1]));
        // Decoder computes: out[n] = res[n] - (pred >> 4)  =>  res[n] = out[n] + (pred >> 4)
        res[n] = samples[n].wrapping_add(pred >> 4);
    }

    let mut bw = BitWriter::new();
    bw.write(0, 9); // placeholder for (len_words - 1)
    bw.write(a, 6);
    bw.write(b, 7);
    bw.write((samples[0] as u32) & 0xFF_FFFF, 24);
    bw.write((samples[1] as u32) & 0xFF_FFFF, 24);
    bw.write((samples[2] as u32) & 0xFF_FFFF, 24);

    for group in res.chunks_exact(32) {
        let mut k = 0u32;
        while k < 25 {
            let min_r = -(8i64 << k);
            let max_r = (8i64 << k) - 1;
            if group.iter().all(|&r| (r as i64) >= min_r && (r as i64) <= max_r) {
                break;
            }
            k += 1;
        }
        if k > 25 {
            return Err(Sd3Error::Format("LPC residual overflow (>25 bits)".into()));
        }
        bw.write(k, 5);
        let low_mask = if k == 0 { 0 } else { (1i32 << k) - 1 };
        for &r in group {
            let q = r >> k;
            let low = (r & low_mask) as u32;
            bw.write_quotient(q);
            bw.write(low, k);
        }
    }

    let mut words = bw.finish();
    if words.is_empty() || words.len() > 512 {
        return Err(Sd3Error::Format(format!(
            "encoded LPC block length {} words exceeds 9-bit header limit (512)",
            words.len()
        )));
    }
    words[0] = (words[0] & !0x1ff) | (((words.len() - 1) as u32) & 0x1ff);
    Ok(words)
}

/// Decode a Toontrack LPC+Rice stream:
/// - Classic header: `[u32 channels][u32 frames][f32 peak][blocks...]` (assumes 44_100 Hz)
/// - Extended header: `[b"TPC1"][u32 sample_rate][u32 channels][u32 frames][f32 peak][blocks...]`
pub fn decode_tpc_stream(data: &[u8], default_sample_rate: u32) -> Result<AudioBuffer> {
    if data.len() < 16 {
        return Err(Sd3Error::Format("TPC stream shorter than 16 bytes".into()));
    }
    let (sample_rate, body) = if &data[0..4] == b"TPC1" {
        if data.len() < 24 {
            return Err(Sd3Error::Format("TPC1 stream header truncated".into()));
        }
        let sr = u32::from_le_bytes(data[4..8].try_into().unwrap());
        (sr, &data[8..])
    } else {
        (default_sample_rate, data)
    };

    let words: Vec<u32> = body
        .chunks_exact(4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect();
    if words.len() < 4 {
        return Err(Sd3Error::Format("TPC body too short".into()));
    }
    let channels = words[0] as usize;
    let frames = words[1] as usize;
    let peak = f32::from_bits(words[2]);
    if !(1..=32).contains(&channels)
        || frames == 0
        || frames > 48_000 * 300
        || !peak.is_finite()
        || !(0.0..=1000.0).contains(&peak)
    {
        return Err(Sd3Error::Format(format!(
            "implausible TPC stream header (ch={channels}, frames={frames}, peak={peak})"
        )));
    }

    let mut out: Vec<Vec<f32>> = vec![Vec::with_capacity(frames + BLOCK_FRAMES); channels];
    let mut pos = 3usize;
    let mut blk_a = [0i32; BLOCK_FRAMES];
    let mut blk_b = [0i32; BLOCK_FRAMES];

    while out[0].len() < frames {
        let mut ch = 0usize;
        while ch + 1 < channels {
            pos += decode_block(&words, pos, &mut blk_a)
                .ok_or_else(|| Sd3Error::Format("corrupt TPC mid block".into()))?;
            pos += decode_block(&words, pos, &mut blk_b)
                .ok_or_else(|| Sd3Error::Format("corrupt TPC side block".into()))?;
            for i in 0..BLOCK_FRAMES {
                let l = (blk_a[i].wrapping_add(blk_b[i].wrapping_mul(2))) >> 1;
                let r = (blk_a[i].wrapping_sub(blk_b[i].wrapping_mul(2))) >> 1;
                out[ch].push(l as f32 * INV_FIXED_ONE);
                out[ch + 1].push(r as f32 * INV_FIXED_ONE);
            }
            ch += 2;
        }
        if ch < channels {
            pos += decode_block(&words, pos, &mut blk_a)
                .ok_or_else(|| Sd3Error::Format("corrupt TPC mono block".into()))?;
            out[ch].extend(blk_a.iter().map(|&v| v as f32 * INV_FIXED_ONE));
        }
    }

    for ch_vec in &mut out {
        ch_vec.truncate(frames);
    }

    Ok(AudioBuffer {
        sample_rate,
        channels: out,
    })
}

/// Encode an `AudioBuffer` into a `TPC1` stream (or classic 12-byte header if `extended_header == false`).
pub fn encode_tpc_stream(audio: &AudioBuffer, extended_header: bool) -> Result<Vec<u8>> {
    let channels = audio.num_channels();
    let frames = audio.num_frames();
    if !(1..=32).contains(&channels) || frames == 0 {
        return Err(Sd3Error::Format("invalid audio dimensions for TPC encode".into()));
    }
    let peak = audio.peak();
    let mut words = Vec::new();
    words.push(channels as u32);
    words.push(frames as u32);
    words.push(peak.to_bits());

    let num_blocks = frames.div_ceil(BLOCK_FRAMES);
    let mut blk_a = [0i32; BLOCK_FRAMES];
    let mut blk_b = [0i32; BLOCK_FRAMES];

    for b_idx in 0..num_blocks {
        let base = b_idx * BLOCK_FRAMES;
        let mut ch = 0usize;
        while ch + 1 < channels {
            for i in 0..BLOCK_FRAMES {
                let l_f = audio.channels[ch].get(base + i).copied().unwrap_or(0.0);
                let r_f = audio.channels[ch + 1].get(base + i).copied().unwrap_or(0.0);
                let l = (l_f.clamp(-4.0, 4.0) * FIXED_ONE).round() as i32;
                let r = (r_f.clamp(-4.0, 4.0) * FIXED_ONE).round() as i32;
                blk_a[i] = l + r;
                blk_b[i] = (l - r) >> 1;
            }
            words.extend_from_slice(&encode_block(&blk_a)?);
            words.extend_from_slice(&encode_block(&blk_b)?);
            ch += 2;
        }
        if ch < channels {
            for i in 0..BLOCK_FRAMES {
                let f = audio.channels[ch].get(base + i).copied().unwrap_or(0.0);
                blk_a[i] = (f.clamp(-4.0, 4.0) * FIXED_ONE).round() as i32;
            }
            words.extend_from_slice(&encode_block(&blk_a)?);
        }
    }

    let mut out = Vec::with_capacity(words.len() * 4 + 8);
    if extended_header {
        out.extend_from_slice(b"TPC1");
        out.extend_from_slice(&audio.sample_rate.to_le_bytes());
    }
    for w in words {
        out.extend_from_slice(&w.to_le_bytes());
    }
    Ok(out)
}

/// Fast heuristic probe: checks whether `data` starts with a valid TPC1 or classic TPC header
/// whose first block length word is consistent with `data.len()`.
pub fn looks_like_tpc(data: &[u8]) -> bool {
    let body = if data.len() >= 24 && &data[0..4] == b"TPC1" {
        &data[8..]
    } else if data.len() >= 16 {
        data
    } else {
        return false;
    };
    let ch = u32::from_le_bytes(body[0..4].try_into().unwrap()) as usize;
    let frames = u32::from_le_bytes(body[4..8].try_into().unwrap()) as usize;
    let peak = f32::from_bits(u32::from_le_bytes(body[8..12].try_into().unwrap()));
    if !(1..=32).contains(&ch) || frames == 0 || frames > 48_000 * 300 || !peak.is_finite() {
        return false;
    }
    if !(0.0..=100.0).contains(&peak) {
        return false;
    }
    let first_hdr = u32::from_le_bytes(body[12..16].try_into().unwrap());
    let first_words = ((first_hdr & 0x1ff) as usize) + 1;
    12 + first_words * 4 <= body.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real Toontrack 512-frame mono LPC+Rice reference vector (from `TA_REVIR` catalog).
    const REF_B64: &str = "AQAAAAACAAAAAIA/dVIjAMgDAAAACAAACYIA5RyFnn9AIuRBEAQHKHlAOFeC4ACF/jmkPCAHKPRzJQgCwrlC+BxSfoCSB0EQBEEQBEEQHKCMOXygIOYQAsTPVID4mQofKIg5QPx8AmoOIYQwQvgA1Bwgfj4BNUP8TIUQQgjhA1Bz+ADUHD4ANZOAcK4EQUA4n4DyY0jnKPkBCj0/UILgAOUclB+TCPkI4QNQcwghhBAgfgbqZyqEEALEz1QIH4CaQxgQP1MhhA8UxBxCCCEEiJ+pD0DN4QNQcwghQIyfqRACxM8noH4Gag4hQPx8AmoOAeJnKoQQIH6mSBAEByh5QDhH+RxSHgQB4Rwl/wBIj5UgCAjnygEKoZ8rwQFKHgQB4Vw5UAjnoDxWgiA4QMkDwrkSBCQICOdKEAQB4XNIeRAQzpUDlJzwOaRzlDwIDkB5TFDOlYBwrhyA8niCQj+fgPJYOUCh5x8AaXwwUQJCfkBQ8iAIDlDyIAgCwjnK55DyA5RzFPrnkPIgCA5AGZMPINE/h5QHByh5EAQHKHlwgJIHByh5EASEcyUISBAQPoeUB8EBSh4EwQFKHgQHKHkQBEEQBEEQjBBCCAHi5xMFMYfwAaj5A1BzCBA/UwFi/kCFDwAAAA==";

    fn decode_b64(s: &str) -> Vec<u8> {
        const LUT: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut table = [255u8; 256];
        for (i, &c) in LUT.iter().enumerate() {
            table[c as usize] = i as u8;
        }
        let mut out = Vec::new();
        let mut buf = 0u32;
        let mut bits = 0u32;
        for b in s.bytes() {
            if b == b'=' {
                break;
            }
            let v = table[b as usize];
            if v == 255 {
                continue;
            }
            buf = (buf << 6) | (v as u32);
            bits += 6;
            if bits >= 8 {
                bits -= 8;
                out.push(((buf >> bits) & 0xFF) as u8);
            }
        }
        out
    }

    #[test]
    fn decodes_reference_toontrack_lpc_block() {
        let raw = decode_b64(REF_B64);
        assert!(looks_like_tpc(&raw));
        let audio = decode_tpc_stream(&raw, 44_100).expect("decodes reference Toontrack block");
        assert_eq!(audio.num_channels(), 1);
        assert_eq!(audio.num_frames(), 512);
        assert!(audio.peak() > 0.01 && audio.peak() <= 1.5);
    }
}
