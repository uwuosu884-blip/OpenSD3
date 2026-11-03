//! `.obw` ("Superior Drummer Soundbunch") container inspector and lossless
//! multi-mic sample extractor.
//!
//! Supports:
//! 1. **Retail Superior Drummer 3 / SDX `RIFF 'ttbw'` soundbunches** (`'oidx'` sample
//!    directory + XOR offset de-obfuscation + 20-bit lossless `RIFF 'ttpw'` streams),
//!    preserving full articulation groups, Toontrack mic names (`OH451`, `Amb67`, ...),
//!    velocity tiers (`A..M`), round-robins (`1..N`), cymbal/hi-hat choke groups,
//!    and CC4 hi-hat openness tiers, as well as merging mics across multi-`.obw`
//!    `SL-*` rooms.
//! 2. **Standalone `RIFF 'ttpw'` files** (e.g. `.ttpw` or SD3 metronome `.wav` clicks).
//! 3. **Manifest-assisted `RIFF 'OBW '` (`OBWI` + `TPC1` 24-bit LPC+Rice) & `WAVE`**
//!    containers (used by synthetic test kits and legacy streams).
//! 4. **Selective extraction filters** (`--filter`, `--mic`, `--max-rr`, `--limit`,
//!    `--normalize`) so multi-gigabyte soundbunches can be exported selectively
//!    without filling disk.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::path::{Path, PathBuf};

use memmap2::Mmap;
use serde::{Deserialize, Serialize};

