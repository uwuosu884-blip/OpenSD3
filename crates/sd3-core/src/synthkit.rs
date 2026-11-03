//! Physical/modal-inspired multi-mic, multi-velocity, round-robin acoustic drum
//! synthesizer for:
//!   1. Immediate zero-dependency playback inside `open-sd3` (VST3/CLAP/Standalone)
//!      before an SD3 library is installed on disk, and
//!   2. Generating realistic synthetic `SL-*` libraries (`.obw` + `soundstats.dat` +
//!      `Grooves.ttfs`) and extracted `kit.json` + `.sfz` kits for testing.

use std::collections::BTreeMap;
use std::f32::consts::TAU;
use std::fs;
use std::path::Path;

use crate::kit::{DrumKitManifest, MicBus, PadSpec, RoundRobinSample, VelocityLayerSpec};
use crate::obw::{self, ObwIndexEntry};
use crate::ttfs;
use crate::wav::{self, AudioBuffer, WavSampleFormat};
use crate::Result;

#[derive(Debug, Clone, Copy)]
pub enum DrumVoiceKind {
    Kick,
    SnareCenter,
    SnareRimshot,
    Sidestick,
    HiHatClosed,
    HiHatPedal,
    HiHatOpen,
    TomHigh,
    TomMid,
    TomFloor,
    Crash,
    RideBow,
}

#[derive(Debug, Clone)]
pub struct InMemoryPad {
    pub id: String,
    pub name: String,
    pub midi_notes: Vec<u8>,
    pub group: Option<u32>,
    pub off_by_this: Option<u32>,
    pub layers: Vec<InMemoryLayer>,
}

#[derive(Debug, Clone)]
pub struct InMemoryLayer {
    pub lovel: u8,
    pub hivel: u8,
    /// `round_robins[rr][bus]` -> stereo `[Vec<f32>; 2]` at `sample_rate`.
    pub round_robins: Vec<BTreeMap<MicBus, [Vec<f32>; 2]>>,
}

struct XorShift32(u32);

impl XorShift32 {
    fn new(seed: u32) -> Self {
        Self(seed.max(1))
    }
    #[inline]
    fn next_f32(&mut self) -> f32 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.0 = x;
        ((x as f32) / (u32::MAX as f32)) * 2.0 - 1.0
    }
}

pub fn default_drum_specs() -> Vec<(&'static str, &'static str, Vec<u8>, Option<u32>, Option<u32>, DrumVoiceKind)> {
    vec![
        ("kick", "Kick", vec![36, 35], None, None, DrumVoiceKind::Kick),
        ("snare_center", "Snare Center", vec![38], None, None, DrumVoiceKind::SnareCenter),
        ("snare_rim", "Snare Rimshot", vec![40], None, None, DrumVoiceKind::SnareRimshot),
        ("sidestick", "Sidestick", vec![37], None, None, DrumVoiceKind::Sidestick),
        ("hh_closed", "Hi-Hat Closed", vec![42, 22, 61, 62], Some(1), None, DrumVoiceKind::HiHatClosed),
        ("hh_pedal", "Hi-Hat Pedal", vec![44, 21], Some(1), None, DrumVoiceKind::HiHatPedal),
        ("hh_open", "Hi-Hat Open", vec![46, 26, 24, 25, 60], Some(2), Some(1), DrumVoiceKind::HiHatOpen),
        ("tom_high", "High Tom", vec![48, 50], None, None, DrumVoiceKind::TomHigh),
        ("tom_mid", "Mid Tom", vec![45, 47], None, None, DrumVoiceKind::TomMid),
        ("tom_floor", "Floor Tom", vec![43, 41], None, None, DrumVoiceKind::TomFloor),
        ("crash1", "Crash 1", vec![49, 57, 55], None, None, DrumVoiceKind::Crash),
        ("ride_bow", "Ride Bow", vec![51, 53, 59], None, None, DrumVoiceKind::RideBow),
    ]
}

