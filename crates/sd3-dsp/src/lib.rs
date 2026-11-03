//! Real-time, allocation-free, lock-free polyphonic multi-mic drum engine with
//! zero-OOM `Mmap` + RAM head-preload + background `ttpw` disk streaming.
//!
//! Designed for VST3 / CLAP / Standalone `process()` callbacks:
//! - Pre-allocated fixed pool of `MAX_VOICES` (64) polyphonic multi-bus voices.
//! - Discrete gain staging across 4 studio buses (`Close`, `OH`, `Room`, `Bleed`)
//!   with smooth fast-choke envelopes for hi-hats & cymbal grabs.
//! - Direct loading of multi-gigabyte Superior Drummer 3 (`SL-*`, `SL/<ROOM>`, `EZX*`)
//!   soundbanks and `.obw` (`RIFF 'ttbw'`) soundbunches via `RoomCatalog` +
//!   sequential `Mmap` head preload + lock-free `AtomicU32` SPSC ring buffers.
//! - Right/Left hand round-robin interleaving, velocity layer selection
//!   (`natural_vel_dynamics` aware), MIDI CC4 variable hi-hat openness,
//!   Polyphonic Aftertouch / `_MT_` cymbal chokes, and GM + SD3 MIDI aliases.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crossbeam_channel::{bounded, Receiver, RecvTimeoutError, Sender};
use memmap2::Mmap;

use sd3_core::kit::{DrumKitManifest, MicBus};
use sd3_core::library::{
    self, DiscoveredBank, KitSelection, ObwCatalogEntry, RoomCatalog,
};
use sd3_core::synthkit;
use sd3_core::tpc;
use sd3_core::ttbw::{self, PieceSlot};
use sd3_core::ttpw::{self, TtpwHeader};
use sd3_core::wav::{self, AudioBuffer};
use sd3_core::{Result, Sd3Error};

pub const MAX_VOICES: usize = 64;
/// Number of pads displayed per page in the 4x3 GUI pad bank.
pub const UI_PAD_COUNT: usize = 12;
/// Maximum pads tracked for real-time GUI hit-glow telemetry across pages.
pub const MAX_UI_PADS: usize = 128;

/// Default head preload (`2048` frames = `46.4 ms` @ 44.1 kHz) pinned in RAM per sample.
pub const DEFAULT_HEAD_FRAMES: usize = 2048;
/// Per-voice-bus lock-free streaming ring buffer capacity (`4096` frames = `92.9 ms`, power of two).
pub const STREAM_RING_FRAMES: usize = 4096;
const STREAM_RING_MASK: usize = STREAM_RING_FRAMES - 1;
const STREAM_CHUNK_FRAMES: usize = 512;
const OFFLINE_CACHE_FRAMES: usize = 512;

#[derive(Clone)]
pub struct TtpwStreamRef {
    pub mmap: Arc<Mmap>,
    pub slice_offset: usize,
    pub slice_len: usize,
    pub header: TtpwHeader,
}

#[derive(Clone)]
pub struct AudioSample {
    pub sample_rate: u32,
    pub total_frames: usize,
    pub head_l: Arc<[f32]>,
    pub head_r: Arc<[f32]>,
    pub ttpw_tail: Option<TtpwStreamRef>,
}

impl AudioSample {
    pub fn from_in_memory(sample_rate: u32, left: Vec<f32>, right: Vec<f32>) -> Self {
        let n = left.len().min(right.len());
        Self {
            sample_rate,
            total_frames: n,
            head_l: left[..n].into(),
            head_r: right[..n].into(),
            ttpw_tail: None,
        }
    }

    pub fn from_ttpw_mmap(
        mmap: Arc<Mmap>,
        slice_offset: usize,
        slice_len: usize,
        head_budget: usize,
    ) -> Result<Self> {
        let end = slice_offset
            .checked_add(slice_len)
            .ok_or_else(|| Sd3Error::Format("ttpw slice offset overflow".into()))?;
        if end > mmap.len() {
            return Err(Sd3Error::Format("ttpw slice exceeds mmap bounds".into()));
        }
        let slice = &mmap[slice_offset..end];
        if ttpw::looks_like_ttpw(slice) {
            let hdr = ttpw::parse_ttpw_header(slice)?;
            let sr = hdr.sample_rate;
            let total_frames = hdr.total_frames as usize;
            let head_n = total_frames.min(head_budget.max(64));

            let mut hl = vec![0.0f32; head_n];
            let mut hr = vec![0.0f32; head_n];
            if head_n > 0 {
                ttpw::decode_ttpw_range_stereo(slice, &hdr, 0, head_n, &mut hl, &mut hr)?;
            }

            let ttpw_tail = if total_frames > head_n {
                Some(TtpwStreamRef {
                    mmap,
                    slice_offset,
                    slice_len,
                    header: hdr,
                })
            } else {
                None
            };

            Ok(Self {
                sample_rate: sr,
                total_frames,
                head_l: hl.into(),
                head_r: hr.into(),
                ttpw_tail,
            })
        } else if slice.len() >= 44 && &slice[0..4] == b"RIFF" && &slice[8..12] == b"WAVE" {
            let buf = wav::read_wav_bytes(slice)?;
            let sr = buf.sample_rate;
            let (l, r) = audio_buffer_to_stereo(buf);
            Ok(Self::from_in_memory(sr, l, r))
        } else {
            Err(Sd3Error::Format(
                "slice is neither RIFF 'ttpw' nor RIFF 'WAVE'".into(),
            ))
        }
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.total_frames
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.total_frames == 0
    }

    #[inline]
    pub fn has_stream_tail(&self) -> bool {
        self.ttpw_tail.is_some()
    }
}

#[derive(Clone)]
pub struct LoadedRoundRobin {
    pub buses: [Option<AudioSample>; 4],
}

#[derive(Clone)]
pub struct LoadedLayer {
    pub lovel: u8,
    pub hivel: u8,
    pub round_robins: Vec<LoadedRoundRobin>,
}

#[derive(Clone)]
pub struct LoadedPad {
    pub id: String,
    pub name: String,
    pub primary_note: u8,
    pub midi_notes: Vec<u8>,
    pub group: Option<u32>,
    pub off_by: Option<u32>,
    pub hihat_openness: Option<u8>,
    pub natural_vel_dynamics: bool,
    pub layers: Vec<LoadedLayer>,
}

pub struct LoadedKit {
    pub name: String,
    pub source_desc: String,
    pub sample_rate: u32,
    pub pads: Vec<LoadedPad>,
    /// O(1) MIDI note -> pad index lookup table.
    pub note_to_pad: [Option<usize>; 128],
    pub total_samples: usize,
    pub streamed_samples: usize,
    pub bus_mic_summary: [String; 4],
}

impl LoadedKit {
    /// Generate the built-in synthesized multi-mic, 3-velocity, 2-RR studio drum kit.
    pub fn built_in_demo(sample_rate: u32) -> Arc<Self> {
        let mem_pads = synthkit::build_in_memory_demo_kit(sample_rate);
        let mut pads = Vec::with_capacity(mem_pads.len());
        let mut note_to_pad = [None; 128];
        let mut total_samples = 0usize;

        for (p_idx, mp) in mem_pads.into_iter().enumerate() {
            let primary_note = mp.midi_notes.first().copied().unwrap_or(36);
            for &n in &mp.midi_notes {
                if (n as usize) < 128 && note_to_pad[n as usize].is_none() {
                    note_to_pad[n as usize] = Some(p_idx);
                }
            }
            let hihat_openness = match mp.id.as_str() {
                "hh_closed" | "hh_pedal" => Some(0),
                "hh_open" => Some(127),
                _ => None,
            };
            let mut layers = Vec::with_capacity(mp.layers.len());
            for ml in mp.layers {
                let mut rrs = Vec::with_capacity(ml.round_robins.len());
                for rr_map in ml.round_robins {
                    let mut buses: [Option<AudioSample>; 4] = [None, None, None, None];
                    for (bus, [l, r]) in rr_map {
                        total_samples += 1;
                        buses[bus.index()] = Some(AudioSample::from_in_memory(sample_rate, l, r));
                    }
                    rrs.push(LoadedRoundRobin { buses });
                }
                layers.push(LoadedLayer {
                    lovel: ml.lovel,
                    hivel: ml.hivel,
                    round_robins: rrs,
                });
            }
            pads.push(LoadedPad {
                id: mp.id,
                name: mp.name,
                primary_note,
                midi_notes: mp.midi_notes,
                group: mp.group,
                off_by: mp.off_by_this,
                hihat_openness,
                natural_vel_dynamics: false,
                layers,
            });
        }

        Arc::new(Self {
            name: "OpenSD3 Studio Reference Kit (Built-in)".into(),
            source_desc: "built-in-modal-multimic".into(),
            sample_rate,
            pads,
            note_to_pad,
            total_samples,
            streamed_samples: 0,
            bus_mic_summary: [
                "Synth Direct".into(),
                "Synth Stereo OH".into(),
                "Synth Studio Room".into(),
                "Synth Snare Bleed".into(),
            ],
        })
    }

    pub fn load_from_path(path: impl AsRef<Path>) -> Result<Arc<Self>> {
        Self::load_from_path_with_progress(path, |_, _| {})
    }

    /// Load a kit from:
    /// - any Toontrack root directory, `SL-*` Superior Drummer 2/3 soundbank, `SL/<ROOM>` SDX, or `EZX*` folder,
    /// - a standalone `.obw` (`RIFF 'ttbw'` or `RIFF 'OBW '`) soundbunch,
    /// - a standalone `.ttpw` / `.wav` file,
    /// - an extracted `kit.json` manifest (or directory containing `kit.json`), or
    /// - a standard `.sfz` file.
    pub fn load_from_path_with_progress<F>(
        path: impl AsRef<Path>,
        mut on_progress: F,
    ) -> Result<Arc<Self>>
    where
        F: FnMut(f32, &str),
    {
        let p = path.as_ref();
        if !p.exists() {
            return Err(Sd3Error::Format(format!(
                "path does not exist: {}",
                p.display()
            )));
        }

        if p.is_dir() {
            let json_cand = p.join("kit.json");
            if json_cand.is_file() {
                on_progress(0.2, "Loading kit.json manifest...");
                let manifest = DrumKitManifest::load_json(&json_cand)?;
                return Self::from_manifest(&manifest, p);
            }
            if let Ok(rd) = fs::read_dir(p) {
                for e in rd.flatten() {
                    if e.path()
                        .extension()
                        .and_then(|x| x.to_str())
                        .map(|s| s.eq_ignore_ascii_case("sfz"))
                        .unwrap_or(false)
                    {
                        on_progress(0.2, "Loading .sfz instrument...");
                        let (manifest, base) = DrumKitManifest::from_sfz_file(e.path())?;
                        return Self::from_manifest(&manifest, &base);
                    }
                }
            }

            on_progress(0.05, "Scanning folder for Superior Drummer soundbanks...");
            let banks = library::scan_soundbanks_fast(p)?;
            if let Some(first_bank) = banks.first() {
                let cat = RoomCatalog::scan_bank(first_bank, |done, tot, fname| {
                    let frac = 0.05 + 0.25 * (done as f32 / tot.max(1) as f32);
                    on_progress(
                        frac,
                        &format!("Indexing {} ({done}/{tot}: {fname})...", first_bank.display_name),
                    );
                })?;
                let sel = KitSelection::default_for_catalog(&cat);
                return Self::load_room_selection(&cat, &sel, |frac, msg| {
                    on_progress(0.30 + 0.70 * frac, msg);
                });
            }

            return Self::load_obw_or_sl_direct(p, &mut on_progress);
        }

        let ext = p
            .extension()
            .and_then(|x| x.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        match ext.as_str() {
            "json" => {
                let manifest = DrumKitManifest::load_json(p)?;
                let base = p.parent().unwrap_or_else(|| Path::new("."));
                Self::from_manifest(&manifest, base)
            }
            "sfz" => {
                let (manifest, base) = DrumKitManifest::from_sfz_file(p)?;
                Self::from_manifest(&manifest, &base)
            }
            "obw" => {
                let fname = p
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "soundbunch.obw".into());
                let fsize = fs::metadata(p).map(|m| m.len()).unwrap_or(0);
                let bank = DiscoveredBank {
                    id: p.to_string_lossy().into_owned(),
                    display_name: discover_display_name(p),
                    folder_name: fname,
                    room_path: p.parent().unwrap_or(p).to_path_buf(),
                    obw_files: vec![p.to_path_buf()],
                    total_bytes: fsize,
                    aversion: None,
                };
                let cat = RoomCatalog::scan_bank(&bank, |_, _, _| {
                    on_progress(0.15, "Reading .obw oidx header...");
                })?;
                if cat.entries.iter().any(|e| e.ttbw_index.is_some()) {
                    // For a standalone single `.obw` file, load ALL pieces inside that `.obw`!
                    let sel = KitSelection {
                        tool: "ALL".into(),
                        kick: "ALL".into(),
                        snare: "ALL".into(),
                        hihat: "ALL".into(),
                        toms: "ALL".into(),
                        ride: "ALL".into(),
                        cymbals: "ALL".into(),
                        include_percussion: true,
                    };
                    Self::load_room_selection(&cat, &sel, |frac, msg| {
                        on_progress(0.20 + 0.80 * frac, msg);
                    })
                } else {
                    Self::load_obw_or_sl_direct(p, &mut on_progress)
                }
            }
            "ttpw" => Self::load_obw_or_sl_direct(p, &mut on_progress),
            "wav" => {
                let file = File::open(p)?;
                let map = unsafe { Mmap::map(&file)? };
                if ttpw::looks_like_ttpw(&map) {
                    Self::load_obw_or_sl_direct(p, &mut on_progress)
                } else {
                    let buf = wav::read_wav_bytes(&map)?;
                    Self::from_single_audio_buffer(p, buf, "wav")
                }
            }
            _ => Err(Sd3Error::Format(format!(
                "unsupported file '{}' (select a Superior Drummer SL-* soundbank folder, .obw, .sfz, or kit.json)",
                p.display()
            ))),
        }
    }

