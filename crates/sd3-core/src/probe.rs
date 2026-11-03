//! Forensic read-only binary inspector (`sd3tool probe`).
//!
//! Designed for iterative reverse-engineering on real user-owned `SL-*` folders
//! (`soundb*.obw`, `soundstats.dat`, `soundDB`, `Configs`) without exporting any
//! copyrighted audio payloads:
//! - Magic / header hex dump
//! - Full RIFF / FourCC chunk hierarchy (offsets, sizes, subtypes)
//! - Per-region Shannon entropy (distinguishes raw PCM vs lossless LPC/FLAC vs
//!   encrypted/XOR'd payloads)
//! - Extracted ASCII/UTF-16 string literals (instrument names, mic channels, tags)
//! - Candidate `(offset, size)` index record detector for `soundstats.dat` / `soundDB`
//! - Automatic codec-signature probes (WAVE PCM/Float, Toontrack TPC LPC blocks,
//!   embedded `RIFF`, `fLaC`, `OggS`, `wvpk`, `ttfs`).

use std::fs::File;
use std::path::{Path, PathBuf};

use memmap2::Mmap;
use serde::{Deserialize, Serialize};

use crate::riff::{self, RiffChunk};
use crate::tpc;
use crate::Result;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProbeReport {
    pub target_path: PathBuf,
    pub files: Vec<FileProbe>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileProbe {
    pub path: PathBuf,
    pub size: u64,
    pub magic_ascii: String,
    pub first_64_hex: String,
    pub head_entropy_bits_per_byte: f64,
    pub body_entropy_bits_per_byte: f64,
    pub entropy_assessment: String,
    pub detected_signatures: Vec<String>,
    pub riff_tree: Option<RiffChunk>,
    pub sample_strings: Vec<String>,
    pub candidate_offset_tables: Vec<OffsetTableHint>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OffsetTableHint {
    pub stride_bytes: usize,
    pub start_offset: usize,
    pub monotonic_entries: usize,
    pub sample_pairs: Vec<(u64, u64)>,
}

pub fn probe_path(path: impl AsRef<Path>, max_obw_files: usize) -> Result<ProbeReport> {
    let root = path.as_ref();
    let mut targets = Vec::new();
    if root.is_file() {
        targets.push(root.to_path_buf());
    } else if root.is_dir() {
        collect_probe_targets(root, 0, max_obw_files, &mut targets);
    }
    targets.sort();

    let mut files = Vec::new();
    for p in targets {
        if let Ok(fp) = probe_single_file(&p) {
            files.push(fp);
        }
    }

    Ok(ProbeReport {
        target_path: root.to_path_buf(),
        files,
    })
}

fn collect_probe_targets(
    dir: &Path,
    depth: usize,
    max_obw_files: usize,
    out: &mut Vec<PathBuf>,
) {
    if depth > 4 {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let mut obw_seen = 0usize;
    let mut entries: Vec<_> = rd.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let p = e.path();
        let name = e.file_name().to_string_lossy().into_owned();
        if p.is_dir() {
            if name != "GraphicsV3" && name != "Graphics" {
                collect_probe_targets(&p, depth + 1, max_obw_files, out);
            }
        } else if p.is_file() {
            let lower = name.to_ascii_lowercase();
            if lower.ends_with(".obw") {
                if obw_seen < max_obw_files {
                    out.push(p);
                    obw_seen += 1;
                }
            } else if !lower.ends_with(".png") && !lower.ends_with(".pdf") {
                out.push(p);
            }
        }
    }
}

pub fn probe_single_file(path: &Path) -> Result<FileProbe> {
    let meta = std::fs::metadata(path)?;
    let size = meta.len();
    if size == 0 {
        return Ok(FileProbe {
            path: path.to_path_buf(),
            size: 0,
            magic_ascii: String::new(),
            first_64_hex: String::new(),
            head_entropy_bits_per_byte: 0.0,
            body_entropy_bits_per_byte: 0.0,
            entropy_assessment: "empty".into(),
            detected_signatures: Vec::new(),
            riff_tree: None,
            sample_strings: Vec::new(),
            candidate_offset_tables: Vec::new(),
        });
    }

    let file = File::open(path)?;
    let map = unsafe { Mmap::map(&file)? };

    let magic_len = map.len().min(16);
    let magic_ascii: String = map[..magic_len]
        .iter()
        .map(|&b| if (0x20..=0x7e).contains(&b) { b as char } else { '.' })
        .collect();
    let first_64_hex = map[..map.len().min(64)]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(" ");

    let head_slice = &map[..map.len().min(4096)];
    let mid_start = if map.len() > 131_072 { map.len() / 2 } else { 0 };
    let body_slice = &map[mid_start..map.len().min(mid_start + 65_536)];

    let head_ent = round3(shannon_entropy(head_slice));
    let body_ent = round3(shannon_entropy(body_slice));
    let entropy_assessment = classify_entropy(body_ent);

    let mut sigs = Vec::new();
    if crate::ttbw::looks_like_ttbw(&map) {
        sigs.push("Toontrack Superior Drummer 3 Soundbunch (RIFF 'ttbw')".into());
        if let Ok(idx) = crate::ttbw::parse_ttbw_slice(&map) {
            sigs.push(format!(
                "Parsed 'oidx': {} groups, {} samples, mics={:?}, xor_mask=0x{:x}",
                idx.groups.len(),
                idx.total_records,
                idx.mics,
                idx.xor_mask
            ));
            if let Some(first_rec) = idx
                .groups
                .first()
                .and_then(|g| g.mics.first())
                .and_then(|m| m.records.first())
            {
                let d0 = (idx.data_payload_offset + first_rec.data_offset) as usize;
                let d1 = d0.saturating_add(first_rec.byte_size as usize).min(map.len());
                if d0 < d1 {
                    if let Ok(th) = crate::ttpw::parse_ttpw_header(&map[d0..d1]) {
                        sigs.push(format!(
                            "Verified inner RIFF 'ttpw' stream @ 0x{:x}: {}ch {} Hz {}-bit lossless ({} frames, {} sections)",
                            d0,
                            th.channels,
                            th.sample_rate,
                            th.bits_per_sample,
                            th.total_frames,
                            th.sections.len()
                        ));
                    }
                }
            }
        }
    } else if crate::ttpw::looks_like_ttpw(&map) {
        sigs.push("Toontrack Packed Wave stream (RIFF 'ttpw')".into());
        if let Ok(th) = crate::ttpw::parse_ttpw_header(&map) {
            sigs.push(format!(
                "ttpw v{}: {}ch {} Hz {}-bit lossless ({} frames, {} sections, {} words/ch)",
                th.version,
                th.channels,
                th.sample_rate,
                th.bits_per_sample,
                th.total_frames,
                th.sections.len(),
                th.words_per_channel
            ));
        }
    } else if crate::ttbw::looks_like_oidx_payload(&map) {
        sigs.push("Standalone Toontrack 'oidx' sample directory".into());
        if let Ok(idx) = crate::ttbw::parse_oidx_payload(&map) {
            sigs.push(format!(
                "Parsed 'oidx': {} groups, {} samples, mics={:?}, xor_mask=0x{:x}",
                idx.groups.len(),
                idx.total_records,
                idx.mics,
                idx.xor_mask
            ));
        }
    } else if map.starts_with(b"RIFF") {
        sigs.push("RIFF (little-endian chunk container)".into());
    } else if map.starts_with(b"RIFX") {
        sigs.push("RIFX (big-endian chunk container)".into());
    } else if map.starts_with(b"ttfs") {
        sigs.push("Toontrack ttfs archive".into());
    } else if map.starts_with(b"MThd") {
        sigs.push("Standard MIDI file".into());
    } else if tpc::looks_like_tpc(&map) {
        sigs.push("Toontrack TPC 24-bit LPC+Rice stream".into());
    }

    let riff_tree = if map.starts_with(b"RIFF") || map.starts_with(b"RIFX") {
        let tree = riff::parse_riff_tree(&map, 3).ok();
        if let Some(ref root) = tree {
            for ch in &root.children {
                let s = ch.data_offset as usize;
                let e = s.saturating_add(ch.data_size as usize).min(map.len());
                if s < e && tpc::looks_like_tpc(&map[s..e]) {
                    sigs.push(format!(
                        "Chunk '{}' @ 0x{:x} matches Toontrack TPC LPC+Rice header",
                        ch.id, ch.header_offset
                    ));
                }
            }
        }
        tree
    } else {
        None
    };

    // Scan first 512 KB for embedded inner signatures
    let scan_window = &map[..map.len().min(512 * 1024)];
    for &(needle, desc) in &[
        (&b"WAVE"[..], "WAVE form marker"),
        (&b"fmt "[..], "WAV fmt chunk"),
        (&b"fLaC"[..], "Embedded FLAC magic"),
        (&b"OggS"[..], "Embedded Ogg page magic"),
        (&b"wvpk"[..], "Embedded WavPack block magic"),
        (&b"TPC1"[..], "Extended TPC1 block stream magic"),
    ] {
        if let Some(off) = find_subslice(scan_window, needle) {
            sigs.push(format!("{desc} @ 0x{off:x}"));
        }
    }

    let sample_strings = extract_ascii_strings(&map[..map.len().min(256 * 1024)], 4, 40);
    let candidate_offset_tables = if map.len() <= 64 * 1024 * 1024 {
        detect_offset_tables(&map[..map.len().min(256 * 1024)])
    } else {
        detect_offset_tables(&map[..map.len().min(65_536)])
    };

    Ok(FileProbe {
        path: path.to_path_buf(),
        size,
        magic_ascii,
        first_64_hex,
        head_entropy_bits_per_byte: head_ent,
        body_entropy_bits_per_byte: body_ent,
        entropy_assessment,
        detected_signatures: sigs,
        riff_tree,
        sample_strings,
        candidate_offset_tables,
    })
}

pub fn shannon_entropy(data: &[u8]) -> f64 {
    if data.is_empty() {
        return 0.0;
    }
    let mut counts = [0u64; 256];
    for &b in data {
        counts[b as usize] += 1;
    }
    let len = data.len() as f64;
    let mut h = 0.0f64;
    for &c in &counts {
        if c > 0 {
            let p = (c as f64) / len;
            h -= p * p.log2();
        }
    }
    h
}

fn round3(v: f64) -> f64 {
    (v * 1000.0).round() / 1000.0
}

fn classify_entropy(h: f64) -> String {
    if h < 3.5 {
        "low (structured text / sparse binary tables)".into()
    } else if h < 6.8 {
        "medium (uncompressed PCM audio or dense binary tables)".into()
    } else if h < 7.96 {
        "high (lossless compressed audio, e.g. LPC/Rice, FLAC, or deflate)".into()
    } else {
        "near-8.000 uniform (tightly packed bitstream or encrypted/obfuscated payload)".into()
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn extract_ascii_strings(data: &[u8], min_len: usize, max_count: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = Vec::new();
    for &b in data {
        if (0x20..=0x7e).contains(&b) {
            cur.push(b);
        } else {
            if cur.len() >= min_len {
                if let Ok(s) = std::str::from_utf8(&cur) {
                    let trimmed = s.trim();
                    if trimmed.len() >= min_len && !out.iter().any(|x| x == trimmed) {
                        out.push(trimmed.to_owned());
                        if out.len() >= max_count {
                            return out;
                        }
                    }
                }
            }
            cur.clear();
        }
    }
    if cur.len() >= min_len && out.len() < max_count {
        if let Ok(s) = std::str::from_utf8(&cur) {
            let trimmed = s.trim();
            if trimmed.len() >= min_len {
                out.push(trimmed.to_owned());
            }
        }
    }
    out
}

/// Heuristic detector for monotonically increasing `(u32 offset, u32 size)` or
/// `(u64 offset, u64 size)` sample-index tables inside `soundstats.dat` / `soundDB` / `.obw`.
fn detect_offset_tables(buf: &[u8]) -> Vec<OffsetTableHint> {
    let mut hints = Vec::new();
    for stride in [8usize, 12, 16, 20, 24] {
        for align in (0..stride.min(16)).step_by(4) {
            if align + stride * 4 > buf.len() {
                continue;
            }
            let mut best_run = 0usize;
            let mut best_start = 0usize;
            let mut cur_run = 0usize;
            let mut cur_start = align;
            let mut prev_end = 0u64;

            let mut p = align;
            while p + 8 <= buf.len() {
                let off = u32::from_le_bytes(buf[p..p + 4].try_into().unwrap()) as u64;
                let sz = u32::from_le_bytes(buf[p + 4..p + 8].try_into().unwrap()) as u64;
                let plausible =
                    sz > 64 && sz < 512 * 1024 * 1024 && off >= prev_end && off < 8_000_000_000;
                if plausible && (cur_run == 0 || off.saturating_sub(prev_end) < 65_536) {
                    if cur_run == 0 {
                        cur_start = p;
                    }
                    cur_run += 1;
                    prev_end = off + sz;
                    if cur_run > best_run {
                        best_run = cur_run;
                        best_start = cur_start;
                    }
                } else {
                    cur_run = 0;
                    prev_end = 0;
                }
                p += stride;
            }

            if best_run >= 6 {
                let mut pairs = Vec::new();
                for i in 0..best_run.min(5) {
                    let q = best_start + i * stride;
                    let off = u32::from_le_bytes(buf[q..q + 4].try_into().unwrap()) as u64;
                    let sz = u32::from_le_bytes(buf[q + 4..q + 8].try_into().unwrap()) as u64;
                    pairs.push((off, sz));
                }
                hints.push(OffsetTableHint {
                    stride_bytes: stride,
                    start_offset: best_start,
                    monotonic_entries: best_run,
                    sample_pairs: pairs,
                });
            }
        }
    }
    hints.truncate(4);
    hints
}
