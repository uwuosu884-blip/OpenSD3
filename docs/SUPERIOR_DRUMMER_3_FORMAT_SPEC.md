# Toontrack Superior Drummer 3 (`SL-*` / `.obw` / `'ttbw'` / `'oidx'` / `'ttpw'` / `'ttfs'`) — Complete Format & VST Implementation Specification

> **Purpose of this document**: A complete, self-contained, byte-exact specification of Toontrack's proprietary Superior Drummer 3 (SD3) and SDX expansion library formats, verified against retail `SL-*` binaries (`SL-Death/Sounds/soundb12.obw`). Another engineer or coding agent can use **this single `.md` file alone** — without inspecting or reverse-engineering any files again — to build a 100% compatible VST3 / CLAP / Standalone sampler and lossless WAV/SFZ extractor capable of streaming 40–230 GB Superior Drummer libraries in real time.

---

## 1. Executive Summary & Core Truths

1. **Are `.obw` soundbunches encrypted?**
   - **No.** Neither the outer `.obw` container nor the inner audio streams use cryptographic encryption (no AES, Blowfish, RSA, or license-key decryption).
   - Every `.obw` file in Superior Drummer 3 / SDX is a little-endian RIFF container with form type **`'ttbw'`** containing two chunks:
     1. **`'oidx'`** (~10–120 KB): The complete human-readable microphone list, articulation group catalog, velocity tiers, round-robin indices, and byte offsets/sizes into `'data'`.
     2. **`'data'`** (hundreds of MB to ~1.5 GB): Back-to-back, 2-byte-aligned **`RIFF 'ttpw'`** ("ToonTrack Packed Wave") lossless audio files.
   - **The only obfuscation in the entire format**: Each 32-bit sample offset inside `'oidx'` is XOR'd with a constant per-file 32-bit mask (`real_offset = stored_offset ^ xor_mask`). Because the very first audio stream in every `.obw` begins at byte `0` of the `'data'` payload (`real_offset == 0`), **`xor_mask` is trivially equal to `first_record.stored_offset`** (`0 ^ xor_mask == first_record.stored_offset`; e.g., `0x000000C8` in `SL-Death/Sounds/soundb12.obw`).

2. **What is the `'ttpw'` audio codec?**
   - **20-bit (or up to 24/32-bit) lossless adaptive bit-width packed signed linear PCM** at `44,100 Hz` (typically `channels = 2` stereo, or `1` mono).
   - **Not DPCM, not LPC, not Rice/Huffman, not floating-point.** As a drum/cymbal hit decays toward silence, `'ttpw'` divides the timeline into `nsec` contiguous frame sections (`'fmt '` section table) and packs each section using only the minimum two's-complement bit-width `B` (`0`..`20` bits) required for that section's peak amplitude.
   - Within each section, `'data'` stores a channel-interleaved grid of 32-bit little-endian words (`u32_grid[words_per_channel][channels]`). Each channel shifts its own `u32` words MSB-first into a bit buffer and extracts `B`-bit signed two's-complement integers directly (`full_scale = 2^(bits_per_sample - 1) = 524,288.0` for `bps = 20`).
   - **Zero cross-section state**: Sections carry **zero predictor or bit-accumulator state** across boundaries, and within any section sample `f` starts at exact bit offset `(f - section_start_frame) * B`. Consequently, **any sample frame in any multi-gigabyte `.obw` file can be random-access seeked and decoded in `O(log nsec)` time with zero heap allocations**.

---

## 2. On-Disk `SL-*` Library Directory Structure

Superior Drummer 3 Core (`SL-SuperiorDrummer3`) and SDX expansions (`SL-Death`, `SL-FieldsOfRock`, `SL-StateOfArt`, `SL-Avatar`, etc.) appear in two directory layouts:

### 2.1 Single-Room Layout (Most Libraries)
```text
SL-SuperiorDrummer3/                (or SL-Death/, SL-RockFoundry/, etc.)
├── Name                            (UTF-8 or UTF-16LE display title, e.g. "Death")
├── Aversion                        (Version string, e.g. "1.0.2")
├── Configs                         (Often a `ttfs` archive or binary config bundle)
├── soundDB                         (Library-wide metadata database)
├── instrumentgroups                (Text/UTF-16LE/binary config)
├── micconf                         (Microphone channel definitions & routing)
├── padconf / GM-padconf            (Pad & MIDI note mapping configs)
├── s3presetconf / kitconf          (Kit presets)
├── StackPack                       (SD3 stack metadata)
├── GraphicsV3/ (or Graphics/)      (Kit piece PNG sprites)
└── Sounds/                         (or lowercase `sounds/`)
    ├── soundstats.dat              (or `soundstats`)
    ├── soundb1.obw
    ├── soundb2.obw
    └── ...                         (up to soundb155.obw in Core SD3)
```

### 2.2 Multi-Room SDX Layout
Some SDX expansions house multiple recording rooms under a nested `SL/` folder:
```text
SL-StateOfArt/
└── SL/
    ├── STATEOFTHEARTLIVE/
    │   ├── Name
    │   ├── soundDB
    │   └── Sounds/soundb1.obw ...
    └── STATEOFTHEARTDEAD/
        └── Sounds/soundb1.obw ...
```
**Discovery rule**: Recursively scan up to depth `5` (skipping `Sounds/`, `GraphicsV3/`, `Graphics/`, `SampleProjs/` subtrees during recursion). Any directory containing `Sounds/` (or `sounds/`) or `soundDB` is a playable Room root.

### 2.3 Modular `soundb*.obw` Installation & Partial Downloads
Toontrack splits large libraries into modular download packs so users can install Close+OH mics without downloading 150 GB of Surround/Bleed mics. For example, in `SL-SuperiorDrummer3`:
- `soundb1.obw .. soundb23.obw`: Part 1 — Basic Sound Library (Close + OH Dyn + Amb Ribbon)
- `soundb24.obw .. soundb52.obw`: Part 2 — Room Mics 1 (OH Cond + Amb Near + Amb Mid)
- `soundb53.obw .. soundb113.obw`: Parts 3–4 — Surround / Height Mics
- `soundb114.obw .. soundb150.obw`: Part 5 — Additional Bleed
- `soundb151.obw .. soundb155.obw`: Library Update Soundbunches

