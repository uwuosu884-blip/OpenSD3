# Building Your Own Superior Drummer 3 Compatible Plugin with OpenSD3

This guide shows audio plugin developers how to integrate **Superior Drummer 3 (`SL-*`, `SL/<ROOM>`, `EZX*`, `.obw` `'ttbw'`, `'oidx'`, 20-bit `'ttpw'`, `.ttfs`)** soundbanks directly into your own **VST3, CLAP, AudioUnit, LV2, or Standalone** drum sampler **without doing any binary reverse engineering**.

For byte-level container layouts and codec math, see [`SUPERIOR_DRUMMER_3_FORMAT_SPEC.md`](./SUPERIOR_DRUMMER_3_FORMAT_SPEC.md).

---

## 1. Pick Your Language / Framework

| Stack | Drop-in Module in This Repo | Dependencies | What You Get Out of the Box |
|---|---|---|---|
| **Rust (Data & Codecs only)** | `crates/sd3-core` | `memmap2`, `serde`, `serde_json` | Fast `SL-*` / `SL/<ROOM>` / `EZX*` discovery, `RoomCatalog` piece & tool scanner (`< 80 ms`), `ttbw`/`oidx` parser, 20-bit `ttpw` lossless decoder + `O(log nsec)` range seeker, 24-bit `tpc` (`TA_REVIR`) codec, `ttfs` MIDI archive unpacker, SFZ/WAV exporter |
| **Rust (Full Real-Time Engine)** | `crates/sd3-dsp` | `sd3-core`, `crossbeam-channel` | Lock-free 64-voice multi-mic `DrumEngine` (`Close`, `OH`, `Room`, `Bleed`), `Mmap` + `AtomicU32` SPSC background disk streamer, `_R`/`_L` round-robin interleaving, CC4 variable hi-hat openness, cymbal choke groups, offline MIDI renderer |
| **C / C++ (JUCE, iPlug2, Steinberg VST3 SDK, CLAP C)** | [`sdk/c/opensd3.h`](../sdk/c/opensd3.h) | **Zero** (`<stdint.h>`, `<stdlib.h>`, `<string.h>`) | Single-header stb-style C99 / C++17 library: `os3_parse_ttbw_index`, `os3_parse_ttpw_header`, and `O(log nsec)` zero-allocation `os3_decode_ttpw_range_stereo_f32` |
| **Python 3 (Research, Datasets, Batch Tools)** | [`sdk/python/opensd3.py`](../sdk/python/opensd3.py) | **Zero** (Python 3 stdlib only) | `scan_soundbanks`, `parse_ttbw_file_index`, `parse_ttpw_header`, `decode_ttpw_range_f32`, plus CLI inspector/WAV exporter |

---

## 2. Rust Plugin Integration (`nih-plug`, `cpal`, Custom Engine)

### 2.1 Using `sd3-core` Only (Bring Your Own Audio Engine)

Add `sd3-core` to your `Cargo.toml`:
```toml
[dependencies]
sd3-core = { git = "https://github.com/uwuosu884-blip/OpenSD3.git" }
memmap2 = "0.9"
```

Discover soundbanks, build a `RoomCatalog` in `< 80 ms`, and stream arbitrary sample frames from an `Mmap`:
```rust
use std::fs::File;
use memmap2::Mmap;
use sd3_core::library::{self, KitSelection, RoomCatalog};
use sd3_core::ttpw;

fn index_user_folder(folder: &std::path::Path) -> sd3_core::Result<()> {
    // 1. Handles Toontrack root, SL-*, SL-*/SL/<ROOM>, EZX*, or even SL-*/Sounds subfolders:
    let banks = library::scan_soundbanks_fast(folder)?;
    let bank = &banks[0];

    // 2. Reads ONLY the ~10-40 KB 'oidx' headers (never decodes audio up front):
    let catalog = RoomCatalog::scan_bank(bank, |_done, _total, _file| {})?;
    let selection = KitSelection::default_for_catalog(&catalog);
    println!("Loaded room '{}' with default pieces: {:?}", catalog.bank.display_name, selection);

    // 3. Memory-map any soundb*.obw and decode any frame range in O(log nsec):
    let entry = catalog.entries.iter().find(|e| e.ttbw_index.is_some()).unwrap();
    let idx = entry.ttbw_index.as_ref().unwrap();
    let mmap = unsafe { Mmap::map(&File::open(&entry.path)?)? };

    let rec = &idx.groups[0].mics[0].records[0];
    let start = idx.data_payload_offset as usize + rec.data_offset as usize;
    let ttpw_slice = &mmap[start .. start + rec.byte_size as usize];

    let hdr = ttpw::parse_ttpw_header(ttpw_slice)?;
    let mut left = [0.0f32; 512];
    let mut right = [0.0f32; 512];
    let frames = ttpw::decode_ttpw_range_stereo(ttpw_slice, &hdr, 0, 512, &mut left, &mut right)?;
    println!("Decoded {frames}/{} frames @ {} Hz", hdr.total_frames, hdr.sample_rate);
    Ok(())
}
```
Run the complete example anytime with:
```bash
cargo run -p sd3-core --example minimal_plugin_loader -- /path/to/SL-Death
```

### 2.2 Using `sd3-dsp` (Turnkey Real-Time Multi-Mic Engine)

