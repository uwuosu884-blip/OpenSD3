#!/usr/bin/env python3
"""
opensd3.py — Zero-Dependency Python 3 Reference SDK for Superior Drummer 3
(.obw 'ttbw', 'oidx', 20-bit 'ttpw', and SL-* / SL/<ROOM> / EZX* soundbanks)

Part of the OpenSD3 Project (https://github.com/uwuosu884-blip/OpenSD3)
License: MIT OR GPL-3.0-or-later

Quick CLI usage:
    python3 sdk/python/opensd3.py /path/to/SL-Death
    python3 sdk/python/opensd3.py /path/to/soundb12.obw
    python3 sdk/python/opensd3.py /path/to/sample.ttpw --wav out.wav
"""

from __future__ import annotations

import argparse
import os
import re
import struct
import wave
from dataclasses import dataclass, field
from pathlib import Path
from typing import Dict, List, Optional, Tuple


# ---------------------------------------------------------------------------
# Data Structures
# ---------------------------------------------------------------------------

@dataclass
class SampleRecord:
    mic_index: int
    vel_char: str
    rr_num: int
    stored_offset: int
    data_offset: int
    byte_size: int


@dataclass
class OidxGroup:
    name: str
    records: List[SampleRecord] = field(default_factory=list)


@dataclass
class TtbwIndex:
    header_bytes: bytes
    mics: List[str]
    xor_mask: int
    groups: List[OidxGroup]
    total_records: int
    oidx_offset: int = 0
    oidx_size: int = 0
    data_payload_offset: int = 0
    data_payload_size: int = 0


@dataclass
class TtpwSection:
    end_frame: int
    word_offset: int
    bit_width: int


@dataclass
class TtpwHeader:
    version: int
    channels: int
    sample_rate: int
    bits_per_sample: int
    total_frames: int
    words_per_channel: int
    sections: List[TtpwSection]
    data_offset: int
    data_size: int


# ---------------------------------------------------------------------------
# Outer `.obw` (`RIFF 'ttbw'`) & `'oidx'` Parser
# ---------------------------------------------------------------------------

def looks_like_ttbw(buf: bytes) -> bool:
    return len(buf) >= 12 and buf[:4] == b"RIFF" and buf[8:12] == b"ttbw"


def looks_like_ttpw(buf: bytes) -> bool:
    return len(buf) >= 12 and buf[:4] == b"RIFF" and buf[8:12] == b"ttpw"


def _verify_contiguous(groups: List[OidxGroup], mask: int) -> bool:
    expected = 0
    checked = 0
    for grp in groups:
        for rec in grp.records:
            actual = rec.stored_offset ^ mask
            if checked > 0 and actual not in (expected, expected + 1):
                return False
            nxt = actual + rec.byte_size
            expected = nxt + (nxt & 1)
            checked += 1
            if checked >= 32:
                return True
    return True


def parse_oidx_payload(oidx: bytes, data_size_hint: int = 0) -> TtbwIndex:
    if len(oidx) < 10:
        raise ValueError("oidx payload too short")

    header_bytes = oidx[:8]
    num_mics = oidx[7]
    if not (1 <= num_mics <= 64):
        raise ValueError(f"invalid num_mics={num_mics}")

    pos = 8
    mics: List[str] = []
    for _ in range(num_mics):
        nul = oidx.index(0, pos)
        mics.append(oidx[pos:nul].decode("utf-8", errors="replace"))
        pos = nul + 1

    groups: List[OidxGroup] = []
    first_stored: Optional[int] = None
    total_records = 0

    while pos < len(oidx):
        if oidx[pos] == 0:
            break
        nul = oidx.index(0, pos)
        gname = oidx[pos:nul].decode("utf-8", errors="replace")
        (glen,) = struct.unpack_from("<I", oidx, nul + 1)
        pos = nul + 5
        group_end = min(pos + glen, len(oidx))
        if glen == 0:
            continue

        gmics = oidx[pos]
        p = pos + 1
        recs: List[SampleRecord] = []
        for _ in range(gmics):
            if p + 2 > group_end:
                break
            mic_idx = oidx[p]
            hit_cnt = oidx[p + 1]
            p += 2
            avail = (group_end - p) // 10
            for _ in range(min(hit_cnt, avail)):
                so, vc, rr, bsz = struct.unpack_from("<IBBI", oidx, p)
                p += 10
                if first_stored is None:
                    first_stored = so
                vch = chr(vc) if 0x20 <= vc <= 0x7E else "M"
                recs.append(SampleRecord(mic_idx, vch, rr, so, 0, bsz))

        total_records += len(recs)
        groups.append(OidxGroup(gname, recs))
        pos = group_end

    mask = 0
    if first_stored:
        if _verify_contiguous(groups, first_stored) or not _verify_contiguous(groups, 0):
            mask = first_stored

    for grp in groups:
        for rec in grp.records:
            rec.data_offset = rec.stored_offset ^ mask

    return TtbwIndex(
        header_bytes=header_bytes,
        mics=mics,
        xor_mask=mask,
        groups=groups,
        total_records=total_records,
        oidx_size=len(oidx),
        data_payload_size=data_size_hint,
    )