**Critical design requirement for a VST plugin**:
1. **Never require `soundb1..N` to be contiguous** — users routinely install only a subset (or even a single `soundb12.obw` file!).
2. **Every `.obw` file is 100% self-describing via its own `'oidx'` chunk** — you can load a single standalone `.obw` file with **zero** companion config files (`soundDB`, `micconf`, `soundstats.dat` are NOT required to decode or map `.obw` audio!).
3. **Cross-`.obw` group merging**: Across multiple `.obw` files in the same `Sounds/` directory, identical group codes (e.g. `SN01_01_DS_CN_R` appearing in `soundb2.obw` with Close mics `"SnareTop"`/`"SnareBot"` AND in `soundb10.obw` with `"OH451"`/`"Amb67"`) represent **additional microphone channels of the same drum articulation** and MUST be merged by `group_name` into the same playable Pad / Velocity Layer / Round-Robin slot!

---

## 3. Outer Container Format: `RIFF 'ttbw'` (`.obw` Soundbunch)

All multi-byte integers in `'ttbw'`, `'oidx'`, and `'ttpw'` are **little-endian**.

### 3.1 Top-Level RIFF Layout
```text
Offset   Size        Type     Description
0x00     4           [u8;4]   ASCII "RIFF" (0x52 0x49 0x46 0x46)
0x04     4           u32_le   riff_size = file_byte_length - 8
0x08     4           [u8;4]   ASCII "ttbw" (0x74 0x74 0x62 0x77)
0x0C     8 + oidx_sz Chunk    'oidx' chunk (sample directory)
...      0 or 1      u8       RIFF even-alignment pad byte (0x00) if `oidx_sz` is odd
data_hdr 8 + data_sz Chunk    'data' chunk (contiguous 2-byte-aligned RIFF 'ttpw' streams)
```
> **Alignment note**: Standard RIFF pads odd-sized chunks with `1` zero byte. In `SL-Death/Sounds/soundb12.obw`, `oidx_sz = 10,716` (even), so `'oidx'` payload spans `0x14 .. 0x29F0` and `'data'` chunk header begins at `0x29F0` (`data_sz = 763,786,300`, payload at `0x29F8 .. 0x2D86CD34`). When `oidx_sz` is odd, check `pos + (oidx_sz & 1)` for a printable 4-byte FourCC (`"data"`) to tolerate both padded and unpadded writers.

### 3.2 `'oidx'` Chunk Payload Grammar (Exact, Zero-Gap Specification)

Let `oidx: &[u8]` be the payload of `'oidx'` (length `oidx_sz`).

#### Step A — 8-Byte Header (`oidx[0x00 .. 0x08]`)
```text
Byte  0..7 : [0x06, 0x01, 0x0A, 0x02, 0x01, 0x00, 0x00]  (format/version flags)
Byte  7    : u8  `num_mics`  (number of microphone channels in this .obw, e.g. 2)
```

#### Step B — Microphone String Table (`num_mics` C-strings)
Starting at `pos = 8`, read `num_mics` NUL-terminated (`0x00`) ASCII strings:
- Example (`soundb12.obw`):
  - `mics[0] = "OH451"` (`0x08..0x0E`, 6 bytes incl. NUL)
  - `mics[1] = "Amb67"` (`0x0E..0x14`, 6 bytes incl. NUL)
- After the last NUL byte, `pos` points to the first Articulation Group (or trailing `0x00`).

#### Step C — Articulation Group Sequence (until `oidx[pos] == 0x00` or `pos >= oidx.len()`)
**CRITICAL**: `'oidx'` ends with a **single trailing `0x00` byte** (empty group-name sentinel) right at `oidx[oidx_sz - 1]`. Always check `if oidx[pos] == 0 { break; }` at the top of the group loop!

Each group at `pos` has the exact binary layout:
```text
1. `group_name`   : NUL-terminated ASCII string starting at `pos` (e.g. "CR01_02_DS_MT_R\0").
                    Let `z` be the index of its NUL byte (`oidx[z] == 0`).
2. `group_len`    : u32_le at `oidx[z + 1 .. z + 5]`.
                    Exact byte length of this group's body (`body_start = z + 5`,
                    `group_end = body_start + group_len as usize`).
3. Inside `oidx[body_start .. group_end]`:
   a. `group_mic_count` : u8 at `oidx[body_start]`. (`p = body_start + 1`)
   b. Repeat `group_mic_count` times:
      - `mic_index` : u8 at `oidx[p]`     (indexes into `mics[0..num_mics]`)
      - `hit_count` : u8 at `oidx[p + 1]` (number of sample records for this mic)
      - Advance `p += 2`
      - Repeat `hit_count` times (each record is **10 bytes**, struct `<IBBI>`):
        * `stored_offset` : u32_le at `oidx[p .. p + 4]`
        * `vel_byte`      : u8     at `oidx[p + 4]`  (ASCII velocity tier: `'A'`, `'B'`, ..., `'M'`, ...)
        * `rr_num`        : u8     at `oidx[p + 5]`  (1-based round-robin number: `1, 2, ..., 9`)
        * `byte_size`     : u32_le at `oidx[p + 6 .. p + 10]` (exact total byte size of `RIFF 'ttpw'`, including its 8-byte `"RIFF"+size` header!)
        * Advance `p += 10`
4. Advance `pos = group_end` (guaranteed `p == group_end` — 0 unparsed bytes!).
```

### 3.3 Solving `xor_mask` (Offset De-Obfuscation)
Every `stored_offset` in `'oidx'` is XOR'd with a constant 32-bit key `xor_mask`:
```rust
let real_data_rel_offset: u64 = (record.stored_offset ^ xor_mask) as u64;
let file_byte_offset: u64     = data_chunk_payload_offset + real_data_rel_offset;
```
**How to compute `xor_mask` automatically for ANY `.obw` file**:
1. Let `first_stored` be the `stored_offset` of the very first record (`group[0].mics[0].records[0]`).
2. Because the first `RIFF 'ttpw'` sample starts at relative offset `0` of the `'data'` chunk payload (`0 ^ xor_mask == first_stored`), **`cand_mask = first_stored`** works for **both** masked files (`0 ^ K = K`) and unmasked files (`0 ^ 0 = 0`)!
3. **Verification check**:
   - If the `'data'` chunk is accessible: verify `&data_payload[0..4] == b"RIFF"` and `&data_payload[8..12] == b"ttpw"`, which immediately proves sample 0 sits at relative offset `0`, so `xor_mask == first_stored`.
   - If parsing a standalone `.oidx` without `'data'`: check contiguity across the first ~10 records: for consecutive records `i` and `i+1`, `end_i = (r[i].stored_offset ^ cand_mask) + r[i].byte_size` equals `(r[i+1].stored_offset ^ cand_mask)` (within `0..=2` alignment bytes).
   - **Subtle binary trap to be aware of**: In `soundb12.obw`, `r[0].stored_offset = 0x000000C8` and `r[0].byte_size = 0x0002B900` (`178,432`). Because the low byte of `0x0002B900` happens to be `0x00`, `0x2B900 ^ 0xC8 == 0x2B9C8 == 0x2B900 + 0xC8`, which makes `r[0] -> r[1]` look contiguous even without XOR! By `r[1] -> r[2]` (`r[1].byte_size = 0x00031118`), `0x2B900 + 0x31118 = 0x5CA18`, and `0x5CA18 ^ 0xC8 = 0x5CAD0 == r[2].stored_offset` (whereas `0x2B9C8 + 0x31118 = 0x5CAE0 != 0x5CAD0`). Checking `>= 4` transitions (or simply using `xor_mask = first_stored` when `data[0..4] == b"RIFF"`) is 100% airtight.

