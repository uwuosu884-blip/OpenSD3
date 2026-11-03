#!/usr/bin/env python3
"""
sd3_diagnostic_bundle.py — Minimal Clean-Room `.obw` Diagnostic Extractor

Extracts ONLY the tiny `'oidx'` directory chunk (~10-40 KB) plus at most 2 small
`RIFF 'ttpw'` slices from an `SL-*` soundbank into a compact `.zip` (< 500 KB) so
developers can inspect new Toontrack expansions without copying gigabytes of data.

Usage:
    python3 tools/forensics/sd3_diagnostic_bundle.py /path/to/SL-SomePack -o bundle.zip
"""

from __future__ import annotations

import argparse
import json
import struct
import zipfile
from pathlib import Path


def parse_oidx_first_hits(oidx: bytes):
    num_mics = oidx[7]
    pos = 8
    mics = []
    for _ in range(num_mics):
        nul = oidx.index(0, pos)
        mics.append(oidx[pos:nul].decode("utf-8", "replace"))
        pos = nul + 1

    groups = []
    first_stored = None
    while pos < len(oidx) and oidx[pos] != 0:
        nul = oidx.index(0, pos)
        gname = oidx[pos:nul].decode("utf-8", "replace")
        (glen,) = struct.unpack_from("<I", oidx, nul + 1)
        pos = nul + 5
        gend = min(pos + glen, len(oidx))
        if glen == 0:
            continue
        gmics = oidx[pos]
        p = pos + 1
        hits = []
        for _ in range(gmics):
            if p + 2 > gend:
                break
            mi, cnt = oidx[p], oidx[p + 1]
            p += 2
            for _ in range(min(cnt, (gend - p) // 10)):
                so, vc, rr, sz = struct.unpack_from("<IBBI", oidx, p)
                p += 10
                if first_stored is None:
                    first_stored = so
                hits.append((mi, so, chr(vc) if 32 <= vc <= 126 else "M", rr, sz))
        groups.append((gname, hits))
        pos = gend
    return mics, first_stored or 0, groups


def main() -> None:
    ap = argparse.ArgumentParser(description="Create a tiny <500 KB .obw diagnostic zip")
    ap.add_argument("sl_dir", type=Path, help="Path to SL-* directory or Sounds/ directory")
    ap.add_argument("-o", "--out", type=Path, default=Path("sd3_diagnostic_bundle.zip"))
    args = ap.parse_args()

    sounds = args.sl_dir / "Sounds" if (args.sl_dir / "Sounds").is_dir() else args.sl_dir
    obws = sorted(sounds.glob("*.obw"))
    if not obws:
        raise SystemExit(f"No .obw files found under {sounds}")

    manifest = {"source_folder": args.sl_dir.name, "files": []}

    with zipfile.ZipFile(args.out, "w", compression=zipfile.ZIP_DEFLATED) as zf:
        for idx_i, obw in enumerate(obws[:3]):
            with obw.open("rb") as f:
                hdr = f.read(12)
                if hdr[:4] != b"RIFF" or hdr[8:12] != b"ttbw":
                    continue
                cid, oidx_sz = struct.unpack("<4sI", f.read(8))
                if cid != b"oidx":
                    continue
                oidx = f.read(oidx_sz)
                if oidx_sz & 1:
                    f.read(1)
                dcid, data_sz = struct.unpack("<4sI", f.read(8))
                data_base = f.tell()

                zf.writestr(f"oidx/{obw.name}.oidx", oidx)
                mics, mask, groups = parse_oidx_first_hits(oidx)
                manifest["files"].append({
                    "obw": obw.name,
                    "size_bytes": obw.stat().st_size,
                    "mics": mics,
                    "xor_mask_hex": f"0x{mask:08x}",
                    "group_count": len(groups),
                })

                if idx_i == 0 and groups:
                    gname, hits = groups[0]
                    for mi, so, vc, rr, sz in hits[:2]:
                        if sz <= 262_144:
                            f.seek(data_base + (so ^ mask))
                            zf.writestr(f"ttpw/{obw.name}__{gname}_m{mi}_{vc}{rr:02d}.ttpw", f.read(sz))

        zf.writestr("manifest.json", json.dumps(manifest, indent=2))

    print(f"Wrote diagnostic bundle -> {args.out} ({args.out.stat().st_size:,} bytes)")


if __name__ == "__main__":
    main()