/// Synthesize a complete in-memory multi-mic (`Close`, `OH`, `Room`), 3-velocity-layer,
/// 2-round-robin 12-pad drum kit at `sample_rate`.
pub fn build_in_memory_demo_kit(sample_rate: u32) -> Vec<InMemoryPad> {
    let vel_ranges: [(u8, u8, f32); 3] = [(1, 50, 0.38), (51, 95, 0.72), (96, 127, 1.0)];
    let mut pads = Vec::new();

    for (pad_idx, (id, name, notes, group, off_by, kind)) in default_drum_specs().into_iter().enumerate() {
        let mut layers = Vec::new();
        for (l_idx, &(lovel, hivel, vel_norm)) in vel_ranges.iter().enumerate() {
            let mut rrs = Vec::new();
            for rr in 0..2usize {
                let seed = 0x9E37_79B9u32
                    .wrapping_mul((pad_idx as u32 + 1) * 31 + (l_idx as u32) * 7 + rr as u32);
                let mics = synth_hit_mics(kind, sample_rate, vel_norm, rr, seed);
                rrs.push(mics);
            }
            layers.push(InMemoryLayer {
                lovel,
                hivel,
                round_robins: rrs,
            });
        }
        pads.push(InMemoryPad {
            id: id.into(),
            name: name.into(),
            midi_notes: notes,
            group,
            off_by_this: off_by,
            layers,
        });
    }
    pads
}

