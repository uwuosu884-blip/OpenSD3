//! `sd3tool` — Superior Drummer 3 (`SL-*` / `.obw` / `.ttpw` / `.ttfs`) library
//! scanner, `oidx` articulation cataloger, forensic binary inspector, lossless WAV+SFZ
//! sample extractor, and offline groove renderer.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process;

use sd3_core::library;
use sd3_core::obw::{self, ExtractOptions};
use sd3_core::probe;
use sd3_core::synthkit;
use sd3_core::ttbw;
use sd3_core::ttfs::TtfsArchive;
use sd3_core::wav::{self, WavSampleFormat};
use sd3_dsp::{LoadedKit, MixerGains};

type CliResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

fn print_usage() {
    eprintln!(
        "sd3tool {} — Native Linux Superior Drummer 3 (SL-* / .obw / .ttpw / .ttfs) Toolkit

USAGE:
  sd3tool scan <SL-dir> [--json]
      Scan SD2/SD3 rooms, soundbunches, mic buses, and missing indices.

  sd3tool list-obw <soundb*.obw | .oidx | SL-dir> [--json]
      List all articulation groups, mic channels, velocity tiers, RRs & MIDI notes (<5 ms).

  sd3tool probe <file-or-SL-dir> [--max-obw N] [--save report.json]
      Forensic RIFF/oidx/ttpw/entropy binary inspector.

  sd3tool extract <input.obw | .ttpw | SL-dir> -o <out-dir> [options]
      Losslessly decode 20-bit 'ttpw' / 'ttbw' soundbunches to 24-bit/32f WAV + kit.json + kit.sfz.
      Options:
        --filter <substr>    Only extract matching group/articulation (e.g. CR_R, Crash 02, SN)
        --mic <name>         Only extract matching mic or bus (e.g. OH451, Amb67, close, room)
        --max-rr <N>         Cap round-robins per velocity tier (e.g. --max-rr 2)
        --limit <N>          Stop after exporting at most N WAV files
        --normalize          Peak-normalize exported WAVs to -1.0 dBFS
        --float32            Write 32-bit IEEE float WAVs instead of 24-bit PCM
        --no-sfz             Skip generating kit.sfz
        --carve-raw          Carve any unmapped chunks to raw_chunks/

  sd3tool ttfs <archive.ttfs> [-e <extract-dir>]
      Inspect or unpack Toontrack ttfs MIDI groove / preset archives.

  sd3tool gen-demo -o <parent-dir> [--sample-rate 44100]
      Create a synthetic multi-mic SL-OpenSD3Demo library for testing.

  sd3tool render-midi <groove.mid> -o <out.wav> [--kit <SL-dir|obw|kit.json|sfz> | --builtin] [--sample-rate 44100]
      Offline render a MIDI groove (with CC4 hi-hat & cymbal chokes) to 24-bit stereo WAV.",
        env!("CARGO_PKG_VERSION")
    );
}

fn main() {
    if let Err(err) = run() {
        eprintln!("error: {err}");
        process::exit(1);
    }
}

fn next_val(args: &mut impl Iterator<Item = String>, flag: &str) -> CliResult<String> {
    args.next()
        .ok_or_else(|| format!("missing value for {flag}").into())
}

