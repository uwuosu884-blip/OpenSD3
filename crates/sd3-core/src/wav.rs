//! Multi-channel WAV reader and writer (16/24/32-bit PCM & 32-bit IEEE float).
//!
//! Supports arbitrary channel counts (mono, stereo, and multi-mic surround/bleed
//! stacks) without external codec dependencies.

use std::fs;
use std::path::Path;

use crate::{Result, Sd3Error};

#[derive(Debug, Clone)]
pub struct AudioBuffer {
    pub sample_rate: u32,
    /// De-interleaved channels (`channels[ch][frame]`), normalized to `[-1.0, 1.0]`.
    pub channels: Vec<Vec<f32>>,
}

impl AudioBuffer {
    pub fn num_channels(&self) -> usize {
        self.channels.len()
    }

    pub fn num_frames(&self) -> usize {
        self.channels.first().map(|c| c.len()).unwrap_or(0)
    }

    pub fn peak(&self) -> f32 {
        self.channels
            .iter()
            .flat_map(|c| c.iter())
            .fold(0.0f32, |acc, &v| acc.max(v.abs()))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WavSampleFormat {
    Pcm16,
    Pcm24,
    Float32,
}

const WAVE_FORMAT_PCM: u16 = 0x0001;
const WAVE_FORMAT_IEEE_FLOAT: u16 = 0x0003;
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;

pub fn read_wav_file(path: impl AsRef<Path>) -> Result<AudioBuffer> {
    let bytes = fs::read(path)?;
    read_wav_bytes(&bytes)
}

pub fn read_wav_bytes(bytes: &[u8]) -> Result<AudioBuffer> {
    if bytes.len() < 44 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err(Sd3Error::Format("not a RIFF/WAVE stream".into()));
    }

    let mut pos = 12usize;
    let mut fmt_tag = 0u16;
    let mut num_channels = 0usize;
    let mut sample_rate = 0u32;
    let mut bits_per_sample = 0u16;
    let mut data_slice: Option<&[u8]> = None;

    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let sz = u32::from_le_bytes(bytes[pos + 4..pos + 8].try_into().unwrap()) as usize;
        let start = pos + 8;
        let end = start.saturating_add(sz).min(bytes.len());
        let chunk = &bytes[start..end];

        if id == b"fmt " {
            if chunk.len() < 16 {
                return Err(Sd3Error::Format("WAV fmt chunk too short".into()));
            }
            fmt_tag = u16::from_le_bytes([chunk[0], chunk[1]]);
            num_channels = u16::from_le_bytes([chunk[2], chunk[3]]) as usize;
            sample_rate = u32::from_le_bytes(chunk[4..8].try_into().unwrap());
            bits_per_sample = u16::from_le_bytes([chunk[14], chunk[15]]);
            if fmt_tag == WAVE_FORMAT_EXTENSIBLE && chunk.len() >= 40 {
                fmt_tag = u16::from_le_bytes([chunk[24], chunk[25]]);
            }
        } else if id == b"data" {
            data_slice = Some(chunk);
            break;
        }
        pos = end.saturating_add(sz & 1);
    }

    let raw = data_slice.ok_or_else(|| Sd3Error::Format("WAV missing data chunk".into()))?;
    decode_pcm_slice(fmt_tag, num_channels, sample_rate, bits_per_sample, raw)
}