fn synth_hit_mics(
    kind: DrumVoiceKind,
    sr: u32,
    vel: f32,
    rr: usize,
    seed: u32,
) -> BTreeMap<MicBus, [Vec<f32>; 2]> {
    let dur_s = match kind {
        DrumVoiceKind::Kick => 0.42,
        DrumVoiceKind::SnareCenter | DrumVoiceKind::SnareRimshot => 0.38,
        DrumVoiceKind::Sidestick | DrumVoiceKind::HiHatClosed | DrumVoiceKind::HiHatPedal => 0.16,
        DrumVoiceKind::HiHatOpen => 0.70,
        DrumVoiceKind::TomHigh | DrumVoiceKind::TomMid | DrumVoiceKind::TomFloor => 0.52,
        DrumVoiceKind::Crash | DrumVoiceKind::RideBow => 1.15,
    };
    let n = ((dur_s * sr as f32) as usize).max(512);
    let mut rng = XorShift32::new(seed);
    let rr_detune = if rr == 0 { 0.994 } else { 1.006 };
    let brightness = 0.55 + 0.45 * vel;

    let mut close_mono = vec![0.0f32; n];
    let pan: f32 = match kind {
        DrumVoiceKind::Kick | DrumVoiceKind::SnareCenter | DrumVoiceKind::SnareRimshot | DrumVoiceKind::Sidestick => 0.0,
        DrumVoiceKind::HiHatClosed | DrumVoiceKind::HiHatPedal | DrumVoiceKind::HiHatOpen => -0.38,
        DrumVoiceKind::TomHigh => -0.22,
        DrumVoiceKind::TomMid => 0.15,
        DrumVoiceKind::TomFloor => 0.42,
        DrumVoiceKind::Crash => -0.45,
        DrumVoiceKind::RideBow => 0.46,
    };

    let mut phase = 0.0f32;
    let mut lp_state = 0.0f32;
    for (i, sample) in close_mono.iter_mut().enumerate() {
        let t = i as f32 / sr as f32;
        let white = rng.next_f32();
        lp_state += 0.28 * brightness * (white - lp_state);
        let hp = white - lp_state;

        let s = match kind {
            DrumVoiceKind::Kick => {
                let f = (49.0 + 135.0 * (-t * 48.0).exp()) * rr_detune;
                phase += TAU * f / sr as f32;
                let body = phase.sin() * (-t * 9.5).exp();
                let click = lp_state * (-t * 140.0).exp() * 0.55 * brightness;
                (body * 0.9 + click) * vel
            }
            DrumVoiceKind::SnareCenter | DrumVoiceKind::SnareRimshot => {
                let rim_boost = if matches!(kind, DrumVoiceKind::SnareRimshot) { 1.35 } else { 1.0 };
                let f1 = 182.0 * rr_detune + 45.0 * (-t * 60.0).exp();
                let f2 = 330.0 * rr_detune;
                phase += TAU * f1 / sr as f32;
                let shell = (phase.sin() * 0.65 + (TAU * f2 * t).sin() * 0.35 * rim_boost) * (-t * 20.0).exp();
                let wires = (hp * 0.7 + lp_state * 0.3) * (-t * (13.0 - 3.0 * vel)).exp() * 0.72 * brightness;
                (shell * 0.65 + wires).tanh() * vel * 0.9
            }
            DrumVoiceKind::Sidestick => {
                let click = ((TAU * 1180.0 * rr_detune * t).sin() * 0.6
                    + (TAU * 2430.0 * rr_detune * t).sin() * 0.4)
                    * (-t * 55.0).exp();
                (click + hp * (-t * 90.0).exp() * 0.3) * vel * 0.8
            }
            DrumVoiceKind::HiHatClosed | DrumVoiceKind::HiHatPedal => {
                let decay = if matches!(kind, DrumVoiceKind::HiHatPedal) { 42.0 } else { 32.0 };
                let metallic = metallic_cluster(t, rr_detune, 1.0);
                (metallic * 0.55 + hp * 0.65) * (-t * decay).exp() * vel * 0.75
            }
            DrumVoiceKind::HiHatOpen => {
                let metallic = metallic_cluster(t, rr_detune, 1.0);
                (metallic * 0.6 + hp * 0.55) * (-t * 4.8).exp() * vel * 0.72
            }
            DrumVoiceKind::TomHigh | DrumVoiceKind::TomMid | DrumVoiceKind::TomFloor => {
                let base_f = match kind {
                    DrumVoiceKind::TomHigh => 145.0,
                    DrumVoiceKind::TomMid => 108.0,
                    _ => 76.0,
                } * rr_detune;
                let f = base_f + base_f * 0.35 * (-t * 35.0).exp();
                phase += TAU * f / sr as f32;
                let head = (phase.sin() + 0.22 * (phase * 1.58).sin() * (-t * 14.0).exp()) * (-t * 7.2).exp();
                let attack = lp_state * (-t * 110.0).exp() * 0.35 * brightness;
                (head * 0.85 + attack) * vel * 0.88
            }
            DrumVoiceKind::Crash => {
                let m = metallic_cluster(t, rr_detune, 0.78);
                (m * 0.55 + hp * 0.55) * (-t * 2.7).exp() * (1.0 - (-t * 400.0).exp()) * vel * 0.75
            }
            DrumVoiceKind::RideBow => {
                let m = metallic_cluster(t, rr_detune, 0.92);
                let ping = (TAU * 2850.0 * rr_detune * t).sin() * (-t * 38.0).exp() * 0.35;
                (m * 0.48 + ping + hp * 0.22 * (-t * 18.0).exp()) * (-t * 2.2).exp() * vel * 0.75
            }
        };
        *sample = s.clamp(-0.98, 0.98);
    }

    // Smooth 4 ms fade-out at end to eliminate truncation clicks
    let fade = ((0.004 * sr as f32) as usize).min(n / 4).max(16);
    for i in 0..fade {
        let g = (i as f32) / (fade as f32);
        close_mono[n - 1 - i] *= g;
    }

    let gain_l = ((1.0 - pan) * 0.5).sqrt();
    let gain_r = ((1.0 + pan) * 0.5).sqrt();
    let close_l: Vec<f32> = close_mono.iter().map(|&x| x * gain_l).collect();
    let close_r: Vec<f32> = close_mono.iter().map(|&x| x * gain_r).collect();

    // Overhead bus: ~1.4 ms air delay + gentle stereo image
    let oh_delay_l = ((0.0013 * sr as f32) as usize).max(1);
    let oh_delay_r = ((0.0017 * sr as f32) as usize).max(1);
    let mut oh_l = vec![0.0f32; n];
    let mut oh_r = vec![0.0f32; n];
    for i in 0..n {
        if i >= oh_delay_l {
            oh_l[i] = close_mono[i - oh_delay_l] * (0.55 * gain_l + 0.20 * gain_r);
        }
        if i >= oh_delay_r {
            oh_r[i] = close_mono[i - oh_delay_r] * (0.55 * gain_r + 0.20 * gain_l);
        }
    }

    // Room bus: early reflections + warm diffused stereo tail
    let taps_l: [(usize, f32); 4] = [
        ((0.008 * sr as f32) as usize, 0.34),
        ((0.017 * sr as f32) as usize, -0.24),
        ((0.029 * sr as f32) as usize, 0.19),
        ((0.043 * sr as f32) as usize, 0.13),
    ];
    let taps_r: [(usize, f32); 4] = [
        ((0.011 * sr as f32) as usize, 0.33),
        ((0.021 * sr as f32) as usize, -0.23),
        ((0.034 * sr as f32) as usize, 0.18),
        ((0.049 * sr as f32) as usize, -0.12),
    ];
    let mut room_l = vec![0.0f32; n];
    let mut room_r = vec![0.0f32; n];
    let mut rlp_l = 0.0f32;
    let mut rlp_r = 0.0f32;
    for i in 0..n {
        let mut sl = 0.0f32;
        for &(d, g) in &taps_l {
            if i >= d {
                sl += close_mono[i - d] * g;
            }
        }
        let mut sr_v = 0.0f32;
        for &(d, g) in &taps_r {
            if i >= d {
                sr_v += close_mono[i - d] * g;
            }
        }
        rlp_l += 0.22 * (sl - rlp_l);
        rlp_r += 0.22 * (sr_v - rlp_r);
        room_l[i] = rlp_l;
        room_r[i] = rlp_r;
    }

    let mut map = BTreeMap::new();
    map.insert(MicBus::Close, [close_l, close_r]);
    map.insert(MicBus::Oh, [oh_l, oh_r]);
    map.insert(MicBus::Room, [room_l, room_r]);
    map
}

