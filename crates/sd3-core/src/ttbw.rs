//! Parser, indexer, articulation mapper, and builder for Superior Drummer 3
//! `RIFF 'ttbw'` (`.obw` soundbunch) containers and `'oidx'` sample directories.
//!
//! ## Verified `.obw` (`RIFF 'ttbw'`) Container Architecture
//!
//! - `0x00..0x0C`: `'RIFF' <u32 file_size - 8> 'ttbw'`
//! - `'oidx'` chunk (`~10–120 KB` at `0x0C`):
//!   - `8`-byte header: `[06, 01, 0a, 02, 01, 00, 00, num_mics: u8]`
//!   - `num_mics` NUL-terminated ASCII microphone names (e.g. `"OH451"`, `"Amb67"`)
//!   - Sequence of articulation groups terminated by a trailing `0x00` byte:
//!     - `<group_name_cstring>\0` + `<u32 group_payload_len>`
//!     - `<u8 group_mic_count>` blocks of:
//!       - `<u8 mic_idx><u8 hit_count>` + `hit_count × 10`-byte `<u32 stored_off, u8 vel_char, u8 rr, u32 byte_size>`
//!   - Sample offsets in `'oidx'` are XOR-obfuscated with a constant per-file `u32` mask
//!     (`real_data_offset = stored_off ^ xor_mask`).
//! - `'data'` chunk (immediately after `'oidx'` + optional 1-byte RIFF even pad):
//!   - Contiguous 2-byte-aligned `RIFF 'ttpw'` 20-bit lossless audio streams.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::kit::MicBus;
use crate::{Result, Sd3Error};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PieceSlot {
    Kick,
    Snare,
    HiHat,
    Tom,
    Ride,
    Cymbal,
    Percussion,
}

impl PieceSlot {
    pub fn label(&self) -> &'static str {
        match self {
            PieceSlot::Kick => "Kick",
            PieceSlot::Snare => "Snare",
            PieceSlot::HiHat => "Hi-Hat",
            PieceSlot::Tom => "Toms",
            PieceSlot::Ride => "Ride",
            PieceSlot::Cymbal => "Cymbals",
            PieceSlot::Percussion => "Percussion",
        }
    }
}