use crate::kit::{
    gm_drum_name, DrumKitManifest, MicBus, PadSpec, RoundRobinSample, VelocityLayerSpec,
};
use crate::riff::{self, RiffChunk};
use crate::tpc;
use crate::ttbw;
use crate::ttpw;
use crate::wav::{self, AudioBuffer, WavSampleFormat};
use crate::{Result, Sd3Error};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObwIndexEntry {
    pub chunk_index: usize,
    pub pad_id: String,
    pub pad_name: String,
    pub midi_notes: Vec<u8>,
    pub group: Option<u32>,
    pub off_by_this: Option<u32>,
    pub lovel: u8,
    pub hivel: u8,
    pub rr_index: usize,
    pub mic_bus: MicBus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractionSummary {
    pub input_path: PathBuf,
    pub output_dir: PathBuf,
    pub decoded_wav_count: usize,
    pub raw_carved_chunks: usize,
    pub unmapped_fourccs: Vec<String>,
    pub manifest_path: Option<PathBuf>,
    pub sfz_path: Option<PathBuf>,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ExtractOptions {
    pub wav_format: WavSampleFormat,
    pub emit_sfz: bool,
    pub carve_unknown_chunks: bool,
    /// Optional case-insensitive substring filter on group/articulation name (e.g. `"CR_R"` or `"SN"`).
    pub filter: Option<String>,
    /// Optional case-insensitive substring filter on mic name or bus (e.g. `"OH451"` or `"room"`).
    pub mic_filter: Option<String>,
    /// Optional cap on round-robins per velocity tier (e.g. `Some(2)`).
    pub max_rr: Option<usize>,
    /// Optional cap on total decoded WAV files written.
    pub limit: Option<usize>,
    /// Peak-normalize each exported WAV to `-1.0 dBFS` (`0.89125`).
    pub normalize: bool,
}

impl Default for ExtractOptions {
    fn default() -> Self {
        Self {
            wav_format: WavSampleFormat::Pcm24,
            emit_sfz: true,
            carve_unknown_chunks: false,
            filter: None,
            mic_filter: None,
            max_rr: None,
            limit: None,
            normalize: false,
        }
    }
}

/// Extract a `.obw` soundbunch, standalone `.ttpw` file, or entire `SL-*` library folder
/// into `out_dir` (`samples/*.wav` + `kit.json` + `kit.sfz`).
pub fn extract_to_directory(
    input: impl AsRef<Path>,
    out_dir: impl AsRef<Path>,
    opts: ExtractOptions,
) -> Result<ExtractionSummary> {
    let input = input.as_ref();
    let out_dir = out_dir.as_ref();
    fs::create_dir_all(out_dir)?;

    let mut source_files = Vec::new();
    if input.is_file() {
        source_files.push(input.to_path_buf());
    } else if input.is_dir() {
        collect_sound_files(input, 0, &mut source_files);
        source_files.sort();
    } else {
        return Err(Sd3Error::Format(format!(
            "input path does not exist: {}",
            input.display()
        )));
    }

    if source_files.is_empty() {
        return Err(Sd3Error::Format(format!(
            "no .obw or .ttpw files found under {}",
            input.display()
        )));
    }

    let samples_dir = out_dir.join("samples");
    fs::create_dir_all(&samples_dir)?;

    let filter_lc = opts.filter.as_deref().map(|s| s.to_ascii_lowercase());
    let mic_lc = opts.mic_filter.as_deref().map(|s| s.to_ascii_lowercase());

    let mut decoded_wav_count = 0usize;
    let mut raw_carved_chunks = 0usize;
    let mut unmapped_set = Vec::new();
    let mut notes = Vec::new();
    let mut pads_map: BTreeMap<String, PadSpec> = BTreeMap::new();
    let mut detected_sr = 44_100u32;
    let mut saw_ttbw = false;

    'file_loop: for obw_path in &source_files {
        if let Some(lim) = opts.limit {
            if decoded_wav_count >= lim {
                break;
            }
        }

        let stem = obw_path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "soundb".into());
        let file = File::open(obw_path)?;
        let map = unsafe { Mmap::map(&file)? };

        if map.len() < 12 {
            notes.push(format!("{}: skipped (<12 bytes)", obw_path.display()));
            continue;
        }

        // Case 0a: Retail Superior Drummer 3 `RIFF 'ttbw'` soundbunch (`oidx` + `ttpw` streams)
        if ttbw::looks_like_ttbw(&map) {
            saw_ttbw = true;
            let idx = ttbw::parse_ttbw_slice(&map)?;
            let data_base = idx.data_payload_offset as usize;
            let data_end = data_base
                .saturating_add(idx.data_payload_size as usize)
                .min(map.len());

            notes.push(format!(
                "{}: SD3 RIFF 'ttbw' ({} groups, {} samples, mics={:?}, xor_mask=0x{:x})",
                stem,
                idx.groups.len(),
                idx.total_records,
                idx.mics,
                idx.xor_mask
            ));

            for grp in &idx.groups {
                if let Some(ref flt) = filter_lc {
                    let hay = format!(
                        "{} {} {}",
                        grp.name.to_ascii_lowercase(),
                        grp.display_name.to_ascii_lowercase(),
                        grp.instrument_family.to_ascii_lowercase()
                    );
                    if !hay.contains(flt) {
                        continue;
                    }
                }

                // Collect all velocity characters present across mics in this group
                let mut all_vels = Vec::new();
                for mh in &grp.mics {
                    for rec in &mh.records {
                        all_vels.push(rec.vel_char);
                    }
                }
                let vel_ranges = ttbw::compute_velocity_ranges(&all_vels);
                let natural_vel = vel_ranges.len() >= 4;

                let (group_num, off_by_num) = match (grp.member_choke_group, grp.triggers_choke_group) {
                    (Some(_), Some(trig)) => (Some(trig as u32), None),
                    (Some(mem), None) => (Some(100 + mem as u32), Some(mem as u32)),
                    (None, Some(trig)) => (Some(trig as u32), None),
                    (None, None) => (None, None),
                };

                for mh in &grp.mics {
                    if let Some(ref mf) = mic_lc {
                        let m_match = mh.mic_name.to_ascii_lowercase().contains(mf)
                            || mh.bus.as_str().contains(mf);
                        if !m_match {
                            continue;
                        }
                    }

                    for rec in &mh.records {
                        if let Some(lim) = opts.limit {
                            if decoded_wav_count >= lim {
                                break 'file_loop;
                            }
                        }
                        if let Some(max_rr) = opts.max_rr {
                            if (rec.rr_num as usize) > max_rr {
                                continue;
                            }
                        }

                        let s_off = data_base.saturating_add(rec.data_offset as usize);
                        let s_end = s_off.saturating_add(rec.byte_size as usize);
                        if s_end > data_end {
                            notes.push(format!(
                                "{}: sample {} {} {}{:02} exceeds data chunk ({} > {})",
                                stem, grp.name, mh.mic_name, rec.vel_char, rec.rr_num, s_end, data_end
                            ));
                            continue;
                        }

                        let mut audio = ttpw::decode_ttpw(&map[s_off..s_end])?;
                        if opts.normalize {
                            normalize_audio_peak(&mut audio, 0.891_250_9);
                        }
                        detected_sr = audio.sample_rate;

                        let safe_mic = sanitize_token(&mh.mic_name);
                        let rel = format!(
                            "samples/{}__{}__{}_{}{:02}.wav",
                            stem, grp.name, safe_mic, rec.vel_char, rec.rr_num
                        );
                        wav::write_wav_file(out_dir.join(&rel), &audio, opts.wav_format)?;

                        let &(lovel, hivel) = vel_ranges.get(&rec.vel_char).unwrap_or(&(1, 127));
                        let rr_zero = (rec.rr_num as usize).saturating_sub(1);

                        let pad = pads_map.entry(grp.name.clone()).or_insert_with(|| PadSpec {
                            id: grp.name.clone(),
                            name: grp.display_name.clone(),
                            midi_notes: vec![grp.suggested_note],
                            group: group_num,
                            off_by_this: off_by_num,
                            hihat_openness: grp.hihat_openness,
                            natural_vel_dynamics: natural_vel,
                            layers: Vec::new(),
                        });

                        let layer = match pad
                            .layers
                            .iter_mut()
                            .find(|l| l.lovel == lovel && l.hivel == hivel)
                        {
                            Some(l) => l,
                            None => {
                                pad.layers.push(VelocityLayerSpec {
                                    lovel,
                                    hivel,
                                    round_robins: Vec::new(),
                                });
                                pad.layers.last_mut().unwrap()
                            }
                        };

                        let target_bus = choose_free_or_primary_bus(layer, rr_zero, mh.bus);
                        if let Some(rr_slot) =
                            layer.round_robins.iter_mut().find(|r| r.rr_index == rr_zero)
                        {
                            rr_slot.mic_files.entry(target_bus).or_insert(rel);
                        } else {
                            let mut mic_files = BTreeMap::new();
                            mic_files.insert(target_bus, rel);
                            layer.round_robins.push(RoundRobinSample {
                                rr_index: rr_zero,
                                mic_files,
                            });
                        }

                        decoded_wav_count += 1;
                    }
                }
            }
            continue;
        }

        // Case 0b: Standalone `RIFF 'ttpw'` file (`.ttpw` or SD3 click `.wav`)
        if ttpw::looks_like_ttpw(&map) {
            let mut audio = ttpw::decode_ttpw(&map)?;
            if opts.normalize {
                normalize_audio_peak(&mut audio, 0.891_250_9);
            }
            detected_sr = audio.sample_rate;
            let rel = format!("samples/{stem}.wav");
            wav::write_wav_file(out_dir.join(&rel), &audio, opts.wav_format)?;
            register_unindexed_sample(&mut pads_map, decoded_wav_count, &rel);
            decoded_wav_count += 1;
            continue;
        }

        // Case A: raw TPC stream file
        if tpc::looks_like_tpc(&map) {
            let mut audio = tpc::decode_tpc_stream(&map, 44_100)?;
            if opts.normalize {
                normalize_audio_peak(&mut audio, 0.891_250_9);
            }
            detected_sr = audio.sample_rate;
            let rel = format!("samples/{stem}_000.wav");
            wav::write_wav_file(out_dir.join(&rel), &audio, opts.wav_format)?;
            register_unindexed_sample(&mut pads_map, decoded_wav_count, &rel);
            decoded_wav_count += 1;
            continue;
        }

        // Case B: Generic RIFF / RIFX container (`OBW `, `WAVE`, etc.)
        let tree = match riff::parse_riff_tree(&map, 4) {
            Ok(t) => t,
            Err(e) => {
                notes.push(format!(
                    "{}: non-RIFF header ({e}); run `sd3tool probe {}` to inspect magic/offsets",
                    obw_path.display(),
                    obw_path.display()
                ));
                if opts.carve_unknown_chunks {
                    let raw_out = out_dir.join("raw").join(format!("{stem}.bin"));
                    if let Some(p) = raw_out.parent() {
                        fs::create_dir_all(p)?;
                    }
                    fs::write(&raw_out, &map[..map.len().min(4 * 1024 * 1024)])?;
                    raw_carved_chunks += 1;
                }
                continue;
            }
        };

        if tree.form_type.as_deref() == Some("WAVE") {
            if let Ok(mut audio) = wav::read_wav_bytes(&map) {
                if opts.normalize {
                    normalize_audio_peak(&mut audio, 0.891_250_9);
                }
                detected_sr = audio.sample_rate;
                let rel = format!("samples/{stem}_wave.wav");
                wav::write_wav_file(out_dir.join(&rel), &audio, opts.wav_format)?;
                register_unindexed_sample(&mut pads_map, decoded_wav_count, &rel);
                decoded_wav_count += 1;
                continue;
            }
        }

        let mut index_entries: BTreeMap<usize, ObwIndexEntry> = BTreeMap::new();
        for ch in &tree.children {
            if ch.id == "OBWI" {
                let s = ch.data_offset as usize;
                let e = s.saturating_add(ch.data_size as usize).min(map.len());
                if let Ok(parsed) = serde_json::from_slice::<Vec<ObwIndexEntry>>(&map[s..e]) {
                    for item in parsed {
                        index_entries.insert(item.chunk_index, item);
                    }
                }
            }
        }

        let mut leaf_chunks = Vec::new();
        collect_leaf_chunks(&tree, &mut leaf_chunks);

        let mut audio_ordinal = 0usize;
        let mut pending_fmt: Option<&[u8]> = None;

        for leaf in leaf_chunks {
            if let Some(lim) = opts.limit {
                if decoded_wav_count >= lim {
                    break 'file_loop;
                }
            }
            if matches!(leaf.id.as_str(), "OBWI" | "INFO" | "INAM" | "JUNK") {
                continue;
            }
            let s = leaf.data_offset as usize;
            let e = s.saturating_add(leaf.data_size as usize).min(map.len());
            if s >= e {
                continue;
            }
            let slice = &map[s..e];

            if leaf.id == "fmt " {
                pending_fmt = Some(slice);
                continue;
            }

            let decoded: Option<AudioBuffer> = if leaf.id == "data" {
                if let Some(fmt_b) = pending_fmt.take() {
                    decode_fmt_and_data(fmt_b, slice).ok()
                } else {
                    try_decode_chunk_payload(&leaf.id, slice)
                }
            } else {
                try_decode_chunk_payload(&leaf.id, slice)
            };

            if let Some(mut audio) = decoded {
                if opts.normalize {
                    normalize_audio_peak(&mut audio, 0.891_250_9);
                }
                detected_sr = audio.sample_rate;
                let idx_meta = index_entries.get(&audio_ordinal);
                if let Some(meta) = idx_meta {
                    if let Some(ref flt) = filter_lc {
                        if !meta.pad_id.to_ascii_lowercase().contains(flt)
                            && !meta.pad_name.to_ascii_lowercase().contains(flt)
                        {
                            audio_ordinal += 1;
                            continue;
                        }
                    }
                    if let Some(ref mf) = mic_lc {
                        if !meta.mic_bus.as_str().contains(mf) {
                            audio_ordinal += 1;
                            continue;
                        }
                    }
                    if let Some(max_rr) = opts.max_rr {
                        if meta.rr_index + 1 > max_rr {
                            audio_ordinal += 1;
                            continue;
                        }
                    }
                }

                let rel = if let Some(meta) = idx_meta {
                    format!(
                        "samples/{}_{}_v{}-{}_rr{}_{}.wav",
                        stem,
                        meta.pad_id,
                        meta.lovel,
                        meta.hivel,
                        meta.rr_index + 1,
                        meta.mic_bus.as_str()
                    )
                } else {
                    format!("samples/{stem}_{audio_ordinal:04}.wav")
                };

                wav::write_wav_file(out_dir.join(&rel), &audio, opts.wav_format)?;
                if let Some(meta) = idx_meta {
                    register_indexed_sample(&mut pads_map, meta, &rel);
                } else {
                    register_unindexed_sample(&mut pads_map, decoded_wav_count, &rel);
                }
                decoded_wav_count += 1;
                audio_ordinal += 1;
            } else {
                let tag = format!("{} (0x{})", leaf.id, leaf.id_hex);
                if !unmapped_set.contains(&tag) {
                    unmapped_set.push(tag);
                }
                if opts.carve_unknown_chunks {
                    let clean_id: String = leaf
                        .id
                        .chars()
                        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
                        .collect();
                    let raw_path = out_dir.join("raw_chunks").join(format!(
                        "{stem}_{clean_id}_{:08x}.bin",
                        leaf.header_offset
                    ));
                    if let Some(p) = raw_path.parent() {
                        fs::create_dir_all(p)?;
                    }
                    fs::write(&raw_path, slice)?;
                    raw_carved_chunks += 1;
                }
            }
        }
    }

    if decoded_wav_count == 0 && !unmapped_set.is_empty() {
        notes.push(format!(
            "Encountered unmapped proprietary chunk types {:?}. Run `sd3tool probe <SL-folder> --save probe.json`.",
            unmapped_set
        ));
    }

    let (manifest_path, sfz_path) = if !pads_map.is_empty() {
        let mut pads: Vec<PadSpec> = pads_map.into_values().collect();
        for pad in &mut pads {
            pad.layers.sort_by_key(|l| l.lovel);
            for l in &mut pad.layers {
                l.round_robins.sort_by_key(|r| r.rr_index);
            }
        }
        if saw_ttbw {
            deduplicate_pad_midi_notes(&mut pads);
        }

        let kit_name = input
            .file_stem()
            .or_else(|| input.file_name())
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Extracted SD3 Kit".into());
        let manifest = DrumKitManifest {
            name: kit_name,
            source_format: if saw_ttbw {
                "sd3-ttbw-extracted".into()
            } else {
                "obw-extracted".into()
            },
            sample_rate: detected_sr,
            pads,
        };
        let mpath = out_dir.join("kit.json");
        manifest.save_json(&mpath)?;
        let spath = if opts.emit_sfz {
            let p = out_dir.join("kit.sfz");
            fs::write(&p, manifest.to_sfz(None))?;
            Some(p)
        } else {
            None
        };
        (Some(mpath), spath)
    } else {
        (None, None)
    };

    Ok(ExtractionSummary {
        input_path: input.to_path_buf(),
        output_dir: out_dir.to_path_buf(),
        decoded_wav_count,
        raw_carved_chunks,
        unmapped_fourccs: unmapped_set,
        manifest_path,
        sfz_path,
        notes,
    })
}