#[inline]
fn metallic_cluster(t: f32, detune: f32, scale: f32) -> f32 {
    let freqs = [317.0f32, 463.0, 628.0, 847.0, 1193.0, 1685.0];
    let mut acc = 0.0f32;
    for &f in &freqs {
        let p = (TAU * f * detune * scale * t).sin();
        acc += if p >= 0.0 { 0.16 } else { -0.16 };
    }
    acc
}

/// Write a ready-to-play extracted kit (`kit.json` + `kit.sfz` + 24-bit WAVs) to `out_dir`.
pub fn write_extracted_demo_kit(out_dir: impl AsRef<Path>, sample_rate: u32) -> Result<DrumKitManifest> {
    let out_dir = out_dir.as_ref();
    fs::create_dir_all(out_dir.join("samples"))?;
    let demo = build_in_memory_demo_kit(sample_rate);
    let mut pads = Vec::new();

    for pad in demo {
        let mut layers = Vec::new();
        for layer in pad.layers {
            let mut rrs = Vec::new();
            for (rr_idx, mics) in layer.round_robins.into_iter().enumerate() {
                let mut mic_files = BTreeMap::new();
                for (bus, [ch_l, ch_r]) in mics {
                    let rel = format!(
                        "samples/{}_v{}-{}_rr{}_{}.wav",
                        pad.id,
                        layer.lovel,
                        layer.hivel,
                        rr_idx + 1,
                        bus.as_str()
                    );
                    let buf = AudioBuffer {
                        sample_rate,
                        channels: vec![ch_l, ch_r],
                    };
                    wav::write_wav_file(out_dir.join(&rel), &buf, WavSampleFormat::Pcm24)?;
                    mic_files.insert(bus, rel);
                }
                rrs.push(RoundRobinSample {
                    rr_index: rr_idx,
                    mic_files,
                });
            }
            layers.push(VelocityLayerSpec {
                lovel: layer.lovel,
                hivel: layer.hivel,
                round_robins: rrs,
            });
        }
        pads.push(PadSpec {
            id: pad.id,
            name: pad.name,
            midi_notes: pad.midi_notes,
            group: pad.group,
            off_by_this: pad.off_by_this,
            hihat_openness: None,
            natural_vel_dynamics: false,
            layers,
        });
    }

    let manifest = DrumKitManifest {
        name: "OpenSD3 Studio Reference Kit".into(),
        source_format: "synthetic-demo".into(),
        sample_rate,
        pads,
    };
    manifest.save_json(out_dir.join("kit.json"))?;
    fs::write(out_dir.join("kit.sfz"), manifest.to_sfz(None))?;
    Ok(manifest)
}