def parse_ttbw_file_index(path: Path | str) -> TtbwIndex:
    p = Path(path)
    file_size = p.stat().st_size
    with p.open("rb") as f:
        head = f.read(min(file_size, 4 * 1024 * 1024))

    if not looks_like_ttbw(head):
        raise ValueError(f"{p} is not a RIFF 'ttbw' soundbunch")

    pos = 12
    oidx_off = oidx_sz = 0
    data_off = data_sz = 0

    while pos + 8 <= len(head):
        cid = head[pos : pos + 4]
        (csz,) = struct.unpack_from("<I", head, pos + 4)
        payload = pos + 8
        if cid == b"oidx":
            if payload + csz > len(head):
                with p.open("rb") as f:
                    f.seek(payload)
                    oidx_bytes = f.read(csz)
            else:
                oidx_bytes = head[payload : payload + csz]
            oidx_off, oidx_sz = payload, csz
        elif cid == b"data":
            data_off = payload
            data_sz = min(csz, max(0, file_size - payload))
            break
        pos = payload + csz + (csz & 1)

    if oidx_off == 0:
        raise ValueError(f"missing 'oidx' chunk in {p}")

    idx = parse_oidx_payload(oidx_bytes, data_sz)
    idx.oidx_offset = oidx_off
    idx.oidx_size = oidx_sz
    idx.data_payload_offset = data_off
    idx.data_payload_size = data_sz
    return idx


# ---------------------------------------------------------------------------
# Inner `'ttpw'` ("ToonTrack Packed Wave") 20-Bit Lossless Codec
# ---------------------------------------------------------------------------

def parse_ttpw_header(ttpw: bytes) -> TtpwHeader:
    if not looks_like_ttpw(ttpw):
        raise ValueError("not a RIFF 'ttpw' stream")

    pos = 12
    fmt_tuple = None
    sections: List[TtpwSection] = []
    data_offset = data_size = 0

    while pos + 8 <= len(ttpw):
        cid = ttpw[pos : pos + 4]
        (csz,) = struct.unpack_from("<I", ttpw, pos + 4)
        payload = pos + 8
        if cid == b"fmt ":
            ver, ch, sr, bps, nsec, nframes, wpc = struct.unpack_from("<HHIHHII", ttpw, payload)
            fmt_tuple = (ver, ch, sr, bps, nframes, wpc)
            for s in range(nsec):
                ef, wo, bw = struct.unpack_from("<III", ttpw, payload + 20 + s * 12)
                sections.append(TtpwSection(ef, wo, bw))
        elif cid == b"data":
            data_offset = payload
            data_size = min(csz, len(ttpw) - payload)
            break
        pos = payload + csz + (csz & 1)

    if fmt_tuple is None or data_offset == 0:
        raise ValueError("incomplete 'ttpw' header")

    ver, ch, sr, bps, nframes, wpc = fmt_tuple
    return TtpwHeader(ver, ch, sr, bps, nframes, wpc, sections, data_offset, data_size)


