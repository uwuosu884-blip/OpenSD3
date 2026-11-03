//! Minimal example for third-party Rust audio plugin developers showing how to:
//! 1. Discover Superior Drummer 3 (`SL-*`, `SL/<ROOM>`, `EZX*`) soundbanks in a folder;
//! 2. Index a room's drum pieces (`Kick`, `Snare`, `Hi-Hat`, `Toms`, `Ride`, `Cymbals`, `Tool`) in < 80 ms;
//! 3. Decode an individual 20-bit `'ttpw'` sample from an `.obw` soundbunch into stereo `f32` audio.
//!
//! Run with:
//!   cargo run -p sd3-core --example minimal_plugin_loader -- /path/to/SL-Death

use std::env;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;

use sd3_core::library::{self, KitSelection, RoomCatalog};
use sd3_core::ttpw;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let target = env::args()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("Usage: minimal_plugin_loader <Toontrack-or-SL-folder>")?;

    let banks = library::scan_soundbanks_fast(&target)?;
    println!("Discovered {} soundbank(s):", banks.len());
    for b in &banks {
        println!("  - {} -> {}", b.summary_label(), b.room_path.display());
    }

    let Some(first_bank) = banks.first() else {
        return Ok(());
    };

    let catalog = RoomCatalog::scan_bank(first_bank, |done, total, fname| {
        println!("  Indexing [{done}/{total}] {fname}");
    })?;

    println!("\nCatalog for '{}':", catalog.bank.display_name);
    println!("  Total indexed samples : {}", catalog.total_indexed_samples);
    println!("  Tools   : {:?}", catalog.tools.iter().map(|o| &o.label).collect::<Vec<_>>());
    println!("  Kicks   : {:?}", catalog.kicks.iter().map(|o| &o.label).collect::<Vec<_>>());
    println!("  Snares  : {:?}", catalog.snares.iter().map(|o| &o.label).collect::<Vec<_>>());
    println!("  Hi-Hats : {:?}", catalog.hihats.iter().map(|o| &o.label).collect::<Vec<_>>());
    println!("  Toms    : {:?}", catalog.toms.iter().map(|o| &o.label).collect::<Vec<_>>());
    println!("  Cymbals : {:?}", catalog.cymbals.iter().map(|o| &o.label).collect::<Vec<_>>());

    let default_sel = KitSelection::default_for_catalog(&catalog);
    println!("  Default KitSelection: {default_sel:?}");

    // Decode the first sample of the first `.obw` entry as a quick demonstration
    if let Some(entry) = catalog.entries.iter().find(|e| e.ttbw_index.is_some()) {
        let idx = entry.ttbw_index.as_ref().unwrap();
        if let Some(grp) = idx.groups.first() {
            if let Some(mh) = grp.mics.first() {
                if let Some(rec) = mh.records.first() {
                    let mut f = File::open(&entry.path)?;
                    f.seek(SeekFrom::Start(idx.data_payload_offset + rec.data_offset as u64))?;
                    let mut raw_ttpw = vec![0u8; rec.byte_size as usize];
                    f.read_exact(&mut raw_ttpw)?;

                    let hdr = ttpw::parse_ttpw_header(&raw_ttpw)?;
                    let mut head_l = vec![0.0f32; 512];
                    let mut head_r = vec![0.0f32; 512];
                    let n = ttpw::decode_ttpw_range_stereo(&raw_ttpw, &hdr, 0, 512, &mut head_l, &mut head_r)?;
                    println!(
                        "\nStreamed {n}/{} frames from {} :: {} ({}{:02} @ {} Hz, {}-bit)",
                        hdr.total_frames,
                        entry.file_name,
                        grp.name,
                        rec.vel_char,
                        rec.rr_num,
                        hdr.sample_rate,
                        hdr.bits_per_sample
                    );
                }
            }
        }
    }

    Ok(())
}