/// Write a complete synthetic `SL-OpenSD3Demo/` library directory (with `Name`, `Aversion`,
/// `micconf`, `padconf`, `Sounds/soundstats.dat`, `Sounds/soundb1.obw` encoded with Toontrack
/// 24-bit LPC+Rice blocks, and `Grooves.ttfs` containing a playable 120 BPM drum groove MIDI file).
pub fn write_synthetic_sl_library(parent_dir: impl AsRef<Path>, sample_rate: u32) -> Result<std::path::PathBuf> {
    let sl_dir = parent_dir.as_ref().join("SL-OpenSD3Demo");
    let sounds_dir = sl_dir.join("Sounds");
    let gfx_dir = sl_dir.join("GraphicsV3");
    fs::create_dir_all(&sounds_dir)?;
    fs::create_dir_all(&gfx_dir)?;

    fs::write(sl_dir.join("Name"), "OpenSD3 Studio Reference SDX\n")?;
    fs::write(sl_dir.join("Aversion"), "1.0.0\n")?;
    fs::write(
        sl_dir.join("micconf"),
        "Close\nOH Dyn\nAmb Ribbon\n",
    )?;
    fs::write(
        sl_dir.join("padconf"),
        "36=Kick\n38=Snare Center\n40=Snare Rimshot\n37=Sidestick\n42=Hi-Hat Closed\n44=Hi-Hat Pedal\n46=Hi-Hat Open\n48=High Tom\n45=Mid Tom\n43=Floor Tom\n49=Crash 1\n51=Ride Bow\n",
    )?;

    let demo = build_in_memory_demo_kit(sample_rate);
    let mut obw_items = Vec::new();
    for pad in demo {
        for layer in pad.layers {
            for (rr_idx, mics) in layer.round_robins.into_iter().enumerate() {
                for (bus, [l, r]) in mics {
                    let meta = ObwIndexEntry {
                        chunk_index: obw_items.len(),
                        pad_id: pad.id.clone(),
                        pad_name: pad.name.clone(),
                        midi_notes: pad.midi_notes.clone(),
                        group: pad.group,
                        off_by_this: pad.off_by_this,
                        lovel: layer.lovel,
                        hivel: layer.hivel,
                        rr_index: rr_idx,
                        mic_bus: bus,
                    };
                    let buf = AudioBuffer {
                        sample_rate,
                        channels: vec![l, r],
                    };
                    obw_items.push((meta, buf));
                }
            }
        }
    }

    let obw_bytes = obw::build_obw_soundbunch(&obw_items)?;
    fs::write(sounds_dir.join("soundb1.obw"), &obw_bytes)?;

    // Write a compact `soundstats.dat` table recording soundb1 size & sample count
    let mut stats = Vec::new();
    stats.extend_from_slice(b"SDST");
    stats.extend_from_slice(&1u32.to_le_bytes()); // 1 soundbunch
    stats.extend_from_slice(&(obw_items.len() as u32).to_le_bytes());
    stats.extend_from_slice(&(obw_bytes.len() as u32).to_le_bytes());
    fs::write(sounds_dir.join("soundstats.dat"), stats)?;

    // Build a 1-bar 120 BPM rock groove SMF and pack it in `Grooves.ttfs`
    let midi = build_demo_rock_groove_midi();
    let ttfs_bytes = ttfs::build_ttfs(
        "MIDI",
        &[
            ("Rock/Straight_120bpm/Variation_01.mid", &midi),
            ("README.txt", b"Synthetic Toontrack ttfs MIDI pack for OpenSD3 testing\n"),
        ],
    )?;
    fs::write(sl_dir.join("Grooves.ttfs"), ttfs_bytes)?;

    Ok(sl_dir)
}