def decode_ttpw_range_f32(
    ttpw: bytes,
    hdr: Optional[TtpwHeader] = None,
    start_frame: int = 0,
    frame_count: Optional[int] = None,
) -> List[List[float]]:
    """
    Bit-exact random-access `ttpw` decoder (`u32_grid[words_per_channel][channels]`, MSB-first).
    Returns `channels[ch][frame]` normalized to `[-1.0, +1.0)`.
    """
    if hdr is None:
        hdr = parse_ttpw_header(ttpw)
    total = hdr.total_frames
    if start_frame >= total:
        return [[] for _ in range(hdr.channels)]
    count = (total - start_frame) if frame_count is None else min(frame_count, total - start_frame)

    inv_scale = 1.0 / float(1 << (hdr.bits_per_sample - 1))
    nch = hdr.channels
    wpc = hdr.words_per_channel
    base = hdr.data_offset
    out: List[List[float]] = [[] for _ in range(nch)]

    sec_idx = 0
    while sec_idx < len(hdr.sections) and hdr.sections[sec_idx].end_frame <= start_frame:
        sec_idx += 1

    cur_frame = start_frame
    remaining = count

    while remaining > 0 and sec_idx < len(hdr.sections):
        sec = hdr.sections[sec_idx]
        sec_start = 0 if sec_idx == 0 else hdr.sections[sec_idx - 1].end_frame
        sec_end = min(sec.end_frame, total)
        if sec_end <= cur_frame:
            sec_idx += 1
            continue

        chunk = min(sec_end - cur_frame, remaining)
        b = sec.bit_width

        if b == 0:
            for ch in range(nch):
                out[ch].extend([0.0] * chunk)
        else:
            rel = cur_frame - sec_start
            bit_skip = rel * b
            start_wi = sec.word_offset + (bit_skip // 32)
            rem_bits = bit_skip % 32
            mask_b = (1 << b) - 1
            sign_bit = 1 << (b - 1)

            for ch in range(nch):
                wi = start_wi
                bitbuf = 0
                nbits = 0

                if rem_bits > 0:
                    (w,) = struct.unpack_from("<I", ttpw, base + (wi * nch + ch) * 4)
                    wi += 1
                    bitbuf = w & ((1 << (32 - rem_bits)) - 1)
                    nbits = 32 - rem_bits

                ch_out = out[ch]
                for _ in range(chunk):
                    while nbits < b:
                        if wi < wpc:
                            (w,) = struct.unpack_from("<I", ttpw, base + (wi * nch + ch) * 4)
                            wi += 1
                        else:
                            w = 0
                        bitbuf = (bitbuf << 32) | w
                        nbits += 32
                    nbits -= b
                    raw = (bitbuf >> nbits) & mask_b
                    if raw & sign_bit:
                        raw -= 1 << b
                    ch_out.append(raw * inv_scale)

        cur_frame += chunk
        remaining -= chunk
        sec_idx += 1

    return out


# ---------------------------------------------------------------------------
# Fast Soundbank Directory Discovery (`SL-*`, `SL/<ROOM>`, `EZX*`)
# ---------------------------------------------------------------------------

def _soundbunch_num(p: Path) -> Tuple[int, str]:
    m = re.search(r"(\d+)", p.stem)
    return (int(m.group(1)) if m else 999999, p.name.lower())


def scan_soundbanks(root: Path | str) -> List[Tuple[str, Path, List[Path]]]:
    """Return `(display_name, room_dir, sorted_obw_files)` for all soundbanks under `root`."""
    r = Path(root)
    if r.name.lower() in ("sounds", "graphicsv3", "graphics", "kitlayout") and r.parent.exists():
        r = r.parent

    found: List[Tuple[str, Path, List[Path]]] = []
    for cur, dirs, _ in os.walk(r):
        cur_p = Path(cur)
        sounds = cur_p / "Sounds"
        if sounds.is_dir():
            obws = sorted(sounds.glob("*.obw"), key=_soundbunch_num)
            if obws:
                name_file = cur_p / "Name"
                disp = cur_p.name
                if name_file.is_file():
                    raw = name_file.read_bytes()[:512]
                    txt = raw.decode("utf-16-le", errors="ignore") if b"\x00" in raw[:8] else raw.decode("utf-8", errors="ignore")
                    first = txt.strip().splitlines()[0].strip() if txt.strip() else ""
                    if first:
                        disp = first
                found.append((disp, cur_p, obws))
        dirs[:] = [d for d in dirs if d.lower() not in ("sounds", "graphicsv3", "graphics", "midi")]
    return found


# ---------------------------------------------------------------------------
# CLI Entrypoint
# ---------------------------------------------------------------------------

def _write_wav24(path: Path, sr: int, channels: List[List[float]]) -> None:
    nch = len(channels)
    nframes = len(channels[0]) if nch else 0
    buf = bytearray(nframes * nch * 3)
    o = 0
    for i in range(nframes):
        for ch in range(nch):
            v = max(-1.0, min(0.99999988, channels[ch][i]))
            iv = int(round(v * 8388607.0))
            if iv < 0:
                iv += 1 << 24
            buf[o] = iv & 0xFF
            buf[o + 1] = (iv >> 8) & 0xFF
            buf[o + 2] = (iv >> 16) & 0xFF
            o += 3
    with wave.open(str(path), "wb") as wf:
        wf.setnchannels(nch)
        wf.setsampwidth(3)
        wf.setframerate(sr)
        wf.writeframes(bytes(buf))


def main() -> None:
    ap = argparse.ArgumentParser(description="OpenSD3 Python Reference Inspector & Decoder")
    ap.add_argument("target", type=Path, help="SL-* folder, soundb*.obw, *.oidx, or *.ttpw")
    ap.add_argument("--wav", type=Path, default=None, help="Decode sample (or first .obw hit) to 24-bit WAV")
    args = ap.parse_args()

    t: Path = args.target
    if t.is_dir():
        banks = scan_soundbanks(t)
        print(f"Discovered {len(banks)} soundbank(s) under {t}:")
        for disp, rdir, obws in banks:
            total_gb = sum(f.stat().st_size for f in obws) / (1024 ** 3)
            idx0 = parse_ttbw_file_index(obws[0])
            print(f"  - {disp} ({len(obws)} .obw, {total_gb:.2f} GB) @ {rdir}")
            print(f"      {obws[0].name}: mics={idx0.mics}, groups={len(idx0.groups)}, samples={idx0.total_records}")
        return

    raw_head = t.open("rb").read(4 * 1024 * 1024)
    if looks_like_ttpw(raw_head):
        full = t.read_bytes()
        hdr = parse_ttpw_header(full)
        print(f"{t.name}: {hdr.channels} ch, {hdr.sample_rate} Hz, {hdr.bits_per_sample}-bit, {hdr.total_frames} frames, {len(hdr.sections)} sections")
        if args.wav:
            ch = decode_ttpw_range_f32(full, hdr)
            _write_wav24(args.wav, hdr.sample_rate, ch)
            print(f"Wrote 24-bit WAV -> {args.wav}")
    elif looks_like_ttbw(raw_head):
        idx = parse_ttbw_file_index(t)
        print(f"{t.name}: mics={idx.mics} | groups={len(idx.groups)} | samples={idx.total_records} | xor_mask=0x{idx.xor_mask:08x}")
        for g in idx.groups[:20]:
            print(f"  {g.name:<24} ({len(g.records)} samples)")
        if args.wav and idx.groups and idx.groups[0].records:
            rec = idx.groups[0].records[0]
            with t.open("rb") as f:
                f.seek(idx.data_payload_offset + rec.data_offset)
                slice_bytes = f.read(rec.byte_size)
            hdr = parse_ttpw_header(slice_bytes)
            ch = decode_ttpw_range_f32(slice_bytes, hdr)
            _write_wav24(args.wav, hdr.sample_rate, ch)
            print(f"Decoded {idx.groups[0].name} ({rec.vel_char}{rec.rr_num:02d}) -> {args.wav}")
    else:
        idx = parse_oidx_payload(t.read_bytes())
        print(f"{t.name} (oidx): mics={idx.mics} | groups={len(idx.groups)} | samples={idx.total_records} | xor_mask=0x{idx.xor_mask:08x}")


if __name__ == "__main__":
    main()