---

## 4. Inner Audio Stream Format: `RIFF 'ttpw'` ("ToonTrack Packed Wave")

Every sample slice `data_payload[real_offset .. real_offset + byte_size]` (as well as standalone SD3 click/metronome `.wav` files!) is a complete `RIFF` container with form `'ttpw'`.

### 4.1 `RIFF 'ttpw'` Chunk Layout
```text
Offset  Size            Description
0x00    4               "RIFF"
0x04    4               u32_le `byte_size - 8`
0x08    4               "ttpw"
0x0C    8 + fmt_size    'fmt ' chunk (`fmt_size == 20 + 12 * num_sections`)
...     8 + data_size   'data' chunk (`data_size == channels * 4 * words_per_channel`)
```
*(Always walk subchunks inside `RIFF 'ttpw'` by FourCC `"fmt "` and `"data"` rather than hardcoding `data` offset, since `fmt_size` varies with `num_sections`.)*

### 4.2 `'fmt '` Chunk Payload (`20 + 12 * nsec` bytes)

#### Fixed 20-Byte Header (`fmt[0x00 .. 0x14]`, struct `<HHIHHII>`)
| Offset | Type | Field | Typical SD3 Value | Notes |
|---|---|---|---|---|
| `0x00` | `u16_le` | `version` | `1` | Codec version (`1`) |
| `0x02` | `u16_le` | `channels` | `2` (or `1`) | Channel count (`1..=32`) |
| `0x04` | `u32_le` | `sample_rate` | `44100` | Sample rate in Hz |
| `0x08` | `u16_le` | `bits_per_sample` | `20` | Full-scale integer bit depth (`full_scale = 2^(bps - 1)`) |
| `0x0A` | `u16_le` | `num_sections` | `4..16` | Number of adaptive bit-width sections (`nsec`) |
| `0x0C` | `u32_le` | `total_frames` | e.g. `87572` | Total audio sample frames per channel |
| `0x10` | `u32_le` | `words_per_channel` | e.g. `22289` | Total `u32` words per channel in `'data'` |

#### Adaptive Section Table (`fmt[0x14 .. 0x14 + 12 * nsec]`, `nsec × <III>`)
For section index `s` in `0 .. nsec` (`base = 20 + s * 12`):
- `end_frame` (`u32_le` at `base + 0`): Exclusive end frame of section `s`. Section `s` covers frames `start_frame .. end_frame`, where `start_frame = 0` for `s == 0` and `sections[s - 1].end_frame` for `s > 0`. The final section has `end_frame == total_frames`.
- `word_offset` (`u32_le` at `base + 4`): Starting per-channel `u32` word index (`0 .. words_per_channel`) in `'data'`. (Multiply by `channels * 4` for the byte offset inside `'data'`!)
- `bit_width` (`u32_le` at `base + 8`): Number of bits `B` (`0 ..= bits_per_sample`) per signed PCM sample in this section.

**Exact Structural Invariants** (verified across every section of real SD3 files):
1. `data_size == channels * 4 * words_per_channel`
2. Let `next_word_off = if s + 1 < nsec { sections[s + 1].word_offset } else { words_per_channel }`.
   - If `bit_width == 0`: `next_word_off - word_offset == 0` (all samples in `start_frame..end_frame` are `0`).
   - If `bit_width > 0`:
     ```text
     next_word_off - word_offset == ceil(((end_frame - start_frame) * bit_width) / 32)
                                 == ((end_frame - start_frame) * bit_width + 31) / 32
     ```

### 4.3 `'data'` Chunk Bitstream Layout & Exact Decoding Algorithm

#### Memory Layout of `'data'`
View `'data'` (`channels * 4 * words_per_channel` bytes) as a row-major 2D array of little-endian 32-bit unsigned integers `u32_grid[words_per_channel][channels]`:
```text
Byte offset of (word_idx, ch) = (word_idx * channels + ch) * 4
word = u32::from_le_bytes(data[byte_off .. byte_off + 4])
```
For stereo (`channels = 2`), even `u32`s (`0, 2, 4, ...`) belong to Left (`ch = 0`) and odd `u32`s (`1, 3, 5, ...`) belong to Right (`ch = 1`):
```text
data bytes: [L_w0 (4B LE)] [R_w0 (4B LE)] [L_w1 (4B LE)] [R_w1 (4B LE)] ...
```

#### Bit-Unpacking Within Each Section `s`
Each channel `ch in 0..channels` is unpacked **independently** across `f in sec.start_frame .. sec.end_frame` with bit-width `B = sec.bit_width`:
1. If `B == 0`: output `0` for `f in sec.start_frame .. sec.end_frame`.
2. Otherwise (`1 <= B <= 32`):
   - Initialize **at the start of every section**:
     - `wi = sec.word_offset as usize`
     - `acc: u64 = 0`
     - `nbits: u32 = 0`
     - `mask: u64 = if B == 32 { u64::MAX } else { (1u64 << B) - 1 }`
     - `sign: u64 = 1u64 << (B - 1)`
     - `full: i64 = 1i64 << B`
   - For each frame `f in sec.start_frame .. sec.end_frame`:
     ```rust
     if nbits < B {
         let byte_off = (wi * channels + ch) * 4;
         let w = u32::from_le_bytes([
             data[byte_off],
             data[byte_off + 1],
             data[byte_off + 2],
             data[byte_off + 3],
         ]) as u64;
         acc = (acc << 32) | w; // MSB-first concatenation!
         nbits += 32;
         wi += 1;
     }
     nbits -= B;
     let u = (acc >> nbits) & mask;
     let sample_i32: i32 = if (u & sign) != 0 {
         (u as i64 - full) as i32
     } else {
         u as i32
     };
     ```
   - Any leftover `(32 - ((n_frames * B) % 32)) % 32` low bits in the last `u32` word of section `s` are zero-padded and discarded when section `s + 1` starts fresh at `sections[s + 1].word_offset`.

