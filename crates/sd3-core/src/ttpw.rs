//! Decoder, random-access block reader, and lossless encoder for Toontrack's
//! proprietary `RIFF 'ttpw'` ("ToonTrack Packed Wave") 20-bit/24-bit audio codec.
//!
//! ## Verified Binary Specification (Superior Drummer 3 / SDX `.obw` inner streams)
//!
//! Each audio sample inside an SD3 `RIFF 'ttbw'` soundbunch `'data'` chunk (as well as
//! standalone SD3 metronome/click `.wav` files) is a self-contained `RIFF` container
//! with form type `'ttpw'`:
//!
//! 1. **`'fmt '` chunk** (`20 + 12 * nsec` bytes):
//!    - `0x00..0x02`: `u16` `version` (`1`)
//!    - `0x02..0x04`: `u16` `channels` (`1`, `2`, ...)
//!    - `0x04..0x08`: `u32` `sample_rate` (e.g. `44100`)
//!    - `0x08..0x0A`: `u16` `bits_per_sample` (`20` in SD3/SDX)
//!    - `0x0A..0x0C`: `u16` `num_sections` (`nsec`)
//!    - `0x0C..0x10`: `u32` `total_frames`
//!    - `0x10..0x14`: `u32` `words_per_channel`
//!    - `0x14..`: `nsec` records of `<u32 end_frame, u32 word_offset, u32 bit_width>`
//!
//! 2. **`'data'` chunk** (`channels * 4 * words_per_channel` bytes):
//!    - Viewed as a 2D little-endian `u32` grid `u32_grid[words_per_channel][channels]`
//!      (interleaved by channel every 4 bytes: `[ch0_w0, ch1_w0, ch0_w1, ch1_w1, ...]`).
//!    - Each section `s` covers frames `prev_end_frame .. end_frame` at constant bit-width
//!      `B = bit_width` starting at `word_offset` `u32` words per channel, with zero state
//!      carried across sections:
//!      `next_word_offset - word_offset == ceil((end_frame - prev_end_frame) * B / 32)`.
//!    - Within a section, each channel independently shifts 32-bit little-endian words into
//!      an MSB-first bit accumulator (`buf = (buf << 32) | word`) and extracts signed
//!      `B`-bit two's-complement linear PCM integers directly (full scale `= 2^(bps - 1)`).
//!
//! Because sections carry zero predictor state and every sample within a section sits at
//! deterministic bit offset `(frame - sec_start_frame) * B`, `decode_ttpw_range_stereo`
//! seeks to **any arbitrary frame in `O(log nsec)` time** without decoding preceding audio.

use serde::{Deserialize, Serialize};