/// Structured breakdown of a Toontrack `OidxGroup` identifier
/// (e.g. `"CR01_02_DS_MT_R"`, `"SN02_01_DS_CN_L"`, `"KD01_01_FP_HT_R"`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParsedGroupCode {
    pub raw_name: String,
    pub slot: PieceSlot,
    /// Prefix + model digits (e.g. `"KD01"`, `"SN02"`, `"HH01"`, `"TT01"`, `"CR01"`).
    pub piece_code: String,
    /// Numeric/model set identifier (e.g. `"01"`, `"02"`). Used to group multi-pad sets like `TT01`+`FT01`.
    pub model_id: String,
    /// Kit pad position token (e.g. `"01"`, `"02"`, `"04"`, `"05"`).
    pub slot_pos: String,
    /// Tool code (`"DS"` sticks, `"BR"` brushes, `"RD"` rods, `"ML"` mallets, `"HD"` hands, `"FP"`/`"PP"`/`"WP"` beater, `"ANY"`).
    pub tool_code: String,
    /// True if `tool_code` is a selectable drummer hand tool (`DS`, `BR`, `RD`, `ML`, `HD`).
    pub is_hand_tool: bool,
    /// Articulation code (`"CN"`, `"RS"`, `"SS"`, `"CR"`, `"MT"`, `"CL"`, `"OP"`, `"O1"`, ...).
    pub artic_code: String,
    /// Hand/pedal side (`Some('R')` or `Some('L')`).
    pub hand: Option<char>,
    /// Canonical pad merge key: merges `_L` and `_R` hand hits onto one alternating-RR pad for
    /// Snare/HiHat/Toms/Ride/Cymbals, while keeping `_L` and `_R` double-kick pedals distinct!
    pub pad_merge_key: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OidxSampleRecord {
    pub stored_offset: u32,
    /// De-obfuscated byte offset relative to the start of the `'data'` chunk payload.
    pub data_offset: u64,
    pub vel_char: char,
    /// 1-based round-robin index from Toontrack (`1..=N`).
    pub rr_num: u8,
    pub byte_size: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OidxMicHits {
    pub mic_index: u8,
    pub mic_name: String,
    pub bus: MicBus,
    pub records: Vec<OidxSampleRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OidxGroup {
    pub name: String,
    pub parsed: ParsedGroupCode,
    pub instrument_key: String,
    pub instrument_family: String,
    pub articulation_code: String,
    pub display_name: String,
    /// Canonical GM/SD3 MIDI note for this articulation (e.g. `36` for every Kick, `38` for every Snare Center).
    pub preferred_note: u8,
    /// Deduplicated MIDI note within a single `.obw` file (used by single-`.obw` SFZ/WAV exports).
    pub suggested_note: u8,
    pub member_choke_group: Option<u8>,
    pub triggers_choke_group: Option<u8>,
    pub is_hihat_open: bool,
    /// `0` = tight closed .. `127` = wide open (for CC4 hi-hat variable openness).
    pub hihat_openness: Option<u8>,
    pub mics: Vec<OidxMicHits>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TtbwIndex {
    pub header_bytes: [u8; 8],
    pub mics: Vec<String>,
    pub mic_buses: Vec<MicBus>,
    pub xor_mask: u32,
    pub groups: Vec<OidxGroup>,
    pub total_records: usize,
    pub oidx_offset: u64,
    pub oidx_size: u64,
    pub data_payload_offset: u64,
    pub data_payload_size: u64,
}

#[inline]
pub fn looks_like_ttbw(buf: &[u8]) -> bool {
    buf.len() >= 12 && &buf[0..4] == b"RIFF" && &buf[8..12] == b"ttbw"
}

#[inline]
pub fn looks_like_oidx_payload(buf: &[u8]) -> bool {
    buf.len() >= 10
        && buf[0] == 0x06
        && buf[1] == 0x01
        && buf[7] >= 1
        && buf[7] <= 64
        && (0x21..=0x7e).contains(&buf[8])
}

pub fn tool_display_name(tool_code: &str) -> &'static str {
    match tool_code.to_ascii_uppercase().as_str() {
        "DS" => "Drumsticks (DS)",
        "BR" => "Brushes (BR)",
        "RD" => "Rods (RD)",
        "ML" => "Mallets (ML)",
        "HD" => "Hands / Fingers (HD)",
        "FP" => "Felt Beater (FP)",
        "PP" => "Plastic Beater (PP)",
        "WP" => "Wood Beater (WP)",
        "ANY" => "Default",
        "ALL" => "All Tools",
        _ => "Special Tool",
    }
}

/// Classify a Toontrack microphone channel name (e.g. `"OH451"`, `"Amb67"`, `"SnareTop"`,
/// `"BleedSnare"`) into one of the 4 OpenSD3 studio mixer buses.
pub fn classify_mic_name(mic_name: &str) -> MicBus {
    let lower = mic_name.to_ascii_lowercase();
    if lower.contains("bleed") {
        MicBus::Bleed
    } else if lower.starts_with("oh")
        || lower.contains("overhead")
        || lower.contains("_oh")
        || lower.contains(" oh")
    {
        MicBus::Oh
    } else if lower.starts_with("amb")
        || lower.contains("room")
        || lower.contains("far")
        || lower.contains("mid")
        || lower.contains("near")
        || lower.contains("mono")
        || lower.contains("surr")
        || lower.contains("height")
        || lower.contains("chamber")
        || lower.contains("hall")
        || lower.contains("front")
        || lower.contains("rear")
        || lower.contains("wide")
    {
        MicBus::Room
    } else {
        MicBus::Close
    }
}

/// Lower number = higher priority stereo studio mic within the same `MicBus`.
/// Ensures primary direct/OH/Room stereo mics are NEVER overwritten by secondary bottom/sub
/// mics or 11.1 surround height channels (`soundb53..113.obw`)!
pub fn mic_priority_in_bus(mic_name: &str, bus: MicBus) -> u8 {
    let lower = mic_name.to_ascii_lowercase();
    let is_surround = lower.contains("surr")
        || lower.contains("height")
        || lower.contains("rear")
        || lower.contains("wide")
        || lower.contains("lfe");

    match bus {
        MicBus::Close => {
            if is_surround {
                30
            } else if lower.contains("bot")
                || lower.contains("btm")
                || lower.contains("under")
                || lower.contains("sub")
                || lower.contains("wire")
            {
                10
            } else if lower.contains("out") || lower.contains("res") {
                5
            } else {
                0
            }
        }
        MicBus::Oh => {
            if is_surround {
                30
            } else if lower.contains("mono") {
                10
            } else if lower.contains("cond") || lower.contains("451") || lower.contains("main") {
                0
            } else {
                2
            }
        }
        MicBus::Room => {
            if is_surround {
                40
            } else if lower.contains("amb")
                || lower.contains("room")
                || lower.contains("rib")
                || lower.contains("near")
                || lower.contains("mid")
            {
                if lower.contains("mono") {
                    6
                } else {
                    0
                }
            } else if lower.contains("far") || lower.contains("chamber") || lower.contains("hall") {
                4
            } else {
                12
            }
        }
        MicBus::Bleed => {
            if is_surround {
                30
            } else if lower.contains("snare") || lower.contains("oh") || lower.contains("amb") {
                0
            } else {
                5
            }
        }
    }
}

/// Returns true if an `.obw` file ONLY contains 11.1 surround/height channels (`Surr*`, `Height*`, `Rear*`)
/// so stereo drum kits can skip opening 80+ GB of unused surround soundbunches!
pub fn is_surround_only_mic_list(mics: &[String]) -> bool {
    !mics.is_empty()
        && mics.iter().all(|m| {
            let l = m.to_ascii_lowercase();
            l.contains("surr")
                || l.contains("height")
                || l.contains("rear")
                || l.contains("wide")
                || l.contains("front")
                || l.contains("center")
                || l.contains("lfe")
        })
}

/// Parse any Toontrack SD2 / SD3 / EZX `OidxGroup` name into structured slot, model, tool,
/// articulation, hand, and pad-merge keys.
pub fn parse_group_code(group_name: &str) -> ParsedGroupCode {
    let parts: Vec<&str> = group_name.split('_').filter(|s| !s.is_empty()).collect();
    let p0 = parts.first().copied().unwrap_or(group_name);
    let p0_up = p0.to_ascii_uppercase();

    // Split `CR01` -> prefix `CR`, digits `01`
    let alpha_len = p0_up
        .bytes()
        .take_while(|b| b.is_ascii_alphabetic())
        .count();
    let prefix = &p0_up[..alpha_len];
    let digits_raw = &p0_up[alpha_len..];
    let model_id = if digits_raw.is_empty() {
        "01".to_owned()
    } else if digits_raw.len() == 1 {
        format!("0{digits_raw}")
    } else {
        digits_raw.to_owned()
    };
    let piece_code = if digits_raw.is_empty() {
        format!("{p0_up}01")
    } else {
        p0_up.clone()
    };

    let slot = if matches!(prefix, "KD" | "BD" | "BA" | "KICK") {
        PieceSlot::Kick
    } else if matches!(prefix, "SN" | "SD" | "SNR" | "SNARE") {
        PieceSlot::Snare
    } else if matches!(prefix, "HH" | "HAT" | "HIHAT") {
        PieceSlot::HiHat
    } else if matches!(prefix, "TM" | "TT" | "TOM" | "FT" | "RT" | "RTOM" | "FTOM") {
        PieceSlot::Tom
    } else if matches!(prefix, "RD" | "RI" | "RIDE") {
        PieceSlot::Ride
    } else if matches!(prefix, "CR" | "CH" | "SP" | "CY" | "CYM" | "ST") {
        PieceSlot::Cymbal
    } else {
        PieceSlot::Percussion
    };

    let is_known_tool = |s: &str| -> bool {
        matches!(
            s.to_ascii_uppercase().as_str(),
            "DS" | "BR" | "RD" | "ML" | "HD" | "FP" | "PP" | "WP" | "NB" | "ST" | "FG"
        )
    };
    let is_digits = |s: &str| -> bool { !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) };
    let parse_hand = |s: &str| -> Option<char> {
        match s.to_ascii_uppercase().as_str() {
            "R" => Some('R'),
            "L" => Some('L'),
            _ => None,
        }
    };

    let (slot_pos, tool_code, artic_code, hand) = match parts.len() {
        0 | 1 => ("01".to_owned(), "ANY".to_owned(), "HIT".to_owned(), None),
        2 => {
            if let Some(h) = parse_hand(parts[1]) {
                ("01".to_owned(), "ANY".to_owned(), "HIT".to_owned(), Some(h))
            } else {
                (
                    "01".to_owned(),
                    "ANY".to_owned(),
                    parts[1].to_ascii_uppercase(),
                    None,
                )
            }
        }
        3 => {
            if let Some(h) = parse_hand(parts[2]) {
                ("01".to_owned(), "ANY".to_owned(), parts[1].to_ascii_uppercase(), Some(h))
            } else if is_digits(parts[1]) {
                (
                    parts[1].to_owned(),
                    "ANY".to_owned(),
                    parts[2].to_ascii_uppercase(),
                    None,
                )
            } else if is_known_tool(parts[1]) {
                (
                    "01".to_owned(),
                    parts[1].to_ascii_uppercase(),
                    parts[2].to_ascii_uppercase(),
                    None,
                )
            } else {
                (
                    parts[1].to_ascii_uppercase(),
                    "ANY".to_owned(),
                    parts[2].to_ascii_uppercase(),
                    None,
                )
            }
        }
        4 => {
            if let Some(h) = parse_hand(parts[3]) {
                if is_digits(parts[1]) {
                    (
                        parts[1].to_owned(),
                        "ANY".to_owned(),
                        parts[2].to_ascii_uppercase(),
                        Some(h),
                    )
                } else {
                    (
                        "01".to_owned(),
                        parts[1].to_ascii_uppercase(),
                        parts[2].to_ascii_uppercase(),
                        Some(h),
                    )
                }
            } else {
                (
                    parts[1].to_owned(),
                    parts[2].to_ascii_uppercase(),
                    parts[3].to_ascii_uppercase(),
                    None,
                )
            }
        }
        _ => {
            let last = parts.last().copied().unwrap_or("");
            let h = parse_hand(last);
            let artic_idx = if h.is_some() {
                parts.len() - 2
            } else {
                3.min(parts.len() - 1)
            };
            (
                parts[1].to_owned(),
                parts[2].to_ascii_uppercase(),
                parts[artic_idx].to_ascii_uppercase(),
                h,
            )
        }
    };

    let is_hand_tool = matches!(tool_code.as_str(), "DS" | "BR" | "RD" | "ML" | "HD");

    // Merge `_L` and `_R` onto the same pad for hand-struck drums so Left/Right alternate naturally,
    // while keeping Kick `_L` (MIDI 35) and `_R` (MIDI 36) distinct for double-bass pedals!
    let pad_merge_key = if slot == PieceSlot::Kick {
        match hand {
            Some(h) => format!("{piece_code}_{slot_pos}_{tool_code}_{artic_code}_{h}"),
            None => format!("{piece_code}_{slot_pos}_{tool_code}_{artic_code}"),
        }
    } else {
        format!("{piece_code}_{slot_pos}_{tool_code}_{artic_code}")
    };

    ParsedGroupCode {
        raw_name: group_name.to_owned(),
        slot,
        piece_code,
        model_id,
        slot_pos,
        tool_code,
        is_hand_tool,
        artic_code,
        hand,
        pad_merge_key,
    }
}

/// Read ONLY the outer `RIFF 'ttbw'` chunk headers and `'oidx'` directory from disk
/// (`~10–120 KB` I/O, `< 1 ms`) without reading or faulting in the multi-hundred-MB
/// `'data'` audio chunk.
pub fn parse_ttbw_file_index(path: &Path) -> Result<TtbwIndex> {
    let mut f = File::open(path)?;
    let file_len = f.metadata()?.len();
    if file_len < 20 {
        return Err(Sd3Error::Format("file too small for RIFF 'ttbw'".into()));
    }

    let mut top = [0u8; 12];
    f.read_exact(&mut top)?;
    if &top[0..4] != b"RIFF" || &top[8..12] != b"ttbw" {
        return Err(Sd3Error::Format(
            "file is not a RIFF 'ttbw' soundbunch".into(),
        ));
    }

    let mut pos = 12u64;
    let mut oidx_bytes: Option<Vec<u8>> = None;
    let mut oidx_offset = 0u64;
    let mut oidx_size = 0u64;
    let mut data_payload_offset = 0u64;
    let mut data_payload_size = 0u64;

    while pos + 8 <= file_len {
        f.seek(SeekFrom::Start(pos))?;
        let mut chdr = [0u8; 8];
        if f.read_exact(&mut chdr).is_err() {
            break;
        }
        let id = &chdr[0..4];
        let sz = u32::from_le_bytes(chdr[4..8].try_into().unwrap()) as u64;
        let payload_pos = pos + 8;

        if id == b"oidx" {
            if sz > 64 * 1024 * 1024 || payload_pos + sz > file_len {
                return Err(Sd3Error::Format(format!(
                    "invalid 'oidx' chunk size: {sz}"
                )));
            }
            let mut buf = vec![0u8; sz as usize];
            f.read_exact(&mut buf)?;
            oidx_offset = pos;
            oidx_size = sz;
            oidx_bytes = Some(buf);
        } else if id == b"data" {
            data_payload_offset = payload_pos;
            data_payload_size = sz.min(file_len.saturating_sub(payload_pos));
            if oidx_bytes.is_some() {
                break;
            }
        }

        let unpadded = payload_pos.saturating_add(sz);
        let padded = unpadded.saturating_add(sz & 1);
        pos = if (sz & 1) == 1 && padded + 8 <= file_len {
            f.seek(SeekFrom::Start(padded))?;
            let mut peek = [0u8; 4];
            if f.read_exact(&mut peek).is_ok()
                && peek.iter().all(|&b| (0x20..=0x7e).contains(&b))
            {
                padded
            } else {
                unpadded
            }
        } else {
            padded
        };
    }

    let oidx = oidx_bytes
        .ok_or_else(|| Sd3Error::Format("RIFF 'ttbw' missing 'oidx' chunk".into()))?;
    let mut idx = parse_oidx_payload_with_data_len(&oidx, Some(data_payload_size))?;
    idx.oidx_offset = oidx_offset;
    idx.oidx_size = oidx_size;
    idx.data_payload_offset = data_payload_offset;
    idx.data_payload_size = data_payload_size;
    Ok(idx)
}

/// Parse an in-memory `RIFF 'ttbw'` slice (such as an `Mmap`).
pub fn parse_ttbw_slice(buf: &[u8]) -> Result<TtbwIndex> {
    if !looks_like_ttbw(buf) {
        return Err(Sd3Error::Format(
            "buffer is not a RIFF 'ttbw' soundbunch".into(),
        ));
    }
    let riff_size = u32::from_le_bytes(buf[4..8].try_into().unwrap()) as usize;
    let limit = 8usize.saturating_add(riff_size).min(buf.len());

    let mut pos = 12usize;
    let mut oidx_slice: Option<(usize, &[u8])> = None;
    let mut data_range: Option<(usize, usize)> = None;

    while pos + 8 <= limit {
        let id = &buf[pos..pos + 4];
        let sz = u32::from_le_bytes(buf[pos + 4..pos + 8].try_into().unwrap()) as usize;
        let payload_start = pos + 8;
        let payload_end = payload_start.saturating_add(sz).min(limit);
        if id == b"oidx" {
            oidx_slice = Some((pos, &buf[payload_start..payload_end]));
        } else if id == b"data" {
            data_range = Some((payload_start, payload_end - payload_start));
        }
        let unpadded = payload_end;
        let padded = payload_end.saturating_add(sz & 1);
        pos = if (sz & 1) == 1 && padded + 4 <= limit {
            if buf[padded..padded + 4]
                .iter()
                .all(|&b| (0x20..=0x7e).contains(&b))
            {
                padded
            } else {
                unpadded
            }
        } else {
            padded
        };
    }

    let (oidx_off, oidx) =
        oidx_slice.ok_or_else(|| Sd3Error::Format("RIFF 'ttbw' missing 'oidx'".into()))?;
    let hint_sz = data_range.map(|(_, d_sz)| d_sz as u64);
    let mut idx = parse_oidx_payload_with_data_len(oidx, hint_sz)?;
    idx.oidx_offset = oidx_off as u64;
    idx.oidx_size = oidx.len() as u64;
    if let Some((d_off, d_sz)) = data_range {
        idx.data_payload_offset = d_off as u64;
        idx.data_payload_size = d_sz as u64;
    }
    Ok(idx)
}

/// Parse a raw `'oidx'` chunk payload (works on both embedded `'oidx'` chunks and
/// standalone `.obw.oidx` files).
pub fn parse_oidx_payload(oidx: &[u8]) -> Result<TtbwIndex> {
    parse_oidx_payload_with_data_len(oidx, None)
}

fn parse_oidx_payload_with_data_len(
    oidx: &[u8],
    data_payload_len_hint: Option<u64>,
) -> Result<TtbwIndex> {
    if oidx.len() < 9 {
        return Err(Sd3Error::Format(format!(
            "'oidx' payload too short ({} bytes)",
            oidx.len()
        )));
    }

    let header_bytes: [u8; 8] = oidx[0..8].try_into().unwrap();
    let num_mics = header_bytes[7] as usize;
    if num_mics == 0 || num_mics > 128 {
        return Err(Sd3Error::Format(format!(
            "invalid 'oidx' mic count: {num_mics}"
        )));
    }

    let mut pos = 8usize;
    let mut mics = Vec::with_capacity(num_mics);
    let mut mic_buses = Vec::with_capacity(num_mics);
    for _ in 0..num_mics {
        let rel_z = oidx[pos..]
            .iter()
            .position(|&b| b == 0)
            .ok_or_else(|| Sd3Error::Format("unterminated mic string in 'oidx'".into()))?;
        let name = String::from_utf8_lossy(&oidx[pos..pos + rel_z]).into_owned();
        mic_buses.push(classify_mic_name(&name));
        mics.push(name);
        pos += rel_z + 1;
    }

    let mut raw_groups: Vec<(String, Vec<(u8, Vec<(u32, char, u8, u32)>)>)> = Vec::new();
    let mut first_stored: Option<u32> = None;
    let mut total_records = 0usize;

    while pos < oidx.len() {
        if oidx[pos] == 0 {
            break;
        }
        let Some(rel_z) = oidx[pos..].iter().position(|&b| b == 0) else {
            break;
        };
        let gname = String::from_utf8_lossy(&oidx[pos..pos + rel_z]).into_owned();
        let glen_pos = pos + rel_z + 1;
        if glen_pos + 4 > oidx.len() {
            break;
        }
        let glen = u32::from_le_bytes(oidx[glen_pos..glen_pos + 4].try_into().unwrap()) as usize;
        let body_start = glen_pos + 4;
        let Some(group_end) = body_start.checked_add(glen) else {
            break;
        };
        if group_end > oidx.len() {
            break;
        }

        let mut p = body_start;
        if p >= group_end {
            pos = group_end;
            continue;
        }
        let gmics = oidx[p] as usize;
        p += 1;

        let mut mic_blocks = Vec::with_capacity(gmics);
        let mut group_valid = true;
        for _ in 0..gmics {
            if p + 2 > group_end {
                group_valid = false;
                break;
            }
            let mic_idx = oidx[p];
            let hit_count = oidx[p + 1] as usize;
            p += 2;
            let Some(bytes_needed) = hit_count.checked_mul(10) else {
                group_valid = false;
                break;
            };
            if p + bytes_needed > group_end {
                group_valid = false;
                break;
            }
            let mut recs = Vec::with_capacity(hit_count);
            for _ in 0..hit_count {
                let stored_off = u32::from_le_bytes(oidx[p..p + 4].try_into().unwrap());
                let vel_b = oidx[p + 4];
                let vel_char = if (0x20..=0x7e).contains(&vel_b) {
                    vel_b as char
                } else {
                    'A'
                };
                let rr_num = oidx[p + 5];
                let byte_size = u32::from_le_bytes(oidx[p + 6..p + 10].try_into().unwrap());
                if first_stored.is_none() {
                    first_stored = Some(stored_off);
                }
                recs.push((stored_off, vel_char, rr_num, byte_size));
                p += 10;
            }
            mic_blocks.push((mic_idx, recs));
        }

        if group_valid && !mic_blocks.is_empty() {
            for (_, recs) in &mic_blocks {
                total_records += recs.len();
            }
            raw_groups.push((gname, mic_blocks));
        }
        pos = group_end;
    }

    let xor_mask = solve_xor_mask(&raw_groups, first_stored, data_payload_len_hint);

    let mut choke_map: BTreeMap<String, u8> = BTreeMap::new();
    let mut next_choke_id = 1u8;

    let mut groups = Vec::with_capacity(raw_groups.len());
    for (gname, mic_blocks) in raw_groups {
        let parsed = parse_group_code(&gname);
        let meta = classify_group_code(&gname, &parsed, &mut choke_map, &mut next_choke_id);
        let mut mics_out = Vec::with_capacity(mic_blocks.len());
        for (mic_idx, recs) in mic_blocks {
            let m_usize = mic_idx as usize;
            let mic_name = mics
                .get(m_usize)
                .cloned()
                .unwrap_or_else(|| format!("Mic{mic_idx}"));
            let bus = mic_buses.get(m_usize).copied().unwrap_or(MicBus::Close);
            let records = recs
                .into_iter()
                .map(|(stored_offset, vel_char, rr_num, byte_size)| OidxSampleRecord {
                    stored_offset,
                    data_offset: (stored_offset ^ xor_mask) as u64,
                    vel_char,
                    rr_num,
                    byte_size,
                })
                .collect();
            mics_out.push(OidxMicHits {
                mic_index: mic_idx,
                mic_name,
                bus,
                records,
            });
        }

        groups.push(OidxGroup {
            name: gname,
            parsed,
            instrument_key: meta.instrument_key,
            instrument_family: meta.family,
            articulation_code: meta.artic_code,
            display_name: meta.display_name,
            preferred_note: meta.preferred_note,
            suggested_note: meta.preferred_note,
            member_choke_group: meta.member_choke,
            triggers_choke_group: meta.triggers_choke,
            is_hihat_open: meta.is_hihat_open,
            hihat_openness: meta.hihat_openness,
            mics: mics_out,
        });
    }

    assign_unique_notes(&mut groups);

    Ok(TtbwIndex {
        header_bytes,
        mics,
        mic_buses,
        xor_mask,
        groups,
        total_records,
        oidx_offset: 0,
        oidx_size: oidx.len() as u64,
        data_payload_offset: 0,
        data_payload_size: 0,
    })
}

fn solve_xor_mask(
    raw_groups: &[(String, Vec<(u8, Vec<(u32, char, u8, u32)>)>)],
    first_stored: Option<u32>,
    data_len_hint: Option<u64>,
) -> u32 {
    let Some(cand0) = first_stored else {
        return 0;
    };
    if cand0 == 0 {
        return 0;
    }

    let fits_hint = |mask: u32| -> bool {
        let Some(dlen) = data_len_hint else {
            return true;
        };
        if dlen == 0 {
            return true;
        }
        raw_groups.iter().all(|(_, mbs)| {
            mbs.iter().all(|(_, recs)| {
                recs.iter()
                    .all(|&(so, _, _, sz)| ((so ^ mask) as u64) + (sz as u64) <= dlen + 4)
            })
        })
    };

    if verify_contiguous_mask(raw_groups, cand0) && fits_hint(cand0) {
        return cand0;
    }
    if verify_contiguous_mask(raw_groups, 0) && fits_hint(0) {
        return 0;
    }

    // Fallback if groups were written in a different order than `oidx`: the first record of
    // whichever group/mic block sits at `data_offset == 0` has `stored_off == xor_mask`.
    for (_, mbs) in raw_groups {
        for (_, recs) in mbs {
            if let Some(&(so, _, _, _)) = recs.first() {
                if fits_hint(so) && verify_sorted_contiguity(raw_groups, so) {
                    return so;
                }
            }
        }
    }

    cand0
}

fn verify_contiguous_mask(
    raw_groups: &[(String, Vec<(u8, Vec<(u32, char, u8, u32)>)>)],
    mask: u32,
) -> bool {
    let mut flat: Vec<(u64, u64)> = Vec::with_capacity(16);
    'outer: for (_, mblocks) in raw_groups {
        for (_, recs) in mblocks {
            for &(so, _, _, sz) in recs {
                flat.push(((so ^ mask) as u64, sz as u64));
                if flat.len() >= 12 {
                    break 'outer;
                }
            }
        }
    }
    if flat.len() < 2 {
        return true;
    }
    let mut exact = 0usize;
    for pair in flat.windows(2) {
        let end = pair[0].0 + pair[0].1;
        let next = pair[1].0;
        if next >= end && next - end <= 4 {
            exact += 1;
        }
    }
    exact * 2 >= flat.len() - 1
}