#### Normalization & Lossless Export Scaling
- **`sample_i32` is ALREADY the exact `bits_per_sample`-bit signed linear PCM sample** (`[-524_288, +524_287]` for `bps = 20`)!
  - Why small `B` (e.g. `B = 6` -> `[-32, +31]`) works without scaling: when a 20-bit drum tail decays below `±31` LSBs out of `±524,288`, its upper `20 - 6 = 14` sign-extension bits are all identical to bit `5`, so truncating the top 14 redundant sign bits and sign-extending from bit `B - 1` (`5`) on decode reproduces the **exact 20-bit integer** with **0 LSB loss**!
- **To floating-point `f32` (`[-1.0, +1.0]`)**:
  ```rust
  let inv_full_scale = 1.0f32 / ((1u64 << (bits_per_sample - 1)) as f32); // 1.0 / 524288.0 for bps=20
  let sample_f32 = (sample_i32 as f32) * inv_full_scale;
  ```
  *(Note: IEEE-754 `f32` has an exact 24-bit significand and `524,288 = 2^19` is an exact power of two, so `sample_f32` is **100% bit-exact** with zero rounding loss!)*
- **To lossless 24-bit PCM WAV**:
  ```rust
  let pcm24_i32 = sample_i32 << (24 - bits_per_sample); // sample_i32 << 4 for bps=20
  ```

### 4.4 `O(log nsec)` Random-Access Range Decoding (Critical for Disk Streaming!)
Because sections carry zero state and every sample within section `s` starts at deterministic bit offset `(frame - sec.start_frame) * B`, you can decode **any slice `[start_frame .. start_frame + count)` directly from an `Mmap` without decoding earlier frames**:

```rust
// Inside section `sec` (`sec.start_frame <= cur_frame < sec.end_frame`, `B = sec.bit_width > 0`):
let rel_start: usize = cur_frame - (sec.start_frame as usize);
let bit_off: usize   = rel_start * (B as usize);
let mut wi: usize    = (sec.word_offset as usize) + (bit_off / 32);
let skip_bits: u32   = (bit_off % 32) as u32;

let mut acc: u64 = 0;
let mut nbits: u32 = 0;
if skip_bits > 0 {
    let byte_off = (wi * channels + ch) * 4;
    let w = u32::from_le_bytes(data[byte_off..byte_off + 4].try_into().unwrap()) as u64;
    wi += 1;
    nbits = 32 - skip_bits;
    acc = w & ((1u64 << nbits) - 1);
}
// Now run the exact same per-frame loop (`if nbits < B { ... }`) for `seg_frames`!
```

### 4.5 Verified Real-World Reference Vectors (`SL-Death/Sounds/soundb12.obw`)
1. **`CR01_02_DS_MT_R` (`OH451`, Velocity `'A'`, RR `1`, `178,432` bytes)**:
   - `version=1, channels=2, sample_rate=44100, bits_per_sample=20, nsec=6, total_frames=87572, words_per_channel=22289`
   - Sections `(start..end, word_off, B)`:
     - `sec[0]`: `0..1792` (`1792` fr), `w_off=0`, `B=10` (`560` words/ch)
     - `sec[1]`: `1792..53760` (`51968` fr), `w_off=560`, `B=9` (`14,616` words/ch)
     - `sec[2]`: `53760..67424` (`13664` fr), `w_off=15176`, `B=8` (`3,416` words/ch)
     - `sec[3]`: `67424..73696` (`6272` fr), `w_off=18592`, `B=7` (`1,372` words/ch)
     - `sec[4]`: `73696..78688` (`4992` fr), `w_off=19964`, `B=6` (`936` words/ch)
     - `sec[5]`: `78688..87572` (`8884` fr), `w_off=20900`, `B=5` (`1,389` words/ch; `20900 + 1389 == 22289`)
   - First 12 Left-channel samples: `[0, 0, 0, 0, 0, 0, 0, -1, 0, 0, 1, -1]`; peak `±300` LSBs (feather-soft `'A'` layer on overhead mic); lag-1 autocorrelation `r_L = +0.845711, r_R = +0.835987`.
2. **`CR01_04_DS_MT_R` (`OH451`, Velocity `'A'`, RR `2`, `97,372` bytes)**:
   - `version=1, channels=2, sample_rate=44100, bits_per_sample=20, nsec=7, total_frames=67389, words_per_channel=12155`
   - Sections cover `B = 9, 8, 7, 6, 5, 4, 3`; lag-1 autocorrelation `r_L = +0.875499, r_R = +0.895589`.

---

## 5. Critical Reverse-Engineering & DSP Pitfalls (Do NOT Repeat!)

1. **Pitfall #1 — Mistaking soft velocity `'A'` samples (`±100` integer range) for DPCM residuals**:
   - Softest velocity tier `'A'` hits on overhead/ambient mics naturally peak around `100..300` out of `524,288` (`-74..-64 dBFS`), while loudest velocity tier `'M'` hits use `B = 17..20` bits (`-12..-3 dBFS`). **Do NOT integrate/accumulate samples** — `'ttpw'` values are direct linear PCM!
2. **Pitfall #2 — Double-attenuating multi-velocity SD3 pads by MIDI velocity**:
   - In normalized 1-layer samplers, engines multiply output by `(vel / 127)^0.75`. In Superior Drummer 3 `.obw` libraries (`9..15` velocity tiers `'A'..'M'`), **acoustic loudness is already baked into the recordings**! Multiplying layer `'A'` (`-64 dBFS`) by `(8/127)^0.75 = 0.125` (`-18 dB`) buries soft hits at `-82 dBFS` (inaudible).
   - **Correct SD3 velocity gain law**: When a pad has `>= 4` natural velocity layers, select the matching velocity layer `[lovel..=hivel]` and apply **only gentle intra-layer interpolation**:
     ```rust
     let frac = ((vel_u8 - layer.lovel) as f32 / (layer.hivel - layer.lovel).max(1) as f32).clamp(0.0, 1.0);
     let vel_gain = 0.78 + 0.22 * frac; // Smooth -2.1 dB .. 0.0 dB across the tier
     ```
   - Also provide up to **`+24 dB`** of gain range on mixer faders (`Close`, `OH`, `Room`, `Bleed`) because raw 20-bit unclipped overhead/room stems are recorded with generous studio headroom.
