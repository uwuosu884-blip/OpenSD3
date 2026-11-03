use std::f32::consts::TAU;
use std::fs;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use sd3_core::ttbw::{self, TtbwGroupBuildSpec};
use sd3_core::ttpw;
use sd3_core::wav::AudioBuffer;
use sd3_dsp::{DrumEngine, LoadedKit, MixerGains, DEFAULT_HEAD_FRAMES};

#[test]
fn mmap_ttbw_realtime_streaming_and_cymbal_mute_choke() {
    let tmp = std::env::temp_dir().join(format!("opensd3_dsp_stream_{}", std::process::id()));
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp).unwrap();

    // Create a 14,000-frame (> DEFAULT_HEAD_FRAMES = 2,048) stereo cymbal hit so frames
    // 2,048..14,000 MUST come from the Mmap + `open-sd3-streamer` SPSC ring buffer!
    let frames = 14_000usize;
    let left: Vec<f32> = (0..frames)
        .map(|i| 0.55 * (TAU * 380.0 * (i as f32 / 44_100.0)).sin())
        .collect();
    let right: Vec<f32> = (0..frames)
        .map(|i| 0.50 * (TAU * 392.0 * (i as f32 / 44_100.0)).cos())
        .collect();
    let crash_ttpw = ttpw::encode_ttpw_bytes(
        &AudioBuffer {
            sample_rate: 44_100,
            channels: vec![left, right],
        },
        20,
        512,
    )
    .unwrap();

    // Very short mute sample (`_MT_`) that chokes the ringing `_CR_` sustain voice
    let mute_ttpw = ttpw::encode_ttpw_bytes(
        &AudioBuffer {
            sample_rate: 44_100,
            channels: vec![vec![0.0; 128], vec![0.0; 128]],
        },
        20,
        64,
    )
    .unwrap();

    let obw_bytes = ttbw::build_ttbw_bytes(
        &["OH451", "Amb67"],
        &[
            TtbwGroupBuildSpec {
                group_name: "CR01_02_DS_CR_R",
                samples: vec![(0, 'M', 1, &crash_ttpw), (1, 'M', 1, &crash_ttpw)],
            },
            TtbwGroupBuildSpec {
                group_name: "CR01_02_DS_MT_R",
                samples: vec![(0, 'M', 1, &mute_ttpw)],
            },
        ],
        0x0000_00C8,
    );

    let obw_path = tmp.join("soundb12.obw");
    fs::write(&obw_path, &obw_bytes).unwrap();

    let kit = LoadedKit::load_from_path(&obw_path).expect("loads RIFF 'ttbw' directly via mmap");
    assert_eq!(kit.pads.len(), 2);
    assert_eq!(kit.streamed_samples, 2);
    let crash_note = kit.pads[0].primary_note;
    let mute_note = kit.pads[1].primary_note;

    // 1. Offline engine: deterministic synchronous tail decode across all 14,000 frames
    {
        let mut off_eng = DrumEngine::new_offline(Arc::clone(&kit), 44_100.0);
        let mixer = MixerGains::default();
        off_eng.note_on(crash_note, 0.95, &mixer, None);
        let mut l = vec![0.0f32; 12_000];
        let mut r = vec![0.0f32; 12_000];
        off_eng.process_block(&mut l, &mut r, &mixer, None);
        let tail_energy: f32 = l[DEFAULT_HEAD_FRAMES + 500..11_500]
            .iter()
            .map(|x| x.abs())
            .sum();
        assert!(
            tail_energy > 500.0,
            "offline streamed tail had insufficient energy: {tail_energy}"
        );
    }

    // 2. Real-time engine: background `open-sd3-streamer` thread feeds SPSC ring buffer
    {
        let mut rt_eng = DrumEngine::new(Arc::clone(&kit), 44_100.0);
        let mixer = MixerGains::default();
        rt_eng.note_on(crash_note, 0.95, &mixer, None);

        // Give the background streamer a few milliseconds to prefill the 4,096-frame ring
        thread::sleep(Duration::from_millis(15));

        let mut buf_l = [0.0f32; 256];
        let mut buf_r = [0.0f32; 256];
        let mut streamed_tail_peak = 0.0f32;

        // Render 40 blocks of 256 frames = 10,240 frames (well past 2,048 head AND 4,096 initial ring!)
        for blk in 0..40 {
            let p = rt_eng.process_block(&mut buf_l, &mut buf_r, &mixer, None);
            if blk >= 12 {
                streamed_tail_peak = streamed_tail_peak.max(p);
            }
            if blk % 8 == 7 {
                thread::sleep(Duration::from_millis(5));
            }
        }
        assert!(
            streamed_tail_peak > 0.25,
            "real-time background streamer tail peak too low: {streamed_tail_peak}"
        );

        // Trigger `_MT_` mute choke note -> ringing crash must fade to 0 within ~15 ms (1024 frames)
        rt_eng.note_on(mute_note, 0.90, &mixer, None);
        for _ in 0..6 {
            rt_eng.process_block(&mut buf_l, &mut buf_r, &mixer, None);
        }
        let post_choke_peak = rt_eng.process_block(&mut buf_l, &mut buf_r, &mixer, None);
        assert!(
            post_choke_peak < 1e-4,
            "cymbal mute failed to choke sustain crash: peak={post_choke_peak}"
        );
    }

    let _ = fs::remove_dir_all(&tmp);
}

#[test]
fn cc4_variable_hihat_openness_and_pedal_stomp_choke() {
    let kit = LoadedKit::built_in_demo(44_100);
    let mut eng = DrumEngine::new_offline(kit, 44_100.0);
    let mixer = MixerGains::default();

    // CC4 = 0 (pedal up / wide open) + hit GM Closed HH note 42 -> steers to Open HH!
    eng.control_change(4, 0);
    eng.note_on(42, 0.9, &mixer, None);

    let mut l = [0.0f32; 512];
    let mut r = [0.0f32; 512];
    for _ in 0..20 {
        eng.process_block(&mut l, &mut r, &mixer, None);
    }
    // Open HH is still ringing strongly after ~10,000 samples (~0.23s)
    let open_ring_peak = eng.process_block(&mut l, &mut r, &mixer, None);
    assert!(open_ring_peak > 0.02, "expected open hi-hat sustain, got {open_ring_peak}");

    // Stomp CC4 to 127 (foot tight closed) -> chokes open hi-hat voice to silence!
    eng.control_change(4, 127);
    for _ in 0..3 {
        eng.process_block(&mut l, &mut r, &mixer, None);
    }
    let after_stomp = eng.process_block(&mut l, &mut r, &mixer, None);
    assert!(after_stomp < 1e-4, "CC4 pedal stomp should choke open hat, got {after_stomp}");
}