fn run() -> CliResult {
    let mut args = env::args().skip(1);
    let Some(sub) = args.next() else {
        print_usage();
        return Ok(());
    };

    match sub.as_str() {
        "-h" | "--help" | "help" => {
            print_usage();
        }
        "-V" | "--version" | "version" => {
            println!("sd3tool {}", env!("CARGO_PKG_VERSION"));
        }

        "scan" => {
            let mut path: Option<PathBuf> = None;
            let mut json = false;
            while let Some(a) = args.next() {
                match a.as_str() {
                    "--json" => json = true,
                    "-h" | "--help" => {
                        print_usage();
                        return Ok(());
                    }
                    _ if path.is_none() => path = Some(PathBuf::from(a)),
                    _ => return Err(format!("unexpected argument '{a}'").into()),
                }
            }
            let path = path.ok_or("missing <SL-dir> path for `scan`")?;
            let report = library::scan_library_tree(&path)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
                return Ok(());
            }
            println!("=== OpenSD3 Library Scan: {} ===", report.root_path.display());
            if report.rooms.is_empty() && report.ttfs_packs.is_empty() {
                println!("No SL-* rooms or ttfs packs discovered.");
                return Ok(());
            }
            for room in &report.rooms {
                println!(
                    "\n[Room] {} ({})",
                    room.display_name.as_deref().unwrap_or(&room.folder_name),
                    room.generation
                );
                println!("  Path          : {}", room.path.display());
                if let Some(av) = &room.aversion {
                    println!("  Version       : {av}");
                }
                println!(
                    "  Soundbunches  : {} files ({:.2} GB total)",
                    room.soundbunches.len(),
                    room.total_obw_bytes as f64 / 1_073_741_824.0
                );
                for sb in &room.soundbunches {
                    if let (Some(mics), Some(gc), Some(sc)) =
                        (&sb.mics, sb.group_count, sb.sample_count)
                    {
                        println!(
                            "    - {:<16} {:>7.1} MB | {:>3} groups | {:>5} samples | mics: {}",
                            sb.file_name,
                            sb.size as f64 / 1_048_576.0,
                            gc,
                            sc,
                            mics.join(", ")
                        );
                    }
                }
                if !room.missing_indices.is_empty() {
                    println!(
                        "  Missing idx   : {:?} (selective/partial installation)",
                        room.missing_indices
                    );
                }
                if let Some(parts) = &room.core_parts_summary {
                    println!("  Core SD3 5-Part Breakdown:");
                    for p in parts {
                        println!("    - {:<56} [{}] ({})", p.label, p.status, p.expected_range);
                    }
                }
                println!(
                    "  Configs       : {} files | Graphics: {} PNGs",
                    room.config_files.len(),
                    room.graphics_count
                );
            }
            if !report.ttfs_packs.is_empty() {
                println!("\n[ttfs Packs]: {} found", report.ttfs_packs.len());
                for p in &report.ttfs_packs {
                    println!("  - {}", p.display());
                }
            }
        }

        "list-obw" => {
            let mut path: Option<PathBuf> = None;
            let mut json = false;
            while let Some(a) = args.next() {
                match a.as_str() {
                    "--json" => json = true,
                    "-h" | "--help" => {
                        print_usage();
                        return Ok(());
                    }
                    _ if path.is_none() => path = Some(PathBuf::from(a)),
                    _ => return Err(format!("unexpected argument '{a}'").into()),
                }
            }
            let path = path.ok_or("missing <obw|oidx|SL-dir> path for `list-obw`")?;
            let targets = collect_oidx_or_obw_targets(&path)?;
            if targets.is_empty() {
                return Err(format!("no .obw or .oidx files found under {}", path.display()).into());
            }
            let mut parsed: Vec<(PathBuf, ttbw::TtbwIndex)> = Vec::new();
            for t in targets {
                let idx = load_ttbw_or_oidx(&t)?;
                parsed.push((t, idx));
            }
            if json {
                let out: Vec<_> = parsed
                    .iter()
                    .map(|(p, idx)| serde_json::json!({ "path": p, "index": idx }))
                    .collect();
                println!("{}", serde_json::to_string_pretty(&out)?);
                return Ok(());
            }

            for (t, idx) in &parsed {
                println!("=== {} ===", t.display());
                println!(
                    "  Mics ({})     : {}",
                    idx.mics.len(),
                    idx.mics
                        .iter()
                        .zip(&idx.mic_buses)
                        .map(|(m, b)| format!("{m} [{}]", b.as_str()))
                        .collect::<Vec<_>>()
                        .join(", ")
                );
                println!(
                    "  Groups       : {} | Total Samples: {} | XOR Mask: 0x{:08x}",
                    idx.groups.len(),
                    idx.total_records,
                    idx.xor_mask
                );
                println!(
                    "  {:<22} {:<22} {:>4} {:>8} {:>9} {:>8}",
                    "GROUP CODE", "INSTRUMENT / ARTIC", "MIDI", "CHOKE", "VEL TIERS", "SAMPLES"
                );
                for g in &idx.groups {
                    let mut vels: Vec<char> = g
                        .mics
                        .iter()
                        .flat_map(|m| m.records.iter().map(|r| r.vel_char))
                        .collect();
                    vels.sort_unstable();
                    vels.dedup();
                    let vel_desc = match (vels.first(), vels.last()) {
                        (Some(&a), Some(&b)) if a != b => format!("{a}..{b} ({})", vels.len()),
                        (Some(&a), _) => format!("{a} (1)"),
                        _ => "-".into(),
                    };
                    let choke_desc = match (g.member_choke_group, g.triggers_choke_group) {
                        (_, Some(c)) => format!("trig#{c}"),
                        (Some(c), None) => format!("mem#{c}"),
                        (None, None) => "-".into(),
                    };
                    let hits: usize = g.mics.iter().map(|m| m.records.len()).sum();
                    println!(
                        "  {:<22} {:<22} {:>4} {:>8} {:>9} {:>8}",
                        g.name, g.display_name, g.suggested_note, choke_desc, vel_desc, hits
                    );
                }
                println!();
            }
        }

        "probe" => {
            let mut path: Option<PathBuf> = None;
            let mut max_obw = 4usize;
            let mut save: Option<PathBuf> = None;
            while let Some(a) = args.next() {
                match a.as_str() {
                    "--max-obw" => max_obw = next_val(&mut args, "--max-obw")?.parse()?,
                    "--save" => save = Some(PathBuf::from(next_val(&mut args, "--save")?)),
                    "-h" | "--help" => {
                        print_usage();
                        return Ok(());
                    }
                    _ if path.is_none() => path = Some(PathBuf::from(a)),
                    _ => return Err(format!("unexpected argument '{a}'").into()),
                }
            }
            let path = path.ok_or("missing <path> for `probe`")?;
            let rep = probe::probe_path(&path, max_obw)?;
            println!(
                "=== Forensic Binary Probe: {} ({} files) ===",
                rep.target_path.display(),
                rep.files.len()
            );
            for f in &rep.files {
                println!("\n--- {} ({} bytes) ---", f.path.display(), f.size);
                println!("  Magic ASCII : {:?}", f.magic_ascii);
                println!("  First 64B   : {}", f.first_64_hex);
                println!(
                    "  Entropy     : head={:.3} b/B, body={:.3} b/B -> {}",
                    f.head_entropy_bits_per_byte,
                    f.body_entropy_bits_per_byte,
                    f.entropy_assessment
                );
                if !f.detected_signatures.is_empty() {
                    println!("  Signatures  : {:?}", f.detected_signatures);
                }
                if let Some(tree) = &f.riff_tree {
                    println!(
                        "  RIFF Root   : {} form={:?} children={}",
                        tree.id,
                        tree.form_type,
                        tree.children.len()
                    );
                    for ch in tree.children.iter().take(12) {
                        println!(
                            "    chunk '{}' (0x{}) @ 0x{:x} size={} form={:?} sub={}",
                            ch.id,
                            ch.id_hex,
                            ch.header_offset,
                            ch.data_size,
                            ch.form_type,
                            ch.children.len()
                        );
                    }
                }
                if !f.candidate_offset_tables.is_empty() {
                    println!("  Offset Tbls : {:?}", f.candidate_offset_tables);
                }
                if !f.sample_strings.is_empty() {
                    let preview: Vec<_> = f.sample_strings.iter().take(10).collect();
                    println!("  Strings     : {:?}", preview);
                }
            }
            if let Some(save_path) = save {
                fs::write(&save_path, serde_json::to_string_pretty(&rep)?)?;
                println!("\nSaved full JSON probe report to {}", save_path.display());
            }
        }

        "extract" => {
            let mut input: Option<PathBuf> = None;
            let mut out: Option<PathBuf> = None;
            let mut opts = ExtractOptions::default();
            while let Some(a) = args.next() {
                match a.as_str() {
                    "-o" | "--out" => out = Some(PathBuf::from(next_val(&mut args, "-o/--out")?)),
                    "--float32" => opts.wav_format = WavSampleFormat::Float32,
                    "--no-sfz" => opts.emit_sfz = false,
                    "--carve-raw" => opts.carve_unknown_chunks = true,
                    "--filter" => opts.filter = Some(next_val(&mut args, "--filter")?),
                    "--mic" => opts.mic_filter = Some(next_val(&mut args, "--mic")?),
                    "--max-rr" => opts.max_rr = Some(next_val(&mut args, "--max-rr")?.parse()?),
                    "--limit" => opts.limit = Some(next_val(&mut args, "--limit")?.parse()?),
                    "--normalize" => opts.normalize = true,
                    "-h" | "--help" => {
                        print_usage();
                        return Ok(());
                    }
                    _ if input.is_none() => input = Some(PathBuf::from(a)),
                    _ => return Err(format!("unexpected argument '{a}'").into()),
                }
            }
            let input = input.ok_or("missing <input> path for `extract`")?;
            let out = out.ok_or("missing `-o <out-dir>` for `extract`")?;
            let summary = obw::extract_to_directory(&input, &out, opts)?;
            println!("=== Extraction Complete ===");
            println!("  Input         : {}", summary.input_path.display());
            println!("  Output Dir    : {}", summary.output_dir.display());
            println!("  Decoded WAVs  : {}", summary.decoded_wav_count);
            if summary.raw_carved_chunks > 0 {
                println!("  Carved Chunks : {}", summary.raw_carved_chunks);
            }
            if let Some(mp) = &summary.manifest_path {
                println!("  Kit Manifest  : {}", mp.display());
            }
            if let Some(sp) = &summary.sfz_path {
                println!("  SFZ Instrument: {}", sp.display());
            }
            for n in &summary.notes {
                println!("  Info          : {n}");
            }
        }

        "ttfs" => {
            let mut archive: Option<PathBuf> = None;
            let mut extract: Option<PathBuf> = None;
            while let Some(a) = args.next() {
                match a.as_str() {
                    "-e" | "--extract" => {
                        extract = Some(PathBuf::from(next_val(&mut args, "-e/--extract")?))
                    }
                    "-h" | "--help" => {
                        print_usage();
                        return Ok(());
                    }
                    _ if archive.is_none() => archive = Some(PathBuf::from(a)),
                    _ => return Err(format!("unexpected argument '{a}'").into()),
                }
            }
            let archive = archive.ok_or("missing <archive.ttfs> path")?;
            let ttfs = TtfsArchive::open(&archive)?;
            println!(
                "=== ttfs Archive: {} (tag='{}', entries={}) ===",
                archive.display(),
                ttfs.format_tag(),
                ttfs.entries().len()
            );
            for e in ttfs.entries().iter().take(40) {
                if let Some(mi) = &e.midi_info {
                    println!(
                        "  {:56} {:>7} B | fmt={} trk={} ppq={} bpm={:?} ts={:?}",
                        e.path, e.size, mi.format, mi.tracks, mi.ppq, mi.bpm, mi.time_signature
                    );
                } else {
                    println!("  {:56} {:>7} B", e.path, e.size);
                }
            }
            if ttfs.entries().len() > 40 {
                println!("  ... ({} more entries)", ttfs.entries().len() - 40);
            }
            if let Some(dest) = extract {
                let n = ttfs.extract_all(&dest)?;
                println!("Extracted {n} files into {}", dest.display());
            }
        }

        "gen-demo" => {
            let mut out: Option<PathBuf> = None;
            let mut sample_rate = 44_100u32;
            while let Some(a) = args.next() {
                match a.as_str() {
                    "-o" | "--out" => out = Some(PathBuf::from(next_val(&mut args, "-o/--out")?)),
                    "--sample-rate" => {
                        sample_rate = next_val(&mut args, "--sample-rate")?.parse()?
                    }
                    "-h" | "--help" => {
                        print_usage();
                        return Ok(());
                    }
                    _ if out.is_none() => out = Some(PathBuf::from(a)),
                    _ => return Err(format!("unexpected argument '{a}'").into()),
                }
            }
            let out = out.ok_or("missing `-o <out-dir>` for `gen-demo`")?;
            let sl_path = synthkit::write_synthetic_sl_library(&out, sample_rate)?;
            println!(
                "Created synthetic multi-mic Superior Drummer 3 test library at:\n  {}",
                sl_path.display()
            );
        }

        "render-midi" => {
            let mut midi: Option<PathBuf> = None;
            let mut out: Option<PathBuf> = None;
            let mut kit: Option<PathBuf> = None;
            let mut builtin = false;
            let mut sample_rate = 44_100u32;
            while let Some(a) = args.next() {
                match a.as_str() {
                    "-o" | "--out" => out = Some(PathBuf::from(next_val(&mut args, "-o/--out")?)),
                    "--kit" => kit = Some(PathBuf::from(next_val(&mut args, "--kit")?)),
                    "--builtin" => builtin = true,
                    "--sample-rate" => {
                        sample_rate = next_val(&mut args, "--sample-rate")?.parse()?
                    }
                    "-h" | "--help" => {
                        print_usage();
                        return Ok(());
                    }
                    _ if midi.is_none() => midi = Some(PathBuf::from(a)),
                    _ => return Err(format!("unexpected argument '{a}'").into()),
                }
            }
            let midi = midi.ok_or("missing <groove.mid> for `render-midi`")?;
            let out = out.ok_or("missing `-o <out.wav>` for `render-midi`")?;
            let midi_bytes = fs::read(&midi)?;
            let loaded_kit = if builtin || kit.is_none() {
                LoadedKit::built_in_demo(sample_rate)
            } else {
                LoadedKit::load_from_path(kit.as_ref().unwrap())?
            };
            println!(
                "Rendering '{}' through kit '{}' ({} pads, {} streams, {} streamed) ...",
                midi.display(),
                loaded_kit.name,
                loaded_kit.pads.len(),
                loaded_kit.total_samples,
                loaded_kit.streamed_samples
            );
            let rendered = sd3_dsp::render_midi_bytes(
                &midi_bytes,
                loaded_kit,
                sample_rate,
                MixerGains::default(),
                1.8,
            )?;
            wav::write_wav_file(&out, &rendered, WavSampleFormat::Pcm24)?;
            println!(
                "Wrote {:.2}s 24-bit stereo WAV to {}",
                rendered.num_frames() as f64 / sample_rate as f64,
                out.display()
            );
        }

        other => {
            print_usage();
            return Err(format!("unknown subcommand '{other}'").into());
        }
    }

    Ok(())
}