3. **Pitfall #3 — Putting Cymbal Crash (`_CR_`) and Cymbal Mute (`_MT_`) in a symmetric choke group**:
   - If `_CR_` both belongs to AND triggers choke group `#1`, hitting two crashes in a row cuts off the first crash unnaturally!
   - **Correct asymmetric SFZ `(group, off_by)` choke model**:
     - Sustain cymbal articulations (`CR`, `RT`, `BL`, `BT`, `BW`, `ED`): `group = Some(100 + cym_id)`, `off_by = Some(cym_id)`. They layer freely with themselves, and ONLY fade out (~8–15 ms linear/cosine ramp) when `group == cym_id` triggers (or Polyphonic Aftertouch `>= 64` arrives for that note).
     - Cymbal Mute/Grab (`MT`, `CK`, `GR`): `group = Some(cym_id)`, `off_by = None`. Playing `_MT_` triggers its recorded hand-mute audio AND smoothly chokes all ringing sustain voices of **that specific cymbal** (`CR01_02` vs `CR01_05` vs `CR01_04`) without choking other cymbals!
     - Hi-Hat: Open HH (`OP`, `O0..O5`, `LS`, `SP`) sets `group = Some(101), off_by = Some(1)`; Closed/Pedal HH (`CL`, `TG`, `PD`, `FT`) sets `group = Some(1), off_by = None`.
4. **Pitfall #4 — Byte-stream or 64-bit cross-channel bit readers on `B != 8`**:
   - At `B = 8`, packing four 8-bit samples MSB-first `(s0 << 24) | (s1 << 16) | (s2 << 8) | s3` into a little-endian `u32` places bytes in memory as `[s3, s2, s1, s0]`. Do NOT generalize that as a byte-reversal rule or a 64-bit cross-channel stream — for `B != 8`, **only** per-channel `u32::from_le_bytes` shifted MSB-first `(acc << 32) | word` decodes all bit-widths (`B = 1..32`) cleanly.
5. **Pitfall #5 — Extracting or full-decoding `.obw` into RAM or `/tmp` on load**:
   - A single `763 MB` `.obw` expands to `~1.5 GB` of `f32` PCM; a full `SL-SuperiorDrummer3` kit is `230 GB`. Never decode full samples on load or write temp WAVs to `/tmp` (`tmpfs` RAM disk on Linux!). Use `Mmap` + small RAM head preload + background `ttpw` range streaming (Section 8).

---

## 6. Toontrack Naming Conventions, Routing & MIDI Mapping

### 6.1 Articulation Group Code Grammar
Group names in `'oidx'` follow `<INST><VAR>_<POS>_<TOOL>_<ARTIC>[_<SIDE>]` (e.g. `CR01_02_DS_MT_R`):
- **Token 0 (`<INST><VAR>`) — Instrument Family**:
  - `KD*` / `BA*` / `BD*`: Kick Drum
  - `SN*` / `SD*`: Snare Drum
  - `HH*` / `HAT*`: Hi-Hat
  - `TT*` / `TOM*`: Rack Tom (`pos` `01` = High, `02` = Mid-High, `03` = Mid)
  - `FT*`: Floor Tom (`pos` `01` = Floor 1, `02` = Floor 2)
  - `CR*`: Crash Cymbal (`CR01_02`, `CR01_04`, `CR01_05` = distinct crash cymbals at kit positions `02`, `04`, `05` -> distinct choke IDs!)
  - `RD*` / `RI*`: Ride Cymbal
  - `CH*`: China Cymbal | `SP*`: Splash Cymbal | `CB*`: Cowbell
- **Token 1 (`<POS>`) — Kit Position**: Distinguishes multiple drums/cymbals of the same type (`instrument_key = "<INST>_<POS>"`, e.g. `"CR01_02"`). Use `instrument_key` as the unique cymbal/hi-hat choke-group key!
- **Token 2 (`<TOOL>`)**: `DS` = Drumsticks, `BR` = Brushes, `RD` = Rods, `ML` = Felt Mallets, `HD` = Hands/Fingers, `SN` = Snares Off (`SNOFF`).
- **Token 3 (`<ARTIC>`) — Articulation**:
  - **Kick**: `HT`/`HIT`/`CN` -> GM `36` (`_L` left beater -> `35`)
  - **Snare**: `CN`/`CE`/`HT` (Center) -> `38` | `RS`/`RM` (Rimshot) -> `40` | `SS`/`CS`/`XR` (Sidestick) -> `37` | `RU`/`FL`/`BZ` (Ruff/Flam/Buzz) -> `39` | `RO` (Rim Only) -> `71`
  - **Hi-Hat**: `CL`/`TG`/`T0`/`T1` (Closed/Tight, openness `0`) -> `42` | `PD`/`FT`/`CC` (Pedal Chick, openness `0`) -> `44` | `O0` (openness `24`) -> `60` | `O1`/`LS` (`45`) -> `24` | `O2` (`72`) -> `25` | `O3` (`100`) -> `26` | `OP`/`O4`/`O5` (Full Open, `127`) -> `46` | `SP` (Foot Splash, `90`) -> `61`
  - **Toms**: `TT` pos `01, 02, 03` -> `48, 47, 45` (Rims `+32`); `FT` pos `01, 02` -> `43, 41` (Rims `+30`)
  - **Crash / China / Splash**: `CR`/`ED`/`SH` (Hit/Edge) -> `49` (pos 1–2), `57` (pos 3–4), `55` (pos 5 / Splash), `52` (China / pos 6+) | `MT`/`CK`/`GR` (Hand Mute/Choke) -> `base + 5` | `RT`/`BW` (Bow Tip) -> `51` | `BL` (Bell) -> `53` | `BT` (Bell Tip) -> `56`
  - **Ride**: `RT`/`BW` (Bow) -> `51` | `BL` (Bell) -> `53` | `BT` (Bell Tip) -> `56` | `CR`/`ED`/`SH` (Edge Crash) -> `59` | `MT`/`CK` (Mute) -> `58`
- **Token 4 (`<SIDE>`, optional)**: `R` = Right hand, `L` = Left hand.

### 6.2 Collision-Free MIDI Note Assignment
When loading either a full `SL-*` folder or a standalone `.obw` (such as `soundb12.obw` with 15 crash articulations):
1. Sort groups by priority:
   - Priority 0: Core shells & hi-hats (`KD`, `SN`, `HH`, `TT`, `FT`) + primary strikes (`CN`, `CL`, `OP`, `PD`)
   - Priority 1: Primary cymbal hits (`CR`, `BL`, `RT`/`BW` on Ride)
   - Priority 2: Secondary cymbal articulations (`BT`, `RT` on Crash, `MT` mutes, percussion)