fn sanitize_token(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn normalize_audio_peak(audio: &mut AudioBuffer, target_peak: f32) {
    let mut peak = 0.0f32;
    for ch in &audio.channels {
        for &s in ch {
            peak = peak.max(s.abs());
        }
    }
    if peak > 1e-7 {
        let gain = target_peak / peak;
        for ch in &mut audio.channels {
            for s in ch {
                *s = (*s * gain).clamp(-1.0, 1.0);
            }
        }
    }
}

fn choose_free_or_primary_bus(
    layer: &VelocityLayerSpec,
    rr_zero: usize,
    preferred: MicBus,
) -> MicBus {
    let Some(rr) = layer.round_robins.iter().find(|r| r.rr_index == rr_zero) else {
        return preferred;
    };
    if !rr.mic_files.contains_key(&preferred) {
        return preferred;
    }
    for cand in MicBus::ALL {
        if !rr.mic_files.contains_key(&cand) {
            return cand;
        }
    }
    preferred
}

fn deduplicate_pad_midi_notes(pads: &mut [PadSpec]) {
    let mut used = BTreeSet::new();
    let fallback: Vec<u8> = (58u8..=87)
        .chain(24u8..35)
        .chain(35u8..58)
        .chain(88u8..=119)
        .chain(12u8..24)
        .collect();
    let mut order: Vec<usize> = (0..pads.len()).collect();
    order.sort_by_key(|&i| (pad_gm_priority(&pads[i].id, &pads[i].name), i));
    for idx in order {
        let pad = &mut pads[idx];
        let cur = pad.midi_notes.first().copied().unwrap_or(60).clamp(12, 119);
        if used.insert(cur) {
            pad.midi_notes = vec![cur];
        } else if let Some(&free) = fallback.iter().find(|&&n| !used.contains(&n)) {
            used.insert(free);
            pad.midi_notes = vec![free];
        }
    }
}

fn pad_gm_priority(id: &str, name: &str) -> u8 {
    let up = id.to_ascii_uppercase();
    if up.starts_with("KD") || up.starts_with("SN") || up.starts_with("HH") || up.starts_with("TT") || up.starts_with("FT") {
        0
    } else if name.ends_with("Hit") || name.contains("Bow") || name.ends_with("Bell") {
        1
    } else {
        2
    }
}

fn collect_sound_files(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if depth > 4 {
        return;
    }
    let Ok(rd) = fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            collect_sound_files(&p, depth + 1, out);
        } else if let Some(ext) = p.extension().and_then(|x| x.to_str()) {
            if ext.eq_ignore_ascii_case("obw") || ext.eq_ignore_ascii_case("ttpw") {
                out.push(p);
            }
        }
    }
}

