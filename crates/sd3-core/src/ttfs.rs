//! Reader, writer, and MIDI groove inspector for Toontrack `ttfs` containers.
//!
//! Used by Toontrack MIDI groove packs, EZdrummer/SD3 MIDI databases, and
//! resource bundles.
//!
//! Binary layout (little-endian):
//! ```text
//! 0x00  b"ttfs"
//! 0x04  u16 version (typically 1)
//! 0x06  u16 root_dir_offset_minus_0x10 (or u32 at 0x06 when high word is 0)
//! 0x10  ASCII format tag (e.g. "MIDI", "SD3", "EMX3") + signature/padding up to root
//! root  u32 entry_count, followed by entries:
//!         u32 offset, u32 size, NUL-terminated UTF-8/Latin-1 name
//!       - size == 0 && offset != 0xFFFF_FFFF => subdirectory table at `offset`
//!       - offset == 0xFFFF_FFFF => empty 0-byte file
//!       - otherwise => raw file bytes at `offset..offset + size`
//! ```

use std::collections::{BTreeMap, HashSet};
use std::fs::{self, File};
use std::path::{Path, PathBuf};

use memmap2::Mmap;
use serde::{Deserialize, Serialize};

use crate::{Result, Sd3Error};

const MAX_DIR_ENTRIES: u32 = 200_000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TtfsEntry {
    pub path: String,
    pub offset: u64,
    pub size: u64,
    pub is_dir: bool,
    pub midi_info: Option<MidiSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MidiSummary {
    pub format: u16,
    pub tracks: u16,
    pub ppq: u16,
    pub bpm: Option<f64>,
    pub time_signature: Option<String>,
}

pub struct TtfsArchive {
    path: PathBuf,
    map: Mmap,
    format_tag: String,
    entries: Vec<TtfsEntry>,
}

impl TtfsArchive {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let file = File::open(&path)?;
        let map = unsafe { Mmap::map(&file)? };
        if map.len() < 0x14 || &map[0..4] != b"ttfs" {
            return Err(Sd3Error::Format(format!(
                "{} is not a ttfs archive",
                path.display()
            )));
        }
        let rel_root = u32::from_le_bytes([map[6], map[7], map[8], map[9]]) as usize;
        let root = if rel_root + 0x10 < map.len() {
            rel_root + 0x10
        } else {
            (u16::from_le_bytes([map[6], map[7]]) as usize) + 0x10
        };
        let tag_end = map[0x10..map.len().min(0x20)]
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(0);
        let format_tag = String::from_utf8_lossy(&map[0x10..0x10 + tag_end]).into_owned();

        let mut entries = Vec::new();
        let mut visited = HashSet::new();
        walk_dir(&map, root, "", &mut entries, &mut visited, 0)?;
        Ok(Self {
            path,
            map,
            format_tag,
            entries,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn format_tag(&self) -> &str {
        &self.format_tag
    }

    pub fn entries(&self) -> &[TtfsEntry] {
        &self.entries
    }

    pub fn read_entry(&self, entry: &TtfsEntry) -> &[u8] {
        if entry.is_dir || entry.offset == u32::MAX as u64 || entry.size == 0 {
            return &[];
        }
        let s = (entry.offset as usize).min(self.map.len());
        let e = s.saturating_add(entry.size as usize).min(self.map.len());
        &self.map[s..e]
    }

    /// Extract all files into `dest_dir`, sanitizing path components against traversal.
    pub fn extract_all(&self, dest_dir: impl AsRef<Path>) -> Result<usize> {
        let base = dest_dir.as_ref();
        fs::create_dir_all(base)?;
        let mut count = 0usize;
        for entry in &self.entries {
            let rel = sanitize_rel_path(&entry.path);
            if rel.as_os_str().is_empty() {
                continue;
            }
            let out_path = base.join(rel);
            if entry.is_dir {
                fs::create_dir_all(&out_path)?;
            } else {
                if let Some(p) = out_path.parent() {
                    fs::create_dir_all(p)?;
                }
                fs::write(&out_path, self.read_entry(entry))?;
                count += 1;
            }
        }
        Ok(count)
    }
}

fn sanitize_rel_path(p: &str) -> PathBuf {
    let mut out = PathBuf::new();
    for seg in p.split('/').filter(|s| !s.is_empty() && *s != "." && *s != "..") {
        let clean: String = seg
            .chars()
            .map(|c| if c == '\\' || c == ':' { '_' } else { c })
            .collect();
        out.push(clean);
    }
    out
}

fn read_u32(buf: &[u8], at: usize) -> Result<u32> {
    buf.get(at..at + 4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .ok_or_else(|| Sd3Error::Format("truncated ttfs directory table".into()))
}

fn walk_dir(
    buf: &[u8],
    table: usize,
    prefix: &str,
    out: &mut Vec<TtfsEntry>,
    visited: &mut HashSet<usize>,
    depth: usize,
) -> Result<()> {
    if depth > 32 || !visited.insert(table) {
        return Ok(());
    }
    let count = read_u32(buf, table)?;
    if count > MAX_DIR_ENTRIES {
        return Err(Sd3Error::Format(format!(
            "implausible ttfs directory entry count {count}"
        )));
    }
    let mut p = table + 4;
    for _ in 0..count {
        let offset = read_u32(buf, p)?;
        let size = read_u32(buf, p + 4)?;
        p += 8;
        let nul = buf
            .get(p..)
            .and_then(|s| s.iter().position(|&b| b == 0))
            .ok_or_else(|| Sd3Error::Format("unterminated ttfs entry name".into()))?;
        let name = match std::str::from_utf8(&buf[p..p + nul]) {
            Ok(s) => s.to_owned(),
            Err(_) => buf[p..p + nul].iter().map(|&b| b as char).collect(),
        };
        p += nul + 1;
        let full_path = format!("{prefix}/{name}");
        let is_dir = size == 0
            && offset != u32::MAX
            && read_u32(buf, offset as usize)
                .map(|n| n <= MAX_DIR_ENTRIES && (offset as usize) + 4 <= buf.len())
                .unwrap_or(false);

        let midi_info = if !is_dir && size >= 14 && (offset as usize) + (size as usize) <= buf.len()
        {
            let slice = &buf[offset as usize..(offset as usize) + (size as usize)];
            inspect_midi_bytes(slice)
        } else {
            None
        };

        out.push(TtfsEntry {
            path: full_path.clone(),
            offset: offset as u64,
            size: size as u64,
            is_dir,
            midi_info,
        });
        if is_dir {
            walk_dir(buf, offset as usize, &full_path, out, visited, depth + 1)?;
        }
    }
    Ok(())
}

/// Lightweight Standard MIDI File (`MThd`) header + tempo/time-sig inspector.
pub fn inspect_midi_bytes(data: &[u8]) -> Option<MidiSummary> {
    if data.len() < 14 || &data[0..4] != b"MThd" {
        return None;
    }
    let format = u16::from_be_bytes([data[8], data[9]]);
    let tracks = u16::from_be_bytes([data[10], data[11]]);
    let ppq = u16::from_be_bytes([data[12], data[13]]);

    // Scan first 4 KB for FF 51 03 (Set Tempo) and FF 58 04 (Time Signature)
    let scan = &data[14..data.len().min(4096)];
    let mut bpm = None;
    let mut time_signature = None;
    for i in 0..scan.len().saturating_sub(6) {
        if bpm.is_none() && scan[i] == 0xFF && scan[i + 1] == 0x51 && scan[i + 2] == 0x03 {
            let us_per_qn =
                ((scan[i + 3] as u32) << 16) | ((scan[i + 4] as u32) << 8) | (scan[i + 5] as u32);
            if us_per_qn > 0 {
                bpm = Some((60_000_000.0 / us_per_qn as f64 * 100.0).round() / 100.0);
            }
        }
        if time_signature.is_none()
            && scan[i] == 0xFF
            && scan[i + 1] == 0x58
            && scan[i + 2] == 0x04
            && scan[i + 4] < 8
        {
            let num = scan[i + 3];
            let den = 1u32 << scan[i + 4];
            time_signature = Some(format!("{num}/{den}"));
        }
    }

    Some(MidiSummary {
        format,
        tracks,
        ppq,
        bpm,
        time_signature,
    })
}

/// Build a valid `ttfs` container from `(virtual_path, bytes)` pairs.
pub fn build_ttfs(format_tag: &str, files: &[(&str, &[u8])]) -> Result<Vec<u8>> {
    #[derive(Default)]
    struct DirNode {
        subdirs: BTreeMap<String, DirNode>,
        files: BTreeMap<String, Vec<u8>>,
    }

    let mut root = DirNode::default();
    for &(vpath, data) in files {
        let parts: Vec<&str> = vpath.split('/').filter(|s| !s.is_empty()).collect();
        if parts.is_empty() {
            continue;
        }
        let mut cur = &mut root;
        for &dir_name in &parts[..parts.len() - 1] {
            cur = cur.subdirs.entry(dir_name.to_owned()).or_default();
        }
        cur.files.insert(parts[parts.len() - 1].to_owned(), data.to_vec());
    }

    let mut out = vec![0u8; 0x20];
    out[0..4].copy_from_slice(b"ttfs");
    out[4..6].copy_from_slice(&1u16.to_le_bytes());
    let tag_bytes = format_tag.as_bytes();
    let tag_len = tag_bytes.len().min(15);
    out[0x10..0x10 + tag_len].copy_from_slice(&tag_bytes[..tag_len]);

    fn emit_node(node: &DirNode, out: &mut Vec<u8>) -> usize {
        // First emit child payloads and child directory tables so we know their exact offsets.
        let mut child_dirs: Vec<(&str, usize)> = Vec::new();
        for (name, sub) in &node.subdirs {
            let sub_off = emit_node(sub, out);
            child_dirs.push((name.as_str(), sub_off));
        }
        let mut child_files: Vec<(&str, u32, u32)> = Vec::new();
        for (name, payload) in &node.files {
            if payload.is_empty() {
                child_files.push((name.as_str(), u32::MAX, 0));
            } else {
                let off = out.len() as u32;
                out.extend_from_slice(payload);
                child_files.push((name.as_str(), off, payload.len() as u32));
            }
        }
        let table_off = out.len();
        let total = (child_dirs.len() + child_files.len()) as u32;
        out.extend_from_slice(&total.to_le_bytes());
        for (name, sub_off) in child_dirs {
            out.extend_from_slice(&(sub_off as u32).to_le_bytes());
            out.extend_from_slice(&0u32.to_le_bytes());
            out.extend_from_slice(name.as_bytes());
            out.push(0);
        }
        for (name, off, sz) in child_files {
            out.extend_from_slice(&off.to_le_bytes());
            out.extend_from_slice(&sz.to_le_bytes());
            out.extend_from_slice(name.as_bytes());
            out.push(0);
        }
        table_off
    }

    let root_off = emit_node(&root, &mut out);
    let rel = (root_off - 0x10) as u32;
    out[6..10].copy_from_slice(&rel.to_le_bytes());
    Ok(out)
}