2. Give each group its preferred note if unused; otherwise assign the first unused note from `(58..=87).chain(24..35).chain(35..58).chain(88..=119).chain(12..24)`. This preserves `36/38/42/46/49/51/57` for canonical GM grooves while keeping 100% of articulations playable.

### 6.3 Microphone Name -> Mixer Bus Routing
Classify each `oidx` mic string case-insensitively:
- **Bleed**: contains `"bleed"`
- **Overheads (`OH`)**: starts with `"oh"` or contains `"overhead"` (e.g. `"OH451"`, `"OH Dyn"`, `"OH Cond"`)
- **Room / Ambience (`Room`)**: starts with `"amb"` or contains `"room"`, `"far"`, `"mid"`, `"near"`, `"mono"`, `"surr"`, `"chamber"`, `"hall"`, `"front"`, `"rear"` (e.g. `"Amb67"`, `"Amb Ribbon"`, `"Amb Near"`)
- **Close (`Close`)**: everything else (`"KickIn"`, `"KickOut"`, `"KickSub"`, `"SnareTop"`, `"SnareBot"`, `"Hihat"`, `"Tom1"`, `"Ride"`, ...)

### 6.4 Velocity Letter Partitioning (`'A'..'Z'` -> `1..=127`)
Collect all sorted unique `vel_char` values in a group (e.g. `['A', 'B', ..., 'M']`, count `N`, where `'A'` is softest and last letter is loudest). Partition `1..=127` evenly:
```rust
lovel = ((tier_idx * 127) / N) as u8 + 1;
hivel = if tier_idx + 1 == N { 127 } else { (((tier_idx + 1) * 127) / N) as u8 };
```

---

## 7. Companion Toontrack Formats (`ttfs` & `TA_REVIR` / `TPC`)

### 7.1 `ttfs` Archive Container (MIDI Groove Packs, `Configs`, `s3presetconf`)
Used for packed MIDI groove libraries and config bundles:
```text
0x00  [u8; 4]  Magic "ttfs"
0x04  u16_le   Version (typically 1)
0x06  u32_le   rel_root (root directory table offset = rel_root + 0x10; fallback to u16 at 0x06..0x08 + 0x10 if out of range)
0x10  cstring  Format tag (e.g. "MIDI", "SD3", "EMX3")
```
At `root` (and recursively at each subdirectory `offset`):
- `entry_count: u32_le`, followed by `entry_count` variable-length entries:
  - `offset: u32_le`
  - `size: u32_le`
  - `name: NUL-terminated UTF-8 / Latin-1 string`
- Entry interpretation:
  - `offset == 0xFFFF_FFFF`: empty 0-byte file
  - `size == 0 && offset != 0xFFFF_FFFF`: subdirectory table located at byte `offset` -> recurse with path `"{prefix}/{name}"`
  - Otherwise: raw uncompressed file (`Standard MIDI File` `"MThd..."`, XML/JSON/UTF-16 preset, etc.) at `archive[offset .. offset + size]`.

### 7.2 `TA_REVIR` / `TPC` 24-Bit LPC + Golomb-Rice Codec (Impulse Responses & Legacy Packs)
Used in Toontrack reverb IR packs (EZmix / SD3 room IRs), **distinct from `.obw` `'ttpw'`**:
- Operates on **512-sample blocks** (`BLOCK_FRAMES = 512`), LSB-first `u32` bitstream.
- Block header bits:
  - `9` bits: `len_words - 1` (number of `u32` words in this block)
  - `6` bits `a`, `7` bits `b` -> 3rd-order fixed-point LPC predictor coefficients:
    `c3 = a - 64`, `c2 = b - 63`, `c1 = 47 - (b + c3)`
  - `3 × 24` bits: sign-extended 24-bit warm-up samples `out[0], out[1], out[2]`
- Followed by `16` sub-blocks of `32` residuals (covering `0..512`; `res[0..3]` ignored in reconstruction):
  - `5` bits: Rice parameter `k` (`0..=26`)
  - `32` residuals, each encoded as unary quotient `q` (`1`-run up to 8 -> `q = 0..7`; `0`-run up to 8 -> `q = -1..-8`) + `k`-bit unsigned remainder `low`: `res = (q << k) + low`
- Reconstruction for `n in 3..512`:
  ```rust
  let pred = c1 * out[n - 3] + c2 * out[n - 2] + c3 * out[n - 1];
  out[n] = res[n] - (pred >> 4);
  ```
- Stereo mid/side integer transform: `L = (mid + 2 * side) >> 1`, `R = (mid - 2 * side) >> 1`.

---

## 8. Production Real-Time VST3 / CLAP Streaming Architecture

To play **40–230 GB** Superior Drummer 3 libraries inside a real-time DAW audio thread (`process()`) with **zero dropouts, zero locks, zero allocations, and < 200 MB RAM**:

1. **Loader Thread (`open-sd3-loader`)**:
   - Scans `SL-*` or `.obw`, `Mmap`s each `.obw` (`Arc<Mmap>`), parses `'oidx'` in `< 1 ms` per file, parses each sample's 92-byte `TtpwHeader`, and pre-decodes **only the head** into pinned RAM (`Arc<[f32]>`):
     - `<= 2,500` samples: `head_frames = 2,048` (`46.4 ms` @ 44.1 kHz -> `~17 MB` RAM for `1,032` stereo samples!)
     - `2,501..15,000` samples: `head_frames = 1,024` (`23.2 ms`)
     - `> 15,000` samples: `head_frames = 512` (`11.6 ms` -> `< 200 MB` RAM even at `50,000` samples!)
   - Sends `Arc<LoadedKit>` to the Audio Thread via a bounded `crossbeam_channel`. Audio Thread swaps `Arc<LoadedKit>` and sends the old `Arc<LoadedKit>` to a garbage-collection channel so `munmap` / `Drop` **never** runs on the audio thread.