fn collect_leaf_chunks<'a>(chunk: &'a RiffChunk, out: &mut Vec<&'a RiffChunk>) {
    if chunk.children.is_empty() {
        if chunk.form_type.is_none() {
            out.push(chunk);
        }
    } else {
        for c in &chunk.children {
            collect_leaf_chunks(c, out);
        }
    }
}

fn decode_fmt_and_data(fmt_b: &[u8], data_b: &[u8]) -> Result<AudioBuffer> {
    if fmt_b.len() < 16 {
        return Err(Sd3Error::Format("short fmt chunk".into()));
    }
    let mut tag = u16::from_le_bytes([fmt_b[0], fmt_b[1]]);
    let ch = u16::from_le_bytes([fmt_b[2], fmt_b[3]]) as usize;
    let sr = u32::from_le_bytes(fmt_b[4..8].try_into().unwrap());
    let bits = u16::from_le_bytes([fmt_b[14], fmt_b[15]]);
    if tag == 0xFFFE && fmt_b.len() >= 40 {
        tag = u16::from_le_bytes([fmt_b[24], fmt_b[25]]);
    }
    wav::decode_pcm_slice(tag, ch, sr, bits, data_b)
}

fn try_decode_chunk_payload(chunk_id: &str, slice: &[u8]) -> Option<AudioBuffer> {
    if ttpw::looks_like_ttpw(slice) {
        if let Ok(a) = ttpw::decode_ttpw(slice) {
            return Some(a);
        }
    }
    if slice.len() >= 44 && &slice[0..4] == b"RIFF" && &slice[8..12] == b"WAVE" {
        if let Ok(a) = wav::read_wav_bytes(slice) {
            return Some(a);
        }
    }
    if matches!(chunk_id, "TPC1" | "TPC " | "tpc " | "OBWA" | "SDAT") || tpc::looks_like_tpc(slice)
    {
        if let Ok(a) = tpc::decode_tpc_stream(slice, 44_100) {
            return Some(a);
        }
    }
    None
}

