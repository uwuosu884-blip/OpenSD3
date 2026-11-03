# OpenSD3 — Open-Source Superior Drummer 3 Format Spec, Multi-Language Plugin SDK, Extractor & VST3/CLAP Workstation

**OpenSD3** is an open-source clean-room format specification, multi-language developer SDK (**Rust**, **C99/C++17**, **Python 3**), lossless sample/groove CLI toolkit (`sd3tool`), and native **Windows 10/11 & Linux VST3 / CLAP / Standalone** drum workstation (`open-sd3`) for **Toontrack Superior Drummer 3 / SDX / EZX** soundbanks (`SL-*`, `SL/<ROOM>`, `EZX*`, `.obw` `'ttbw'`, `'oidx'`, 20-bit `'ttpw'`, 24-bit `'tpc'`, `.ttfs`).

> **Goal for Plugin Developers**: Anyone building their own drum sampler, converter, groove tool, or VST3/CLAP/AU plugin can use our byte-exact specification, drop-in Rust crates (`sd3-core`, `sd3-dsp`), or zero-dependency single-header C/C++ library (`sdk/c/opensd3.h`) to **skip 100% of binary reverse engineering** and focus straight on DSP and UI.

---

## Start Here if You Are Building Your Own Plugin

1. **[Byte-Exact Binary Specification (`docs/SUPERIOR_DRUMMER_3_FORMAT_SPEC.md`)](docs/SUPERIOR_DRUMMER_3_FORMAT_SPEC.md)**
   - Outer `soundb*.obw` (`RIFF 'ttbw'`) layout, `'oidx'` directory grammar, and automatic 32-bit `xor_mask` recovery (`real_offset = stored_offset ^ xor_mask`).
   - Inner `RIFF 'ttpw'` ("ToonTrack Packed Wave") 20-bit/24-bit lossless codec (`u32_grid[words_per_channel][channels]` MSB-first signed `B`-bit two's-complement PCM) + `O(log nsec)` random-access seek math.
   - Companion `.ttfs` MIDI groove/config archive format and `TA_REVIR` / `TPC` 24-bit 3rd-order LPC + Golomb-Rice impulse-response codec.
   - Complete `SL-*`, multi-room `SL/<ROOM>`, and `EZX*` folder topologies, group naming grammar (`<Piece>_<Pos>_<Tool>_<Artic>_<Hand>`), mic-bus ranking, surround skipping, `_R`/`_L` round-robin interleaving, GM/SD3 MIDI note aliases, and falsified dead-end hypotheses.
2. **[Third-Party Plugin Developer Guide (`docs/PLUGIN_DEVELOPER_GUIDE.md`)](docs/PLUGIN_DEVELOPER_GUIDE.md)**
   - Copy-paste integration recipes for **Rust** (`sd3-core` & `sd3-dsp`), **C / C++ (JUCE / iPlug2 / VST3 SDK)** (`sdk/c/opensd3.h`), and **Python 3** (`sdk/python/opensd3.py`), plus our lock-free `Mmap` + RAM-head + `AtomicU32` SPSC disk-streaming blueprint (< 350 MB RAM on 230 GB libraries) and seven real-world pitfalls to avoid.

---

## Repository Layout

| Path | Description |
|---|---|
| [`docs/SUPERIOR_DRUMMER_3_FORMAT_SPEC.md`](docs/SUPERIOR_DRUMMER_3_FORMAT_SPEC.md) | Complete clean-room binary format & streaming architecture specification |
| [`docs/PLUGIN_DEVELOPER_GUIDE.md`](docs/PLUGIN_DEVELOPER_GUIDE.md) | Step-by-step integration guide for Rust, C/C++ (JUCE/iPlug2), and Python plugin authors |
| [`sdk/c/opensd3.h`](sdk/c/opensd3.h) | **Single-header zero-dependency C99 / C++17 library** (`os3_parse_ttbw_index`, `os3_parse_ttpw_header`, `os3_decode_ttpw_range_stereo_f32`) + [`example_inspect_and_decode.c`](sdk/c/example_inspect_and_decode.c) (`MIT OR GPL-3.0-or-later`) |
| [`sdk/python/opensd3.py`](sdk/python/opensd3.py) | **Zero-dependency Python 3 SDK & CLI** for scanning `SL-*` folders, parsing `'oidx'`, and decoding 20-bit `'ttpw'` streams (`MIT OR GPL-3.0-or-later`) |
| [`tools/forensics/sd3_diagnostic_bundle.py`](tools/forensics/sd3_diagnostic_bundle.py) | Extracts tiny (< 500 KB) `'oidx'` + sample `'ttpw'` diagnostic `.zip` bundles when investigating new SDX/EZX expansions |
| [`crates/sd3-core`](crates/sd3-core) | Pure-Rust parser, codecs (`ttbw`, `ttpw`, `tpc`, `ttfs`, `wav`), fast `SL-*` / `RoomCatalog` soundbank browser, forensic `probe`, and selective WAV + `kit.json` + `.sfz` extractor (minimal deps: `memmap2`, `serde`, `serde_json`) |
| [`crates/sd3-dsp`](crates/sd3-dsp) | Real-time allocation-free 64-voice multi-mic `DrumEngine` (`Close`, `OH`, `Room`, `Bleed`) with `Mmap` + head-preload + lock-free `AtomicU32` SPSC disk streaming, CC4 hi-hat openness, cymbal chokes, and offline MIDI renderer |
| [`crates/sd3-cli`](crates/sd3-cli) | `sd3tool` CLI (`scan`, `list-obw`, `probe`, `extract`, `ttfs`, `gen-demo`, `render-midi`) |
| [`crates/sd3-plugin`](crates/sd3-plugin) | Reference **Windows 10/11 & Linux VST3 + CLAP + Standalone** drum workstation (`nih-plug` + `egui`) with native OS Folder Picker, `Auto-Find Banks`, Soundbank & Kit Pieces selector, and 4-bus studio mixer |
| [`installer/windows/opensd3.nsi`](installer/windows/opensd3.nsi) & [`build-windows-installer.sh`](build-windows-installer.sh) | Cross-compiled 64-bit Windows NSIS graphical installer (`OpenSD3-0.3.0-Windows-x64-Setup.exe`) |
| [`install-arch.sh`](install-arch.sh) & [`PKGBUILD`](PKGBUILD) | One-command Arch Linux build & installer |

---

## Using the Reference Workstation (`open-sd3`) & CLI (`sd3tool`)

### Windows 10 / 11 (VST3 + CLAP + Standalone + CLI)
Run `OpenSD3-0.3.0-Windows-x64-Setup.exe` (built via `./build-windows-installer.sh` or GitHub Actions Releases):
1. Load **OpenSD3 Drum Workstation** (`VST3` or `CLAP`) in REAPER, FL Studio, Ableton Live, Cubase, Studio One, or Bitwig—or launch **OpenSD3 Standalone**.
2. Click **`Locate Soundbank Folder...`** and select either your root **`Toontrack`** folder or any **`SL-*`** directory (e.g. `D:\Toontrack\SL-SuperiorDrummer3`, `D:\Toontrack\SL-Death`).
3. Switch soundbanks/rooms in the **`Soundbank:`** menu, customize **`Tool`** (`Drumsticks`, `Brushes`, `Hot Rods`, `Mallets`), **`Kick`**, **`Snare`**, **`Hi-Hat`**, **`Toms`**, **`Ride`**, and **`Cymbals`** in the **`Kit Pieces:`** bar, and dial in **`Close`**, **`Overheads`**, **`Room / Amb`**, and **`Bleed`** in the mixer!

### Linux (Arch / Debian / Ubuntu / Fedora)
```bash
# Arch Linux one-step build & user install (~/.vst3, ~/.clap, ~/.local/bin):
./install-arch.sh

# Or standard Cargo workspace build & test on any Linux distro:
cargo test --workspace
cargo run --package xtask -- bundle open-sd3 --release
cargo build --release -p sd3-cli -p open-sd3 --bins
```

### `sd3tool` CLI Quick Reference
```bash
# 1. List all articulation groups, mics, choke groups & MIDI notes in < 5 ms:
sd3tool list-obw /path/to/SL-Death

# 2. Scan an SL-* library directory & check soundb*.obw completeness:
sd3tool scan /path/to/SL-SuperiorDrummer3

# 3. Selectively extract lossless 24-bit WAV + playable kit.sfz from any .obw or SL-* dir:
sd3tool extract /path/to/SL-Death/Sounds/soundb12.obw \
  -o ./sl-death-compact --max-rr 2 --normalize

# 4. Unpack Toontrack .ttfs MIDI groove archives & render a groove offline to 24-bit WAV:
sd3tool ttfs /path/to/Grooves -e ./grooves
sd3tool render-midi ./grooves/Beat_01.mid --kit /path/to/SL-Death -o ./rendered_groove.wav
```

---

## Clean-Room & Legal Notice

- This repository contains **zero proprietary Toontrack audio samples, presets, graphics, or binary files**. All automated tests generate synthetic waveforms and containers from scratch at test time.
- All format documentation and decoders were produced via clean-room interoperability research so legitimate owners of Toontrack soundbanks can use their purchased libraries across platforms (including Linux) and custom workflows.
- *Superior Drummer*, *EZdrummer*, *SDX*, *EZX*, and *Toontrack* are trademarks of Toontrack Music AB, which is not affiliated with or endorsing this project.

## License

- The standalone C/C++ (`sdk/c/opensd3.h`) and Python (`sdk/python/opensd3.py`) SDKs are dual-licensed under **`MIT OR GPL-3.0-or-later`** so third-party plugin authors can integrate format parsing and decoding with maximum flexibility.
- The `open-sd3` VST3/CLAP/Standalone plugin (`nih-plug`) and workspace crates are licensed under **`GPL-3.0-or-later`** (see [`COPYING`](COPYING)).