use crate::wav::AudioBuffer;
use crate::{Result, Sd3Error};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TtpwSection {
    pub start_frame: u32,
    pub end_frame: u32,
    pub word_offset: u32,
    pub bit_width: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TtpwHeader {
    pub version: u16,
    pub channels: u16,
    pub sample_rate: u32,
    pub bits_per_sample: u16,
    pub total_frames: u32,
    pub words_per_channel: u32,
    pub sections: Vec<TtpwSection>,
    /// Byte offset of the `'data'` payload relative to the start of the `RIFF 'ttpw'` slice.
    pub data_offset: usize,
    /// Byte length of the `'data'` payload.
    pub data_size: usize,
}

#[inline]
pub fn looks_like_ttpw(buf: &[u8]) -> bool {
    buf.len() >= 12 && &buf[0..4] == b"RIFF" && &buf[8..12] == b"ttpw"
}

/// Parse and validate the `'fmt '` and `'data'` chunks of a `RIFF 'ttpw'` byte slice.
pub fn parse_ttpw_header(buf: &[u8]) -> Result<TtpwHeader> {
    if !looks_like_ttpw(buf) {
        return Err(Sd3Error::Format(
            "missing RIFF 'ttpw' magic header".into(),
        ));
    }
    let riff_size = u32::from_le_bytes(buf[4..8].try_into().unwrap()) as usize;
    let riff_end = 8usize.saturating_add(riff_size).min(buf.len());

    let mut pos = 12usize;
    let mut fmt_slice: Option<&[u8]> = None;
    let mut data_range: Option<(usize, usize)> = None;

    while pos + 8 <= riff_end {
        let id = &buf[pos..pos + 4];
        let sz = u32::from_le_bytes(buf[pos + 4..pos + 8].try_into().unwrap()) as usize;
        let payload_start = pos + 8;
        let payload_end = payload_start.checked_add(sz).ok_or_else(|| {
            Sd3Error::Format("ttpw chunk size overflow".into())
        })?;
        if payload_end > buf.len() {
            return Err(Sd3Error::Format(format!(
                "ttpw chunk '{}' extends past buffer ({} > {})",
                String::from_utf8_lossy(id),
                payload_end,
                buf.len()
            )));
        }
        match id {
            b"fmt " => fmt_slice = Some(&buf[payload_start..payload_end]),
            b"data" => data_range = Some((payload_start, sz)),
            _ => {}
        }
        pos = payload_end.saturating_add(sz & 1);
    }

    let fmt = fmt_slice.ok_or_else(|| Sd3Error::Format("ttpw missing 'fmt ' chunk".into()))?;
    let (data_offset, data_size) =
        data_range.ok_or_else(|| Sd3Error::Format("ttpw missing 'data' chunk".into()))?;

    if fmt.len() < 20 {
        return Err(Sd3Error::Format(format!(
            "ttpw 'fmt ' chunk too short ({} bytes < 20)",
            fmt.len()
        )));
    }

    let version = u16::from_le_bytes([fmt[0], fmt[1]]);
    let channels = u16::from_le_bytes([fmt[2], fmt[3]]);
    let sample_rate = u32::from_le_bytes([fmt[4], fmt[5], fmt[6], fmt[7]]);
    let bits_per_sample = u16::from_le_bytes([fmt[8], fmt[9]]);
    let nsec = u16::from_le_bytes([fmt[10], fmt[11]]) as usize;
    let total_frames = u32::from_le_bytes([fmt[12], fmt[13], fmt[14], fmt[15]]);
    let words_per_channel = u32::from_le_bytes([fmt[16], fmt[17], fmt[18], fmt[19]]);

    if channels == 0 || channels > 32 {
        return Err(Sd3Error::Format(format!(
            "unsupported ttpw channel count: {channels}"
        )));
    }
    if bits_per_sample == 0 || bits_per_sample > 32 {
        return Err(Sd3Error::Format(format!(
            "unsupported ttpw bits_per_sample: {bits_per_sample}"
        )));
    }
    let expected_fmt_len = 20usize.saturating_add(nsec.saturating_mul(12));
    if fmt.len() < expected_fmt_len {
        return Err(Sd3Error::Format(format!(
            "ttpw 'fmt ' truncated for {nsec} sections ({} < {expected_fmt_len})",
            fmt.len()
        )));
    }

    let expected_data_bytes = (words_per_channel as usize)
        .checked_mul(channels as usize)
        .and_then(|n| n.checked_mul(4))
        .ok_or_else(|| Sd3Error::Format("ttpw data size overflow".into()))?;
    if data_size < expected_data_bytes {
        return Err(Sd3Error::Format(format!(
            "ttpw 'data' chunk smaller than words_per_ch * ch * 4 ({data_size} < {expected_data_bytes})"
        )));
    }

    let mut sections = Vec::with_capacity(nsec);
    let mut prev_ef = 0u32;
    for i in 0..nsec {
        let base = 20 + i * 12;
        let end_frame = u32::from_le_bytes(fmt[base..base + 4].try_into().unwrap());
        let word_offset = u32::from_le_bytes(fmt[base + 4..base + 8].try_into().unwrap());
        let bit_width = u32::from_le_bytes(fmt[base + 8..base + 12].try_into().unwrap());

        if end_frame < prev_ef || end_frame > total_frames {
            return Err(Sd3Error::Format(format!(
                "invalid ttpw section[{i}] end_frame={end_frame} (prev={prev_ef}, total={total_frames})"
            )));
        }
        if bit_width > 32 {
            return Err(Sd3Error::Format(format!(
                "invalid ttpw section[{i}] bit_width={bit_width}"
            )));
        }
        if word_offset > words_per_channel {
            return Err(Sd3Error::Format(format!(
                "invalid ttpw section[{i}] word_offset={word_offset} > words_per_channel={words_per_channel}"
            )));
        }
        sections.push(TtpwSection {
            start_frame: prev_ef,
            end_frame,
            word_offset,
            bit_width,
        });
        prev_ef = end_frame;
    }

    Ok(TtpwHeader {
        version,
        channels,
        sample_rate: sample_rate.max(8_000),
        bits_per_sample,
        total_frames,
        words_per_channel,
        sections,
        data_offset,
        data_size,
    })
}

#[inline]
fn read_grid_u32(data: &[u8], ch_count: usize, word_idx: usize, ch: usize) -> u32 {
    let byte_off = (word_idx * ch_count + ch) * 4;
    u32::from_le_bytes([
        data[byte_off],
        data[byte_off + 1],
        data[byte_off + 2],
        data[byte_off + 3],
    ])
}

/// Decode a complete `RIFF 'ttpw'` buffer into exact signed integer samples
/// (`[-2^(bps-1), +2^(bps-1)-1]`), one `Vec<i32>` per channel.
pub fn decode_ttpw_i32(buf: &[u8]) -> Result<(TtpwHeader, Vec<Vec<i32>>)> {
    let hdr = parse_ttpw_header(buf)?;
    let ch_count = hdr.channels as usize;
    let total_frames = hdr.total_frames as usize;
    let wpc = hdr.words_per_channel as usize;
    let data = &buf[hdr.data_offset..hdr.data_offset + hdr.data_size];

    let mut channels = vec![vec![0i32; total_frames]; ch_count];

    for sec in &hdr.sections {
        let f_start = sec.start_frame as usize;
        let f_end = sec.end_frame as usize;
        let n_frames = f_end - f_start;
        let b = sec.bit_width;
        if n_frames == 0 || b == 0 {
            continue;
        }
        let needed_words = (n_frames * (b as usize) + 31) / 32;
        if (sec.word_offset as usize) + needed_words > wpc {
            return Err(Sd3Error::Format(
                "ttpw section word range exceeds words_per_channel".into(),
            ));
        }

        let mask: u64 = if b == 32 { u64::MAX } else { (1u64 << b) - 1 };
        let sign: u64 = 1u64 << (b - 1);
        let full: i64 = 1i64 << b;

        for (ch, out_ch) in channels.iter_mut().enumerate() {
            let mut wi = sec.word_offset as usize;
            let mut acc: u64 = 0;
            let mut nbits: u32 = 0;
            for dst in &mut out_ch[f_start..f_end] {
                if nbits < b {
                    let w = read_grid_u32(data, ch_count, wi, ch) as u64;
                    acc = (acc << 32) | w;
                    nbits += 32;
                    wi += 1;
                }
                nbits -= b;
                let u = (acc >> nbits) & mask;
                *dst = if (u & sign) != 0 {
                    (u as i64 - full) as i32
                } else {
                    u as i32
                };
            }
        }
    }

    Ok((hdr, channels))
}

/// Decode a complete `RIFF 'ttpw'` buffer into normalized floating-point `AudioBuffer`
/// (`full_scale = 2^(bits_per_sample - 1)`).
///
/// Because `bits_per_sample = 20` (`full_scale = 524,288 = 2^19`) and IEEE-754 `f32`
/// has an exact 24-bit mantissa, this conversion is 100% lossless and bit-exact.
pub fn decode_ttpw(buf: &[u8]) -> Result<AudioBuffer> {
    let (hdr, int_ch) = decode_ttpw_i32(buf)?;
    let inv_scale = 1.0f32 / ((1u64 << (hdr.bits_per_sample - 1)) as f32);
    let channels = int_ch
        .into_iter()
        .map(|v| v.into_iter().map(|s| (s as f32) * inv_scale).collect())
        .collect();
    Ok(AudioBuffer {
        sample_rate: hdr.sample_rate,
        channels,
    })
}

/// Random-access stereo slice decoder: decodes `[start_frame .. start_frame + count)`
/// directly from an `Mmap`-backed `RIFF 'ttpw'` slice in `O(log nsec)` seek time
/// with zero heap allocations.
///
/// - If `hdr.channels == 1`, mono samples are written identically to `dst_l` and `dst_r`.
/// - If `hdr.channels >= 2`, channels `0` and `1` are decoded into `dst_l` and `dst_r`.
/// - Returns the number of frames actually decoded (`<= count`). Any trailing portion of
///   `dst_l[..count]` / `dst_r[..count]` past `hdr.total_frames` is zero-filled.
pub fn decode_ttpw_range_stereo(
    buf: &[u8],
    hdr: &TtpwHeader,
    start_frame: usize,
    count: usize,
    dst_l: &mut [f32],
    dst_r: &mut [f32],
) -> Result<usize> {
    let n = count.min(dst_l.len()).min(dst_r.len());
    if n == 0 {
        return Ok(0);
    }
    let total = hdr.total_frames as usize;
    if start_frame >= total {
        dst_l[..n].fill(0.0);
        dst_r[..n].fill(0.0);
        return Ok(0);
    }

    let req_end = (start_frame + n).min(total);
    let actual = req_end - start_frame;
    if actual < n {
        dst_l[actual..n].fill(0.0);
        dst_r[actual..n].fill(0.0);
    }

    if hdr.data_offset + hdr.data_size > buf.len() {
        return Err(Sd3Error::Format("ttpw slice truncated".into()));
    }
    let data = &buf[hdr.data_offset..hdr.data_offset + hdr.data_size];
    let ch_count = hdr.channels as usize;
    let wpc = hdr.words_per_channel as usize;
    let inv_scale = 1.0f32 / ((1u64 << (hdr.bits_per_sample - 1)) as f32);

    // Binary-search the first section whose `end_frame > start_frame`.
    let first_sec_idx = hdr
        .sections
        .partition_point(|s| (s.end_frame as usize) <= start_frame);

    let mut cur_frame = start_frame;
    for sec in &hdr.sections[first_sec_idx..] {
        if cur_frame >= req_end {
            break;
        }
        let sec_start = sec.start_frame as usize;
        let sec_end = sec.end_frame as usize;
        if cur_frame < sec_start {
            let gap_end = sec_start.min(req_end);
            let out_s = cur_frame - start_frame;
            let out_e = gap_end - start_frame;
            dst_l[out_s..out_e].fill(0.0);
            dst_r[out_s..out_e].fill(0.0);
            cur_frame = gap_end;
            if cur_frame >= req_end {
                break;
            }
        }
        let seg_end = sec_end.min(req_end);
        if seg_end <= cur_frame {
            continue;
        }
        let out_s = cur_frame - start_frame;
        let out_e = seg_end - start_frame;
        let b = sec.bit_width;

        if b == 0 {
            dst_l[out_s..out_e].fill(0.0);
            dst_r[out_s..out_e].fill(0.0);
            cur_frame = seg_end;
            continue;
        }

        let rel_start = cur_frame - sec_start;
        let seg_frames = seg_end - cur_frame;
        let bit_off = rel_start * (b as usize);
        let word_skip = bit_off / 32;
        let skip_bits = (bit_off % 32) as u32;
        let last_word = (sec.word_offset as usize)
            + (rel_start + seg_frames) * (b as usize) / 32
            + usize::from(((rel_start + seg_frames) * (b as usize)) % 32 != 0);
        if last_word > wpc {
            return Err(Sd3Error::Format(
                "ttpw range decode exceeds words_per_channel".into(),
            ));
        }

        decode_channel_segment(
            data,
            ch_count,
            0,
            (sec.word_offset as usize) + word_skip,
            skip_bits,
            b,
            inv_scale,
            &mut dst_l[out_s..out_e],
        );

        if ch_count >= 2 {
            decode_channel_segment(
                data,
                ch_count,
                1,
                (sec.word_offset as usize) + word_skip,
                skip_bits,
                b,
                inv_scale,
                &mut dst_r[out_s..out_e],
            );
        } else {
            dst_r[out_s..out_e].copy_from_slice(&dst_l[out_s..out_e]);
        }

        cur_frame = seg_end;
    }

    if cur_frame < req_end {
        let out_s = cur_frame - start_frame;
        dst_l[out_s..actual].fill(0.0);
        dst_r[out_s..actual].fill(0.0);
    }

    Ok(actual)
}

#[inline]
#[allow(clippy::too_many_arguments)]
fn decode_channel_segment(
    data: &[u8],
    ch_count: usize,
    ch: usize,
    mut wi: usize,
    skip_bits: u32,
    b: u32,
    inv_scale: f32,
    dst: &mut [f32],
) {
    let mask: u64 = if b == 32 { u64::MAX } else { (1u64 << b) - 1 };
    let sign: u64 = 1u64 << (b - 1);
    let full: i64 = 1i64 << b;

    let mut acc: u64 = 0;
    let mut nbits: u32 = 0;
    if skip_bits > 0 {
        let w = read_grid_u32(data, ch_count, wi, ch) as u64;
        wi += 1;
        nbits = 32 - skip_bits;
        acc = w & ((1u64 << nbits) - 1);
    }

    for out in dst.iter_mut() {
        if nbits < b {
            let w = read_grid_u32(data, ch_count, wi, ch) as u64;
            acc = (acc << 32) | w;
            nbits += 32;
            wi += 1;
        }
        nbits -= b;
        let u = (acc >> nbits) & mask;
        let s = if (u & sign) != 0 {
            (u as i64 - full) as i32
        } else {
            u as i32
        };
        *out = (s as f32) * inv_scale;
    }
}

/// Lossless adaptive-bitwidth `RIFF 'ttpw'` encoder (defaults to `bits_per_sample = 20`,
/// matching Toontrack Superior Drummer 3 `.obw` streams).
pub fn encode_ttpw_bytes(
    audio: &AudioBuffer,
    bits_per_sample: u16,
    block_frames: usize,
) -> Result<Vec<u8>> {
    let ch_count = audio.num_channels();
    if ch_count == 0 || ch_count > 32 {
        return Err(Sd3Error::Format(format!(
            "cannot encode ttpw with {ch_count} channels"
        )));
    }
    let bps = bits_per_sample.clamp(8, 24);
    let total_frames = audio.num_frames();
    let blk = block_frames.max(32);

    let full_pos = (1i64 << (bps - 1)) - 1;
    let full_neg = -(1i64 << (bps - 1));
    let scale = (1u64 << (bps - 1)) as f32;

    // Quantize to exact signed `bps`-bit integers.
    let mut q_ch = vec![vec![0i32; total_frames]; ch_count];
    for ch in 0..ch_count {
        for f in 0..total_frames {
            let v = (audio.channels[ch][f] * scale).round() as i64;
            q_ch[ch][f] = v.clamp(full_neg, full_pos) as i32;
        }
    }

    // Determine minimal two's-complement bit-width B (`0`, or `2..=bps`) per block,
    // and coalesce adjacent blocks sharing the same bit-width.
    let mut raw_secs: Vec<(usize, usize, u32)> = Vec::new();
    let mut f0 = 0usize;
    while f0 < total_frames {
        let f1 = (f0 + blk).min(total_frames);
        let mut min_v = 0i32;
        let mut max_v = 0i32;
        for ch_buf in &q_ch {
            for &s in &ch_buf[f0..f1] {
                min_v = min_v.min(s);
                max_v = max_v.max(s);
            }
        }
        let b = required_twos_comp_bits(min_v, max_v, bps as u32);
        if let Some(last) = raw_secs.last_mut() {
            if last.2 == b {
                last.1 = f1;
                f0 = f1;
                continue;
            }
        }
        raw_secs.push((f0, f1, b));
        f0 = f1;
    }

    let mut sec_entries: Vec<(u32, u32, u32)> = Vec::with_capacity(raw_secs.len());
    let mut per_ch_words: Vec<Vec<u32>> = vec![Vec::new(); ch_count];
    let mut word_cursor = 0u32;

    for (s_start, s_end, b) in raw_secs {
        let n_frames = s_end - s_start;
        sec_entries.push((s_end as u32, word_cursor, b));
        if b == 0 || n_frames == 0 {
            continue;
        }
        let sec_words = (n_frames * (b as usize) + 31) / 32;
        let mask: u64 = if b == 32 { u64::MAX } else { (1u64 << b) - 1 };

        for ch in 0..ch_count {
            let mut acc: u64 = 0;
            let mut nbits: u32 = 0;
            let mut emitted = 0usize;
            for &s in &q_ch[ch][s_start..s_end] {
                let u = ((s as i64) as u64) & mask;
                acc = (acc << b) | u;
                nbits += b;
                if nbits >= 32 {
                    nbits -= 32;
                    let w = ((acc >> nbits) & 0xFFFF_FFFF) as u32;
                    per_ch_words[ch].push(w);
                    emitted += 1;
                }
            }
            if nbits > 0 {
                let w = ((acc << (32 - nbits)) & 0xFFFF_FFFF) as u32;
                per_ch_words[ch].push(w);
                emitted += 1;
            }
            debug_assert_eq!(emitted, sec_words);
        }
        word_cursor += sec_words as u32;
    }

    let words_per_channel = word_cursor as usize;
    let mut fmt_payload = Vec::with_capacity(20 + sec_entries.len() * 12);
    fmt_payload.extend_from_slice(&1u16.to_le_bytes());
    fmt_payload.extend_from_slice(&(ch_count as u16).to_le_bytes());
    fmt_payload.extend_from_slice(&audio.sample_rate.to_le_bytes());
    fmt_payload.extend_from_slice(&bps.to_le_bytes());
    fmt_payload.extend_from_slice(&(sec_entries.len() as u16).to_le_bytes());
    fmt_payload.extend_from_slice(&(total_frames as u32).to_le_bytes());
    fmt_payload.extend_from_slice(&(words_per_channel as u32).to_le_bytes());
    for (ef, wo, b) in &sec_entries {
        fmt_payload.extend_from_slice(&ef.to_le_bytes());
        fmt_payload.extend_from_slice(&wo.to_le_bytes());
        fmt_payload.extend_from_slice(&b.to_le_bytes());
    }

    let mut data_payload = Vec::with_capacity(words_per_channel * ch_count * 4);
    for wi in 0..words_per_channel {
        for ch_words in &per_ch_words {
            data_payload.extend_from_slice(&ch_words[wi].to_le_bytes());
        }
    }

    let mut children = crate::riff::build_chunk(b"fmt ", &fmt_payload);
    children.extend_from_slice(&crate::riff::build_chunk(b"data", &data_payload));
    Ok(crate::riff::build_container(b"RIFF", b"ttpw", &children))
}

fn required_twos_comp_bits(min_v: i32, max_v: i32, max_bps: u32) -> u32 {
    if min_v == 0 && max_v == 0 {
        return 0;
    }
    for b in 2..=max_bps {
        let lo = -(1i64 << (b - 1));
        let hi = (1i64 << (b - 1)) - 1;
        if (min_v as i64) >= lo && (max_v as i64) <= hi {
            return b;
        }
    }
    max_bps
}