fn register_indexed_sample(
    pads_map: &mut BTreeMap<String, PadSpec>,
    meta: &ObwIndexEntry,
    rel_path: &str,
) {
    let pad = pads_map
        .entry(meta.pad_id.clone())
        .or_insert_with(|| PadSpec {
            id: meta.pad_id.clone(),
            name: meta.pad_name.clone(),
            midi_notes: meta.midi_notes.clone(),
            group: meta.group,
            off_by_this: meta.off_by_this,
            hihat_openness: None,
            natural_vel_dynamics: false,
            layers: Vec::new(),
        });
    let layer = match pad
        .layers
        .iter_mut()
        .find(|l| l.lovel == meta.lovel && l.hivel == meta.hivel)
    {
        Some(l) => l,
        None => {
            pad.layers.push(VelocityLayerSpec {
                lovel: meta.lovel,
                hivel: meta.hivel,
                round_robins: Vec::new(),
            });
            pad.layers.last_mut().unwrap()
        }
    };
    if let Some(rr) = layer
        .round_robins
        .iter_mut()
        .find(|r| r.rr_index == meta.rr_index)
    {
        rr.mic_files.insert(meta.mic_bus, rel_path.to_owned());
    } else {
        let mut mic_files = BTreeMap::new();
        mic_files.insert(meta.mic_bus, rel_path.to_owned());
        layer.round_robins.push(RoundRobinSample {
            rr_index: meta.rr_index,
            mic_files,
        });
    }
}