fn verify_sorted_contiguity(
    raw_groups: &[(String, Vec<(u8, Vec<(u32, char, u8, u32)>)>)],
    mask: u32,
) -> bool {
    let mut intervals: Vec<(u64, u64)> = Vec::new();
    for (_, mbs) in raw_groups {
        for (_, recs) in mbs {
            for &(so, _, _, sz) in recs {
                intervals.push(((so ^ mask) as u64, sz as u64));
            }
        }
    }
    if intervals.len() < 2 {
        return true;
    }
    intervals.sort_unstable_by_key(|&(off, _)| off);
    if intervals[0].0 != 0 {
        return false;
    }
    let check_n = intervals.len().min(24);
    let mut adj = 0usize;
    for pair in intervals[..check_n].windows(2) {
        let end = pair[0].0 + pair[0].1;
        let next = pair[1].0;
        if next >= end && next - end <= 4 {
            adj += 1;
        }
    }
    adj * 2 >= check_n - 1
}

struct GroupMeta {
    instrument_key: String,
    family: String,
    artic_code: String,
    display_name: String,
    preferred_note: u8,
    member_choke: Option<u8>,
    triggers_choke: Option<u8>,
    is_hihat_open: bool,
    hihat_openness: Option<u8>,
}

fn classify_group_code(
    group_name: &str,
    parsed: &ParsedGroupCode,
    choke_map: &mut BTreeMap<String, u8>,
    next_choke_id: &mut u8,
) -> GroupMeta {
    let p0_up = parsed.piece_code.as_str();
    let pos_str = parsed.slot_pos.as_str();
    let pos_num: u8 = pos_str.parse::<u8>().unwrap_or(1);
    let artic_up = parsed.artic_code.clone();
    let instrument_key = format!("{}_{}", p0_up, pos_str);

    let mut get_choke = |key: &str| -> u8 {
        if let Some(&id) = choke_map.get(key) {
            id
        } else {
            let id = *next_choke_id;
            *next_choke_id = next_choke_id.saturating_add(1).max(1);
            choke_map.insert(key.to_owned(), id);
            id
        }
    };

    match parsed.slot {
        PieceSlot::Kick => {
            let (note, side) = if parsed.hand == Some('L') {
                (35, "L")
            } else {
                (36, "R")
            };
            return GroupMeta {
                instrument_key,
                family: "Kick".into(),
                artic_code: artic_up.clone(),
                display_name: format!("Kick {} ({side})", parsed.model_id),
                preferred_note: note,
                member_choke: None,
                triggers_choke: None,
                is_hihat_open: false,
                hihat_openness: None,
            };
        }
        PieceSlot::Snare => {
            let (note, label) = match artic_up.as_str() {
                "RS" | "RM" => (40, "Rimshot"),
                "SS" | "CS" | "XR" => (37, "Sidestick"),
                "RU" | "FL" | "BZ" => (39, "Ruff/Buzz"),
                "RO" => (71, "Rim Only"),
                "ED" | "OFF" => (33, "Edge"),
                _ => (38, "Center"),
            };
            return GroupMeta {
                instrument_key,
                family: "Snare".into(),
                artic_code: artic_up,
                display_name: format!("Snare {} {label}", parsed.model_id),
                preferred_note: note,
                member_choke: None,
                triggers_choke: None,
                is_hihat_open: false,
                hihat_openness: None,
            };
        }
        PieceSlot::HiHat => {
            let cg = get_choke(&instrument_key);
            let (note, label, is_open, openness, trig_choke) = match artic_up.as_str() {
                "PD" | "FT" | "CC" | "CH" | "PED" => {
                    (44, "Pedal", false, Some(0), Some(cg))
                }
                "OP" | "O4" | "O5" | "OP4" | "OP5" | "OE4" | "OE5" | "OT4" | "OT5" => {
                    (46, "Open Full", true, Some(127), None)
                }
                "O3" | "OP3" | "OE3" | "OT3" => (26, "Open 3", true, Some(100), None),
                "O2" | "OP2" | "OE2" | "OT2" => (25, "Open 2", true, Some(72), None),
                "O1" | "OP1" | "OE1" | "OT1" | "LS" | "LO" => {
                    (24, "Open 1", true, Some(45), None)
                }
                "O0" | "OP0" | "OE0" | "OT0" | "HO" => {
                    (60, "Half-Open", true, Some(24), None)
                }
                "SP" | "SPL" => (61, "Foot Splash", true, Some(90), None),
                "ED" | "SH" | "CE" | "TG" => (22, "Closed Edge", false, Some(8), Some(cg)),
                "BL" | "BT" => (62, "Bell", false, Some(12), None),
                _ => (42, "Closed Tip", false, Some(0), Some(cg)),
            };
            return GroupMeta {
                instrument_key,
                family: "Hi-Hat".into(),
                artic_code: artic_up,
                display_name: format!("Hi-Hat {label}"),
                preferred_note: note,
                member_choke: Some(cg),
                triggers_choke: trig_choke,
                is_hihat_open: is_open,
                hihat_openness: openness,
            };
        }
        PieceSlot::Tom => {
            let is_floor = p0_up.starts_with("FT");
            let base_note: u8 = if is_floor {
                match pos_num {
                    1 => 43,
                    _ => 41,
                }
            } else {
                match pos_num {
                    1 => 48,
                    2 => 47,
                    3 => 45,
                    _ => 43,
                }
            };
            let (note, label) = if matches!(artic_up.as_str(), "RS" | "RM" | "RO") {
                (base_note.saturating_add(30).min(119), "Rim")
            } else {
                (base_note, "Center")
            };
            let fam = if is_floor { "Floor Tom" } else { "Rack Tom" };
            return GroupMeta {
                instrument_key,
                family: fam.into(),
                artic_code: artic_up,
                display_name: format!("{fam} {pos_str} {label}"),
                preferred_note: note,
                member_choke: None,
                triggers_choke: None,
                is_hihat_open: false,
                hihat_openness: None,
            };
        }
        PieceSlot::Ride => {
            let cg = get_choke(&instrument_key);
            let is_mute = matches!(artic_up.as_str(), "MT" | "CK" | "CH" | "GR");
            let (note, label) = match artic_up.as_str() {
                "BL" => (53, "Bell"),
                "BT" => (83, "Bell Tip"),
                "CR" | "ED" | "SH" => (59, "Edge/Crash"),
                "MT" | "CK" | "CH" | "GR" => (58, "Mute/Choke"),
                _ => (51, "Bow Tip"),
            };
            return GroupMeta {
                instrument_key,
                family: "Ride".into(),
                artic_code: artic_up,
                display_name: format!("Ride {} {label}", parsed.model_id),
                preferred_note: note,
                member_choke: Some(cg),
                triggers_choke: if is_mute { Some(cg) } else { None },
                is_hihat_open: false,
                hihat_openness: None,
            };
        }
        PieceSlot::Cymbal => {
            let cg = get_choke(&instrument_key);
            let fam = if p0_up.starts_with("CH") {
                "China"
            } else if p0_up.starts_with("SP") {
                "Splash"
            } else if p0_up.starts_with("ST") {
                "Stack"
            } else {
                "Crash"
            };
            let is_mute = matches!(artic_up.as_str(), "MT" | "CK" | "CH" | "GR");
            let base_crash_note: u8 = if fam == "China" {
                52
            } else if fam == "Splash" {
                55
            } else {
                match pos_num {
                    1 | 2 => 49,
                    3 | 4 => 57,
                    5 => 55,
                    _ => 52,
                }
            };
            let (note, label) = match artic_up.as_str() {
                "CR" | "ED" | "SH" | "HT" | "HIT" => (base_crash_note, "Hit"),
                "MT" | "CK" | "CH" | "GR" => (base_crash_note.saturating_add(5), "Mute"),
                "RT" | "BW" => (base_crash_note.saturating_add(16).min(119), "Bow"),
                "BL" => (base_crash_note.saturating_add(18).min(119), "Bell"),
                "BT" => (base_crash_note.saturating_add(20).min(119), "Bell Tip"),
                _ => (base_crash_note, artic_up.as_str()),
            };
            return GroupMeta {
                instrument_key,
                family: fam.into(),
                artic_code: artic_up.clone(),
                display_name: format!("{fam} {pos_str} {label}"),
                preferred_note: note,
                member_choke: Some(cg),
                triggers_choke: if is_mute { Some(cg) } else { None },
                is_hihat_open: false,
                hihat_openness: None,
            };
        }
        PieceSlot::Percussion => {
            let pref = if p0_up.starts_with("CB") || p0_up.starts_with("COW") {
                56
            } else if p0_up.starts_with("TB") || p0_up.starts_with("TM") {
                54
            } else {
                60
            };
            GroupMeta {
                instrument_key,
                family: "Percussion".into(),
                artic_code: artic_up,
                display_name: group_name.to_owned(),
                preferred_note: pref,
                member_choke: None,
                triggers_choke: None,
                is_hihat_open: false,
                hihat_openness: None,
            }
        }
    }
}