    /// Assemble a playable multi-mic drum kit from a scanned Superior Drummer `RoomCatalog`
    /// and `KitSelection` using sequential `.obw` offset sweeping (`< 0.3s` load time even on
    /// huge multi-GB soundbanks!).
    pub fn load_room_selection<F>(
        catalog: &RoomCatalog,
        selection: &KitSelection,
        mut on_progress: F,
    ) -> Result<Arc<Self>>
    where
        F: FnMut(f32, &str),
    {
        // Check if this bank has legacy/non-ttbw files only
        if !catalog.entries.iter().any(|e| e.ttbw_index.is_some()) {
            return Self::load_entries_legacy(&catalog.bank.display_name, &catalog.entries, &mut on_progress);
        }

        // Determine per-slot whether the requested hand tool (`DS`/`BR`/...) actually exists for
        // the selected piece; if not (e.g. percussion recorded only with `HD`), fall back gracefully!
        let piece_matches = |grp: &ttbw::OidxGroup| -> bool {
            let p = &grp.parsed;
            match p.slot {
                PieceSlot::Kick => {
                    selection.kick.is_empty()
                        || selection.kick == "ALL"
                        || p.piece_code == selection.kick
                }
                PieceSlot::Snare => {
                    selection.snare.is_empty()
                        || selection.snare == "ALL"
                        || p.piece_code == selection.snare
                }
                PieceSlot::HiHat => {
                    selection.hihat.is_empty()
                        || selection.hihat == "ALL"
                        || p.piece_code == selection.hihat
                }
                PieceSlot::Tom => {
                    selection.toms.is_empty()
                        || selection.toms == "ALL"
                        || p.model_id == selection.toms
                }
                PieceSlot::Ride => {
                    selection.ride.is_empty()
                        || selection.ride == "ALL"
                        || p.piece_code == selection.ride
                }
                PieceSlot::Cymbal => {
                    selection.cymbals.is_empty()
                        || selection.cymbals == "ALL"
                        || p.model_id == selection.cymbals
                }
                PieceSlot::Percussion => selection.include_percussion,
            }
        };

        let want_specific_tool = !selection.tool.is_empty() && selection.tool != "ALL";
        let mut slots_with_exact_tool = [false; 7];
        let slot_ord = |s: PieceSlot| -> usize {
            match s {
                PieceSlot::Kick => 0,
                PieceSlot::Snare => 1,
                PieceSlot::HiHat => 2,
                PieceSlot::Tom => 3,
                PieceSlot::Ride => 4,
                PieceSlot::Cymbal => 5,
                PieceSlot::Percussion => 6,
            }
        };

        if want_specific_tool {
            for entry in &catalog.entries {
                if entry.surround_only {
                    continue;
                }
                if let Some(ref idx) = entry.ttbw_index {
                    for grp in &idx.groups {
                        if piece_matches(grp)
                            && grp.parsed.is_hand_tool
                            && grp.parsed.tool_code == selection.tool
                        {
                            slots_with_exact_tool[slot_ord(grp.parsed.slot)] = true;
                        }
                    }
                }
            }
        }

        // Check whether ANY non-surround stereo `.obw` exists; if a user ONLY has a surround `.obw`, allow it.
        let has_stereo_obws = catalog
            .entries
            .iter()
            .any(|e| e.ttbw_index.is_some() && !e.surround_only);

        #[derive(Clone, Copy)]
        struct WinningSlice {
            entry_idx: usize,
            data_offset: usize,
            byte_size: usize,
            prio: u8,
        }

        struct PendingPad {
            id: String,
            name: String,
            slot: PieceSlot,
            artic_code: String,
            slot_pos_num: u8,
            preferred_note: u8,
            group: Option<u32>,
            off_by: Option<u32>,
            hihat_openness: Option<u8>,
            /// `vel_char -> (rr_num, hand_ord) -> [Option<WinningSlice>; 4]`
            hits: BTreeMap<char, BTreeMap<(u8, u8), [Option<WinningSlice>; 4]>>,
        }

        let mut pending_pads: BTreeMap<String, PendingPad> = BTreeMap::new();
        let mut pad_order: Vec<String> = Vec::new();

        for (entry_idx, entry) in catalog.entries.iter().enumerate() {
            if has_stereo_obws && entry.surround_only {
                continue;
            }
            let Some(ref idx) = entry.ttbw_index else {
                continue;
            };
            let data_base = idx.data_payload_offset as usize;
            let data_end = data_base.saturating_add(idx.data_payload_size as usize);

            for grp in &idx.groups {
                if !piece_matches(grp) {
                    continue;
                }
                if want_specific_tool
                    && grp.parsed.is_hand_tool
                    && slots_with_exact_tool[slot_ord(grp.parsed.slot)]
                    && grp.parsed.tool_code != selection.tool
                {
                    continue;
                }

                let pad_key = grp.parsed.pad_merge_key.clone();
                if !pending_pads.contains_key(&pad_key) {
                    let (group_num, off_by_num) =
                        match (grp.member_choke_group, grp.triggers_choke_group) {
                            (Some(_), Some(trig)) => (Some(trig as u32), None),
                            (Some(mem), None) => (Some(100 + mem as u32), Some(mem as u32)),
                            (None, Some(trig)) => (Some(trig as u32), None),
                            (None, None) => (None, None),
                        };
                    let slot_pos_num = grp.parsed.slot_pos.parse::<u8>().unwrap_or(1);
                    pad_order.push(pad_key.clone());
                    pending_pads.insert(
                        pad_key.clone(),
                        PendingPad {
                            id: pad_key.clone(),
                            name: grp.display_name.clone(),
                            slot: grp.parsed.slot,
                            artic_code: grp.articulation_code.clone(),
                            slot_pos_num,
                            preferred_note: grp.preferred_note,
                            group: group_num,
                            off_by: off_by_num,
                            hihat_openness: grp.hihat_openness,
                            hits: BTreeMap::new(),
                        },
                    );
                }

                let pp = pending_pads.get_mut(&pad_key).unwrap();
                let hand_ord = if grp.parsed.slot == PieceSlot::Kick {
                    0u8
                } else if grp.parsed.hand == Some('L') {
                    1u8
                } else {
                    0u8
                };

                for mh in &grp.mics {
                    let bus_idx = mh.bus.index();
                    let prio = ttbw::mic_priority_in_bus(&mh.mic_name, mh.bus);

                    for rec in &mh.records {
                        let abs_off = data_base.saturating_add(rec.data_offset as usize);
                        let sz = rec.byte_size as usize;
                        if sz < 20 || abs_off.saturating_add(sz) > data_end {
                            continue;
                        }
                        let rr_map = pp.hits.entry(rec.vel_char).or_default();
                        let buses = rr_map.entry((rec.rr_num, hand_ord)).or_default();
                        let replace = match buses[bus_idx] {
                            None => true,
                            Some(prev) => prio < prev.prio,
                        };
                        if replace {
                            buses[bus_idx] = Some(WinningSlice {
                                entry_idx,
                                data_offset: abs_off,
                                byte_size: sz,
                                prio,
                            });
                        }
                    }
                }
            }
        }

        // Collect all unique `(entry_idx, data_offset, byte_size)` winning slices and sort sequentially!
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
        struct SliceKey {
            entry_idx: usize,
            data_offset: usize,
            byte_size: usize,
        }

        let mut unique_slices: Vec<SliceKey> = Vec::new();
        for pp in pending_pads.values() {
            for rr_map in pp.hits.values() {
                for buses in rr_map.values() {
                    for s in buses.iter().flatten() {
                        unique_slices.push(SliceKey {
                            entry_idx: s.entry_idx,
                            data_offset: s.data_offset,
                            byte_size: s.byte_size,
                        });
                    }
                }
            }
        }
        unique_slices.sort_unstable();
        unique_slices.dedup();

        if unique_slices.is_empty() {
            return Err(Sd3Error::Format(format!(
                "no playable samples matched selection in '{}'",
                catalog.bank.display_name
            )));
        }

        let total_to_preload = unique_slices.len();
        let head_budget = if total_to_preload <= 3_000 {
            DEFAULT_HEAD_FRAMES // 46.4 ms
        } else if total_to_preload <= 12_000 {
            1024 // 23.2 ms
        } else {
            512 // 11.6 ms
        };

        // Sequential forward sweep per `.obw` file — maximizes OS page-cache read-ahead!
        let mut decoded_map: BTreeMap<SliceKey, AudioSample> = BTreeMap::new();
        let mut current_entry: Option<(usize, Arc<Mmap>)> = None;
        let mut streamed_samples = 0usize;
        let mut detected_sr = 44_100u32;

        for (done_idx, sk) in unique_slices.iter().copied().enumerate() {
            if done_idx % 64 == 0 || done_idx + 1 == total_to_preload {
                let frac = done_idx as f32 / total_to_preload.max(1) as f32;
                let fname = &catalog.entries[sk.entry_idx].file_name;
                on_progress(
                    frac,
                    &format!(
                        "Preloading {} ({}/{total_to_preload} samples, {fname})...",
                        catalog.bank.display_name,
                        done_idx + 1
                    ),
                );
            }

            let need_open = match current_entry {
                Some((idx, _)) => idx != sk.entry_idx,
                None => true,
            };
            if need_open {
                let path = &catalog.entries[sk.entry_idx].path;
                let Ok(file) = File::open(path) else {
                    continue;
                };
                let Ok(mmap) = (unsafe { Mmap::map(&file) }) else {
                    continue;
                };
                current_entry = Some((sk.entry_idx, Arc::new(mmap)));
            }

            let mmap = &current_entry.as_ref().unwrap().1;
            if let Ok(sample) =
                AudioSample::from_ttpw_mmap(Arc::clone(mmap), sk.data_offset, sk.byte_size, head_budget)
            {
                detected_sr = sample.sample_rate;
                if sample.has_stream_tail() {
                    streamed_samples += 1;
                }
                decoded_map.insert(sk, sample);
            }
        }

        // Assemble final `LoadedPad` list with velocity tiers & interleaved L/R round-robins
        struct PadWithMeta {
            pad: LoadedPad,
            slot: PieceSlot,
            artic_code: String,
            slot_pos_num: u8,
        }

        let mut built: Vec<PadWithMeta> = Vec::new();
        let mut total_samples = 0usize;

        for key in pad_order {
            let Some(pp) = pending_pads.remove(&key) else {
                continue;
            };
            let vel_chars: Vec<char> = pp.hits.keys().copied().collect();
            if vel_chars.is_empty() {
                continue;
            }
            let vel_ranges = ttbw::compute_velocity_ranges(&vel_chars);
            let natural_vel = vel_ranges.len() >= 4;

            let mut layers = Vec::with_capacity(vel_chars.len());
            for (vch, rr_map) in pp.hits {
                let &(lovel, hivel) = vel_ranges.get(&vch).unwrap_or(&(1, 127));
                let mut rrs = Vec::with_capacity(rr_map.len());
                for (_, win_buses) in rr_map {
                    let mut buses: [Option<AudioSample>; 4] = [None, None, None, None];
                    for b in 0..4 {
                        if let Some(ws) = win_buses[b] {
                            let sk = SliceKey {
                                entry_idx: ws.entry_idx,
                                data_offset: ws.data_offset,
                                byte_size: ws.byte_size,
                            };
                            if let Some(s) = decoded_map.get(&sk) {
                                buses[b] = Some(s.clone());
                                total_samples += 1;
                            }
                        }
                    }
                    if buses.iter().any(Option::is_some) {
                        rrs.push(LoadedRoundRobin { buses });
                    }
                }
                if !rrs.is_empty() {
                    layers.push(LoadedLayer {
                        lovel,
                        hivel,
                        round_robins: rrs,
                    });
                }
            }

            if !layers.is_empty() {
                layers.sort_by_key(|l| l.lovel);
                built.push(PadWithMeta {
                    pad: LoadedPad {
                        id: pp.id,
                        name: pp.name,
                        primary_note: pp.preferred_note,
                        midi_notes: vec![pp.preferred_note],
                        group: pp.group,
                        off_by: pp.off_by,
                        hihat_openness: pp.hihat_openness,
                        natural_vel_dynamics: natural_vel,
                        layers,
                    },
                    slot: pp.slot,
                    artic_code: pp.artic_code,
                    slot_pos_num: pp.slot_pos_num,
                });
            }
        }

        if built.is_empty() {
            return Err(Sd3Error::Format(format!(
                "no playable audio streams decoded from '{}'",
                catalog.bank.display_name
            )));
        }

        // Sort pads in logical drummer order: Kick -> Snare -> Hi-Hat -> Toms -> Ride -> Cymbals -> Perc
        built.sort_by_key(|m| {
            let slot_rank = match m.slot {
                PieceSlot::Kick => 0u8,
                PieceSlot::Snare => 1,
                PieceSlot::HiHat => 2,
                PieceSlot::Tom => 3,
                PieceSlot::Ride => 4,
                PieceSlot::Cymbal => 5,
                PieceSlot::Percussion => 6,
            };
            let artic_rank = match m.artic_code.as_str() {
                "CN" | "HT" | "HIT" | "CL" | "CR" | "RT" => 0u8,
                "RS" | "PD" | "OP" | "BL" => 1,
                "SS" | "O1" | "O2" | "O3" | "ED" => 2,
                "MT" | "CK" => 9,
                _ => 4,
            };
            (slot_rank, m.slot_pos_num, artic_rank, m.pad.id.clone())
        });

        let mut pads: Vec<LoadedPad> = built.into_iter().map(|m| m.pad).collect();
        let mut note_to_pad = [None; 128];
        assign_loaded_pad_notes_and_aliases(&mut pads, &mut note_to_pad);

        let bus_mic_summary = [
            catalog.mic_summary_for_bus(MicBus::Close),
            catalog.mic_summary_for_bus(MicBus::Oh),
            catalog.mic_summary_for_bus(MicBus::Room),
            catalog.mic_summary_for_bus(MicBus::Bleed),
        ];

        on_progress(1.0, "Ready");

        let display_kit_name = if selection.tool != "DS" && selection.tool != "ALL" {
            format!(
                "{} [{}]",
                catalog.bank.display_name,
                ttbw::tool_display_name(&selection.tool)
            )
        } else {
            catalog.bank.display_name.clone()
        };

        Ok(Arc::new(Self {
            name: display_kit_name,
            source_desc: format!(
                "{} .obw, {streamed_samples} streamed",
                catalog.entries.len()
            ),
            sample_rate: detected_sr,
            pads,
            note_to_pad,
            total_samples,
            streamed_samples,
            bus_mic_summary,
        }))
    }