fn register_unindexed_sample(
    pads_map: &mut BTreeMap<String, PadSpec>,
    ordinal: usize,
    rel_path: &str,
) {
    let gm_notes: [u8; 12] = [36, 38, 40, 37, 42, 44, 46, 48, 45, 43, 49, 51];
    let note = gm_notes[ordinal % gm_notes.len()];
    let pad_id = format!("pad_{note}");
    let pad = pads_map.entry(pad_id.clone()).or_insert_with(|| PadSpec {
        id: pad_id,
        name: gm_drum_name(note).to_owned(),
        midi_notes: vec![note],
        group: matches!(note, 42 | 44 | 46).then_some(1),
        off_by_this: (note == 46).then_some(1),
        hihat_openness: None,
        natural_vel_dynamics: false,
        layers: vec![VelocityLayerSpec {
            lovel: 1,
            hivel: 127,
            round_robins: Vec::new(),
        }],
    });
    let rr_idx = pad.layers[0].round_robins.len();
    let mut mic_files = BTreeMap::new();
    mic_files.insert(MicBus::Close, rel_path.to_owned());
    pad.layers[0].round_robins.push(RoundRobinSample {
        rr_index: rr_idx,
        mic_files,
    });
}

/// Build a RIFF `OBW ` soundbunch file with an `OBWI` sample-index chunk and
/// `TPC1` 24-bit lossless LPC+Rice audio chunks (used for synthetic test libraries).
pub fn build_obw_soundbunch(samples: &[(ObwIndexEntry, AudioBuffer)]) -> Result<Vec<u8>> {
    let mut index_vec = Vec::with_capacity(samples.len());
    let mut audio_chunks = Vec::new();

    for (idx, (meta, buf)) in samples.iter().enumerate() {
        let mut m = meta.clone();
        m.chunk_index = idx;
        index_vec.push(m);
        let tpc_bytes = tpc::encode_tpc_stream(buf, true)?;
        audio_chunks.extend_from_slice(&riff::build_chunk(b"TPC1", &tpc_bytes));
    }

    let idx_json = serde_json::to_vec(&index_vec)?;
    let mut body = riff::build_chunk(b"OBWI", &idx_json);
    body.extend_from_slice(&riff::build_container(b"LIST", b"SDAT", &audio_chunks));
    Ok(riff::build_container(b"RIFF", b"OBW ", &body))
}