/// Ensure every group in an index gets a collision-free playable MIDI note (`24..=108`),
/// prioritizing primary strike articulations (`CR`, `CN`, `CL`, `OP`, `HIT`) on their
/// canonical General MIDI drum notes first!
pub fn assign_unique_notes(groups: &mut [OidxGroup]) {
    let mut used = BTreeSet::new();

    let mut order: Vec<usize> = (0..groups.len()).collect();
    order.sort_by_key(|&i| {
        let a = groups[i].articulation_code.as_str();
        let prio = match a {
            "CR" | "CN" | "CE" | "HIT" | "HT" | "CL" | "OP" => 0u8,
            "RS" | "BL" | "PD" | "RT" | "O1" | "O2" | "O3" => 1,
            "BT" | "SS" | "ED" => 2,
            "MT" | "CK" => 3,
            _ => 4,
        };
        (prio, i)
    });

    let fallback_notes: Vec<u8> = (58u8..=87)
        .chain(24u8..35)
        .chain(35u8..58)
        .chain(88u8..=119)
        .chain(12u8..24)
        .collect();

    for idx in order {
        let pref = groups[idx].suggested_note.clamp(12, 119);
        if used.insert(pref) {
            groups[idx].suggested_note = pref;
        } else if let Some(&free) = fallback_notes.iter().find(|&&n| !used.contains(&n)) {
            used.insert(free);
            groups[idx].suggested_note = free;
        }
    }
}