    fn load_entries_legacy<F>(
        title: &str,
        entries: &[ObwCatalogEntry],
        on_progress: &mut F,
    ) -> Result<Arc<Self>>
    where
        F: FnMut(f32, &str),
    {
        let paths: Vec<PathBuf> = entries.iter().map(|e| e.path.clone()).collect();
        Self::load_legacy_files_list(title, &paths, on_progress)
    }

    fn from_single_audio_buffer(path: &Path, buf: AudioBuffer, desc: &str) -> Result<Arc<Self>> {
        let sr = buf.sample_rate;
        let (l, r) = audio_buffer_to_stereo(buf);
        let sample = AudioSample::from_in_memory(sr, l, r);
        let mut buses: [Option<AudioSample>; 4] = [None, None, None, None];
        buses[MicBus::Close.index()] = Some(sample);
        let name = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Single Sample".into());
        let mut note_to_pad = [None; 128];
        note_to_pad[36] = Some(0);
        Ok(Arc::new(Self {
            name: name.clone(),
            source_desc: desc.into(),
            sample_rate: sr,
            pads: vec![LoadedPad {
                id: "pad_36".into(),
                name,
                primary_note: 36,
                midi_notes: vec![36],
                group: None,
                off_by: None,
                hihat_openness: None,
                natural_vel_dynamics: false,
                layers: vec![LoadedLayer {
                    lovel: 1,
                    hivel: 127,
                    round_robins: vec![LoadedRoundRobin { buses }],
                }],
            }],
            note_to_pad,
            total_samples: 1,
            streamed_samples: 0,
            bus_mic_summary: ["Direct".into(), "—".into(), "—".into(), "—".into()],
        }))
    }

    pub fn from_manifest(manifest: &DrumKitManifest, base_dir: &Path) -> Result<Arc<Self>> {
        let mut pads = Vec::new();
        let mut note_to_pad = [None; 128];
        let mut total_samples = 0usize;
        let mut sr = manifest.sample_rate.max(8_000);
        let mut saw_bus = [false; 4];

        for ps in &manifest.pads {
            let primary_note = ps.midi_notes.first().copied().unwrap_or(36);
            let mut layers = Vec::new();
            for ls in &ps.layers {
                let mut rrs = Vec::new();
                for rr in &ls.round_robins {
                    let mut buses: [Option<AudioSample>; 4] = [None, None, None, None];
                    for (&bus, rel_path) in &rr.mic_files {
                        let full = base_dir.join(rel_path);
                        if let Ok(buf) = wav::read_wav_file(&full) {
                            sr = buf.sample_rate;
                            let (left, right) = audio_buffer_to_stereo(buf);
                            buses[bus.index()] = Some(AudioSample::from_in_memory(sr, left, right));
                            saw_bus[bus.index()] = true;
                            total_samples += 1;
                        }
                    }
                    if buses.iter().any(Option::is_some) {
                        rrs.push(LoadedRoundRobin { buses });
                    }
                }
                if !rrs.is_empty() {
                    layers.push(LoadedLayer {
                        lovel: ls.lovel,
                        hivel: ls.hivel,
                        round_robins: rrs,
                    });
                }
            }
            if !layers.is_empty() {
                let p_idx = pads.len();
                for &n in &ps.midi_notes {
                    if (n as usize) < 128 && note_to_pad[n as usize].is_none() {
                        note_to_pad[n as usize] = Some(p_idx);
                    }
                }
                pads.push(LoadedPad {
                    id: ps.id.clone(),
                    name: ps.name.clone(),
                    primary_note,
                    midi_notes: ps.midi_notes.clone(),
                    group: ps.group,
                    off_by: ps.off_by_this,
                    hihat_openness: ps.hihat_openness,
                    natural_vel_dynamics: ps.natural_vel_dynamics,
                    layers,
                });
            }
        }

        if pads.is_empty() {
            return Err(Sd3Error::Format(format!(
                "manifest '{}' yielded 0 playable audio samples",
                manifest.name
            )));
        }

        Ok(Arc::new(Self {
            name: manifest.name.clone(),
            source_desc: manifest.source_format.clone(),
            sample_rate: sr,
            pads,
            note_to_pad,
            total_samples,
            streamed_samples: 0,
            bus_mic_summary: [
                if saw_bus[0] { "Close WAV".into() } else { "—".into() },
                if saw_bus[1] { "OH WAV".into() } else { "—".into() },
                if saw_bus[2] { "Room WAV".into() } else { "—".into() },
                if saw_bus[3] { "Bleed WAV".into() } else { "—".into() },
            ],
        }))
    }

    fn load_obw_or_sl_direct<F>(path: &Path, on_progress: &mut F) -> Result<Arc<Self>>
    where
        F: FnMut(f32, &str),
    {
        let mut files = Vec::new();
        if path.is_file() {
            files.push(path.to_path_buf());
        } else if path.is_dir() {
            collect_obw_and_ttpw(path, 0, &mut files);
            files.sort();
        }
        if files.is_empty() {
            return Err(Sd3Error::Format(format!(
                "no Superior Drummer .obw, .ttpw, .sfz, or kit.json files found under {}",
                path.display()
            )));
        }
        let title = discover_display_name(path);
        Self::load_legacy_files_list(&title, &files, on_progress)
    }

    fn load_legacy_files_list<F>(
        title: &str,
        files: &[PathBuf],
        on_progress: &mut F,
    ) -> Result<Arc<Self>>
    where
        F: FnMut(f32, &str),
    {
        let mut pads_by_id: BTreeMap<String, LoadedPad> = BTreeMap::new();
        let mut pad_order: Vec<String> = Vec::new();
        let mut total_samples = 0usize;
        let mut streamed_samples = 0usize;
        let mut detected_sr = 44_100u32;

        for (i, fpath) in files.iter().enumerate() {
            on_progress(
                i as f32 / files.len().max(1) as f32,
                &format!("Loading {}...", fpath.display()),
            );
            let file = File::open(fpath)?;
            let mmap = Arc::new(unsafe { Mmap::map(&file)? });
            if mmap.len() < 12 {
                continue;
            }
            if ttpw::looks_like_ttpw(&mmap) {
                let sample =
                    AudioSample::from_ttpw_mmap(Arc::clone(&mmap), 0, mmap.len(), DEFAULT_HEAD_FRAMES)?;
                detected_sr = sample.sample_rate;
                if sample.has_stream_tail() {
                    streamed_samples += 1;
                }
                total_samples += 1;
                let id = fpath
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| format!("ttpw_{total_samples}"));
                let note = (36 + (pads_by_id.len() as u8)).min(108);
                let mut buses = [None, None, None, None];
                buses[MicBus::Close.index()] = Some(sample);
                pad_order.push(id.clone());
                pads_by_id.insert(
                    id.clone(),
                    LoadedPad {
                        id: id.clone(),
                        name: id,
                        primary_note: note,
                        midi_notes: vec![note],
                        group: None,
                        off_by: None,
                        hihat_openness: None,
                        natural_vel_dynamics: false,
                        layers: vec![LoadedLayer {
                            lovel: 1,
                            hivel: 127,
                            round_robins: vec![LoadedRoundRobin { buses }],
                        }],
                    },
                );
            } else {
                load_legacy_obw_into_pads(
                    &mmap,
                    &mut pads_by_id,
                    &mut pad_order,
                    &mut total_samples,
                    &mut detected_sr,
                )?;
            }
        }

        let pads: Vec<LoadedPad> = pad_order
            .into_iter()
            .filter_map(|id| pads_by_id.remove(&id))
            .filter(|p| !p.layers.is_empty())
            .collect();

        if pads.is_empty() {
            return Err(Sd3Error::Format(format!(
                "no playable audio streams decoded from '{title}'"
            )));
        }

        let mut note_to_pad = [None; 128];
        for (i, p) in pads.iter().enumerate() {
            for &n in &p.midi_notes {
                if (n as usize) < 128 && note_to_pad[n as usize].is_none() {
                    note_to_pad[n as usize] = Some(i);
                }
            }
        }

        Ok(Arc::new(Self {
            name: title.to_owned(),
            source_desc: if streamed_samples > 0 {
                format!("ttpw-mmap ({streamed_samples} streamed)")
            } else {
                "obw-ram".into()
            },
            sample_rate: detected_sr,
            pads,
            note_to_pad,
            total_samples,
            streamed_samples,
            bus_mic_summary: [
                "Close".into(),
                "Overheads".into(),
                "Room".into(),
                "Bleed".into(),
            ],
        }))
    }
}

