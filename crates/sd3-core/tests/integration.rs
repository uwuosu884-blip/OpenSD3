use std::f32::consts::TAU;
use std::fs;
use std::path::PathBuf;

use sd3_core::kit::DrumKitManifest;
use sd3_core::library;
use sd3_core::obw::{self, ExtractOptions};
use sd3_core::probe;
use sd3_core::synthkit;
use sd3_core::tpc;
use sd3_core::ttfs::{self, TtfsArchive};
use sd3_core::wav::{self, AudioBuffer, WavSampleFormat};

fn temp_test_dir(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("opensd3_test_{tag}_{}", std::process::id()));
    let _ = fs::remove_dir_all(&p);
    fs::create_dir_all(&p).unwrap();
    p
}

#[test]
fn tpc_lossless_roundtrip_mono_stereo_and_multichannel() {
    for nch in [1usize, 2, 6] {
        let frames = 1300; // non-multiple of 512 to test tail truncation
        let mut channels = vec![vec![0.0f32; frames]; nch];
        for (ch, buf) in channels.iter_mut().enumerate() {
            for (i, s) in buf.iter_mut().enumerate() {
                let t = i as f32 / 44_100.0;
                *s = 0.65 * (TAU * (110.0 + 55.0 * ch as f32) * t).sin() * (-t * 12.0).exp();
            }
        }
        let orig = AudioBuffer {
            sample_rate: 44_100,
            channels,
        };
        let encoded = tpc::encode_tpc_stream(&orig, true).expect("encodes TPC");
        assert!(tpc::looks_like_tpc(&encoded));
        let decoded = tpc::decode_tpc_stream(&encoded, 44_100).expect("decodes TPC");

        assert_eq!(decoded.sample_rate, 44_100);
        assert_eq!(decoded.num_channels(), nch);
        assert_eq!(decoded.num_frames(), frames);

        for ch in 0..nch {
            for i in 0..frames {
                let diff = (orig.channels[ch][i] - decoded.channels[ch][i]).abs();
                assert!(
                    diff < 3e-6,
                    "ch={ch} frame={i}: orig={} dec={} diff={diff}",
                    orig.channels[ch][i],
                    decoded.channels[ch][i]
                );
            }
        }
    }
}

#[test]
fn wav_pcm24_and_float32_roundtrip() {
    let orig = AudioBuffer {
        sample_rate: 48_000,
        channels: vec![
            (0..600).map(|i| ((i as f32) * 0.05).sin() * 0.8).collect(),
            (0..600).map(|i| ((i as f32) * 0.03).cos() * 0.7).collect(),
        ],
    };
    for fmt in [WavSampleFormat::Pcm24, WavSampleFormat::Float32] {
        let bytes = wav::encode_wav_bytes(&orig, fmt).unwrap();
        let back = wav::read_wav_bytes(&bytes).unwrap();
        assert_eq!(back.sample_rate, 48_000);
        assert_eq!(back.num_channels(), 2);
        assert_eq!(back.num_frames(), 600);
        for i in 0..600 {
            assert!((orig.channels[0][i] - back.channels[0][i]).abs() < 1e-5);
            assert!((orig.channels[1][i] - back.channels[1][i]).abs() < 1e-5);
        }
    }
}

