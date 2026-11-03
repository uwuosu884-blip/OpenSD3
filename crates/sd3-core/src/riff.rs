//! Generic RIFF / RIFX / nested-LIST chunk walker and builder.
//!
//! `.obw` soundbunches and standard `.wav` files use RIFF-style chunk framing
//! (`[4CC][u32 size][payload...]`). Because real-world proprietary RIFF
//! variants occasionally omit the 1-byte even-alignment pad or nest custom
//! container chunks, this parser handles standard word-aligned RIFF as well as
//! safe fallback probing over memory-mapped multi-gigabyte files.

use serde::{Deserialize, Serialize};

use crate::{Result, Sd3Error};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RiffChunk {
    pub id: String,
    pub id_hex: String,
    /// Form/list subtype FourCC for `RIFF`, `RIFX`, or `LIST` chunks.
    pub form_type: Option<String>,
    /// Byte offset of the 8-byte chunk header in the file/buffer.
    pub header_offset: u64,
    /// Byte offset where the chunk payload starts (after form_type if container).
    pub data_offset: u64,
    /// Payload byte length (excluding form_type if container).
    pub data_size: u64,
    /// Nested child chunks if this chunk is a container (`RIFF`, `RIFX`, `LIST`, `OBWC`).
    pub children: Vec<RiffChunk>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Endian {
    Little,
    Big,
}

pub fn fourcc_to_string(b: &[u8; 4]) -> String {
    b.iter()
        .map(|&c| {
            if (0x20..=0x7e).contains(&c) {
                c as char
            } else {
                '.'
            }
        })
        .collect()
}

pub fn fourcc_hex(b: &[u8; 4]) -> String {
    format!("{:02x}{:02x}{:02x}{:02x}", b[0], b[1], b[2], b[3])
}

pub fn is_printable_fourcc(b: &[u8; 4]) -> bool {
    b.iter().all(|&c| (0x20..=0x7e).contains(&c))
}

/// Parse a top-level RIFF/RIFX buffer (or a chunk sequence starting at offset 0).
pub fn parse_riff_tree(buf: &[u8], max_depth: usize) -> Result<RiffChunk> {
    if buf.len() < 12 {
        return Err(Sd3Error::Format("file smaller than 12-byte RIFF header".into()));
    }
    let id: [u8; 4] = buf[0..4].try_into().unwrap();
    let endian = match &id {
        b"RIFF" => Endian::Little,
        b"RIFX" => Endian::Big,
        _ => {
            return Err(Sd3Error::Format(format!(
                "missing RIFF/RIFX magic (got '{}' / 0x{})",
                fourcc_to_string(&id),
                fourcc_hex(&id)
            )))
        }
    };
    let raw_size = read_u32(buf, 4, endian) as usize;
    let form: [u8; 4] = buf[8..12].try_into().unwrap();
    let end = (8usize.saturating_add(raw_size)).min(buf.len());
    let children = if max_depth > 0 && end > 12 {
        parse_children(buf, 12, end, endian, max_depth - 1)
    } else {
        Vec::new()
    };
    Ok(RiffChunk {
        id: fourcc_to_string(&id),
        id_hex: fourcc_hex(&id),
        form_type: Some(fourcc_to_string(&form)),
        header_offset: 0,
        data_offset: 12,
        data_size: end.saturating_sub(12) as u64,
        children,
    })
}

fn parse_children(
    buf: &[u8],
    mut pos: usize,
    end: usize,
    endian: Endian,
    depth_left: usize,
) -> Vec<RiffChunk> {
    let mut out = Vec::new();
    let limit = end.min(buf.len());
    while pos + 8 <= limit && out.len() < 100_000 {
        let id: [u8; 4] = buf[pos..pos + 4].try_into().unwrap();
        if !is_printable_fourcc(&id) {
            break;
        }
        let sz = read_u32(buf, pos + 4, endian) as usize;
        let payload_start = pos + 8;
        let payload_end = payload_start.saturating_add(sz).min(limit);
        let is_container = matches!(&id, b"LIST" | b"RIFF" | b"RIFX" | b"OBWC");
        if is_container && payload_end >= payload_start + 4 {
            let sub_form: [u8; 4] = buf[payload_start..payload_start + 4].try_into().unwrap();
            let children = if depth_left > 0 && is_printable_fourcc(&sub_form) {
                parse_children(buf, payload_start + 4, payload_end, endian, depth_left - 1)
            } else {
                Vec::new()
            };
            out.push(RiffChunk {
                id: fourcc_to_string(&id),
                id_hex: fourcc_hex(&id),
                form_type: Some(fourcc_to_string(&sub_form)),
                header_offset: pos as u64,
                data_offset: (payload_start + 4) as u64,
                data_size: payload_end.saturating_sub(payload_start + 4) as u64,
                children,
            });
        } else {
            out.push(RiffChunk {
                id: fourcc_to_string(&id),
                id_hex: fourcc_hex(&id),
                form_type: None,
                header_offset: pos as u64,
                data_offset: payload_start as u64,
                data_size: payload_end.saturating_sub(payload_start) as u64,
                children: Vec::new(),
            });
        }

        // Standard RIFF pads odd-sized chunks with 1 zero byte, but tolerate
        // unpadded custom writers by checking whether the next FourCC aligns.
        let unpadded_next = payload_end;
        let padded_next = payload_end.saturating_add(sz & 1);
        pos = if (sz & 1) == 1 && padded_next + 8 <= limit {
            let cand_pad: [u8; 4] = buf[padded_next..padded_next + 4].try_into().unwrap();
            let cand_unpad: [u8; 4] = if unpadded_next + 4 <= limit {
                buf[unpadded_next..unpadded_next + 4].try_into().unwrap()
            } else {
                [0; 4]
            };
            if is_printable_fourcc(&cand_pad) || !is_printable_fourcc(&cand_unpad) {
                padded_next
            } else {
                unpadded_next
            }
        } else {
            padded_next
        };
    }
    out
}

fn read_u32(buf: &[u8], at: usize, endian: Endian) -> u32 {
    let b: [u8; 4] = buf[at..at + 4].try_into().unwrap();
    match endian {
        Endian::Little => u32::from_le_bytes(b),
        Endian::Big => u32::from_be_bytes(b),
    }
}

/// Build a little-endian RIFF leaf chunk `[id][u32 size][payload][pad]`.
pub fn build_chunk(id: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + payload.len() + (payload.len() & 1));
    out.extend_from_slice(id);
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(payload);
    if payload.len() & 1 == 1 {
        out.push(0);
    }
    out
}

/// Build a `RIFF` or `LIST` container chunk `[kind][u32 size][form][children...]`.
pub fn build_container(kind: &[u8; 4], form: &[u8; 4], children_bytes: &[u8]) -> Vec<u8> {
    let total = 4 + children_bytes.len();
    let mut out = Vec::with_capacity(8 + total + (total & 1));
    out.extend_from_slice(kind);
    out.extend_from_slice(&(total as u32).to_le_bytes());
    out.extend_from_slice(form);
    out.extend_from_slice(children_bytes);
    if total & 1 == 1 {
        out.push(0);
    }
    out
}