fn discover_display_name(path: &Path) -> String {
    let dir = if path.is_dir() {
        path
    } else {
        path.parent().unwrap_or(path)
    };
    for cand in [dir.join("Name"), dir.parent().unwrap_or(dir).join("Name")] {
        if let Ok(bytes) = fs::read(&cand) {
            let txt = if bytes.starts_with(&[0xFF, 0xFE]) {
                let units: Vec<u16> = bytes[2..]
                    .chunks_exact(2)
                    .map(|c| u16::from_le_bytes([c[0], c[1]]))
                    .collect();
                String::from_utf16_lossy(&units)
            } else {
                String::from_utf8_lossy(&bytes).into_owned()
            };
            if let Some(first) = txt.lines().map(str::trim).find(|l| !l.is_empty()) {
                return first.to_owned();
            }
        }
    }
    path.file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "SD3 Kit".into())
}

/// Assign collision-free primary GM/SD3 MIDI notes AND populate musical alias notes
/// (`35` double-kick, `33` SD3 left snare, `22`/`24..26` SD3 hi-hats, and missing GM tom notes)
/// so ANY General MIDI or Toontrack MIDI groove triggers every drum seamlessly!
fn assign_loaded_pad_notes_and_aliases(
    pads: &mut [LoadedPad],
    note_to_pad: &mut [Option<usize>; 128],
) {
    let fallback: Vec<u8> = (58u8..=87)
        .chain(24u8..35)
        .chain(35u8..58)
        .chain(88u8..=119)
        .chain(12u8..24)
        .collect();

    let mut order: Vec<usize> = (0..pads.len()).collect();
    order.sort_by_key(|&i| {
        let up = pads[i].id.to_ascii_uppercase();
        let prio = if up.starts_with("KD")
            || up.starts_with("SN")
            || up.starts_with("HH")
            || up.starts_with("TT")
            || up.starts_with("FT")
        {
            0u8
        } else if pads[i].name.ends_with("Hit")
            || pads[i].name.contains("Bow")
            || pads[i].name.ends_with("Bell")
        {
            1
        } else {
            2
        };
        (prio, i)
    });

    for idx in order {
        let p = &mut pads[idx];
        let pref = p.primary_note.clamp(12, 119);
        let chosen = if note_to_pad[pref as usize].is_none() {
            pref
        } else {
            fallback
                .iter()
                .copied()
                .find(|&n| note_to_pad[n as usize].is_none())
                .unwrap_or(pref)
        };
        p.primary_note = chosen;
        p.midi_notes = vec![chosen];
        if note_to_pad[chosen as usize].is_none() {
            note_to_pad[chosen as usize] = Some(idx);
        }
    }

    // Populate GM + Superior Drummer 3 MIDI alias notes for any unassigned standard drum keys!
    let mut bind_alias = |alias_note: u8, target_note: u8| {
        if note_to_pad[alias_note as usize].is_none() {
            if let Some(pad_idx) = note_to_pad[target_note as usize] {
                note_to_pad[alias_note as usize] = Some(pad_idx);
                if !pads[pad_idx].midi_notes.contains(&alias_note) {
                    pads[pad_idx].midi_notes.push(alias_note);
                }
            }
        }
    };

    // Double-kick fallback (`35 <-> 36`)
    bind_alias(35, 36);
    bind_alias(36, 35);
    // Snare aliases (`33` = SD3 Snare Left/Edge, `40` = Rimshot fallback to Center)
    bind_alias(33, 38);
    bind_alias(40, 38);
    // Hi-Hat GM & SD3 aliases (`42<->22` Closed, `44<->21` Pedal, `46<->24..26` Open Hat tiers)
    bind_alias(42, 22);
    bind_alias(22, 42);
    bind_alias(44, 21);
    bind_alias(21, 44);
    bind_alias(46, 26);
    bind_alias(46, 25);
    bind_alias(46, 24);
    bind_alias(46, 60);
    bind_alias(26, 46);
    bind_alias(25, 46);
    bind_alias(24, 46);

    // Smart Tom pitch-fill across standard GM tom notes `[50, 48, 47, 45, 43, 41]`
    let gm_toms: [u8; 6] = [50, 48, 47, 45, 43, 41];
    let present_toms: Vec<(u8, usize)> = gm_toms
        .iter()
        .copied()
        .filter_map(|n| note_to_pad[n as usize].map(|pi| (n, pi)))
        .filter(|&(_, pi)| {
            let up = pads[pi].id.to_ascii_uppercase();
            up.starts_with("TM")
                || up.starts_with("TT")
                || up.starts_with("FT")
                || up.starts_with("RT")
                || up.starts_with("TOM")
        })
        .collect();

    if !present_toms.is_empty() {
        for &tn in &gm_toms {
            if note_to_pad[tn as usize].is_none() {
                if let Some(&(_, nearest_pi)) = present_toms
                    .iter()
                    .min_by_key(|&&(pn, _)| (pn as i32 - tn as i32).abs())
                {
                    note_to_pad[tn as usize] = Some(nearest_pi);
                    if !pads[nearest_pi].midi_notes.contains(&tn) {
                        pads[nearest_pi].midi_notes.push(tn);
                    }
                }
            }
        }
    }
}

fn collect_obw_and_ttpw(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if depth > 4 {
        return;
    }
    let Ok(rd) = fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            collect_obw_and_ttpw(&p, depth + 1, out);
        } else if let Some(ext) = p.extension().and_then(|x| x.to_str()) {
            if ext.eq_ignore_ascii_case("obw") || ext.eq_ignore_ascii_case("ttpw") {
                out.push(p);
            }
        }
    }
}

fn load_legacy_obw_into_pads(
    map: &[u8],
    pads_by_id: &mut BTreeMap<String, LoadedPad>,
    pad_order: &mut Vec<String>,
    total_samples: &mut usize,
    detected_sr: &mut u32,
) -> Result<()> {
    let tree = sd3_core::riff::parse_riff_tree(map, 4)?;
    let mut index_entries: BTreeMap<usize, sd3_core::obw::ObwIndexEntry> = BTreeMap::new();
    for ch in &tree.children {
        if ch.id == "OBWI" {
            let s = ch.data_offset as usize;
            let e = s.saturating_add(ch.data_size as usize).min(map.len());
            if let Ok(parsed) =
                serde_json::from_slice::<Vec<sd3_core::obw::ObwIndexEntry>>(&map[s..e])
            {
                for item in parsed {
                    index_entries.insert(item.chunk_index, item);
                }
            }
        }
    }

    let mut leaves = Vec::new();
    collect_riff_leaves(&tree, &mut leaves);
    let mut ordinal = 0usize;
    for leaf in leaves {
        if matches!(leaf.id.as_str(), "OBWI" | "INFO" | "INAM" | "JUNK") {
            continue;
        }
        let s = leaf.data_offset as usize;
        let e = s.saturating_add(leaf.data_size as usize).min(map.len());
        if s >= e {
            continue;
        }
        let slice = &map[s..e];
        let decoded = if tpc::looks_like_tpc(slice) {
            tpc::decode_tpc_stream(slice, 44_100).ok()
        } else if slice.len() >= 44 && &slice[0..4] == b"RIFF" && &slice[8..12] == b"WAVE" {
            wav::read_wav_bytes(slice).ok()
        } else {
            None
        };
        let Some(buf) = decoded else {
            continue;
        };
        *detected_sr = buf.sample_rate;
        let (l, r) = audio_buffer_to_stereo(buf);
        let sample = AudioSample::from_in_memory(*detected_sr, l, r);
        *total_samples += 1;

        if let Some(meta) = index_entries.get(&ordinal) {
            if !pads_by_id.contains_key(&meta.pad_id) {
                pad_order.push(meta.pad_id.clone());
                pads_by_id.insert(
                    meta.pad_id.clone(),
                    LoadedPad {
                        id: meta.pad_id.clone(),
                        name: meta.pad_name.clone(),
                        primary_note: meta.midi_notes.first().copied().unwrap_or(36),
                        midi_notes: meta.midi_notes.clone(),
                        group: meta.group,
                        off_by: meta.off_by_this,
                        hihat_openness: None,
                        natural_vel_dynamics: false,
                        layers: Vec::new(),
                    },
                );
            }
            let pad = pads_by_id.get_mut(&meta.pad_id).unwrap();
            let layer = match pad
                .layers
                .iter_mut()
                .find(|ly| ly.lovel == meta.lovel && ly.hivel == meta.hivel)
            {
                Some(ly) => ly,
                None => {
                    pad.layers.push(LoadedLayer {
                        lovel: meta.lovel,
                        hivel: meta.hivel,
                        round_robins: Vec::new(),
                    });
                    pad.layers.last_mut().unwrap()
                }
            };
            while layer.round_robins.len() <= meta.rr_index {
                layer.round_robins.push(LoadedRoundRobin {
                    buses: [None, None, None, None],
                });
            }
            layer.round_robins[meta.rr_index].buses[meta.mic_bus.index()] = Some(sample);
        }
        ordinal += 1;
    }
    Ok(())
}

fn collect_riff_leaves<'a>(
    chunk: &'a sd3_core::riff::RiffChunk,
    out: &mut Vec<&'a sd3_core::riff::RiffChunk>,
) {
    if chunk.children.is_empty() {
        if chunk.form_type.is_none() {
            out.push(chunk);
        }
    } else {
        for c in &chunk.children {
            collect_riff_leaves(c, out);
        }
    }
}

fn audio_buffer_to_stereo(buf: AudioBuffer) -> (Vec<f32>, Vec<f32>) {
    match buf.channels.len() {
        0 => (Vec::new(), Vec::new()),
        1 => {
            let l = buf.channels.into_iter().next().unwrap();
            let r = l.clone();
            (l, r)
        }
        _ => {
            let mut it = buf.channels.into_iter();
            let l = it.next().unwrap();
            let r = it.next().unwrap();
            (l, r)
        }
    }
}

/// Lock-free atomic telemetry shared between the audio thread and the `egui` UI.
pub struct UiTelemetry {
    pub pad_velocities: [AtomicU8; MAX_UI_PADS],
    pub active_voices: AtomicUsize,
}