If you want a ready-made real-time engine with 64-voice polyphony, `Mmap` tail streaming, 4-bus mixer (`Close`, `OH`, `Room`, `Bleed`), CC4 hi-hat openness, and choke groups:
```rust
use sd3_core::library::{self, KitSelection, RoomCatalog};
use sd3_dsp::{DrumEngine, LoadedKit, MixerGains};

// On your background loader thread:
let banks = library::scan_soundbanks_fast(path)?;
let catalog = RoomCatalog::scan_bank(&banks[0], |_, _, _| {})?;
let selection = KitSelection::default_for_catalog(&catalog);
let kit = LoadedKit::load_room_selection(&catalog, &selection, |_frac, _msg| {})?;

// On your real-time audio thread (0 locks, 0 allocations, 0 disk I/O):
let mut engine = DrumEngine::new(kit, 48_000.0);
let mixer = MixerGains::default();
engine.note_on(38, 0.92, &mixer, None); // GM Snare hit
engine.process_block(&mut out_left, &mut out_right, &mixer, None);
```

---

## 3. C / C++ Plugin Integration (JUCE, iPlug2, VST3 SDK)

Copy [`sdk/c/opensd3.h`](../sdk/c/opensd3.h) into your C++ plugin source tree. In **one** `.cpp` file:
```cpp
#define OPENSD3_IMPLEMENTATION
#include "opensd3.h"
```

### Example: Indexing a `soundb*.obw` File & Streaming 512-Frame Blocks in C++
```cpp
#include "opensd3.h"
#include <vector>

void streamExample(const uint8_t* obwMmapPtr, size_t obwFileSize) {
    os3_ttbw_index idx{};
    // Only needs the first few MB (or full memory-mapped pointer) to read 'oidx':
    if (os3_parse_ttbw_index(obwMmapPtr, obwFileSize, obwFileSize, &idx) != OS3_OK) {
        return;
    }

    // Grab the first sample in group 0:
    const os3_sample_record& rec = idx.groups[0].records[0];
    const uint8_t* ttpwPtr = obwMmapPtr + idx.data_payload_offset + rec.data_offset;

    os3_ttpw_header hdr{};
    if (os3_parse_ttpw_header(ttpwPtr, rec.byte_size, &hdr) == OS3_OK) {
        float outL[512], outR[512];
        // Random-access seek to ANY frame (e.g. frame 2048) with ZERO heap allocations:
        int64_t decoded = os3_decode_ttpw_range_stereo_f32(
            ttpwPtr, rec.byte_size, &hdr,
            /*start_frame=*/2048, /*frame_count=*/512,
            outL, outR
        );
        (void)decoded;
    }

    os3_free_ttbw_index(&idx);
}
```

Test the C/C++ reference tool directly:
```bash
gcc -std=c99 -O2 sdk/c/example_inspect_and_decode.c -o example_inspect
./example_inspect /path/to/SL-Death/Sounds/soundb12.obw first_hit.wav
```

---

## 4. Seven Rules Every SD3-Compatible Plugin Must Follow

During development and testing of OpenSD3 against real multi-gigabyte Toontrack libraries (`SL-Death`, `SL-SuperiorDrummer3`), we hit and solved seven subtle pitfalls that every third-party plugin author should know up front:

1. **Sort `soundb*.obw` Numerically, Never Lexicographically**:
   Lexical sort places `soundb100.obw` (surround/bleed) ahead of `soundb2.obw` (core close/OH mics). Always sort by the integer in `soundb<N>.obw` (`0, 1, 2, ..., 155`). Also support `soundb0.obw` for `EZX*` expansions.
2. **Never Merge `SL-*/SL/<ROOM_A>` and `SL-*/SL/<ROOM_B>`**:
   Multi-room SDX packs store independent studio recordings under `SL/<ROOM>/Sounds/`. Treat every directory containing a direct `Sounds/*.obw` folder as a separate selectable room.
3. **Never Eagerly Load All Alternative Pieces at Once**:
   A single `SL-*` room often contains 6 kicks (`KD01..06`), 9 snares (`SN01..09`), and 5 hi-hats (`HH01..05`) across 100,000+ `ttpw` streams. Always parse `'oidx'` headers first (`< 80 ms`), let the user select **one active piece per slot** (`KitSelection`), and preload RAM heads **only** for the selected pieces (~1,000–2,500 streams).
4. **Skip Surround-Only `.obw` Files in Stereo Plugins**:
   In `SL-SuperiorDrummer3`, `soundb53.obw .. soundb113.obw` (~80 GB) contain exclusively 11.1 surround/height mics (`Front_*`, `Center`, `Rear_*`, `Wide_*`, `Height_*`). Detect surround-only mic lists and skip mapping them in stereo mode.
5. **Rank Mics Within Each Bus — Never Spill Overflow Mics Into Other Buses**:
   Keep `Close`, `OH`, `Room`, and `Bleed` strictly isolated, and rank mics within each bus (`OH Cond` > `OH Dyn`, `Amb Near`/`Ribbon` > `Amb Far`) so secondary `.obw` files never overwrite your primary studio stereo pair.
6. **Keep `preferred_note` Unmodified Until After Piece Selection**:
   Inside `soundb1.obw`, `KD01` and `KD02` both want GM note `36`. Deduplicate notes **after** filtering to the user's chosen Kick/Snare, or `KD02` will end up on a fallback note (`58`) instead of `36`!
7. **Windows VST3 GUI Focus & Folder Browsing**:
   - Let users browse for **soundbank folders** (`IFileOpenDialog` with `FOS_PICKFOLDERS` on an STA worker thread), not individual `.obw` files, and step upward automatically if they select `SL-*/Sounds` or `SL-*/GraphicsV3`.
   - If you use `nih-plug` + `egui` (`egui-baseview`), enable `egui-baseview/windows_keyboard_workaround` on Windows—otherwise `WM_LBUTTONDOWN` never calls `SetFocus(hwnd)` and text boxes ignore keyboard input in Windows DAWs!