#[test]
fn ttfs_archive_roundtrip_and_midi_metadata() {
    let dir = temp_test_dir("ttfs");
    let midi = synthkit::build_demo_rock_groove_midi();
    let pack_bytes = ttfs::build_ttfs(
        "MIDI",
        &[
            ("Grooves/Rock_4_4/Beat_01.mid", &midi),
            ("Info/Header.txt", b"Superior Drummer MIDI Pack\n"),
            ("EmptyPlaceholder.dat", b""),
        ],
    )
    .unwrap();
    let pack_path = dir.join("test.ttfs");
    fs::write(&pack_path, pack_bytes).unwrap();

    let arch = TtfsArchive::open(&pack_path).unwrap();
    assert_eq!(arch.format_tag(), "MIDI");
    let midi_entry = arch
        .entries()
        .iter()
        .find(|e| e.path.ends_with("Beat_01.mid"))
        .expect("finds MIDI entry");
    let info = midi_entry.midi_info.as_ref().expect("parses MIDI header");
    assert_eq!(info.bpm, Some(120.0));
    assert_eq!(info.time_signature.as_deref(), Some("4/4"));

    let out_dir = dir.join("unpacked");
    let extracted_count = arch.extract_all(&out_dir).unwrap();
    assert_eq!(extracted_count, 3);
    assert!(out_dir.join("Grooves/Rock_4_4/Beat_01.mid").is_file());
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn full_sl_library_scan_probe_extract_and_sfz_reimport() {
    let dir = temp_test_dir("sl_pipeline");
    let sl_dir = synthkit::write_synthetic_sl_library(&dir, 44_100).unwrap();

    // 1. Scan library
    let scan = library::scan_library_tree(&dir).unwrap();
    assert_eq!(scan.rooms.len(), 1);
    assert_eq!(scan.ttfs_packs.len(), 1);
    assert_eq!(scan.rooms[0].soundbunches.len(), 1);
    assert!(scan.rooms[0].missing_indices.is_empty());

    // 2. Forensic probe
    let pr = probe::probe_path(&sl_dir, 2).unwrap();
    let obw_probe = pr
        .files
        .iter()
        .find(|f| f.path.to_string_lossy().ends_with("soundb1.obw"))
        .expect("probed soundb1.obw");
    assert!(obw_probe.riff_tree.is_some());

    // 3. Extract .obw -> WAV + kit.json + kit.sfz
    let ext_dir = dir.join("extracted");
    let summary = obw::extract_to_directory(&sl_dir, &ext_dir, ExtractOptions::default()).unwrap();
    // 12 pads * 3 velocity layers * 2 round-robins * 3 mic buses = 216 stereo WAVs!
    assert_eq!(summary.decoded_wav_count, 216);
    assert!(summary.manifest_path.as_ref().unwrap().is_file());
    let sfz_path = summary.sfz_path.as_ref().unwrap();
    assert!(sfz_path.is_file());

    // 4. Re-import generated SFZ
    let (sfz_manifest, _) = DrumKitManifest::from_sfz_file(sfz_path).unwrap();
    assert!(sfz_manifest.pads.len() >= 12);

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn core_sd3_missing_soundbunch_gap_detection() {
    let dir = temp_test_dir("core_gaps");
    let core = dir.join("SL-SuperiorDrummer3").join("Sounds");
    fs::create_dir_all(&core).unwrap();
    fs::write(core.join("soundstats.dat"), b"SDST").unwrap();
    for i in 1..=23u32 {
        if i != 7 {
            fs::write(core.join(format!("soundb{i}.obw")), b"RIFF\x04\x00\x00\x00OBW ").unwrap();
        }
    }
    let scan = library::scan_library_tree(&dir).unwrap();
    assert_eq!(scan.rooms.len(), 1);
    assert_eq!(scan.rooms[0].missing_indices, vec![7]);
    let parts = scan.rooms[0].core_parts_summary.as_ref().unwrap();
    assert_eq!(parts[0].status, "partial (22/23)");
    assert_eq!(parts[1].status, "not installed");

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn ttpw_20bit_lossless_roundtrip_and_arbitrary_range_seek() {
    use sd3_core::ttpw;

    let frames = 4115; // exercises multiple sections + non-aligned tail
    let scale = (1u64 << 19) as f32; // 20-bit signed full scale = 524,288
    let mut ch0 = vec![0.0f32; frames];
    let mut ch1 = vec![0.0f32; frames];
    for i in 0..frames {
        if i >= 3700 {
            // exact zero section (B = 0)
            ch0[i] = 0.0;
            ch1[i] = 0.0;
        } else {
            let t = i as f32 / 44_100.0;
            let env = (-t * 45.0).exp();
            let q0 = ((0.75 * env * (TAU * 220.0 * t).sin()) * scale).round();
            let q1 = ((0.60 * env * (TAU * 330.0 * t).cos()) * scale).round();
            ch0[i] = q0 / scale;
            ch1[i] = q1 / scale;
        }
    }

    let orig = AudioBuffer {
        sample_rate: 44_100,
        channels: vec![ch0, ch1],
    };
    let ttpw_bytes = ttpw::encode_ttpw_bytes(&orig, 20, 256).expect("encodes ttpw");
    assert!(ttpw::looks_like_ttpw(&ttpw_bytes));

    let hdr = ttpw::parse_ttpw_header(&ttpw_bytes).expect("parses ttpw header");
    assert_eq!(hdr.channels, 2);
    assert_eq!(hdr.bits_per_sample, 20);
    assert_eq!(hdr.total_frames as usize, frames);
    assert!(hdr.sections.len() >= 4);

    let full = ttpw::decode_ttpw(&ttpw_bytes).expect("decodes full ttpw");
    assert_eq!(full.num_frames(), frames);
    for f in 0..frames {
        assert_eq!(full.channels[0][f], orig.channels[0][f], "L mismatch at {f}");
        assert_eq!(full.channels[1][f], orig.channels[1][f], "R mismatch at {f}");
    }

    // Verify O(log nsec) random-access range decoder at unaligned start frames & lengths
    for &(start, count) in &[
        (0usize, 64usize),
        (1, 31),
        (3, 129),
        (31, 66),
        (32, 64),
        (255, 515),
        (1021, 777),
        (3650, 500),
    ] {
        let mut dl = vec![-9.0f32; count];
        let mut dr = vec![-9.0f32; count];
        let got =
            ttpw::decode_ttpw_range_stereo(&ttpw_bytes, &hdr, start, count, &mut dl, &mut dr)
                .unwrap();
        let expected_got = count.min(frames.saturating_sub(start));
        assert_eq!(got, expected_got);
        for i in 0..got {
            assert_eq!(dl[i], orig.channels[0][start + i], "range L @ {}+{i}", start);
            assert_eq!(dr[i], orig.channels[1][start + i], "range R @ {}+{i}", start);
        }
    }
}

#[test]
fn ttbw_oidx_xor_mask_and_selective_extraction() {
    use sd3_core::kit::MicBus;
    use sd3_core::ttbw::{self, TtbwGroupBuildSpec};
    use sd3_core::ttpw;

    let dir = temp_test_dir("ttbw_extract");
    let make_hit = |amp: f32, freq: f32| -> Vec<u8> {
        let n = 600;
        let l: Vec<f32> = (0..n)
            .map(|i| amp * (TAU * freq * (i as f32 / 44_100.0)).sin())
            .collect();
        let r: Vec<f32> = (0..n)
            .map(|i| amp * 0.9 * (TAU * freq * (i as f32 / 44_100.0)).cos())
            .collect();
        ttpw::encode_ttpw_bytes(
            &AudioBuffer {
                sample_rate: 44_100,
                channels: vec![l, r],
            },
            20,
            128,
        )
        .unwrap()
    };

    let s_oh_a1 = make_hit(0.05, 410.0);
    let s_oh_m1 = make_hit(0.65, 420.0);
    let s_oh_m2 = make_hit(0.68, 425.0);
    let s_amb_a1 = make_hit(0.03, 310.0);
    let s_amb_m1 = make_hit(0.45, 320.0);
    let s_mt_m1 = make_hit(0.50, 390.0);

    let obw_bytes = ttbw::build_ttbw_bytes(
        &["OH451", "Amb67"],
        &[
            TtbwGroupBuildSpec {
                group_name: "CR01_02_DS_CR_R",
                samples: vec![
                    (0, 'A', 1, &s_oh_a1),
                    (0, 'M', 1, &s_oh_m1),
                    (0, 'M', 2, &s_oh_m2),
                    (1, 'A', 1, &s_amb_a1),
                    (1, 'M', 1, &s_amb_m1),
                ],
            },
            TtbwGroupBuildSpec {
                group_name: "CR01_02_DS_MT_R",
                samples: vec![(0, 'M', 1, &s_mt_m1)],
            },
        ],
        0x0000_00C8,
    );

    let obw_path = dir.join("soundb12.obw");
    fs::write(&obw_path, &obw_bytes).unwrap();

    let idx = ttbw::parse_ttbw_file_index(&obw_path).unwrap();
    assert_eq!(idx.xor_mask, 0x0000_00C8);
    assert_eq!(idx.mics, vec!["OH451", "Amb67"]);
    assert_eq!(idx.mic_buses, vec![MicBus::Oh, MicBus::Room]);
    assert_eq!(idx.groups.len(), 2);
    assert_eq!(idx.total_records, 6);
    // Sustain crash is member of choke group #1; mute crash actively triggers choke group #1!
    assert_eq!(idx.groups[0].member_choke_group, Some(1));
    assert_eq!(idx.groups[0].triggers_choke_group, None);
    assert_eq!(idx.groups[1].triggers_choke_group, Some(1));

    // Selective extraction: filter to `CR_R`, mic `OH451`, max_rr 1 -> exactly 2 WAVs (`A01` & `M01`)!
    let out_sel = dir.join("sel_out");
    let sum = obw::extract_to_directory(
        &obw_path,
        &out_sel,
        ExtractOptions {
            filter: Some("CR_R".into()),
            mic_filter: Some("OH451".into()),
            max_rr: Some(1),
            ..ExtractOptions::default()
        },
    )
    .unwrap();
    assert_eq!(sum.decoded_wav_count, 2);

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn real_sl_death_bundle_verification_if_present() {
    use sd3_core::{ttbw, ttpw};

    let bundle = PathBuf::from("/home/user/sd3_bundle");
    let oidx_path = bundle.join("oidx/soundb12.obw.oidx");
    if !oidx_path.is_file() {
        return;
    }

    let oidx_bytes = fs::read(&oidx_path).unwrap();
    let idx = ttbw::parse_oidx_payload(&oidx_bytes).unwrap();
    assert_eq!(idx.mics, vec!["OH451", "Amb67"]);
    assert_eq!(idx.xor_mask, 0x0000_00C8);
    assert_eq!(idx.groups.len(), 15);
    assert_eq!(idx.total_records, 1032);

    // Every single one of the 15 articulation groups must receive a unique playable MIDI note
    let mut seen_notes = std::collections::BTreeSet::new();
    for g in &idx.groups {
        assert!(
            seen_notes.insert(g.suggested_note),
            "duplicate note {} on {}",
            g.suggested_note,
            g.name
        );
    }

    for (fname, expected_frames, expected_secs) in [
        ("soundb12.obw__CR01_02_DS_MT_R_m0_A01.ttpw", 87_572usize, 6usize),
        ("soundb12.obw__CR01_04_DS_MT_R_m0_A02.ttpw", 67_389usize, 7usize),
    ] {
        let p = bundle.join("ttpw").join(fname);
        let bytes = fs::read(&p).unwrap();
        let (hdr, int_ch) = ttpw::decode_ttpw_i32(&bytes).unwrap();
        assert_eq!(hdr.channels, 2);
        assert_eq!(hdr.sample_rate, 44_100);
        assert_eq!(hdr.bits_per_sample, 20);
        assert_eq!(hdr.total_frames as usize, expected_frames);
        assert_eq!(hdr.sections.len(), expected_secs);

        // Verify adjacent-sample lag-1 correlation > +0.83 on real studio cymbal waveform
        let corr_l = lag1_corr(&int_ch[0]);
        let corr_r = lag1_corr(&int_ch[1]);
        assert!(corr_l > 0.83 && corr_r > 0.83, "{fname}: corr=({corr_l}, {corr_r})");

        // Verify streaming 257-frame random-access chunks match full decode bit-for-bit across all frames!
        let full = ttpw::decode_ttpw(&bytes).unwrap();
        let mut dl = [0.0f32; 257];
        let mut dr = [0.0f32; 257];
        let mut cursor = 0usize;
        while cursor < expected_frames {
            let n = ttpw::decode_ttpw_range_stereo(&bytes, &hdr, cursor, 257, &mut dl, &mut dr)
                .unwrap();
            assert!(n > 0);
            assert_eq!(&dl[..n], &full.channels[0][cursor..cursor + n]);
            assert_eq!(&dr[..n], &full.channels[1][cursor..cursor + n]);
            cursor += n;
        }
    }
}

fn lag1_corr(x: &[i32]) -> f64 {
    let mut num = 0.0f64;
    let mut da = 0.0f64;
    let mut db = 0.0f64;
    for w in x.windows(2) {
        let a = w[0] as f64;
        let b = w[1] as f64;
        num += a * b;
        da += a * a;
        db += b * b;
    }
    num / (da * db).sqrt()
}
