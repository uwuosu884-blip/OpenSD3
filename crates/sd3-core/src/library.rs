//! Superior Drummer 2 / 3 (`SL-*`, `SL/<ROOM>`, `EZX*`) soundbank discovery,
//! fast `oidx` room cataloging, kit-piece inventory, and completeness verifier.
//!
//! Handles every real-world Toontrack directory structure:
//! - Root Toontrack folder (`D:\Toontrack\`) holding multiple `SL-*` & `EZDrummer\EZX*` banks
//! - Single-room libraries: `SL-SuperiorDrummer3\`, `SL-Death\`, `SL-RockFoundry\`, `SL-Avatar\`
//! - Multi-room SDX libraries: `SL-FieldsOfRock\SL\FIELDSOFROCKLIVE\`, `SL-StateOfArt\SL\*`
//! - EZdrummer 2 / 3 libraries: `EZX3_EZdrummer3_MainRoom\`, `EZX_MetalMachine\`
//! - User selecting `...\SL-Death\Sounds` or `...\SL-Death\GraphicsV3` directly
//! - Unmerged multi-part download directories (`Part1\...\SL-SuperiorDrummer3` + `Part2\...`)

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::kit::MicBus;
use crate::ttbw::{self, PieceSlot, TtbwIndex};
use crate::Result;

// ---------------------------------------------------------------------------
// Fast Soundbank Discovery & Room Catalog (for VST3 / Standalone / CLI)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveredBank {
    pub id: String,
    pub display_name: String,
    pub folder_name: String,
    pub room_path: PathBuf,
    pub obw_files: Vec<PathBuf>,
    pub total_bytes: u64,
    pub aversion: Option<String>,
}