fn collect_oidx_or_obw_targets(path: &Path) -> CliResult<Vec<PathBuf>> {
    let mut out = Vec::new();
    if path.is_file() {
        out.push(path.to_path_buf());
        return Ok(out);
    }
    fn walk(d: &Path, depth: usize, out: &mut Vec<PathBuf>) {
        if depth > 4 {
            return;
        }
        let Ok(rd) = fs::read_dir(d) else {
            return;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, depth + 1, out);
            } else if let Some(ext) = p.extension().and_then(|x| x.to_str()) {
                if ext.eq_ignore_ascii_case("obw") || ext.eq_ignore_ascii_case("oidx") {
                    out.push(p);
                }
            }
        }
    }
    walk(path, 0, &mut out);
    out.sort_by(|a, b| {
        let num = |p: &Path| -> u32 {
            let s = p.file_name().and_then(|x| x.to_str()).unwrap_or("");
            let digits: String = s.chars().filter(|c| c.is_ascii_digit()).collect();
            digits.parse::<u32>().unwrap_or(u32::MAX)
        };
        num(a).cmp(&num(b)).then_with(|| a.cmp(b))
    });
    Ok(out)
}

fn load_ttbw_or_oidx(path: &Path) -> CliResult<ttbw::TtbwIndex> {
    let ext = path
        .extension()
        .and_then(|x| x.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if ext == "oidx" {
        let bytes = fs::read(path)?;
        Ok(ttbw::parse_oidx_payload(&bytes)?)
    } else {
        match ttbw::parse_ttbw_file_index(path) {
            Ok(idx) => Ok(idx),
            Err(_) => {
                let bytes = fs::read(path)?;
                Ok(ttbw::parse_oidx_payload(&bytes)?)
            }
        }
    }
}