2. **Audio Thread (`DrumEngine::process_block`) + Streamer Thread (`open-sd3-streamer`)**:
   - Pre-allocate `MAX_VOICES = 64` `StreamSlot`s once at plugin init (`8 MB` total fixed pool):
     - `generation: AtomicU64`
     - `read_frame: AtomicUsize`
     - `write_frame: AtomicUsize`
     - `ring_l / ring_r: [[AtomicU32; 4096]; 4]` (`92.9 ms` power-of-two ring per mic bus, storing `f32::to_bits` / `f32::from_bits` — **100% safe Rust, zero `unsafe`!**)
   - On `NoteOn`:
     - Audio thread immediately plays frames `0 .. head_frames` (`46.4 ms`) from `sample.head_l/r` in L1/RAM (`0.00 ms` latency, zero disk I/O).
     - If `total_frames > head_frames`, increments `slot.generation`, resets `read_frame = write_frame = head_frames`, and `try_send`s `StreamCmd` (`slot_idx, generation, Arc::clone(&kit), pad, layer, rr`) to `open-sd3-streamer`.
   - `open-sd3-streamer` wakes immediately (well within the `11–46 ms` head cushion!), decodes `512`-frame chunks via `decode_ttpw_range_stereo` from the `Mmap` into `ring_l/r[idx & 4095]`, and publishes `write_frame.store(wf + 512, Ordering::Release)`. When `active_jobs` is empty, it blocks on `rx.recv()` (`0.0%` idle CPU).

3. **DAW / MIDI Integration Essentials (`nih-plug` or C++ VST3/CLAP)**:
   - Set `MIDI_INPUT = MidiConfig::MidiCCs` (in `nih-plug`, `MidiConfig::Basic` drops `MidiCC` events!).
   - Handle `MidiCC { cc: 4, value }`: `0` = pedal up (wide open hi-hat), `127` = pedal tight closed. When `cc4_active`, redirect variable hi-hat notes (except dedicated pedal chick `44`) to the loaded hi-hat pad with closest `hihat_openness` to `127 - cc4`, and choke open hi-hat voices when `cc4 >= 115`.
   - Handle `PolyPressure { note, pressure >= 64 }`: trigger ~15 ms fast choke fadeout on active voices matching `note` or sharing that cymbal's `off_by` group.

---

## 9. Soundbank Directory Topologies (`SL-*`, `SL/<ROOM>`, `EZX*`)

Any plugin that mimics Superior Drummer's soundbank browser must handle **four real-world directory layouts**:

### 9.1 Single-Room SD3 / SDX Soundbank (`SL-*`)
```text
SL-SuperiorDrummer3/              (or SL-Death/, SL-FieldsOfRock/, ...)
├── Name                          UTF-8 or UTF-16LE display name (e.g. "Superior Drummer 3")
├── Aversion                      Library version string (e.g. "1.2.1")
├── Configs                       Config container (often `ttfs` or binary config)
├── instrumentgroups              Instrument slot & group definitions
├── micconf                       Studio mixer microphone channel descriptions
├── padconf                       E-drum / pad mapping definitions
├── s3presetconf                  Factory kit & mixer presets
├── soundDB                       Precomputed sound metadata database
├── StackPack                     Instrument stacking rules
├── uberconf                      Top-level soundbank configuration
├── GraphicsV3/
│   └── KitLayout/
│       ├── Kick/*.png            Drum shell / model artwork (e.g. "Gretsch_22_C.png")
│       ├── Snare/*.png
│       ├── HiHat/*.png
│       ├── Tom/*.png
│       ├── Cymbal/*.png
│       ├── Percussion/*.png
│       └── Stand/*.png
└── Sounds/
    ├── soundstats.dat            Soundbunch statistics table
    ├── soundb1.obw               RIFF 'ttbw' soundbunches (1-based: soundb1..soundbN.obw)
    ├── soundb2.obw
    └── ...
```

For the 230 GB Core **`SL-SuperiorDrummer3`** library (`soundb1.obw .. soundb155.obw`), Toontrack splits installation across 5 optional downloads:
| Download Part | File Range | Contents |
|---|---|---|
| **Part 1 — Basic Sound Library** | `soundb1.obw .. soundb23.obw` | Core close mics, stereo overheads (`OH Cond`, `OH Dyn`), and stereo room ambience |
| **Part 2 — Room Mics 1 (Extra Stereo)** | `soundb24.obw .. soundb52.obw` | Additional stereo ambience/room/mono microphones + Brushes/Rods/Mallets |
| **Part 3 — Surround Setup (5.0 / 9.1 / 11.1)** | `soundb53.obw .. soundb113.obw` | Surround (`Front`, `Center`, `Rear`, `Wide`) & height channels (`Height_*`) |
| **Part 4 — Extra Bleed Part 1** | `soundb114.obw .. soundb134.obw` | Additional multi-channel bleed buses |
| **Part 5 — Extra Bleed Part 2** | `soundb135.obw .. soundb155.obw` | Full 11.1 surround & close bleed completion |

### 9.2 Multi-Room SDX Expansion (`SL-<Pack>/SL/<ROOM>/Sounds/`)
Expansions recorded in two or more distinct studios (e.g. `SL-MetalMachinery`, `SL-DeathAndDarkness`, `SL-Decades`) nest separate room directories under `SL/`:
```text
SL-ExamplePack/
├── Name
└── SL/
    ├── ROOM_A/
    │   └── Sounds/soundb1.obw .. soundb18.obw
    └── ROOM_B/
        └── Sounds/soundb1.obw .. soundb22.obw
```
**Critical Rule**: Never merge `.obw` files from `ROOM_A` and `ROOM_B` into a single kit! Treat every directory containing a direct `Sounds/*.obw` folder as its own independent `DiscoveredBank` / `RoomCatalog`.

### 9.3 EZdrummer 2 / EZdrummer 3 Soundbanks (`EZX_*` / `EZX3_*`)
Located under `Toontrack/EZDrummer/EZX*` (or `Common Files/Toontrack/EZDrummer/`):
- Same `Sounds/soundb*.obw` (`RIFF 'ttbw'` + `'ttpw'`) container format!
- Uses **0-based numbering (`soundb0.obw`, `soundb1.obw`, ...)** in many EZX packs.

### 9.4 Subfolder Normalization & Unmerged Installer Folders
- **Subfolder selection**: Users frequently navigate *inside* `SL-Name/Sounds` or `SL-Name/GraphicsV3/KitLayout` in an OS folder picker before clicking OK. Always step upward (`normalize_bank_search_root`) if the chosen folder is named `Sounds`, `GraphicsV3`, `Graphics`, `KitLayout`, or `SL`.
- **Unmerged multi-part downloads**: Users sometimes unpack `SL-SuperiorDrummer3 Part 1/Sounds/soundb1..23.obw` and `SL-SuperiorDrummer3 Part 2/Sounds/soundb24..52.obw` side-by-side without merging them. If sibling folders strip `"Part N"` / `"Disc N"` to the same `SL-*` base name **and** their `soundb*.obw` filenames are 100% disjoint, merge their `.obw` file lists virtually and sort numerically.

---

## 10. Multi-`.obw` Room Assembly & Active Kit Selection