impl DiscoveredBank {
    pub fn summary_label(&self) -> String {
        let gb = self.total_bytes as f64 / (1024.0 * 1024.0 * 1024.0);
        let size_str = if gb >= 0.95 {
            format!("{gb:.1} GB")
        } else {
            let mb = self.total_bytes as f64 / (1024.0 * 1024.0);
            format!("{mb:.0} MB")
        };
        format!(
            "{} ({} .obw, {})",
            self.display_name,
            self.obw_files.len(),
            size_str
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PieceOption {
    /// Internal filter key (e.g. `"KD01"`, `"SN02"`, `"01"`, `"DS"`, `"ALL"`).
    pub code: String,
    /// User-facing dropdown label (e.g. `"Snare 01 (SN01)"`, `"Drumsticks (DS)"`).
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct KitSelection {
    pub tool: String,
    pub kick: String,
    pub snare: String,
    pub hihat: String,
    pub toms: String,
    pub ride: String,
    pub cymbals: String,
    pub include_percussion: bool,
}

impl KitSelection {
    pub fn default_for_catalog(cat: &RoomCatalog) -> Self {
        let pick_first = |opts: &[PieceOption], prefer: &str| -> String {
            if let Some(found) = opts.iter().find(|o| o.code == prefer) {
                found.code.clone()
            } else {
                opts.first()
                    .map(|o| o.code.clone())
                    .unwrap_or_else(|| "ALL".into())
            }
        };

        Self {
            tool: pick_first(&cat.tools, "DS"),
            kick: pick_first(&cat.kicks, ""),
            snare: pick_first(&cat.snares, ""),
            hihat: pick_first(&cat.hihats, ""),
            toms: pick_first(&cat.toms, ""),
            ride: pick_first(&cat.rides, ""),
            // Default to first cymbal set (or ALL if only one set) so all kit crashes/splashes/chinas load!
            cymbals: pick_first(&cat.cymbals, ""),
            include_percussion: true,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ObwCatalogEntry {
    pub path: PathBuf,
    pub file_name: String,
    pub file_size: u64,
    /// `Some(TtbwIndex)` for `RIFF 'ttbw'` soundbunches, `None` for legacy/standalone files.
    pub ttbw_index: Option<TtbwIndex>,
    pub surround_only: bool,
}

#[derive(Debug, Clone)]
pub struct RoomCatalog {
    pub bank: DiscoveredBank,
    pub entries: Vec<ObwCatalogEntry>,
    pub tools: Vec<PieceOption>,
    pub kicks: Vec<PieceOption>,
    pub snares: Vec<PieceOption>,
    pub hihats: Vec<PieceOption>,
    pub toms: Vec<PieceOption>,
    pub rides: Vec<PieceOption>,
    pub cymbals: Vec<PieceOption>,
    pub has_percussion: bool,
    pub graphics_models: BTreeMap<String, Vec<String>>,
    pub bus_mic_names: [Vec<String>; 4],
    pub total_indexed_samples: usize,
}

impl RoomCatalog {
    /// Rapidly index all `.obw` soundbunches in `bank` by reading ONLY their `10–120 KB`
    /// `'oidx'` header chunks (`< 80 ms` even for 155 `.obw` files / 230 GB!).
    pub fn scan_bank<F>(bank: &DiscoveredBank, mut on_progress: F) -> Result<Self>
    where
        F: FnMut(usize, usize, &str),
    {
        let total_files = bank.obw_files.len().max(1);
        let mut entries = Vec::with_capacity(bank.obw_files.len());

        let mut tool_set: BTreeSet<String> = BTreeSet::new();
        let mut kick_set: BTreeSet<String> = BTreeSet::new();
        let mut snare_set: BTreeSet<String> = BTreeSet::new();
        let mut hihat_set: BTreeSet<String> = BTreeSet::new();
        let mut tom_set: BTreeSet<String> = BTreeSet::new();
        let mut ride_set: BTreeSet<String> = BTreeSet::new();
        let mut cymbal_set: BTreeSet<String> = BTreeSet::new();
        let mut has_percussion = false;
        let mut bus_mics: [BTreeSet<String>; 4] = Default::default();
        let mut total_indexed_samples = 0usize;

        for (i, obw_path) in bank.obw_files.iter().enumerate() {
            let fname = obw_path
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| format!("soundb{i}.obw"));
            on_progress(i + 1, total_files, &fname);

            let file_size = fs::metadata(obw_path).map(|m| m.len()).unwrap_or(0);
            match ttbw::parse_ttbw_file_index(obw_path) {
                Ok(idx) => {
                    let surround_only = ttbw::is_surround_only_mic_list(&idx.mics);
                    total_indexed_samples += idx.total_records;
                    for (m_name, &bus) in idx.mics.iter().zip(idx.mic_buses.iter()) {
                        if !surround_only {
                            bus_mics[bus.index()].insert(m_name.clone());
                        }
                    }
                    for grp in &idx.groups {
                        let p = &grp.parsed;
                        if p.is_hand_tool {
                            tool_set.insert(p.tool_code.clone());
                        }
                        match p.slot {
                            PieceSlot::Kick => {
                                kick_set.insert(p.piece_code.clone());
                            }
                            PieceSlot::Snare => {
                                snare_set.insert(p.piece_code.clone());
                            }
                            PieceSlot::HiHat => {
                                hihat_set.insert(p.piece_code.clone());
                            }
                            PieceSlot::Tom => {
                                tom_set.insert(p.model_id.clone());
                            }
                            PieceSlot::Ride => {
                                ride_set.insert(p.piece_code.clone());
                            }
                            PieceSlot::Cymbal => {
                                cymbal_set.insert(p.model_id.clone());
                            }
                            PieceSlot::Percussion => {
                                has_percussion = true;
                            }
                        }
                    }
                    entries.push(ObwCatalogEntry {
                        path: obw_path.clone(),
                        file_name: fname,
                        file_size,
                        ttbw_index: Some(idx),
                        surround_only,
                    });
                }
                Err(_) => {
                    entries.push(ObwCatalogEntry {
                        path: obw_path.clone(),
                        file_name: fname,
                        file_size,
                        ttbw_index: None,
                        surround_only: false,
                    });
                }
            }
        }

        let graphics_models = scan_graphics_v3_models(&bank.room_path);

        let build_piece_opts =
            |codes: BTreeSet<String>, prefix_label: &str, gfx_cat: &str| -> Vec<PieceOption> {
                let gfx = graphics_models.get(gfx_cat);
                let mut out: Vec<PieceOption> = codes
                    .into_iter()
                    .enumerate()
                    .map(|(idx, code)| {
                        let hint = gfx.and_then(|v| v.get(idx)).cloned();
                        let label = match hint {
                            Some(h) => format!("{code} — {h}"),
                            None => format!("{prefix_label} ({code})"),
                        };
                        PieceOption { code, label }
                    })
                    .collect();
                if out.len() > 1 {
                    out.push(PieceOption {
                        code: "ALL".into(),
                        label: format!("All {prefix_label}s"),
                    });
                }
                out
            };

        let mut tools: Vec<PieceOption> = Vec::new();
        // Put Drumsticks (DS) first whenever present
        if tool_set.remove("DS") {
            tools.push(PieceOption {
                code: "DS".into(),
                label: ttbw::tool_display_name("DS").into(),
            });
        }
        for t in tool_set {
            tools.push(PieceOption {
                label: format!("{} ({t})", ttbw::tool_display_name(&t)),
                code: t,
            });
        }
        if tools.len() > 1 {
            tools.push(PieceOption {
                code: "ALL".into(),
                label: "All Tools".into(),
            });
        } else if tools.is_empty() {
            tools.push(PieceOption {
                code: "ALL".into(),
                label: "Default".into(),
            });
        }

        let kicks = build_piece_opts(kick_set, "Kick", "Kick");
        let snares = build_piece_opts(snare_set, "Snare", "Snare");
        let hihats = build_piece_opts(hihat_set, "Hi-Hat", "HiHat");
        let rides = build_piece_opts(ride_set, "Ride", "Ride");

        let mut toms: Vec<PieceOption> = tom_set
            .into_iter()
            .map(|id| PieceOption {
                label: format!("Tom Set {id}"),
                code: id,
            })
            .collect();
        if toms.len() > 1 {
            toms.push(PieceOption {
                code: "ALL".into(),
                label: "All Toms".into(),
            });
        }

        let mut cymbals: Vec<PieceOption> = cymbal_set
            .into_iter()
            .map(|id| PieceOption {
                label: format!("Cymbal Set {id}"),
                code: id,
            })
            .collect();
        if cymbals.len() > 1 {
            cymbals.push(PieceOption {
                code: "ALL".into(),
                label: "All Cymbals".into(),
            });
        }

        let bus_mic_names = [
            bus_mics[0].iter().cloned().collect(),
            bus_mics[1].iter().cloned().collect(),
            bus_mics[2].iter().cloned().collect(),
            bus_mics[3].iter().cloned().collect(),
        ];

        Ok(Self {
            bank: bank.clone(),
            entries,
            tools,
            kicks,
            snares,
            hihats,
            toms,
            rides,
            cymbals,
            has_percussion,
            graphics_models,
            bus_mic_names,
            total_indexed_samples,
        })
    }

    pub fn mic_summary_for_bus(&self, bus: MicBus) -> String {
        let list = &self.bus_mic_names[bus.index()];
        if list.is_empty() {
            "—".into()
        } else if list.len() <= 3 {
            list.join(", ")
        } else {
            format!("{}, +{} more", list[..3].join(", "), list.len() - 3)
        }
    }
}

/// Normalize a user-chosen folder path: if the user pointed inside `Sounds` or `GraphicsV3`
/// or `KitLayout`, automatically step up to the enclosing `SL-*` / room directory!
pub fn normalize_bank_search_root(input: &Path) -> PathBuf {
    let mut cur = if input.is_file() {
        input.parent().unwrap_or(input).to_path_buf()
    } else {
        input.to_path_buf()
    };

    for _ in 0..3 {
        let Some(fname) = cur.file_name().map(|s| s.to_string_lossy().into_owned()) else {
            break;
        };
        let lower = fname.to_ascii_lowercase();
        if matches!(
            lower.as_str(),
            "sounds"
                | "graphicsv3"
                | "graphics"
                | "kitlayout"
                | "kick"
                | "snare"
                | "hihat"
                | "tom"
                | "cymbal"
                | "percussion"
                | "stand"
        ) {
            if let Some(parent) = cur.parent() {
                cur = parent.to_path_buf();
                continue;
            }
        }
        break;
    }
    cur
}

/// Ultra-fast soundbank scanner (`< 20 ms`): discovers all Superior Drummer 2/3 (`SL-*`,
/// `SL/<ROOM>`) and EZdrummer (`EZX*`) rooms under `root` without faulting in `.obw` audio.
pub fn scan_soundbanks_fast(root: impl AsRef<Path>) -> Result<Vec<DiscoveredBank>> {
    let norm_root = normalize_bank_search_root(root.as_ref());
    let mut raw_room_dirs = Vec::new();
    discover_room_dirs_fast(&norm_root, 0, &mut raw_room_dirs);
    raw_room_dirs.sort();
    raw_room_dirs.dedup();

    // If user pointed directly at a folder holding `.obw` / `.ttpw` files with no `Sounds/` subdir
    if raw_room_dirs.is_empty() && dir_has_direct_obw_or_ttpw(&norm_root) {
        raw_room_dirs.push(norm_root.clone());
    }

    let mut banks: Vec<DiscoveredBank> = Vec::new();
    for dir in raw_room_dirs {
        if let Some(bank) = build_discovered_bank(&dir) {
            banks.push(bank);
        }
    }

    // Merge unmerged multi-part downloads (e.g. `Part1/.../SL-SuperiorDrummer3` + `Part2/.../SL-SuperiorDrummer3`)
    // when two directories share the same `SL-*` folder_name and hold disjoint `soundbN.obw` indices.
    let mut merged: Vec<DiscoveredBank> = Vec::new();
    for b in banks {
        if let Some(existing) = merged.iter_mut().find(|e| {
            e.folder_name.eq_ignore_ascii_case(&b.folder_name)
                && e.folder_name.to_ascii_uppercase().starts_with("SL-")
                && obw_sets_are_disjoint(&e.obw_files, &b.obw_files)
        }) {
            if existing.aversion.is_none() && b.aversion.is_some() {
                existing.aversion = b.aversion.clone();
                existing.room_path = b.room_path.clone();
            }
            existing.total_bytes += b.total_bytes;
            existing.obw_files.extend(b.obw_files);
            sort_obw_paths(&mut existing.obw_files);
        } else {
            merged.push(b);
        }
    }

    Ok(merged)
}

/// Scan standard Windows & Linux Toontrack library directories plus any user-registered roots.
pub fn auto_discover_system_banks(extra_roots: &[PathBuf]) -> Vec<DiscoveredBank> {
    let mut roots: Vec<PathBuf> = extra_roots.to_vec();
    roots.extend(default_toontrack_search_roots());

    let mut seen_paths = BTreeSet::new();
    let mut out = Vec::new();
    for r in roots {
        if !r.exists() {
            continue;
        }
        if let Ok(found) = scan_soundbanks_fast(&r) {
            for b in found {
                let key = b.room_path.to_string_lossy().to_ascii_lowercase();
                if seen_paths.insert(key) {
                    out.push(b);
                }
            }
        }
    }
    out
}

pub fn default_toontrack_search_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();

    #[cfg(target_os = "windows")]
    {
        for env_key in ["PROGRAMFILES", "PROGRAMFILES(X86)", "COMMONPROGRAMFILES(X86)", "PROGRAMDATA", "USERPROFILE"] {
            if let Ok(val) = std::env::var(env_key) {
                let base = PathBuf::from(val);
                roots.push(base.join("Toontrack"));
                roots.push(base.join("Documents").join("Toontrack"));
                roots.push(base.join("Documents").join("Superior Drummer 3 Sounds"));
            }
        }
        for letter in b'C'..=b'Z' {
            let drive = format!("{}:\\", letter as char);
            let dp = PathBuf::from(&drive);
            if !dp.exists() {
                continue;
            }
            for sub in [
                "Toontrack",
                "Superior Drummer 3",
                "Superior Drummer",
                "SD3",
                "EZdrummer",
                "Samples\\Toontrack",
                "Audio\\Toontrack",
            ] {
                let cand = dp.join(sub);
                if cand.is_dir() {
                    roots.push(cand);
                }
            }
            // Also detect top-level `<Drive>:\SL-*` or `<Drive>:\EZX*` folders directly!
            if let Ok(rd) = fs::read_dir(&dp) {
                for e in rd.flatten() {
                    let fname = e.file_name().to_string_lossy().into_owned();
                    let up = fname.to_ascii_uppercase();
                    if (up.starts_with("SL-") || up.starts_with("EZX")) && e.path().is_dir() {
                        roots.push(e.path());
                    }
                }
            }
        }
    }

    #[cfg(not(target_os = "windows"))]
    {
        if let Ok(home) = std::env::var("HOME") {
            let hp = PathBuf::from(home);
            for rel in [
                "Toontrack",
                "Samples/Toontrack",
                "Music/Toontrack",
                ".wine/drive_c/Program Files (x86)/Common Files/Toontrack",
                ".wine/drive_c/ProgramData/Toontrack",
            ] {
                let cand = hp.join(rel);
                if cand.is_dir() {
                    roots.push(cand);
                }
            }
        }
        for mnt_root in ["/mnt", "/media", "/run/media"] {
            let mp = Path::new(mnt_root);
            if let Ok(rd) = fs::read_dir(mp) {
                for e in rd.flatten() {
                    let p = e.path();
                    if p.is_dir() {
                        for sub in ["Toontrack", "Superior Drummer 3", "SD3"] {
                            let c = p.join(sub);
                            if c.is_dir() {
                                roots.push(c);
                            }
                        }
                    }
                }
            }
        }
    }

    roots
}

fn discover_room_dirs_fast(dir: &Path, depth: usize, rooms: &mut Vec<PathBuf>) {
    if depth > 6 || !dir.is_dir() {
        return;
    }

    let sounds_upper = dir.join("Sounds");
    let sounds_lower = dir.join("sounds");
    if (sounds_upper.is_dir() && dir_has_direct_obw_or_ttpw(&sounds_upper))
        || (sounds_lower.is_dir() && dir_has_direct_obw_or_ttpw(&sounds_lower))
    {
        rooms.push(dir.to_path_buf());
    }

    let Ok(rd) = fs::read_dir(dir) else {
        return;
    };
    for entry in rd.flatten() {
        let p = entry.path();
        if !p.is_dir() {
            continue;
        }
        let name_s = entry.file_name().to_string_lossy().into_owned();
        if matches!(
            name_s.to_ascii_lowercase().as_str(),
            "sounds"
                | "graphicsv3"
                | "graphics"
                | "sampleprojs"
                | "midi"
                | "windows"
                | "program files"
                | "$recycle.bin"
                | "system volume information"
        ) {
            continue;
        }
        discover_room_dirs_fast(&p, depth + 1, rooms);
    }
}

fn dir_has_direct_obw_or_ttpw(dir: &Path) -> bool {
    let Ok(rd) = fs::read_dir(dir) else {
        return false;
    };
    for e in rd.flatten() {
        let fname = e.file_name().to_string_lossy().to_ascii_lowercase();
        if fname.ends_with(".obw") || fname.ends_with(".ttpw") {
            return true;
        }
    }
    false
}

fn build_discovered_bank(room_dir: &Path) -> Option<DiscoveredBank> {
    let folder_name = room_dir
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| room_dir.display().to_string());

    let sounds_dir = if room_dir.join("Sounds").is_dir() {
        room_dir.join("Sounds")
    } else if room_dir.join("sounds").is_dir() {
        room_dir.join("sounds")
    } else {
        room_dir.to_path_buf()
    };

    let mut obw_files = Vec::new();
    let mut total_bytes = 0u64;
    if let Ok(rd) = fs::read_dir(&sounds_dir) {
        for e in rd.flatten() {
            let p = e.path();
            let fname = e.file_name().to_string_lossy().to_ascii_lowercase();
            if fname.ends_with(".obw") || fname.ends_with(".ttpw") {
                total_bytes += e.metadata().map(|m| m.len()).unwrap_or(0);
                obw_files.push(p);
            }
        }
    }
    if obw_files.is_empty() {
        return None;
    }
    sort_obw_paths(&mut obw_files);

    let room_name = read_first_text_line(&room_dir.join("Name"));
    let aversion = read_first_text_line(&room_dir.join("Aversion"));

    // For multi-room SDXs (`SL-FieldsOfRock/SL/FIELDSOFROCKLIVE`), include parent SDX `Name`!
    let parent_sdx_name = room_dir
        .parent()
        .and_then(|p| {
            let pf = p.file_name()?.to_string_lossy();
            if pf.eq_ignore_ascii_case("SL") {
                p.parent()
            } else {
                None
            }
        })
        .and_then(|gp| read_first_text_line(&gp.join("Name")));

    let display_name = match (parent_sdx_name, room_name) {
        (Some(parent_n), Some(room_n)) if !room_n.contains(&parent_n) => {
            format!("{parent_n} — {room_n}")
        }
        (_, Some(room_n)) => room_n,
        (Some(parent_n), None) => format!("{parent_n} — {folder_name}"),
        (None, None) => folder_name.clone(),
    };

    let id = room_dir.to_string_lossy().into_owned();
    Some(DiscoveredBank {
        id,
        display_name,
        folder_name,
        room_path: room_dir.to_path_buf(),
        obw_files,
        total_bytes,
        aversion,
    })
}

fn sort_obw_paths(paths: &mut [PathBuf]) {
    paths.sort_by_key(|p| {
        let fname = p
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        (parse_soundb_index(&fname).unwrap_or(u32::MAX), fname)
    });
}

fn obw_sets_are_disjoint(a: &[PathBuf], b: &[PathBuf]) -> bool {
    let set_a: BTreeSet<String> = a
        .iter()
        .filter_map(|p| p.file_name().map(|s| s.to_string_lossy().to_ascii_lowercase()))
        .collect();
    !b.iter().any(|p| {
        p.file_name()
            .map(|s| set_a.contains(&s.to_string_lossy().to_ascii_lowercase()))
            .unwrap_or(false)
    })
}

fn read_first_text_line(path: &Path) -> Option<String> {
    let mut bytes = fs::read(path).ok()?;
    bytes.truncate(4096);
    let text = if bytes.len() >= 2 && (bytes.starts_with(&[0xFF, 0xFE]) || looks_utf16le(&bytes)) {
        let start = if bytes.starts_with(&[0xFF, 0xFE]) { 2 } else { 0 };
        let units: Vec<u16> = bytes[start..]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        String::from_utf16_lossy(&units)
    } else {
        String::from_utf8_lossy(&bytes).into_owned()
    };
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && l.chars().all(|c| !c.is_control()))
        .map(ToOwned::to_owned)
}

fn scan_graphics_v3_models(room_dir: &Path) -> BTreeMap<String, Vec<String>> {
    let mut map: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let gfx_v3 = room_dir.join("GraphicsV3");
    let gfx_roots: Vec<PathBuf> = [gfx_v3.join("KitLayout"), gfx_v3]
        .into_iter()
        .filter(|p| p.is_dir())
        .collect();
    if gfx_roots.is_empty() {
        return map;
    }

    for cat in ["Kick", "Snare", "HiHat", "Tom", "Cymbal", "Percussion"] {
        let mut names = BTreeSet::new();
        for gfx_root in &gfx_roots {
            let cat_dir = gfx_root.join(cat);
            if !cat_dir.is_dir() {
                continue;
            }
            if let Ok(rd) = fs::read_dir(&cat_dir) {
                for e in rd.flatten() {
                    let p = e.path();
                    if p.is_dir() {
                        let sub_name = e.file_name().to_string_lossy().into_owned();
                        names.insert(sub_name);
                        continue;
                    }
                    let fname = e.file_name().to_string_lossy().into_owned();
                    let Some(stem) =
                        fname.strip_suffix(".png").or_else(|| fname.strip_suffix(".PNG"))
                    else {
                        continue;
                    };
                    let lower = stem.to_ascii_lowercase();
                    if lower.contains("highlight")
                        || lower.contains("unloaded")
                        || lower.contains("pedalanimation")
                        || lower.contains("sidestick")
                        || lower.contains("generic")
                        || lower.ends_with("_l")
                        || lower.ends_with("_r")
                        || lower.ends_with("_l45")
                        || lower.ends_with("_r45")
                    {
                        continue;
                    }
                    let clean = stem
                        .strip_suffix("_C")
                        .unwrap_or(stem)
                        .replace('_', " ")
                        .trim()
                        .to_owned();
                    if !clean.is_empty() {
                        names.insert(clean);
                    }
                }
            }
        }
        if !names.is_empty() {
            map.insert(cat.to_owned(), names.into_iter().collect());
        }
    }
    map
}

// ---------------------------------------------------------------------------
// Full Library Completeness Report (`sd3tool scan`)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SdLibraryReport {
    pub root_path: PathBuf,
    pub rooms: Vec<SdRoomInfo>,
    pub ttfs_packs: Vec<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SdRoomInfo {
    pub folder_name: String,
    pub path: PathBuf,
    pub display_name: Option<String>,
    pub aversion: Option<String>,
    pub generation: String,
    pub config_files: Vec<ConfigFileSummary>,
    pub graphics_count: usize,
    pub soundstats_path: Option<PathBuf>,
    pub soundstats_size: Option<u64>,
    pub soundbunches: Vec<SoundbunchInfo>,
    pub missing_indices: Vec<u32>,
    pub total_obw_bytes: u64,
    pub core_parts_summary: Option<Vec<PartGroupStatus>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigFileSummary {
    pub name: String,
    pub size: u64,
    pub kind: String,
    pub preview_lines: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SoundbunchInfo {
    pub file_name: String,
    pub index: Option<u32>,
    pub size: u64,
    pub path: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mics: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_count: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample_count: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PartGroupStatus {
    pub label: String,
    pub expected_range: String,
    pub present_count: usize,
    pub expected_count: usize,
    pub status: String,
}

const KNOWN_CONFIG_NAMES: &[&str] = &[
    "Name",
    "Aversion",
    "Configs",
    "InstallerConfig",
    "instrumentgroups",
    "micconf",
    "padconf",
    "s3presetconf",
    "soundDB",
    "StackPack",
    "uberconf",
    "kitconf",
    "GM-padconf",
    "bleedlevelpresetconf",
    "bleedusagepresetconf",
    "layerlimitpresetconf",
    "mixpresetconf",
    "padsoundpresetconf",
];

pub fn scan_library_tree(root: impl AsRef<Path>) -> Result<SdLibraryReport> {
    let root = normalize_bank_search_root(root.as_ref());
    let mut room_dirs = Vec::new();
    let mut ttfs_packs = Vec::new();
    discover_rooms(&root, 0, &mut room_dirs, &mut ttfs_packs)?;
    room_dirs.sort();
    room_dirs.dedup();
    ttfs_packs.sort();

    let mut rooms = Vec::new();
    for dir in room_dirs {
        rooms.push(inspect_room(&dir)?);
    }

    Ok(SdLibraryReport {
        root_path: root,
        rooms,
        ttfs_packs,
    })
}

fn discover_rooms(
    dir: &Path,
    depth: usize,
    rooms: &mut Vec<PathBuf>,
    ttfs_packs: &mut Vec<PathBuf>,
) -> Result<()> {
    if depth > 6 || !dir.exists() {
        return Ok(());
    }
    if dir.is_file() {
        if has_magic(dir, b"ttfs") {
            ttfs_packs.push(dir.to_path_buf());
        }
        return Ok(());
    }

    let sounds_upper = dir.join("Sounds");
    let sounds_lower = dir.join("sounds");
    if sounds_upper.is_dir() || sounds_lower.is_dir() || dir.join("soundDB").is_file() {
        rooms.push(dir.to_path_buf());
    }

    let Ok(rd) = fs::read_dir(dir) else {
        return Ok(());
    };
    for entry in rd.flatten() {
        let p = entry.path();
        let name = entry.file_name();
        let name_s = name.to_string_lossy();
        if p.is_dir() {
            if matches!(
                name_s.as_ref(),
                "Sounds" | "sounds" | "GraphicsV3" | "Graphics" | "SampleProjs"
            ) {
                continue;
            }
            discover_rooms(&p, depth + 1, rooms, ttfs_packs)?;
        } else if p.is_file() && has_magic(&p, b"ttfs") {
            ttfs_packs.push(p);
        }
    }
    Ok(())
}

fn has_magic(path: &Path, magic: &[u8]) -> bool {
    use std::io::Read;
    let Ok(mut f) = fs::File::open(path) else {
        return false;
    };
    let mut buf = [0u8; 4];
    if f.read_exact(&mut buf).is_err() {
        return false;
    }
    &buf[..magic.len()] == magic
}

fn inspect_room(dir: &Path) -> Result<SdRoomInfo> {
    let folder_name = dir
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| dir.display().to_string());

    let mut config_files = Vec::new();
    let mut display_name = None;
    let mut aversion = None;
    let mut has_sd3_markers = dir.join("GraphicsV3").is_dir();

    for &cfg_name in KNOWN_CONFIG_NAMES {
        let p = dir.join(cfg_name);
        if let Ok(meta) = fs::metadata(&p) {
            if meta.is_file() {
                if matches!(cfg_name, "s3presetconf" | "soundDB" | "Configs" | "StackPack") {
                    has_sd3_markers = true;
                }
                let summary = summarize_config_file(&p, cfg_name, meta.len());
                if cfg_name == "Name" {
                    display_name = summary.preview_lines.first().cloned();
                } else if cfg_name == "Aversion" {
                    aversion = summary.preview_lines.first().cloned();
                }
                config_files.push(summary);
            }
        }
    }

    let graphics_count = ["GraphicsV3", "Graphics"]
        .iter()
        .map(|g| {
            fs::read_dir(dir.join(g))
                .map(|rd| rd.flatten().count())
                .unwrap_or(0)
        })
        .sum();

    let sounds_dir = if dir.join("Sounds").is_dir() {
        Some(dir.join("Sounds"))
    } else if dir.join("sounds").is_dir() {
        Some(dir.join("sounds"))
    } else {
        None
    };

    let mut soundstats_path = None;
    let mut soundstats_size = None;
    let mut soundbunches = Vec::new();

    if let Some(sdir) = sounds_dir {
        for cand in ["soundstats.dat", "soundstats"] {
            let sp = sdir.join(cand);
            if let Ok(m) = fs::metadata(&sp) {
                if m.is_file() {
                    if cand == "soundstats.dat" {
                        has_sd3_markers = true;
                    }
                    soundstats_path = Some(sp);
                    soundstats_size = Some(m.len());
                    break;
                }
            }
        }
        if let Ok(rd) = fs::read_dir(&sdir) {
            for e in rd.flatten() {
                let p = e.path();
                let fname = e.file_name().to_string_lossy().into_owned();
                if fname.to_ascii_lowercase().ends_with(".obw") {
                    let size = e.metadata().map(|m| m.len()).unwrap_or(0);
                    let index = parse_soundb_index(&fname);
                    let (mics, group_count, sample_count) =
                        match crate::ttbw::parse_ttbw_file_index(&p) {
                            Ok(idx) => {
                                has_sd3_markers = true;
                                (
                                    Some(idx.mics),
                                    Some(idx.groups.len()),
                                    Some(idx.total_records),
                                )
                            }
                            Err(_) => (None, None, None),
                        };
                    soundbunches.push(SoundbunchInfo {
                        file_name: fname,
                        index,
                        size,
                        path: p,
                        mics,
                        group_count,
                        sample_count,
                    });
                }
            }
        }
    }

    soundbunches.sort_by_key(|s| (s.index.unwrap_or(u32::MAX), s.file_name.clone()));
    let total_obw_bytes: u64 = soundbunches.iter().map(|s| s.size).sum();

    let indices: BTreeSet<u32> = soundbunches.iter().filter_map(|s| s.index).collect();
    let mut missing_indices = Vec::new();
    if let (Some(&min_i), Some(&max_i)) = (indices.first(), indices.last()) {
        let start = if min_i == 0 { 0 } else { 1 };
        for idx in start..=max_i {
            if !indices.contains(&idx) {
                missing_indices.push(idx);
            }
        }
    }

    let is_core_sd3 = folder_name.eq_ignore_ascii_case("SL-SuperiorDrummer3")
        || display_name
            .as_deref()
            .map(|n| n.contains("Superior Drummer 3"))
            .unwrap_or(false);

    let core_parts_summary = if is_core_sd3 {
        Some(evaluate_core_sd3_parts(&indices))
    } else {
        None
    };

    let generation = if has_sd3_markers {
        "SD3 (SDX)".into()
    } else if dir.join("SampleProjs").is_dir() {
        "SD2 (Legacy SL)".into()
    } else {
        "SD2/SD3 Compatible".into()
    };

    Ok(SdRoomInfo {
        folder_name,
        path: dir.to_path_buf(),
        display_name,
        aversion,
        generation,
        config_files,
        graphics_count,
        soundstats_path,
        soundstats_size,
        soundbunches,
        missing_indices,
        total_obw_bytes,
        core_parts_summary,
    })
}

pub fn parse_soundb_index(file_name: &str) -> Option<u32> {
    let lower = file_name.to_ascii_lowercase();
    let stem = lower.strip_suffix(".obw")?;
    let digits = stem
        .strip_prefix("soundb")
        .or_else(|| stem.strip_prefix("sound"))?;
    digits.parse::<u32>().ok()
}

fn evaluate_core_sd3_parts(indices: &BTreeSet<u32>) -> Vec<PartGroupStatus> {
    let ranges = [
        (
            "Part 1: Basic Sound Library (Close + OH Dyn + Amb Ribbon)",
            1u32,
            23u32,
        ),
        ("Part 2: Room Mics 1 (OH Cond + Amb Near + Amb Mid)", 24, 52),
        ("Parts 3-4: Surround Mics", 53, 113),
        ("Part 5: Additional Bleed", 114, 150),
        ("Library Update Soundbunches", 151, 155),
    ];
    ranges
        .into_iter()
        .map(|(label, lo, hi)| {
            let expected = (hi - lo + 1) as usize;
            let present = (lo..=hi).filter(|i| indices.contains(i)).count();
            let status = if present == expected {
                "complete".into()
            } else if present == 0 {
                "not installed".into()
            } else {
                format!("partial ({present}/{expected})")
            };
            PartGroupStatus {
                label: label.into(),
                expected_range: format!("soundb{lo}.obw .. soundb{hi}.obw"),
                present_count: present,
                expected_count: expected,
                status,
            }
        })
        .collect()
}

fn summarize_config_file(path: &Path, name: &str, size: u64) -> ConfigFileSummary {
    let bytes = fs::read(path)
        .map(|mut v| {
            v.truncate(8192);
            v
        })
        .unwrap_or_default();

    if bytes.starts_with(b"ttfs") {
        return ConfigFileSummary {
            name: name.into(),
            size,
            kind: "ttfs-container".into(),
            preview_lines: vec!["Toontrack ttfs archive".into()],
        };
    }

    if bytes.len() >= 2 && (bytes.starts_with(&[0xFF, 0xFE]) || looks_utf16le(&bytes)) {
        let start = if bytes.starts_with(&[0xFF, 0xFE]) { 2 } else { 0 };
        let units: Vec<u16> = bytes[start..]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        let text = String::from_utf16_lossy(&units);
        let lines: Vec<String> = text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .take(8)
            .map(ToOwned::to_owned)
            .collect();
        return ConfigFileSummary {
            name: name.into(),
            size,
            kind: "utf16le-text".into(),
            preview_lines: lines,
        };
    }

    let printable = bytes
        .iter()
        .filter(|&&b| (0x20..=0x7e).contains(&b) || matches!(b, b'\n' | b'\r' | b'\t'))
        .count();
    if !bytes.is_empty() && printable * 100 / bytes.len() >= 85 {
        let text = String::from_utf8_lossy(&bytes);
        let lines: Vec<String> = text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .take(8)
            .map(ToOwned::to_owned)
            .collect();
        ConfigFileSummary {
            name: name.into(),
            size,
            kind: "text".into(),
            preview_lines: lines,
        }
    } else {
        let hex: String = bytes
            .iter()
            .take(16)
            .map(|b| format!("{b:02x}"))
            .collect::<Vec<_>>()
            .join(" ");
        ConfigFileSummary {
            name: name.into(),
            size,
            kind: "binary".into(),
            preview_lines: vec![format!("magic: {hex}")],
        }
    }
}

fn looks_utf16le(bytes: &[u8]) -> bool {
    if bytes.len() < 8 {
        return false;
    }
    let sample = &bytes[..bytes.len().min(64)];
    let odd_zeros = sample.iter().skip(1).step_by(2).filter(|&&b| b == 0).count();
    let even_printable = sample
        .iter()
        .step_by(2)
        .filter(|&&b| (0x20..=0x7e).contains(&b) || b == b'\n' || b == b'\r')
        .count();
    odd_zeros * 4 >= sample.len() / 2 * 3 && even_printable * 2 >= sample.len() / 2
}