/// Partition MIDI velocities `1..=127` across sorted Toontrack velocity tier characters
/// (e.g. `['A', 'B', ..., 'M']` -> `A = 1..=9`, ..., `M = 118..=127`).
pub fn compute_velocity_ranges(vel_chars: &[char]) -> BTreeMap<char, (u8, u8)> {
    let mut unique: Vec<char> = vel_chars.to_vec();
    unique.sort_unstable();
    unique.dedup();
    let n = unique.len().max(1);
    let mut out = BTreeMap::new();
    for (i, &ch) in unique.iter().enumerate() {
        let lo = ((i * 127) / n) as u8 + 1;
        let hi = if i + 1 == n {
            127
        } else {
            (((i + 1) * 127) / n) as u8
        };
        out.insert(ch, (lo, hi.max(lo)));
    }
    out
}

/// Input specification for building a synthetic `RIFF 'ttbw'` container (used in round-trip tests).
pub struct TtbwGroupBuildSpec<'a> {
    pub group_name: &'a str,
    /// `(mic_index, vel_char, rr_num, ttpw_riff_bytes)`
    pub samples: Vec<(u8, char, u8, &'a [u8])>,
}

/// Construct a genuine Toontrack-compatible `RIFF 'ttbw'` container (`'oidx'` + `'data'`)
/// with optional XOR offset obfuscation (`xor_mask`).
pub fn build_ttbw_bytes(
    mics: &[&str],
    groups: &[TtbwGroupBuildSpec<'_>],
    xor_mask: u32,
) -> Vec<u8> {
    let mut oidx = Vec::new();
    oidx.extend_from_slice(&[0x06, 0x01, 0x0a, 0x02, 0x01, 0x00, 0x00, mics.len() as u8]);
    for m in mics {
        oidx.extend_from_slice(m.as_bytes());
        oidx.push(0);
    }

    let mut data_payload = Vec::new();

    for grp in groups {
        let mut by_mic: BTreeMap<u8, Vec<(u32, u8, u8, u32)>> = BTreeMap::new();
        for &(mic_idx, vel_ch, rr_num, ttpw_bytes) in &grp.samples {
            if data_payload.len() & 1 == 1 {
                data_payload.push(0);
            }
            let real_off = data_payload.len() as u32;
            let stored_off = real_off ^ xor_mask;
            let sz = ttpw_bytes.len() as u32;
            data_payload.extend_from_slice(ttpw_bytes);
            by_mic
                .entry(mic_idx)
                .or_default()
                .push((stored_off, vel_ch as u8, rr_num, sz));
        }

        let mut gbody = Vec::new();
        gbody.push(by_mic.len() as u8);
        for (mic_idx, recs) in by_mic {
            gbody.push(mic_idx);
            gbody.push(recs.len() as u8);
            for (so, vb, rr, sz) in recs {
                gbody.extend_from_slice(&so.to_le_bytes());
                gbody.push(vb);
                gbody.push(rr);
                gbody.extend_from_slice(&sz.to_le_bytes());
            }
        }

        oidx.extend_from_slice(grp.group_name.as_bytes());
        oidx.push(0);
        oidx.extend_from_slice(&(gbody.len() as u32).to_le_bytes());
        oidx.extend_from_slice(&gbody);
    }

    oidx.push(0);

    let mut children = crate::riff::build_chunk(b"oidx", &oidx);
    children.extend_from_slice(&crate::riff::build_chunk(b"data", &data_payload));
    crate::riff::build_container(b"RIFF", b"ttbw", &children)
}