Loading a real `SL-*` folder with 25–155 `.obw` files requires **two-phase indexing + active-piece assembly** (implemented in `sd3_core::library::RoomCatalog` and `sd3_dsp::LoadedKit::load_room_selection`):

1. **Numeric `soundb<N>.obw` Ordering**:
   Always sort `.obw` paths by the parsed integer `<N>` (`0, 1, 2, ..., 12, ..., 100, ..., 155`), **never** lexicographically (`soundb100.obw` < `soundb2.obw`).
2. **Fast `'oidx'` Catalog Pass (`< 80 ms` for 155 `.obw` files)**:
   Read only the `'oidx'` chunk at byte `0x0C` of each `.obw` (~8–40 KB per file; zero sample decoding!). Extract all available drum pieces per slot (`Kick: KD01..KD06`, `Snare: SN01..SN09`, `HiHat: HH01..HH03`, `Tom: TM01..TM03`, `Ride: RD01..RD04`, `Cymbal: CR01..CR03`) and hand tools (`DS` Drumsticks, `BR` Brushes, `RD` Hot Rods, `ML` Mallets, `HD` Hands).
3. **Skip Surround-Only Soundbunches in Stereo Mode**:
   If every mic name in an `.obw` matches `surr | height | rear | wide | front | center | lfe` (e.g. `soundb53..113.obw`), skip opening/preloading that `.obw` in a stereo plugin—saving up to 80 GB of virtual address space and tens of thousands of unused sample descriptors.
4. **Strict Within-Bus Microphone Priority (Never Spill Across Buses)**:
   Route each mic strictly to its own `MicBus` (`Close = 0`, `Oh = 1`, `Room = 2`, `Bleed = 3`). When multiple `.obw` files provide mics in the same bus for the same hit, keep the highest-priority stereo studio mic:
   - `Close`: `KickIn`/`SnareTop`/primary direct (`prio 0`) > `KickOut`/resonant (`prio 5`) > `SnareBot`/`KickSub`/wires (`prio 10`).
   - `Oh`: `OH Cond`/`OH451`/`OH Main` (`prio 0`) > `OH Dyn`/secondary (`prio 2`) > `OH Mono` (`prio 10`).
   - `Room`: Stereo `Amb Near`/`Amb Mid`/`Amb Ribbon`/`Amb67` (`prio 0`) > `Amb Far`/`Chamber`/`Hall` (`prio 4`) > `Amb Mono` (`prio 6`) > surround (`prio 40`).
5. **`_R` / `_L` Hand Round-Robin Interleaving**:
   Merge `_R` and `_L` groups of the same piece & articulation (except double kicks `KD*_L` = GM `35`, `KD*_R` = GM `36`) onto a single pad keyed by `(slot, piece_code, slot_pos, artic_code)`, interleaving `(rr_num, hand_ord)` so `(1, R), (1, L), (2, R), (2, L), ...` alternate naturally on single-note MIDI rolls.
6. **Separate `preferred_note` vs. Single-File `suggested_note`**:
   Inside `soundb1.obw`, `KD01` and `KD02` both prefer GM note `36`. Store `preferred_note = 36` unmodified on every `OidxGroup`, and deduplicate MIDI notes **only after filtering to the active `KitSelection`**—otherwise selecting `KD02` or `SN02` would inherit single-file collision fallback notes (`58+`) instead of GM `36` / `38`!
7. **Sequential `(obw_idx, data_offset)` Preload Sweep**:
   Collect all winning `(obw_idx, abs_data_offset, byte_size)` slices across the active kit, sort by `(obw_idx, abs_data_offset)`, and decode RAM heads in one monotonic forward sweep per `.obw` (maximizing OS read-ahead on both NVMe SSDs and spinning HDDs).
8. **GM + Toontrack SD3 MIDI Note Aliases**:
   Bind unoccupied standard MIDI keys after primary assignment:
   - Kick: `35 <-> 36`
   - Snare: `33 -> 38` (SD3 left-hand center), `40 -> 38` (rimshot fallback)
   - Hi-Hat: `42 <-> 22` (closed), `44 <-> 21` (pedal), `46 <-> 26/25/24/60` (open tiers)
   - Toms: nearest-neighbor fill across GM tom keys `[50, 48, 47, 45, 43, 41]`

---

## 11. Falsified Reverse-Engineering Hypotheses (Dead Ends to Skip)

To save future developers from repeating dead-end experiments, all of the following hypotheses were **empirically tested against retail SD3 `.obw` binaries and falsified**:

1. **"`.obw` audio or `'oidx'` headers are AES / Blowfish / stream-cipher encrypted"** — **FALSE.** Outer `.obw` files are plain `RIFF 'ttbw'` containers with readable ASCII mic names and group codes inside `'oidx'`. The only obfuscation anywhere in `'oidx'` is a single constant 32-bit `xor_mask` XORed into each record's `stored_offset` (`xor_mask == first_record_at_offset_0.stored_offset`, e.g. `0x000000C8` in `SL-Death/Sounds/soundb12.obw`).
2. **"`'ttpw'` uses Zstandard, zlib, LZ4, FLAC, Vorbis, or WavPack"** — **FALSE.** `'ttpw'` contains zero entropy coding, zero Huffman/Rice tables, and zero third-party codec headers. Its ~2.3:1 compression comes purely from partitioning each drum hit's decaying envelope into `nsec` sections (`4..16` sections) and packing each section at its exact peak bit-width `B` (`0..=20` bits) instead of 24/32 bits!
3. **"`'ttpw'` uses DPCM / delta / LPC prediction like `TA_REVIR`"** — **FALSE.** Unlike Toontrack's `TA_REVIR` reverb IR format (Section 7.2), `'ttpw'` stores **direct signed `B`-bit two's-complement PCM integers** (no predictor state across frames!), which is why any arbitrary frame range `[start_frame .. start_frame + count]` can be decoded in `O(log nsec)` without decoding earlier audio!
4. **"`'ttpw'` packs bits LSB-first or in planar `[ch][words]` arrays"** — **FALSE.** `'ttpw'` interleaves 32-bit little-endian words across channels (`u32_grid[words_per_ch][ch]`), and within each channel's `u32` word stream, `B`-bit signed samples are packed **MSB-first** (`bit 31` down to `bit 0`). Testing LSB-first or frame-interleaved bitstreams yields white noise (lag-1 autocorrelation `~0.0`), whereas `u32_grid[words_per_ch][ch]` MSB-first yields smooth studio waveforms (lag-1 autocorrelation `> +0.85` on cymbals, `> +0.98` on shells).