impl Default for UiTelemetry {
    fn default() -> Self {
        Self {
            pad_velocities: std::array::from_fn(|_| AtomicU8::new(0)),
            active_voices: AtomicUsize::new(0),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct MixerGains {
    pub close: f32,
    pub oh: f32,
    pub room: f32,
    pub bleed: f32,
    pub master: f32,
    /// `0.0..=1.0` — lifts soft hits (`0.0` = natural dynamics, `1.0` = max velocity).
    pub vel_floor: f32,
    /// `0.0..=1.0` — subtle round-robin / micro-gain humanization.
    pub humanize: f32,
}

impl Default for MixerGains {
    fn default() -> Self {
        Self {
            close: 1.0,
            oh: 0.75,
            room: 0.50,
            bleed: 0.25,
            master: 0.90,
            vel_floor: 0.0,
            humanize: 0.15,
        }
    }
}

/// Lock-free SPSC ring-buffer slot for streaming 4 stereo mic buses of one active voice.
struct StreamSlot {
    generation: AtomicU64,
    read_frame: AtomicUsize,
    write_frame: AtomicUsize,
    ring_l: [Box<[AtomicU32]>; 4],
    ring_r: [Box<[AtomicU32]>; 4],
}

impl StreamSlot {
    fn new() -> Self {
        let make_ring = || {
            (0..STREAM_RING_FRAMES)
                .map(|_| AtomicU32::new(0))
                .collect::<Vec<_>>()
                .into_boxed_slice()
        };
        Self {
            generation: AtomicU64::new(0),
            read_frame: AtomicUsize::new(0),
            write_frame: AtomicUsize::new(0),
            ring_l: [make_ring(), make_ring(), make_ring(), make_ring()],
            ring_r: [make_ring(), make_ring(), make_ring(), make_ring()],
        }
    }
}

struct StreamCmd {
    slot_idx: usize,
    generation: u64,
    kit: Arc<LoadedKit>,
    pad_idx: usize,
    layer_idx: usize,
    rr_idx: usize,
    start_frame: usize,
    max_frames: usize,
}

struct ActiveStreamJob {
    slot_idx: usize,
    generation: u64,
    kit: Arc<LoadedKit>,
    pad_idx: usize,
    layer_idx: usize,
    rr_idx: usize,
    write_frame: usize,
    max_frames: usize,
}

fn run_streamer_loop(pool: Arc<[StreamSlot]>, rx: Receiver<StreamCmd>) {
    let mut jobs: Vec<ActiveStreamJob> = Vec::with_capacity(MAX_VOICES);
    let mut scratch_l = [0.0f32; STREAM_CHUNK_FRAMES];
    let mut scratch_r = [0.0f32; STREAM_CHUNK_FRAMES];

    loop {
        if jobs.is_empty() {
            let Ok(cmd) = rx.recv() else {
                break;
            };
            push_or_replace_job(&mut jobs, cmd);
        } else {
            while let Ok(cmd) = rx.try_recv() {
                push_or_replace_job(&mut jobs, cmd);
            }
        }

        let mut made_progress = false;
        let mut idx = 0usize;
        while idx < jobs.len() {
            let remove = {
                let job = &mut jobs[idx];
                let slot = &pool[job.slot_idx];
                if slot.generation.load(Ordering::Acquire) != job.generation
                    || job.write_frame >= job.max_frames
                {
                    true
                } else {
                    let rf = slot.read_frame.load(Ordering::Acquire);
                    let buffered = job.write_frame.saturating_sub(rf);
                    if buffered + STREAM_CHUNK_FRAMES <= STREAM_RING_FRAMES {
                        let wf = job.write_frame;
                        let count = (job.max_frames - wf).min(STREAM_CHUNK_FRAMES);
                        let rr = &job.kit.pads[job.pad_idx].layers[job.layer_idx].round_robins
                            [job.rr_idx];
                        for b in 0..4 {
                            if let Some(ref sample) = rr.buses[b] {
                                if let Some(ref tail) = sample.ttpw_tail {
                                    let slice = &tail.mmap
                                        [tail.slice_offset..tail.slice_offset + tail.slice_len];
                                    if ttpw::decode_ttpw_range_stereo(
                                        slice,
                                        &tail.header,
                                        wf,
                                        count,
                                        &mut scratch_l[..count],
                                        &mut scratch_r[..count],
                                    )
                                    .is_ok()
                                    {
                                        for i in 0..count {
                                            let r_pos = (wf + i) & STREAM_RING_MASK;
                                            slot.ring_l[b][r_pos]
                                                .store(scratch_l[i].to_bits(), Ordering::Relaxed);
                                            slot.ring_r[b][r_pos]
                                                .store(scratch_r[i].to_bits(), Ordering::Relaxed);
                                        }
                                    }
                                }
                            }
                        }
                        job.write_frame = wf + count;
                        slot.write_frame.store(job.write_frame, Ordering::Release);
                        made_progress = true;
                        job.write_frame >= job.max_frames
                    } else {
                        false
                    }
                }
            };

            if remove {
                jobs.swap_remove(idx);
            } else {
                idx += 1;
            }
        }

        if !jobs.is_empty() && !made_progress {
            match rx.recv_timeout(Duration::from_millis(2)) {
                Ok(cmd) => push_or_replace_job(&mut jobs, cmd),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
    }
}

fn push_or_replace_job(jobs: &mut Vec<ActiveStreamJob>, cmd: StreamCmd) {
    jobs.retain(|j| j.slot_idx != cmd.slot_idx);
    jobs.push(ActiveStreamJob {
        slot_idx: cmd.slot_idx,
        generation: cmd.generation,
        kit: cmd.kit,
        pad_idx: cmd.pad_idx,
        layer_idx: cmd.layer_idx,
        rr_idx: cmd.rr_idx,
        write_frame: cmd.start_frame,
        max_frames: cmd.max_frames,
    });
}

struct OfflineVoiceCache {
    valid: bool,
    window_start: usize,
    buf_l: [[f32; OFFLINE_CACHE_FRAMES]; 4],
    buf_r: [[f32; OFFLINE_CACHE_FRAMES]; 4],
}

impl OfflineVoiceCache {
    fn new() -> Self {
        Self {
            valid: false,
            window_start: 0,
            buf_l: [[0.0; OFFLINE_CACHE_FRAMES]; 4],
            buf_r: [[0.0; OFFLINE_CACHE_FRAMES]; 4],
        }
    }
}

#[derive(Clone, Copy)]
struct Voice {
    active: bool,
    pad_idx: usize,
    layer_idx: usize,
    rr_idx: usize,
    triggered_note: u8,
    pos: f64,
    step: f64,
    vel_gain: f32,
    env: f32,
    choking: bool,
    choke_step: f32,
    off_by: Option<u32>,
    age: u64,
    uses_stream: bool,
    min_head_frames: usize,
}

impl Default for Voice {
    fn default() -> Self {
        Self {
            active: false,
            pad_idx: 0,
            layer_idx: 0,
            rr_idx: 0,
            triggered_note: 0,
            pos: 0.0,
            step: 1.0,
            vel_gain: 1.0,
            env: 1.0,
            choking: false,
            choke_step: 0.002,
            off_by: None,
            age: 0,
            uses_stream: false,
            min_head_frames: 0,
        }
    }
}

pub struct DrumEngine {
    kit: Arc<LoadedKit>,
    voices: [Voice; MAX_VOICES],
    rr_counters: [usize; 128],
    rng_state: u32,
    age_counter: u64,
    host_sample_rate: f64,

    // MIDI CC4 variable hi-hat state (`0` = pedal up / open .. `127` = tight closed)
    cc4_active: bool,
    cc4_pedal: u8,

    // Real-time background disk/ttpw streaming pool (None in deterministic offline render mode)
    stream_pool: Option<Arc<[StreamSlot]>>,
    stream_tx: Option<Sender<StreamCmd>>,
    _streamer_handle: Option<JoinHandle<()>>,

    // Deterministic synchronous block cache for faster-than-realtime CLI rendering
    offline_caches: Vec<OfflineVoiceCache>,
}

impl DrumEngine {
    /// Construct a real-time `DrumEngine` with its dedicated background `open-sd3-streamer`
    /// thread and pre-allocated lock-free SPSC ring buffers.
    pub fn new(initial_kit: Arc<LoadedKit>, host_sample_rate: f64) -> Self {
        let pool: Arc<[StreamSlot]> = (0..MAX_VOICES)
            .map(|_| StreamSlot::new())
            .collect::<Vec<_>>()
            .into();
        let (tx, rx) = bounded::<StreamCmd>(MAX_VOICES * 4);
        let pool_for_thread = Arc::clone(&pool);
        let handle = thread::Builder::new()
            .name("open-sd3-streamer".into())
            .spawn(move || run_streamer_loop(pool_for_thread, rx))
            .ok();

        Self {
            kit: initial_kit,
            voices: [Voice::default(); MAX_VOICES],
            rr_counters: [0; 128],
            rng_state: 0xA53C_9E17,
            age_counter: 0,
            host_sample_rate: host_sample_rate.max(8_000.0),
            cc4_active: false,
            cc4_pedal: 0,
            stream_pool: Some(pool),
            stream_tx: Some(tx),
            _streamer_handle: handle,
            offline_caches: Vec::new(),
        }
    }

    /// Construct a deterministic offline `DrumEngine` (for `sd3tool render-midi` and tests)
    /// that decodes `ttpw` blocks synchronously with zero background threads or underruns.
    pub fn new_offline(initial_kit: Arc<LoadedKit>, host_sample_rate: f64) -> Self {
        let offline_caches = (0..MAX_VOICES).map(|_| OfflineVoiceCache::new()).collect();
        Self {
            kit: initial_kit,
            voices: [Voice::default(); MAX_VOICES],
            rr_counters: [0; 128],
            rng_state: 0xA53C_9E17,
            age_counter: 0,
            host_sample_rate: host_sample_rate.max(8_000.0),
            cc4_active: false,
            cc4_pedal: 0,
            stream_pool: None,
            stream_tx: None,
            _streamer_handle: None,
            offline_caches,
        }
    }

    pub fn set_sample_rate(&mut self, sr: f64) {
        self.host_sample_rate = sr.max(8_000.0);
    }

    pub fn kit(&self) -> &Arc<LoadedKit> {
        &self.kit
    }

    /// Swap the active kit on the audio thread and return the old `Arc<LoadedKit>` so the
    /// caller can drop it on a non-realtime garbage-collector thread.
    pub fn swap_kit(&mut self, new_kit: Arc<LoadedKit>) -> Arc<LoadedKit> {
        self.reset();
        std::mem::replace(&mut self.kit, new_kit)
    }

    pub fn reset(&mut self) {
        for (i, v) in self.voices.iter_mut().enumerate() {
            v.active = false;
            if let Some(ref pool) = self.stream_pool {
                pool[i].generation.fetch_add(1, Ordering::Release);
            }
        }
        self.cc4_active = false;
        self.cc4_pedal = 0;
    }

    /// Handle MIDI Control Change (CC4 = Hi-Hat Foot Controller).
    pub fn control_change(&mut self, cc: u8, value: u8) {
        if cc == 4 {
            let v = value.min(127);
            self.cc4_active = true;
            self.cc4_pedal = v;
            if v >= 115 {
                let choke_samples = (0.008 * self.host_sample_rate).max(32.0) as f32;
                let step = 1.0 / choke_samples;
                for voice in &mut self.voices {
                    if voice.active {
                        if let Some(pad) = self.kit.pads.get(voice.pad_idx) {
                            if pad.hihat_openness.unwrap_or(0) > 20 {
                                voice.choking = true;
                                voice.choke_step = step;
                            }
                        }
                    }
                }
            }
        }
    }

    /// Handle MIDI Polyphonic Key Pressure / Aftertouch (cymbal hand-grab choke).
    pub fn poly_pressure(&mut self, note: u8, pressure: u8) {
        if pressure < 64 {
            return;
        }
        let choke_samples = (0.015 * self.host_sample_rate).max(48.0) as f32;
        let step = 1.0 / choke_samples;
        let target_off_by = self
            .kit
            .note_to_pad
            .get(note as usize)
            .and_then(|&o| o)
            .and_then(|pi| self.kit.pads.get(pi))
            .and_then(|p| p.off_by);

        for voice in &mut self.voices {
            if voice.active
                && (voice.triggered_note == note
                    || (target_off_by.is_some() && voice.off_by == target_off_by))
            {
                voice.choking = true;
                voice.choke_step = step;
            }
        }
    }

    #[inline]
    fn next_rand_01(&mut self) -> f32 {
        let mut x = self.rng_state;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.rng_state = x.max(1);
        (x as f32) / (u32::MAX as f32)
    }

    pub fn note_on(
        &mut self,
        note: u8,
        velocity_01: f32,
        mixer: &MixerGains,
        telemetry: Option<&UiTelemetry>,
    ) {
        if note >= 128 || velocity_01 <= 0.0 {
            return;
        }
        let Some(mut pad_idx) = self.kit.note_to_pad[note as usize] else {
            return;
        };

        if self.cc4_active
            && note != 44
            && note != 21
            && self.kit.pads[pad_idx].hihat_openness.is_some()
        {
            let target_open = 127i32 - (self.cc4_pedal as i32);
            let mut best = (pad_idx, i32::MAX);
            for (i, p) in self.kit.pads.iter().enumerate() {
                if let Some(op) = p.hihat_openness {
                    if p.primary_note != 44 && p.primary_note != 21 {
                        let d = (op as i32 - target_open).abs();
                        if d < best.1 {
                            best = (i, d);
                        }
                    }
                }
            }
            pad_idx = best.0;
        }

        let r_rr = self.next_rand_01();
        let r_det = self.next_rand_01();
        let r_jit = self.next_rand_01();

        let (
            layer_idx,
            rr_idx,
            sample_sr,
            uses_stream,
            min_head,
            max_frames,
            natural_vel_dynamics,
            layer_lovel,
            layer_hivel,
            off_by,
        ) = {
            let pad = &self.kit.pads[pad_idx];
            if pad.layers.is_empty() {
                return;
            }
            let lifted_vel =
                mixer.vel_floor + (1.0 - mixer.vel_floor) * velocity_01.clamp(0.0, 1.0);
            let vel_u8 = ((lifted_vel * 127.0).round() as i32).clamp(1, 127) as u8;

            if let Some(tel) = telemetry {
                if pad_idx < MAX_UI_PADS {
                    tel.pad_velocities[pad_idx].store(vel_u8, Ordering::Relaxed);
                }
            }

            if let Some(trig_group) = pad.group {
                let choke_samples = (0.008 * self.host_sample_rate).max(24.0) as f32;
                let step = 1.0 / choke_samples;
                for v in &mut self.voices {
                    if v.active && v.off_by == Some(trig_group) {
                        v.choking = true;
                        v.choke_step = step;
                    }
                }
            }

            let layer_idx = pad
                .layers
                .iter()
                .position(|l| vel_u8 >= l.lovel && vel_u8 <= l.hivel)
                .unwrap_or(pad.layers.len() - 1);
            let layer = &pad.layers[layer_idx];
            if layer.round_robins.is_empty() {
                return;
            }

            let rr_count = layer.round_robins.len();
            let rr_slot_key = pad_idx & 127;
            let rr_idx = if rr_count == 1 {
                0
            } else {
                let prev = self.rr_counters[rr_slot_key] % rr_count;
                let step_inc = if mixer.humanize > 0.35 && rr_count > 2 {
                    1 + ((r_rr * (rr_count as f32 - 1.0)) as usize)
                } else {
                    1
                };
                let next = (prev + step_inc) % rr_count;
                self.rr_counters[rr_slot_key] = next;
                next
            };

            let rr = &layer.round_robins[rr_idx];
            let mut sample_sr = self.kit.sample_rate as f64;
            let mut uses_stream = false;
            let mut min_head = usize::MAX;
            let mut max_frames = 0usize;

            for s in rr.buses.iter().flatten() {
                sample_sr = s.sample_rate as f64;
                max_frames = max_frames.max(s.total_frames);
                if s.has_stream_tail() {
                    uses_stream = true;
                    min_head = min_head.min(s.head_l.len());
                }
            }
            if min_head == usize::MAX {
                min_head = 0;
            }

            (
                layer_idx,
                rr_idx,
                sample_sr,
                uses_stream,
                min_head,
                max_frames,
                pad.natural_vel_dynamics,
                layer.lovel,
                layer.hivel,
                pad.off_by,
            )
        };

        let lifted_vel = mixer.vel_floor + (1.0 - mixer.vel_floor) * velocity_01.clamp(0.0, 1.0);
        let vel_u8 = ((lifted_vel * 127.0).round() as i32).clamp(1, 127) as u8;
        let rand_detune = (r_det * 2.0 - 1.0) * 0.004 * mixer.humanize;
        let step = (sample_sr / self.host_sample_rate) * (1.0 + rand_detune as f64);

        let jitter_gain = 1.0 + (r_jit * 2.0 - 1.0) * 0.05 * mixer.humanize;
        let base_vel_gain = if natural_vel_dynamics {
            let span = (layer_hivel.saturating_sub(layer_lovel) as f32).max(1.0);
            let frac = ((vel_u8.saturating_sub(layer_lovel)) as f32 / span).clamp(0.0, 1.0);
            0.78 + 0.22 * frac
        } else {
            lifted_vel.powf(0.75)
        };
        let vel_gain = (base_vel_gain * jitter_gain).clamp(0.02, 1.5);

        let slot = self.find_voice_slot();
        self.age_counter = self.age_counter.wrapping_add(1);

        if let Some(oc) = self.offline_caches.get_mut(slot) {
            oc.valid = false;
        }

        if uses_stream {
            if let (Some(ref pool), Some(ref tx)) = (&self.stream_pool, &self.stream_tx) {
                let s_slot = &pool[slot];
                let gen = s_slot.generation.fetch_add(1, Ordering::AcqRel).wrapping_add(1);
                s_slot.read_frame.store(min_head, Ordering::Release);
                s_slot.write_frame.store(min_head, Ordering::Release);
                let _ = tx.try_send(StreamCmd {
                    slot_idx: slot,
                    generation: gen,
                    kit: Arc::clone(&self.kit),
                    pad_idx,
                    layer_idx,
                    rr_idx,
                    start_frame: min_head,
                    max_frames,
                });
            }
        } else if let Some(ref pool) = self.stream_pool {
            pool[slot].generation.fetch_add(1, Ordering::Release);
        }

        self.voices[slot] = Voice {
            active: true,
            pad_idx,
            layer_idx,
            rr_idx,
            triggered_note: note,
            pos: 0.0,
            step,
            vel_gain,
            env: 1.0,
            choking: false,
            choke_step: 0.002,
            off_by,
            age: self.age_counter,
            uses_stream,
            min_head_frames: min_head,
        };
    }

    fn find_voice_slot(&self) -> usize {
        if let Some(idx) = self.voices.iter().position(|v| !v.active) {
            return idx;
        }
        if let Some((idx, _)) = self
            .voices
            .iter()
            .enumerate()
            .filter(|(_, v)| v.choking)
            .min_by_key(|(_, v)| v.age)
        {
            return idx;
        }
        self.voices
            .iter()
            .enumerate()
            .min_by_key(|(_, v)| v.age)
            .map(|(i, _)| i)
            .unwrap_or(0)
    }

    /// Mix `out_l.len()` stereo samples into `out_l` and `out_r` (overwriting them).
    /// Returns peak absolute sample value for UI metering.
    pub fn process_block(
        &mut self,
        out_l: &mut [f32],
        out_r: &mut [f32],
        mixer: &MixerGains,
        telemetry: Option<&UiTelemetry>,
    ) -> f32 {
        let frames = out_l.len().min(out_r.len());
        out_l[..frames].fill(0.0);
        out_r[..frames].fill(0.0);

        let bus_gains: [f32; 4] = [mixer.close, mixer.oh, mixer.room, mixer.bleed];
        let mut active_count = 0usize;

        for v_idx in 0..MAX_VOICES {
            if !self.voices[v_idx].active {
                continue;
            }
            let voice_pad = self.voices[v_idx].pad_idx;
            let voice_layer = self.voices[v_idx].layer_idx;
            let voice_rr = self.voices[v_idx].rr_idx;

            let Some(pad) = self.kit.pads.get(voice_pad) else {
                self.voices[v_idx].active = false;
                continue;
            };
            let Some(layer) = pad.layers.get(voice_layer) else {
                self.voices[v_idx].active = false;
                continue;
            };
            let Some(rr) = layer.round_robins.get(voice_rr) else {
                self.voices[v_idx].active = false;
                continue;
            };

            let max_len = rr
                .buses
                .iter()
                .filter_map(|b| b.as_ref().map(|s| s.len()))
                .max()
                .unwrap_or(0);
            if max_len < 2 {
                self.voices[v_idx].active = false;
                continue;
            }

            let stream_slot = self.stream_pool.as_ref().map(|p| &p[v_idx]);
            let mut offline_cache = self.offline_caches.get_mut(v_idx);

            for i in 0..frames {
                let idx0 = self.voices[v_idx].pos as usize;
                if idx0 + 1 >= max_len {
                    self.voices[v_idx].active = false;
                    if let Some(s_slot) = stream_slot {
                        s_slot.generation.fetch_add(1, Ordering::Release);
                    }
                    break;
                }
                let frac = (self.voices[v_idx].pos - idx0 as f64) as f32;

                if self.voices[v_idx].choking {
                    self.voices[v_idx].env -= self.voices[v_idx].choke_step;
                    if self.voices[v_idx].env <= 0.0 {
                        self.voices[v_idx].env = 0.0;
                        self.voices[v_idx].active = false;
                        if let Some(s_slot) = stream_slot {
                            s_slot.generation.fetch_add(1, Ordering::Release);
                        }
                        break;
                    }
                }

                if self.voices[v_idx].uses_stream {
                    if let Some(ref mut oc) = offline_cache {
                        ensure_offline_window(oc, rr, idx0);
                    }
                }

                let mut sum_l = 0.0f32;
                let mut sum_r = 0.0f32;

                for b in 0..4 {
                    let g = bus_gains[b];
                    if g <= 0.000_01 {
                        continue;
                    }
                    if let Some(ref s) = rr.buses[b] {
                        if idx0 + 1 < s.total_frames {
                            let (l0, r0) = fetch_stereo_frame(
                                s,
                                b,
                                idx0,
                                stream_slot,
                                offline_cache.as_deref(),
                            );
                            let (l1, r1) = fetch_stereo_frame(
                                s,
                                b,
                                idx0 + 1,
                                stream_slot,
                                offline_cache.as_deref(),
                            );
                            let l = l0 + (l1 - l0) * frac;
                            let r = r0 + (r1 - r0) * frac;
                            sum_l += l * g;
                            sum_r += r * g;
                        }
                    }
                }

                let v_scale = self.voices[v_idx].vel_gain * self.voices[v_idx].env;
                out_l[i] += sum_l * v_scale;
                out_r[i] += sum_r * v_scale;
                self.voices[v_idx].pos += self.voices[v_idx].step;
            }

            if self.voices[v_idx].active {
                if self.voices[v_idx].uses_stream {
                    if let Some(s_slot) = stream_slot {
                        let consumed =
                            (self.voices[v_idx].pos as usize).max(self.voices[v_idx].min_head_frames);
                        s_slot.read_frame.store(consumed, Ordering::Release);
                    }
                }
                active_count += 1;
            }
        }

        if let Some(tel) = telemetry {
            tel.active_voices.store(active_count, Ordering::Relaxed);
        }

        let mut peak = 0.0f32;
        let master = mixer.master;
        for i in 0..frames {
            let l = soft_clip(out_l[i] * master);
            let r = soft_clip(out_r[i] * master);
            out_l[i] = l;
            out_r[i] = r;
            peak = peak.max(l.abs()).max(r.abs());
        }
        peak
    }
}

#[inline]
fn fetch_stereo_frame(
    sample: &AudioSample,
    bus_idx: usize,
    frame_idx: usize,
    stream_slot: Option<&StreamSlot>,
    offline_cache: Option<&OfflineVoiceCache>,
) -> (f32, f32) {
    let head_len = sample.head_l.len();
    if frame_idx < head_len {
        return (sample.head_l[frame_idx], sample.head_r[frame_idx]);
    }
    if let Some(oc) = offline_cache {
        if oc.valid
            && frame_idx >= oc.window_start
            && frame_idx < oc.window_start + OFFLINE_CACHE_FRAMES
        {
            let off = frame_idx - oc.window_start;
            return (oc.buf_l[bus_idx][off], oc.buf_r[bus_idx][off]);
        }
    }
    if let Some(slot) = stream_slot {
        let wf = slot.write_frame.load(Ordering::Acquire);
        if frame_idx < wf {
            let r_pos = frame_idx & STREAM_RING_MASK;
            let l = f32::from_bits(slot.ring_l[bus_idx][r_pos].load(Ordering::Relaxed));
            let r = f32::from_bits(slot.ring_r[bus_idx][r_pos].load(Ordering::Relaxed));
            return (l, r);
        }
    }
    (0.0, 0.0)
}

fn ensure_offline_window(oc: &mut OfflineVoiceCache, rr: &LoadedRoundRobin, idx0: usize) {
    let need_second = idx0 + 1;
    if oc.valid
        && idx0 >= oc.window_start
        && need_second < oc.window_start + OFFLINE_CACHE_FRAMES
    {
        return;
    }
    let start = idx0;
    for b in 0..4 {
        oc.buf_l[b].fill(0.0);
        oc.buf_r[b].fill(0.0);
        if let Some(ref s) = rr.buses[b] {
            if start >= s.head_l.len() {
                if let Some(ref tail) = s.ttpw_tail {
                    let slice = &tail.mmap[tail.slice_offset..tail.slice_offset + tail.slice_len];
                    let _ = ttpw::decode_ttpw_range_stereo(
                        slice,
                        &tail.header,
                        start,
                        OFFLINE_CACHE_FRAMES,
                        &mut oc.buf_l[b],
                        &mut oc.buf_r[b],
                    );
                }
            } else {
                let from_head = (s.head_l.len() - start).min(OFFLINE_CACHE_FRAMES);
                oc.buf_l[b][..from_head].copy_from_slice(&s.head_l[start..start + from_head]);
                oc.buf_r[b][..from_head].copy_from_slice(&s.head_r[start..start + from_head]);
                if from_head < OFFLINE_CACHE_FRAMES {
                    if let Some(ref tail) = s.ttpw_tail {
                        let slice =
                            &tail.mmap[tail.slice_offset..tail.slice_offset + tail.slice_len];
                        let _ = ttpw::decode_ttpw_range_stereo(
                            slice,
                            &tail.header,
                            start + from_head,
                            OFFLINE_CACHE_FRAMES - from_head,
                            &mut oc.buf_l[b][from_head..],
                            &mut oc.buf_r[b][from_head..],
                        );
                    }
                }
            }
        }
    }
    oc.window_start = start;
    oc.valid = true;
}

#[inline]
fn soft_clip(x: f32) -> f32 {
    if x.abs() <= 0.95 {
        x
    } else {
        x.signum() * (0.95 + 0.05 * ((x.abs() - 0.95) * 10.0).tanh())
    }
}

#[derive(Clone, Copy)]
enum SmfEvent {
    NoteOn { sample_pos: usize, note: u8, vel: u8 },
    Cc { sample_pos: usize, cc: u8, val: u8 },
    PolyPressure { sample_pos: usize, note: u8, pressure: u8 },
}

impl SmfEvent {
    fn sample_pos(&self) -> usize {
        match *self {
            Self::NoteOn { sample_pos, .. }
            | Self::Cc { sample_pos, .. }
            | Self::PolyPressure { sample_pos, .. } => sample_pos,
        }
    }
}

/// Offline Standard MIDI File renderer (for `sd3tool render-midi` and automated tests).
pub fn render_midi_bytes(
    midi_bytes: &[u8],
    kit: Arc<LoadedKit>,
    sample_rate: u32,
    mixer: MixerGains,
    tail_seconds: f32,
) -> Result<AudioBuffer> {
    let events = parse_smf_events(midi_bytes, sample_rate)?;
    let last_sample = events.last().map(|e| e.sample_pos()).unwrap_or(0);
    let total_frames = last_sample + ((tail_seconds.max(0.2) * sample_rate as f32) as usize);

    let mut engine = DrumEngine::new_offline(kit, sample_rate as f64);
    let mut left = vec![0.0f32; total_frames];
    let mut right = vec![0.0f32; total_frames];

    let mut ev_idx = 0usize;
    let mut cursor = 0usize;
    let block = 128usize;

    while cursor < total_frames {
        let next_ev_frame = events
            .get(ev_idx)
            .map(|e| e.sample_pos().min(total_frames))
            .unwrap_or(total_frames);
        if next_ev_frame > cursor {
            let chunk_end = (cursor + block).min(next_ev_frame);
            engine.process_block(
                &mut left[cursor..chunk_end],
                &mut right[cursor..chunk_end],
                &mixer,
                None,
            );
            cursor = chunk_end;
        } else {
            while ev_idx < events.len() && events[ev_idx].sample_pos() <= cursor {
                match events[ev_idx] {
                    SmfEvent::NoteOn { note, vel, .. } => {
                        engine.note_on(note, (vel as f32) / 127.0, &mixer, None);
                    }
                    SmfEvent::Cc { cc, val, .. } => {
                        engine.control_change(cc, val);
                    }
                    SmfEvent::PolyPressure { note, pressure, .. } => {
                        engine.poly_pressure(note, pressure);
                    }
                }
                ev_idx += 1;
            }
        }
    }

    Ok(AudioBuffer {
        sample_rate,
        channels: vec![left, right],
    })
}

fn parse_smf_events(data: &[u8], sample_rate: u32) -> Result<Vec<SmfEvent>> {
    if data.len() < 14 || &data[0..4] != b"MThd" {
        return Err(Sd3Error::Format("invalid MIDI header (missing MThd)".into()));
    }
    let hdr_len = u32::from_be_bytes(data[4..8].try_into().unwrap()) as usize;
    let ntrks = u16::from_be_bytes([data[10], data[11]]) as usize;
    let division = u16::from_be_bytes([data[12], data[13]]);
    if division & 0x8000 != 0 || division == 0 {
        return Err(Sd3Error::Format("SMPTE MIDI division unsupported".into()));
    }
    let ppq = division as f64;

    #[derive(Clone, Copy)]
    enum RawEv {
        Tempo(u32),
        NoteOn(u8, u8),
        Cc(u8, u8),
        PolyPressure(u8, u8),
    }
    let mut raw: Vec<(u64, RawEv)> = Vec::new();

    let mut pos = 8 + hdr_len;
    for _ in 0..ntrks {
        if pos + 8 > data.len() {
            break;
        }
        let id = &data[pos..pos + 4];
        let tlen = u32::from_be_bytes(data[pos + 4..pos + 8].try_into().unwrap()) as usize;
        pos += 8;
        let tend = (pos + tlen).min(data.len());
        if id != b"MTrk" {
            pos = tend;
            continue;
        }

        let mut p = pos;
        let mut tick = 0u64;
        let mut running = 0u8;
        while p < tend {
            let (dt, np) = read_vlq(data, p, tend);
            p = np;
            tick += dt as u64;
            if p >= tend {
                break;
            }
            let b = data[p];
            if b == 0xFF {
                p += 1;
                if p >= tend {
                    break;
                }
                let meta = data[p];
                p += 1;
                let (mlen, mp) = read_vlq(data, p, tend);
                p = mp;
                if meta == 0x51 && mlen == 3 && p + 3 <= tend {
                    let us = ((data[p] as u32) << 16)
                        | ((data[p + 1] as u32) << 8)
                        | (data[p + 2] as u32);
                    raw.push((tick, RawEv::Tempo(us.max(1))));
                }
                p = (p + mlen as usize).min(tend);
            } else if b == 0xF0 || b == 0xF7 {
                p += 1;
                let (slen, sp) = read_vlq(data, p, tend);
                p = (sp + slen as usize).min(tend);
            } else {
                let status = if b & 0x80 != 0 {
                    p += 1;
                    running = b;
                    b
                } else {
                    running
                };
                let hi = status & 0xF0;
                let dlen = if matches!(hi, 0xC0 | 0xD0) { 1 } else { 2 };
                if p + dlen > tend {
                    break;
                }
                match hi {
                    0x90 if data[p + 1] > 0 => {
                        raw.push((tick, RawEv::NoteOn(data[p], data[p + 1])));
                    }
                    0xB0 => {
                        raw.push((tick, RawEv::Cc(data[p], data[p + 1])));
                    }
                    0xA0 => {
                        raw.push((tick, RawEv::PolyPressure(data[p], data[p + 1])));
                    }
                    _ => {}
                }
                p += dlen;
            }
        }
        pos = tend;
    }

    raw.sort_by_key(|(t, ev)| (*t, matches!(ev, RawEv::NoteOn(_, _)) as u8));

    let mut cur_tick = 0u64;
    let mut cur_sec = 0.0f64;
    let mut us_per_qn = 500_000.0f64;
    let mut out = Vec::new();

    for (t, ev) in raw {
        let dt = (t - cur_tick) as f64;
        cur_sec += (dt / ppq) * (us_per_qn / 1_000_000.0);
        cur_tick = t;
        let sample_pos = (cur_sec * sample_rate as f64).round() as usize;
        match ev {
            RawEv::Tempo(us) => us_per_qn = us as f64,
            RawEv::NoteOn(note, vel) => out.push(SmfEvent::NoteOn {
                sample_pos,
                note,
                vel,
            }),
            RawEv::Cc(cc, val) => out.push(SmfEvent::Cc {
                sample_pos,
                cc,
                val,
            }),
            RawEv::PolyPressure(note, pressure) => out.push(SmfEvent::PolyPressure {
                sample_pos,
                note,
                pressure,
            }),
        }
    }
    Ok(out)
}

fn read_vlq(data: &[u8], mut p: usize, end: usize) -> (u32, usize) {
    let mut val = 0u32;
    for _ in 0..4 {
        if p >= end {
            break;
        }
        let b = data[p];
        p += 1;
        val = (val << 7) | ((b & 0x7F) as u32);
        if b & 0x80 == 0 {
            break;
        }
    }
    (val, p)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::TAU;
    use std::fs;
    use sd3_core::library::{self, KitSelection, RoomCatalog};
    use sd3_core::synthkit;
    use sd3_core::ttbw::{self, TtbwGroupBuildSpec};
    use sd3_core::ttpw;
    use sd3_core::wav::AudioBuffer;

    fn make_ttpw_hit(frames: usize, amp: f32, freq: f32) -> Vec<u8> {
        let l: Vec<f32> = (0..frames)
            .map(|i| {
                let t = i as f32 / 44_100.0;
                amp * (-t * 14.0).exp() * (TAU * freq * t).sin()
            })
            .collect();
        let r: Vec<f32> = (0..frames)
            .map(|i| {
                let t = i as f32 / 44_100.0;
                amp * 0.92 * (-t * 14.0).exp() * (TAU * (freq * 1.01) * t).cos()
            })
            .collect();
        ttpw::encode_ttpw_bytes(
            &AudioBuffer {
                sample_rate: 44_100,
                channels: vec![l, r],
            },
            20,
            256,
        )
        .unwrap()
    }

    #[test]
    fn builtin_demo_renders_rock_groove_cleanly() {
        let midi = synthkit::build_demo_rock_groove_midi();
        let kit = LoadedKit::built_in_demo(44_100);
        let out = render_midi_bytes(&midi, kit, 44_100, MixerGains::default(), 0.5).unwrap();
        assert_eq!(out.num_channels(), 2);
        assert!(out.num_frames() > 44_100);
        let peak = out.channels[0]
            .iter()
            .chain(out.channels[1].iter())
            .fold(0.0f32, |m, &x| {
                assert!(x.is_finite());
                m.max(x.abs())
            });
        assert!(peak > 0.05 && peak <= 1.0, "unexpected peak={peak}");
    }

    #[test]
    fn multi_obw_sl_folder_discovery_piece_switching_mic_ranking_and_streaming() {
        let root = std::env::temp_dir().join(format!("opensd3_v030_bank_{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);

        // 1. Build a realistic Toontrack folder tree with:
        //    - SL-SuperiorDrummer3 (multi-part 1..155 numeric .obw layout + GraphicsV3)
        //    - SL-DualRooms/SL/ROOM1 and SL/ROOM2 (separate rooms inside one SDX!)
        //    - EZDrummer/EZX3_Modern/Sounds/soundb0.obw
        let sd3_dir = root.join("Toontrack").join("SL-SuperiorDrummer3");
        let sd3_sounds = sd3_dir.join("Sounds");
        fs::create_dir_all(&sd3_sounds).unwrap();
        fs::create_dir_all(sd3_dir.join("GraphicsV3/KitLayout/Kick")).unwrap();
        fs::create_dir_all(sd3_dir.join("GraphicsV3/KitLayout/Snare")).unwrap();
        fs::write(sd3_dir.join("Name"), "Superior Drummer 3\n").unwrap();
        fs::write(sd3_dir.join("Aversion"), "1.2.1\n").unwrap();
        fs::write(sd3_sounds.join("soundstats.dat"), b"SDST").unwrap();
        fs::write(
            sd3_dir.join("GraphicsV3/KitLayout/Kick/Gretsch_22.png"),
            b"PNG",
        )
        .unwrap();

        // Long 6000-frame samples exercise both RAM head preload AND lock-free mmap tail streaming!
        let s_kd1 = make_ttpw_hit(6_000, 0.75, 62.0);
        let s_kd2 = make_ttpw_hit(6_000, 0.72, 54.0);
        let s_sn1_r = make_ttpw_hit(6_000, 0.68, 195.0);
        let s_sn1_l = make_ttpw_hit(6_000, 0.65, 202.0);
        let s_sn2_r = make_ttpw_hit(6_000, 0.70, 225.0);
        let s_sn1_br = make_ttpw_hit(6_000, 0.45, 310.0);
        let s_hh_cl = make_ttpw_hit(3_200, 0.42, 920.0);
        let s_hh_op = make_ttpw_hit(6_000, 0.55, 860.0);
        let s_tm1 = make_ttpw_hit(5_000, 0.64, 145.0);
        let s_tm2 = make_ttpw_hit(5_000, 0.64, 118.0);
        let s_tm3 = make_ttpw_hit(5_000, 0.64, 92.0);
        let s_cr = make_ttpw_hit(6_500, 0.62, 540.0);
        let s_cr_mt = make_ttpw_hit(1_500, 0.48, 530.0);
        let s_rd = make_ttpw_hit(6_500, 0.52, 680.0);
        let s_surr = make_ttpw_hit(4_000, 0.90, 999.0);
        let s_bleed = make_ttpw_hit(4_000, 0.20, 180.0);

        // soundb1.obw: Kicks (`KD01`, `KD02`) with `KickIn` + `OH_Cond`
        fs::write(
            sd3_sounds.join("soundb1.obw"),
            ttbw::build_ttbw_bytes(
                &["KickIn", "OH_Cond"],
                &[
                    TtbwGroupBuildSpec {
                        group_name: "KD01_DS_HIT_R",
                        samples: vec![(0, 'M', 1, &s_kd1), (1, 'M', 1, &s_kd1)],
                    },
                    TtbwGroupBuildSpec {
                        group_name: "KD02_DS_HIT_R",
                        samples: vec![(0, 'M', 1, &s_kd2), (1, 'M', 1, &s_kd2)],
                    },
                ],
                0xC8,
            ),
        )
        .unwrap();

        // soundb2.obw: Snares (`SN01` R+L + Brushes `BR` + `SN02`) with `SnareTop`, `OH_Cond`, `Amb_Near`
        fs::write(
            sd3_sounds.join("soundb2.obw"),
            ttbw::build_ttbw_bytes(
                &["SnareTop", "OH_Cond", "Amb_Near"],
                &[
                    TtbwGroupBuildSpec {
                        group_name: "SN01_DS_HIT_R",
                        samples: vec![
                            (0, 'M', 1, &s_sn1_r),
                            (1, 'M', 1, &s_sn1_r),
                            (2, 'M', 1, &s_sn1_r),
                        ],
                    },
                    TtbwGroupBuildSpec {
                        group_name: "SN01_DS_HIT_L",
                        samples: vec![
                            (0, 'M', 1, &s_sn1_l),
                            (1, 'M', 1, &s_sn1_l),
                            (2, 'M', 1, &s_sn1_l),
                        ],
                    },
                    TtbwGroupBuildSpec {
                        group_name: "SN01_BR_HIT_R",
                        samples: vec![(0, 'M', 1, &s_sn1_br), (1, 'M', 1, &s_sn1_br)],
                    },
                    TtbwGroupBuildSpec {
                        group_name: "SN02_DS_HIT_R",
                        samples: vec![(0, 'M', 1, &s_sn2_r), (1, 'M', 1, &s_sn2_r)],
                    },
                ],
                0xC8,
            ),
        )
        .unwrap();

        // soundb12.obw: Hats, Toms, Ride, Crash + secondary OH_Dyn / Amb_Far
        // Note: `soundb12.obw` would sort BEFORE `soundb2.obw` lexicographically!
        fs::write(
            sd3_sounds.join("soundb12.obw"),
            ttbw::build_ttbw_bytes(
                &["OH_Dyn", "Amb_Far"],
                &[
                    TtbwGroupBuildSpec {
                        group_name: "SN01_DS_HIT_R",
                        samples: vec![(0, 'M', 1, &s_surr), (1, 'M', 1, &s_surr)],
                    },
                    TtbwGroupBuildSpec {
                        group_name: "HH01_DS_CL_R",
                        samples: vec![(0, 'M', 1, &s_hh_cl), (1, 'M', 1, &s_hh_cl)],
                    },
                    TtbwGroupBuildSpec {
                        group_name: "HH01_DS_OP3_R",
                        samples: vec![(0, 'M', 1, &s_hh_op), (1, 'M', 1, &s_hh_op)],
                    },
                    TtbwGroupBuildSpec {
                        group_name: "TM01_01_DS_HIT_R",
                        samples: vec![(0, 'M', 1, &s_tm1)],
                    },
                    TtbwGroupBuildSpec {
                        group_name: "TM01_02_DS_HIT_R",
                        samples: vec![(0, 'M', 1, &s_tm2)],
                    },
                    TtbwGroupBuildSpec {
                        group_name: "TM01_03_DS_HIT_R",
                        samples: vec![(0, 'M', 1, &s_tm3)],
                    },
                    TtbwGroupBuildSpec {
                        group_name: "CR01_02_DS_CR_R",
                        samples: vec![(0, 'M', 1, &s_cr), (1, 'M', 1, &s_cr)],
                    },
                    TtbwGroupBuildSpec {
                        group_name: "CR01_02_DS_MT_R",
                        samples: vec![(0, 'M', 1, &s_cr_mt)],
                    },
                    TtbwGroupBuildSpec {
                        group_name: "RD01_05_DS_BW_R",
                        samples: vec![(0, 'M', 1, &s_rd)],
                    },
                ],
                0xC8,
            ),
        )
        .unwrap();

        // soundb60.obw: Surround-only mics (`Front_L`, `Center`, `Rear_LR`) -> must be skipped!
        fs::write(
            sd3_sounds.join("soundb60.obw"),
            ttbw::build_ttbw_bytes(
                &["Front_L", "Center", "Rear_LR"],
                &[TtbwGroupBuildSpec {
                    group_name: "SN01_DS_HIT_R",
                    samples: vec![(0, 'M', 1, &s_surr), (1, 'M', 1, &s_surr)],
                }],
                0xC8,
            ),
        )
        .unwrap();

        // soundb120.obw: Bleed mics (`Bleed_Kick`) -> routes strictly to Bus 3 (Bleed)
        fs::write(
            sd3_sounds.join("soundb120.obw"),
            ttbw::build_ttbw_bytes(
                &["Bleed_Kick"],
                &[TtbwGroupBuildSpec {
                    group_name: "SN01_DS_HIT_R",
                    samples: vec![(0, 'M', 1, &s_bleed)],
                }],
                0xC8,
            ),
        )
        .unwrap();

        // Multi-room SDX: SL-DualRooms/SL/ROOM1 and SL/ROOM2
        for rname in ["ROOM1", "ROOM2"] {
            let rp = root
                .join("Toontrack/SL-DualRooms/SL")
                .join(rname)
                .join("Sounds");
            fs::create_dir_all(&rp).unwrap();
            fs::write(
                rp.join("soundb1.obw"),
                ttbw::build_ttbw_bytes(
                    &["KickIn"],
                    &[TtbwGroupBuildSpec {
                        group_name: "KD01_DS_HIT_R",
                        samples: vec![(0, 'M', 1, &s_kd1)],
                    }],
                    0xC8,
                ),
            )
            .unwrap();
        }

        // EZX3 bank with `soundb0.obw`
        let ezx_sounds = root.join("Toontrack/EZDrummer/EZX3_Modern/Sounds");
        fs::create_dir_all(&ezx_sounds).unwrap();
        fs::write(
            ezx_sounds.join("soundb0.obw"),
            ttbw::build_ttbw_bytes(
                &["Kick"],
                &[TtbwGroupBuildSpec {
                    group_name: "KD01_DS_HIT_R",
                    samples: vec![(0, 'M', 1, &s_kd1)],
                }],
                0xC8,
            ),
        )
        .unwrap();

        // 2. Verify fast folder discovery from Toontrack parent AND from inside `Sounds/` subfolder
        let all_banks = library::scan_soundbanks_fast(&root.join("Toontrack")).unwrap();
        assert_eq!(all_banks.len(), 4, "expected SD3 + ROOM1 + ROOM2 + EZX3");

        let from_sounds = library::scan_soundbanks_fast(&sd3_sounds).unwrap();
        assert_eq!(from_sounds.len(), 1);
        assert_eq!(from_sounds[0].display_name, "Superior Drummer 3");
        // Verify numeric ordering: 1, 2, 12, 60, 120!
        let fnames: Vec<String> = from_sounds[0]
            .obw_files
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            fnames,
            vec![
                "soundb1.obw",
                "soundb2.obw",
                "soundb12.obw",
                "soundb60.obw",
                "soundb120.obw"
            ]
        );

        // 3. Verify RoomCatalog piece & tool discovery (+ GraphicsV3 model name + "ALL" option)
        let cat = RoomCatalog::scan_bank(&from_sounds[0], |_, _, _| {}).unwrap();
        assert_eq!(cat.kicks.len(), 3); // KD01, KD02, ALL
        assert!(cat.kicks[0].label.contains("Gretsch"));
        assert_eq!(cat.snares.len(), 3); // SN01, SN02, ALL
        assert_eq!(cat.tools.len(), 3); // DS, BR, ALL

        // 4. Load default selection (`KD01`, `SN01`, `DS`)
        let def_sel = KitSelection::default_for_catalog(&cat);
        let kit = LoadedKit::load_room_selection(&cat, &def_sel, |_, _| {}).unwrap();

        // Verify `_R` + `_L` interleaving on Snare Center (note 38): 2 round-robins!
        let snare_pad_idx = kit.note_to_pad[38].expect("GM Snare 38 mapped");
        assert_eq!(kit.note_to_pad[40], Some(snare_pad_idx), "GM Electric Snare 40 alias");
        let snare_pad = &kit.pads[snare_pad_idx];
        assert_eq!(
            snare_pad.layers[0].round_robins.len(),
            2,
            "SN01_DS_HIT_R and SN01_DS_HIT_L must interleave as 2 RRs!"
        );
        // Verify Snare has Close, OH (`OH_Cond`, NOT overwritten by `OH_Dyn`), Room (`Amb_Near`, NOT overwritten by `Amb_Far` or Surround), AND Bleed (`Bleed_Kick`)!
        let rr0 = &snare_pad.layers[0].round_robins[0];
        assert!(rr0.buses[0].is_some(), "Snare Close mic present");
        assert!(rr0.buses[1].is_some(), "Snare OH mic present");
        assert!(rr0.buses[2].is_some(), "Snare Room mic present");
        assert!(rr0.buses[3].is_some(), "Snare Bleed mic present");
        // Verify OH_Cond (amp 0.68) beat OH_Dyn (amp 0.90) on Bus 1!
        let oh_peak = rr0.buses[1]
            .as_ref()
            .unwrap()
            .head_l
            .iter()
            .fold(0.0f32, |m, &v| m.max(v.abs()));
        assert!(oh_peak < 0.75, "OH_Cond (0.68) must win over OH_Dyn (0.90), got {oh_peak}");

        // Verify GM & SD3 MIDI note aliases across Kick, Hats, Toms, Ride, Crash
        assert!(kit.note_to_pad[36].is_some() && kit.note_to_pad[35] == kit.note_to_pad[36]);
        assert!(kit.note_to_pad[42].is_some() && kit.note_to_pad[22] == kit.note_to_pad[42]);
        assert!(kit.note_to_pad[46].is_some() && kit.note_to_pad[24] == kit.note_to_pad[46]);
        assert!(kit.note_to_pad[50].is_some(), "High Tom 50 mapped");
        assert!(kit.note_to_pad[48].is_some(), "Mid Tom 48 mapped");
        assert!(kit.note_to_pad[45].is_some(), "Low Tom 45 mapped");
        assert!(kit.note_to_pad[51].is_some(), "Ride 51 mapped");
        assert!(kit.note_to_pad[49].is_some(), "Crash 49 mapped");
        assert!(kit.streamed_samples > 0, "6000-frame samples must engage mmap tail streaming");

        // 5. Switch Kick -> `KD02`, Snare -> `SN02`, and Tool -> `BR` (Brushes, with automatic fallback to `DS`!)
        let mut custom_sel = def_sel.clone();
        custom_sel.kick = "KD02".into();
        custom_sel.tool = "BR".into();
        let kit_br = LoadedKit::load_room_selection(&cat, &custom_sel, |_, _| {}).unwrap();
        assert!(kit_br.name.contains("Brushes"));
        assert!(kit_br.note_to_pad[36].is_some(), "Kick KD02 mapped to GM 36");
        assert!(kit_br.note_to_pad[38].is_some(), "Snare switched to BR on GM 38");

        custom_sel.snare = "SN02".into();
        custom_sel.tool = "DS".into();
        let kit_sn2 = LoadedKit::load_room_selection(&cat, &custom_sel, |_, _| {}).unwrap();
        assert!(kit_sn2.note_to_pad[38].is_some(), "Snare SN02 mapped to GM 38");

        // 6. Stream real-time audio blocks across head->mmap boundary
        let mut engine = DrumEngine::new(kit, 44_100.0);
        let mixer = MixerGains::default();
        for &note in &[36u8, 38, 42, 49, 50, 51] {
            engine.note_on(note, 0.9, &mixer, None);
        }
        let mut out_l = [0.0f32; 256];
        let mut out_r = [0.0f32; 256];
        let mut total_energy = 0.0f64;
        for block in 0..30 {
            if block == 12 {
                // Give streamer worker 15 ms to fill ring buffers past the 2048-frame RAM head
                std::thread::sleep(std::time::Duration::from_millis(15));
            }
            engine.process_block(&mut out_l, &mut out_r, &mixer, None);
            for (&l, &r) in out_l.iter().zip(out_r.iter()) {
                assert!(l.is_finite() && r.is_finite());
                total_energy += (l * l + r * r) as f64;
            }
        }
        assert!(total_energy > 1.0);

        let _ = fs::remove_dir_all(&root);
    }
}