pub fn decode_pcm_slice(
    fmt_tag: u16,
    num_channels: usize,
    sample_rate: u32,
    bits_per_sample: u16,
    raw: &[u8],
) -> Result<AudioBuffer> {
    if num_channels == 0 || sample_rate == 0 {
        return Err(Sd3Error::Format("invalid WAV channel count or sample rate".into()));
    }
    let bytes_per_sample = (bits_per_sample as usize) / 8;
    if bytes_per_sample == 0 {
        return Err(Sd3Error::Unsupported(format!(
            "unsupported bits_per_sample={bits_per_sample}"
        )));
    }
    let frame_stride = num_channels * bytes_per_sample;
    let frames = raw.len() / frame_stride;
    let mut channels = vec![Vec::with_capacity(frames); num_channels];

    match (fmt_tag, bits_per_sample) {
        (WAVE_FORMAT_PCM, 16) => {
            for frame in raw.chunks_exact(frame_stride) {
                for (ch, s) in frame.chunks_exact(2).enumerate() {
                    let v = i16::from_le_bytes([s[0], s[1]]) as f32 / 32768.0;
                    channels[ch].push(v);
                }
            }
        }
        (WAVE_FORMAT_PCM, 24) => {
            for frame in raw.chunks_exact(frame_stride) {
                for (ch, s) in frame.chunks_exact(3).enumerate() {
                    let u = (s[0] as u32) | ((s[1] as u32) << 8) | ((s[2] as u32) << 16);
                    let signed = ((u << 8) as i32) >> 8;
                    channels[ch].push(signed as f32 / 8_388_608.0);
                }
            }
        }
        (WAVE_FORMAT_PCM, 32) => {
            for frame in raw.chunks_exact(frame_stride) {
                for (ch, s) in frame.chunks_exact(4).enumerate() {
                    let v = i32::from_le_bytes([s[0], s[1], s[2], s[3]]) as f64 / 2_147_483_648.0;
                    channels[ch].push(v as f32);
                }
            }
        }
        (WAVE_FORMAT_IEEE_FLOAT, 32) => {
            for frame in raw.chunks_exact(frame_stride) {
                for (ch, s) in frame.chunks_exact(4).enumerate() {
                    let v = f32::from_le_bytes([s[0], s[1], s[2], s[3]]);
                    channels[ch].push(if v.is_finite() { v } else { 0.0 });
                }
            }
        }
        _ => {
            return Err(Sd3Error::Unsupported(format!(
                "unsupported WAV format tag=0x{fmt_tag:04x}, bits={bits_per_sample}"
            )))
        }
    }

    Ok(AudioBuffer {
        sample_rate,
        channels,
    })
}

pub fn write_wav_file(
    path: impl AsRef<Path>,
    audio: &AudioBuffer,
    format: WavSampleFormat,
) -> Result<()> {
    let bytes = encode_wav_bytes(audio, format)?;
    if let Some(parent) = path.as_ref().parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, bytes)?;
    Ok(())
}

pub fn encode_wav_bytes(audio: &AudioBuffer, format: WavSampleFormat) -> Result<Vec<u8>> {
    let num_channels = audio.num_channels();
    let num_frames = audio.num_frames();
    if num_channels == 0 || num_channels > 64 {
        return Err(Sd3Error::Format(format!(
            "invalid channel count {num_channels}"
        )));
    }
    for ch in &audio.channels {
        if ch.len() != num_frames {
            return Err(Sd3Error::Format("mismatched channel frame lengths".into()));
        }
    }

    let (fmt_tag, bits_per_sample): (u16, u16) = match format {
        WavSampleFormat::Pcm16 => (WAVE_FORMAT_PCM, 16),
        WavSampleFormat::Pcm24 => (WAVE_FORMAT_PCM, 24),
        WavSampleFormat::Float32 => (WAVE_FORMAT_IEEE_FLOAT, 32),
    };
    let bytes_per_sample = (bits_per_sample / 8) as usize;
    let block_align = (num_channels * bytes_per_sample) as u16;
    let byte_rate = audio.sample_rate * (block_align as u32);
    let data_bytes_len = num_frames * (block_align as usize);

    let mut out = Vec::with_capacity(44 + data_bytes_len + 1);
    let riff_size = (36 + data_bytes_len + (data_bytes_len & 1)) as u32;
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&riff_size.to_le_bytes());
    out.extend_from_slice(b"WAVE");

    // fmt chunk
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&fmt_tag.to_le_bytes());
    out.extend_from_slice(&(num_channels as u16).to_le_bytes());
    out.extend_from_slice(&audio.sample_rate.to_le_bytes());
    out.extend_from_slice(&byte_rate.to_le_bytes());
    out.extend_from_slice(&block_align.to_le_bytes());
    out.extend_from_slice(&bits_per_sample.to_le_bytes());

    // data chunk
    out.extend_from_slice(b"data");
    out.extend_from_slice(&(data_bytes_len as u32).to_le_bytes());

    for i in 0..num_frames {
        for ch in 0..num_channels {
            let s = audio.channels[ch][i];
            match format {
                WavSampleFormat::Pcm16 => {
                    let v = (s.clamp(-1.0, 1.0) * 32767.0).round() as i16;
                    out.extend_from_slice(&v.to_le_bytes());
                }
                WavSampleFormat::Pcm24 => {
                    let v = (s.clamp(-1.0, 1.0) * 8_388_607.0).round() as i32;
                    let b = v.to_le_bytes();
                    out.extend_from_slice(&[b[0], b[1], b[2]]);
                }
                WavSampleFormat::Float32 => {
                    out.extend_from_slice(&s.to_le_bytes());
                }
            }
        }
    }
    if data_bytes_len & 1 == 1 {
        out.push(0);
    }

    Ok(out)
}