/// Build a valid Format-0 Standard MIDI File (1 bar of 4/4 @ 120 BPM: kick, snare, 8th hi-hats, open hat + crash).
pub fn build_demo_rock_groove_midi() -> Vec<u8> {
    let ppq: u16 = 480;
    // Events: (tick, status, data1, data2)
    let events: &[(u32, u8, u8, u8)] = &[
        (0, 0x99, 36, 112),   // Beat 1: Kick + Crash + Closed HH
        (0, 0x99, 49, 104),
        (60, 0x89, 36, 0),
        (60, 0x89, 49, 0),
        (240, 0x99, 42, 82),  // 1-and: Closed HH
        (300, 0x89, 42, 0),
        (480, 0x99, 38, 116), // Beat 2: Snare + Closed HH
        (480, 0x99, 42, 95),
        (540, 0x89, 38, 0),
        (540, 0x89, 42, 0),
        (720, 0x99, 42, 78),  // 2-and: Closed HH + Kick pickup
        (720, 0x99, 36, 96),
        (780, 0x89, 42, 0),
        (780, 0x89, 36, 0),
        (960, 0x99, 36, 114), // Beat 3: Kick + Closed HH
        (960, 0x99, 42, 94),
        (1020, 0x89, 36, 0),
        (1020, 0x89, 42, 0),
        (1200, 0x99, 46, 100), // 3-and: Open HH!
        (1260, 0x89, 46, 0),
        (1440, 0x99, 38, 120), // Beat 4: Snare Rimshot + Closed HH (chokes Open HH!)
        (1440, 0x99, 42, 102),
        (1500, 0x89, 38, 0),
        (1500, 0x89, 42, 0),
        (1680, 0x99, 48, 92),  // 4-and: High Tom fill
        (1740, 0x89, 48, 0),
        (1800, 0x99, 43, 105), // 4-a: Floor Tom
        (1900, 0x89, 43, 0),
    ];

    let mut trk = Vec::new();
    // Delta 0: Tempo = 500_000 us/qn (120 BPM)
    trk.extend_from_slice(&[0x00, 0xFF, 0x51, 0x03, 0x07, 0xA1, 0x20]);
    // Delta 0: Time Signature 4/4
    trk.extend_from_slice(&[0x00, 0xFF, 0x58, 0x04, 0x04, 0x02, 0x18, 0x08]);

    let mut prev_tick = 0u32;
    for &(tick, st, d1, d2) in events {
        let delta = tick.saturating_sub(prev_tick);
        prev_tick = tick;
        push_vlq(&mut trk, delta);
        trk.extend_from_slice(&[st, d1, d2]);
    }
    // End of track at bar end (tick 1920)
    push_vlq(&mut trk, 1920u32.saturating_sub(prev_tick));
    trk.extend_from_slice(&[0xFF, 0x2F, 0x00]);

    let mut smf = Vec::with_capacity(22 + trk.len());
    smf.extend_from_slice(b"MThd");
    smf.extend_from_slice(&6u32.to_be_bytes());
    smf.extend_from_slice(&0u16.to_be_bytes());
    smf.extend_from_slice(&1u16.to_be_bytes());
    smf.extend_from_slice(&ppq.to_be_bytes());
    smf.extend_from_slice(b"MTrk");
    smf.extend_from_slice(&(trk.len() as u32).to_be_bytes());
    smf.extend_from_slice(&trk);
    smf
}

fn push_vlq(out: &mut Vec<u8>, mut val: u32) {
    let mut bytes = [0u8; 4];
    let mut len = 1usize;
    bytes[0] = (val & 0x7F) as u8;
    val >>= 7;
    while val > 0 {
        bytes[len] = ((val & 0x7F) as u8) | 0x80;
        len += 1;
        val >>= 7;
    }
    for i in (0..len).rev() {
        out.push(bytes[i]);
    }
}
