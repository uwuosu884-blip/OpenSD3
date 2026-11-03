//! OpenSD3 — Native Windows & Linux VST3, CLAP & Standalone Drum Workstation
//! for Superior Drummer 3 (`SL-*`, `SL/<ROOM>`, `EZX*`, `.obw`) & SFZ Soundbanks.
//!
//! Real-time thread architecture:
//! - Audio thread owns `DrumEngine` exclusively (zero locks, zero heap allocs,
//!   zero disk I/O — plays RAM heads immediately while `open-sd3-streamer`
//!   decodes 20-bit `ttpw` blocks from `Mmap` into lock-free `AtomicU32` rings).
//! - Background `open-sd3-loader` worker discovers Toontrack soundbank folders,
//!   indexes `oidx` headers in milliseconds, preloads active kit pieces via
//!   sequential `.obw` sweeps, and swaps `Arc<LoadedKit>` lock-free.
//! - Native non-blocking OS Folder/File dialogs (`IFileOpenDialog` COM on Windows,
//!   `zenity`/`kdialog` on Linux) + direct Win32 clipboard paste.

pub mod platform;

use std::num::NonZeroU32;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use crossbeam_channel::{bounded, unbounded, Receiver, Sender};
use nih_plug::prelude::*;
use nih_plug_egui::{
    create_egui_editor,
    egui::{self, Color32, RichText, Stroke, Vec2},
    resizable_window::ResizableWindow,
    widgets, EguiState,
};
use serde::{Deserialize, Serialize};

use sd3_core::library::{
    self, DiscoveredBank, KitSelection, PieceOption, RoomCatalog,
};
use sd3_dsp::{DrumEngine, LoadedKit, MixerGains, UiTelemetry, MAX_UI_PADS, UI_PAD_COUNT};

use crate::platform::GlobalBankRegistry;

const PEAK_DECAY_MS: f64 = 150.0;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PersistedKitState {
    pub path: String,
    pub selection: Option<KitSelection>,
}

#[derive(Clone, Debug)]
enum LoaderCommand {
    AutoDiscoverStartup,
    AutoDiscoverAllDrives,
    ScanAndLoadPath {
        path: PathBuf,
        preferred_selection: Option<KitSelection>,
        remember_in_registry: bool,
    },
    SelectDiscoveredBank(usize),
    ApplyKitSelection(KitSelection),
    LoadBuiltIn(u32),
}

#[derive(Clone, Debug, Default)]
pub struct CatalogUiInfo {
    pub bank_id: String,
    pub tools: Vec<PieceOption>,
    pub kicks: Vec<PieceOption>,
    pub snares: Vec<PieceOption>,
    pub hihats: Vec<PieceOption>,
    pub toms: Vec<PieceOption>,
    pub rides: Vec<PieceOption>,
    pub cymbals: Vec<PieceOption>,
    pub selection: KitSelection,
}

#[derive(Clone, Debug)]
pub struct PadUiItem {
    pub name: String,
    pub primary_note: u8,
    pub alias_label: String,
}

#[derive(Clone, Debug)]
pub struct KitUiSnapshot {
    pub kit_name: String,
    pub source_desc: String,
    pub status_msg: String,
    pub is_error: bool,
    pub loading_progress: Option<f32>,
    pub discovered_banks: Vec<DiscoveredBank>,
    pub active_bank_idx: Option<usize>,
    pub catalog_ui: Option<CatalogUiInfo>,
    pub bus_mic_summary: [String; 4],
    pub active_path_display: String,
    pub pads: Vec<PadUiItem>,
}

impl Default for KitUiSnapshot {
    fn default() -> Self {
        let demo = LoadedKit::built_in_demo(44_100);
        Self::from_loaded(
            &demo,
            "Ready — Click 'Locate Soundbank Folder...' to select your Superior Drummer (SL-*) or Toontrack directory.".into(),
            Vec::new(),
            None,
            None,
            String::new(),
        )
    }
}

impl KitUiSnapshot {
    fn from_loaded(
        kit: &LoadedKit,
        status_msg: String,
        discovered_banks: Vec<DiscoveredBank>,
        active_bank_idx: Option<usize>,
        catalog_ui: Option<CatalogUiInfo>,
        active_path_display: String,
    ) -> Self {
        let mut pads: Vec<PadUiItem> = kit
            .pads
            .iter()
            .take(MAX_UI_PADS)
            .map(|p| {
                let alias_label = if p.midi_notes.len() <= 1 {
                    format!("MIDI {}", p.primary_note)
                } else {
                    let extras: Vec<String> = p.midi_notes[1..]
                        .iter()
                        .take(3)
                        .map(|n| n.to_string())
                        .collect();
                    format!("MIDI {} (+{})", p.primary_note, extras.join(","))
                };
                PadUiItem {
                    name: p.name.clone(),
                    primary_note: p.primary_note,
                    alias_label,
                }
            })
            .collect();
        if pads.is_empty() {
            pads.push(PadUiItem {
                name: "Empty".into(),
                primary_note: 0,
                alias_label: "—".into(),
            });
        }
        Self {
            kit_name: kit.name.clone(),
            source_desc: kit.source_desc.clone(),
            status_msg,
            is_error: false,
            loading_progress: None,
            discovered_banks,
            active_bank_idx,
            catalog_ui,
            bus_mic_summary: kit.bus_mic_summary.clone(),
            active_path_display,
            pads,
        }
    }
}

struct EditorGuiState {
    path_input: String,
    last_synced_path: String,
    audition_vel: f32,
    pad_page: usize,
    pad_glow: [f32; MAX_UI_PADS],
    dialog_busy: Arc<AtomicBool>,
    dialog_rx: Receiver<PathBuf>,
    dialog_tx: Sender<PathBuf>,
}

pub struct OpenSd3Plugin {
    params: Arc<OpenSd3Params>,
    engine: DrumEngine,
    telemetry: Arc<UiTelemetry>,
    ui_snapshot: Arc<Mutex<KitUiSnapshot>>,

    kit_rx: Receiver<Arc<LoadedKit>>,
    garbage_tx: Sender<Arc<LoadedKit>>,
    loader_tx: Sender<LoaderCommand>,
    audition_rx: Receiver<(u8, f32)>,
    audition_tx: Sender<(u8, f32)>,

    peak_meter: Arc<AtomicF32>,
    peak_decay_weight: f32,
    sample_rate: u32,
}

#[derive(Params)]
pub struct OpenSd3Params {
    #[persist = "editor-state"]
    editor_state: Arc<EguiState>,

    #[persist = "kit-path"]
    persisted_kit_path: Mutex<String>,

    #[persist = "kit-state-v3"]
    persisted_state: Mutex<PersistedKitState>,

    #[id = "close_gain"]
    pub close_gain: FloatParam,

    #[id = "oh_gain"]
    pub oh_gain: FloatParam,

    #[id = "room_gain"]
    pub room_gain: FloatParam,

    #[id = "bleed_gain"]
    pub bleed_gain: FloatParam,

    #[id = "master_gain"]
    pub master_gain: FloatParam,

    #[id = "vel_floor"]
    pub vel_floor: FloatParam,

    #[id = "humanize"]
    pub humanize: FloatParam,
}

impl Default for OpenSd3Params {
    fn default() -> Self {
        let gain_param = |name: &str, default_db: f32| {
            FloatParam::new(
                name,
                util::db_to_gain(default_db),
                FloatRange::Skewed {
                    min: util::db_to_gain(-48.0),
                    max: util::db_to_gain(24.0),
                    factor: FloatRange::gain_skew_factor(-48.0, 24.0),
                },
            )
            .with_smoother(SmoothingStyle::Logarithmic(25.0))
            .with_unit(" dB")
            .with_value_to_string(formatters::v2s_f32_gain_to_db(1))
            .with_string_to_value(formatters::s2v_f32_gain_to_db())
        };

        Self {
            editor_state: EguiState::from_size(880, 610),
            persisted_kit_path: Mutex::new(String::new()),
            persisted_state: Mutex::new(PersistedKitState::default()),
            close_gain: gain_param("Close Mics", 0.0),
            oh_gain: gain_param("Overheads", -2.5),
            room_gain: gain_param("Room / Amb", -6.0),
            bleed_gain: gain_param("Bleed", -12.0),
            master_gain: gain_param("Master", -1.0),
            vel_floor: FloatParam::new("Vel Floor", 0.0, FloatRange::Linear { min: 0.0, max: 1.0 })
                .with_unit("%")
                .with_value_to_string(formatters::v2s_f32_percentage(0)),
            humanize: FloatParam::new("Humanize", 0.15, FloatRange::Linear { min: 0.0, max: 1.0 })
                .with_unit("%")
                .with_value_to_string(formatters::v2s_f32_percentage(0)),
        }
    }
}

impl Default for OpenSd3Plugin {
    fn default() -> Self {
        let params = Arc::new(OpenSd3Params::default());
        let initial_kit = LoadedKit::built_in_demo(44_100);
        let ui_snapshot = Arc::new(Mutex::new(KitUiSnapshot::default()));

        let (loader_tx, loader_rx) = unbounded::<LoaderCommand>();
        let (kit_tx, kit_rx) = bounded::<Arc<LoadedKit>>(4);
        let (garbage_tx, garbage_rx) = bounded::<Arc<LoadedKit>>(8);
        let (audition_tx, audition_rx) = bounded::<(u8, f32)>(64);

        let ui_for_worker = ui_snapshot.clone();
        let params_for_worker = params.clone();

        thread::Builder::new()
            .name("open-sd3-loader".into())
            .spawn(move || {
                run_loader_worker(
                    loader_rx,
                    kit_tx,
                    garbage_rx,
                    ui_for_worker,
                    params_for_worker,
                );
            })
            .expect("spawn open-sd3 loader thread");

        // Discover previously saved / system Toontrack soundbanks in the background right away
        let _ = loader_tx.send(LoaderCommand::AutoDiscoverStartup);

        Self {
            params,
            engine: DrumEngine::new(initial_kit, 44_100.0),
            telemetry: Arc::new(UiTelemetry::default()),
            ui_snapshot,
            kit_rx,
            garbage_tx,
            loader_tx,
            audition_rx,
            audition_tx,
            peak_meter: Arc::new(AtomicF32::new(util::MINUS_INFINITY_DB)),
            peak_decay_weight: 0.99,
            sample_rate: 44_100,
        }
    }
}

fn run_loader_worker(
    loader_rx: Receiver<LoaderCommand>,
    kit_tx: Sender<Arc<LoadedKit>>,
    garbage_rx: Receiver<Arc<LoadedKit>>,
    ui_snapshot: Arc<Mutex<KitUiSnapshot>>,
    params: Arc<OpenSd3Params>,
) {
    let mut registry = GlobalBankRegistry::load();
    let mut banks: Vec<DiscoveredBank> = Vec::new();
    let mut active_bank_idx: Option<usize> = None;
    let mut active_catalog: Option<Arc<RoomCatalog>> = None;

    let set_progress = |frac: f32, msg: &str| {
        if let Ok(mut snap) = ui_snapshot.lock() {
            snap.loading_progress = Some(frac.clamp(0.0, 1.0));
            snap.is_error = false;
            snap.status_msg = msg.to_owned();
        }
    };

    let set_error = |msg: String| {
        if let Ok(mut snap) = ui_snapshot.lock() {
            snap.loading_progress = None;
            snap.is_error = true;
            snap.status_msg = msg;
        }
    };

    let merge_banks = |dest: &mut Vec<DiscoveredBank>, incoming: Vec<DiscoveredBank>| -> Option<usize> {
        let mut first_idx = None;
        for b in incoming {
            let key = b.room_path.to_string_lossy().to_ascii_lowercase();
            if let Some(pos) = dest
                .iter()
                .position(|x| x.room_path.to_string_lossy().to_ascii_lowercase() == key)
            {
                dest[pos] = b;
                if first_idx.is_none() {
                    first_idx = Some(pos);
                }
            } else {
                let pos = dest.len();
                dest.push(b);
                if first_idx.is_none() {
                    first_idx = Some(pos);
                }
            }
        }
        first_idx
    };

    loop {
        while garbage_rx.try_recv().is_ok() {}
        let Ok(cmd) = loader_rx.recv() else {
            break;
        };
        while garbage_rx.try_recv().is_ok() {}

        match cmd {
            LoaderCommand::AutoDiscoverStartup => {
                if !registry.search_roots.is_empty() {
                    let found = library::auto_discover_system_banks(&registry.search_roots);
                    let _ = merge_banks(&mut banks, found);
                    if let Ok(mut snap) = ui_snapshot.lock() {
                        snap.discovered_banks = banks.clone();
                        if !banks.is_empty() && snap.active_bank_idx.is_none() {
                            snap.status_msg = format!(
                                "Discovered {} Toontrack soundbank(s). Pick one from the Soundbank menu or click 'Locate Soundbank Folder...'.",
                                banks.len()
                            );
                        }
                    }
                }
            }

            LoaderCommand::AutoDiscoverAllDrives => {
                set_progress(0.15, "Scanning drives for Toontrack / SL-* / EZX soundbanks...");
                let found = library::auto_discover_system_banks(&registry.search_roots);
                let first_new = merge_banks(&mut banks, found);
                if banks.is_empty() {
                    set_error(
                        "No standard Toontrack folders found automatically — click 'Locate Soundbank Folder...' and select your SL-* folder.".into(),
                    );
                } else if active_bank_idx.is_none() {
                    if let Some(idx) = first_new {
                        let _ = loader_LoadBankHelper(
                            idx,
                            None,
                            &banks,
                            &mut active_bank_idx,
                            &mut active_catalog,
                            &mut registry,
                            &kit_tx,
                            &ui_snapshot,
                            &params,
                        );
                    }
                } else if let Ok(mut snap) = ui_snapshot.lock() {
                    snap.loading_progress = None;
                    snap.discovered_banks = banks.clone();
                    snap.status_msg = format!("Found {} Superior Drummer / EZX soundbank(s).", banks.len());
                }
            }

            LoaderCommand::LoadBuiltIn(sr) => {
                active_bank_idx = None;
                active_catalog = None;
                if let Ok(mut p) = params.persisted_kit_path.lock() {
                    p.clear();
                }
                if let Ok(mut st) = params.persisted_state.lock() {
                    *st = PersistedKitState::default();
                }
                let kit = LoadedKit::built_in_demo(sr);
                if let Ok(mut snap) = ui_snapshot.lock() {
                    *snap = KitUiSnapshot::from_loaded(
                        &kit,
                        "Loaded Built-in Studio Reference Kit.".into(),
                        banks.clone(),
                        None,
                        None,
                        String::new(),
                    );
                }
                let _ = kit_tx.send(kit);
            }

            LoaderCommand::SelectDiscoveredBank(idx) => {
                if idx < banks.len() {
                    let _ = loader_LoadBankHelper(
                        idx,
                        None,
                        &banks,
                        &mut active_bank_idx,
                        &mut active_catalog,
                        &mut registry,
                        &kit_tx,
                        &ui_snapshot,
                        &params,
                    );
                }
            }

            LoaderCommand::ApplyKitSelection(sel) => {
                if let Some(ref cat) = active_catalog {
                    let cat_clone = Arc::clone(cat);
                    set_progress(0.05, &format!("Updating kit pieces in {}...", cat_clone.bank.display_name));
                    match LoadedKit::load_room_selection(&cat_clone, &sel, |frac, msg| {
                        set_progress(0.05 + 0.95 * frac, msg);
                    }) {
                        Ok(kit) => {
                            let path_str = cat_clone.bank.room_path.to_string_lossy().into_owned();
                            if let Ok(mut st) = params.persisted_state.lock() {
                                st.path = path_str.clone();
                                st.selection = Some(sel.clone());
                            }
                            let cat_ui = build_catalog_ui(&cat_clone, sel);
                            let msg = format!(
                                "Ready: {} — {} pads, {} samples ({} mmap-streamed)",
                                kit.name,
                                kit.pads.len(),
                                kit.total_samples,
                                kit.streamed_samples
                            );
                            if let Ok(mut snap) = ui_snapshot.lock() {
                                *snap = KitUiSnapshot::from_loaded(
                                    &kit,
                                    msg,
                                    banks.clone(),
                                    active_bank_idx,
                                    Some(cat_ui),
                                    path_str,
                                );
                            }
                            let _ = kit_tx.send(kit);
                        }
                        Err(e) => set_error(format!("Kit update failed: {e}")),
                    }
                }
            }

            LoaderCommand::ScanAndLoadPath {
                path,
                preferred_selection,
                remember_in_registry,
            } => {
                if !path.exists() {
                    set_error(format!("Path not found: {}", path.display()));
                    continue;
                }

                if path.is_dir() {
                    set_progress(
                        0.05,
                        &format!("Scanning folder {} for Superior Drummer soundbanks...", path.display()),
                    );
                    if let Ok(found_banks) = library::scan_soundbanks_fast(&path) {
                        if !found_banks.is_empty() {
                            if remember_in_registry {
                                let norm = library::normalize_bank_search_root(&path);
                                registry.add_root(&norm);
                                registry.save();
                            }
                            let count_new = found_banks.len();
                            if let Some(target_idx) = merge_banks(&mut banks, found_banks) {
                                let _ = loader_LoadBankHelper(
                                    target_idx,
                                    preferred_selection,
                                    &banks,
                                    &mut active_bank_idx,
                                    &mut active_catalog,
                                    &mut registry,
                                    &kit_tx,
                                    &ui_snapshot,
                                    &params,
                                );
                                if count_new > 1 {
                                    if let Ok(mut snap) = ui_snapshot.lock() {
                                        snap.status_msg = format!(
                                            "{} (Found {} soundbanks in folder — switch anytime in the Soundbank menu!)",
                                            snap.status_msg, count_new
                                        );
                                    }
                                }
                                continue;
                            }
                        }
                    }
                }

                // Fallback: single `.obw`, `.sfz`, `kit.json`, `.ttpw`, or directory containing `kit.json`/`.sfz`
                set_progress(0.10, &format!("Loading {}...", path.display()));
                match LoadedKit::load_from_path_with_progress(&path, |frac, msg| {
                    set_progress(0.10 + 0.90 * frac, msg);
                }) {
                    Ok(kit) => {
                        active_bank_idx = None;
                        active_catalog = None;
                        let path_str = path.to_string_lossy().into_owned();
                        if let Ok(mut p) = params.persisted_kit_path.lock() {
                            *p = path_str.clone();
                        }
                        if let Ok(mut st) = params.persisted_state.lock() {
                            st.path = path_str.clone();
                            st.selection = None;
                        }
                        let msg = format!(
                            "Loaded '{}' — {} pads, {} samples ({} mmap-streamed)",
                            kit.name,
                            kit.pads.len(),
                            kit.total_samples,
                            kit.streamed_samples
                        );
                        if let Ok(mut snap) = ui_snapshot.lock() {
                            *snap = KitUiSnapshot::from_loaded(
                                &kit,
                                msg,
                                banks.clone(),
                                None,
                                None,
                                path_str,
                            );
                        }
                        let _ = kit_tx.send(kit);
                    }
                    Err(e) => {
                        set_error(format!("Could not load '{}': {e}", path.display()));
                    }
                }
            }
        }
    }
}

#[allow(non_snake_case)]
fn loader_LoadBankHelper(
    bank_idx: usize,
    preferred_selection: Option<KitSelection>,
    banks: &[DiscoveredBank],
    active_bank_idx: &mut Option<usize>,
    active_catalog: &mut Option<Arc<RoomCatalog>>,
    registry: &mut GlobalBankRegistry,
    kit_tx: &Sender<Arc<LoadedKit>>,
    ui_snapshot: &Arc<Mutex<KitUiSnapshot>>,
    params: &Arc<OpenSd3Params>,
) -> std::result::Result<(), ()> {
    let bank = &banks[bank_idx];
    let update_prog = |frac: f32, msg: String| {
        if let Ok(mut snap) = ui_snapshot.lock() {
            snap.loading_progress = Some(frac.clamp(0.0, 1.0));
            snap.is_error = false;
            snap.discovered_banks = banks.to_vec();
            snap.status_msg = msg;
        }
    };

    update_prog(
        0.04,
        format!(
            "Indexing soundbank '{}' ({} .obw files)...",
            bank.display_name,
            bank.obw_files.len()
        ),
    );

    let cat = match RoomCatalog::scan_bank(bank, |done, tot, fname| {
        let frac = 0.04 + 0.26 * (done as f32 / tot.max(1) as f32);
        update_prog(
            frac,
            format!(
                "Indexing '{}' ({done}/{tot}: {fname})...",
                bank.display_name
            ),
        );
    }) {
        Ok(c) => Arc::new(c),
        Err(e) => {
            if let Ok(mut snap) = ui_snapshot.lock() {
                snap.loading_progress = None;
                snap.is_error = true;
                snap.status_msg = format!("Failed to index '{}': {e}", bank.display_name);
            }
            return Err(());
        }
    };

    let sel = preferred_selection.unwrap_or_else(|| KitSelection::default_for_catalog(&cat));

    match LoadedKit::load_room_selection(&cat, &sel, |frac, msg| {
        update_prog(0.30 + 0.70 * frac, msg.to_owned());
    }) {
        Ok(kit) => {
            *active_bank_idx = Some(bank_idx);
            *active_catalog = Some(Arc::clone(&cat));

            let path_str = bank.room_path.to_string_lossy().into_owned();
            registry.last_bank_path = Some(bank.room_path.clone());
            registry.add_root(&bank.room_path);
            registry.save();

            if let Ok(mut p) = params.persisted_kit_path.lock() {
                *p = path_str.clone();
            }
            if let Ok(mut st) = params.persisted_state.lock() {
                st.path = path_str.clone();
                st.selection = Some(sel.clone());
            }

            let cat_ui = build_catalog_ui(&cat, sel);
            let msg = format!(
                "Ready: {} — {} pads, {} samples ({} mmap-streamed across {} .obw files)",
                kit.name,
                kit.pads.len(),
                kit.total_samples,
                kit.streamed_samples,
                bank.obw_files.len()
            );

            if let Ok(mut snap) = ui_snapshot.lock() {
                *snap = KitUiSnapshot::from_loaded(
                    &kit,
                    msg,
                    banks.to_vec(),
                    Some(bank_idx),
                    Some(cat_ui),
                    path_str,
                );
            }
            let _ = kit_tx.send(kit);
            Ok(())
        }
        Err(e) => {
            if let Ok(mut snap) = ui_snapshot.lock() {
                snap.loading_progress = None;
                snap.is_error = true;
                snap.discovered_banks = banks.to_vec();
                snap.status_msg = format!("Failed to load '{}': {e}", bank.display_name);
            }
            Err(())
        }
    }
}

fn build_catalog_ui(cat: &RoomCatalog, selection: KitSelection) -> CatalogUiInfo {
    CatalogUiInfo {
        bank_id: cat.bank.id.clone(),
        tools: cat.tools.clone(),
        kicks: cat.kicks.clone(),
        snares: cat.snares.clone(),
        hihats: cat.hihats.clone(),
        toms: cat.toms.clone(),
        rides: cat.rides.clone(),
        cymbals: cat.cymbals.clone(),
        selection,
    }
}

fn sanitize_user_path(raw: &str) -> String {
    raw.trim()
        .trim_matches('"')
        .trim_matches('\'')
        .trim()
        .to_owned()
}

impl Plugin for OpenSd3Plugin {
    const NAME: &'static str = "OpenSD3 Drum Workstation";
    const VENDOR: &'static str = "OpenSD3 Project";
    const URL: &'static str = "https://github.com/uwuosu884-blip/OpenSD3";
    const EMAIL: &'static str = "opensd3@localhost";
    const VERSION: &'static str = env!("CARGO_PKG_VERSION");

    const AUDIO_IO_LAYOUTS: &'static [AudioIOLayout] = &[AudioIOLayout {
        main_input_channels: None,
        main_output_channels: NonZeroU32::new(2),
        ..AudioIOLayout::const_default()
    }];

    const MIDI_INPUT: MidiConfig = MidiConfig::MidiCCs;
    const SAMPLE_ACCURATE_AUTOMATION: bool = true;

    type SysExMessage = ();
    type BackgroundTask = ();

    fn params(&self) -> Arc<dyn Params> {
        self.params.clone()
    }

    fn initialize(
        &mut self,
        _audio_io_layout: &AudioIOLayout,
        buffer_config: &BufferConfig,
        _context: &mut impl InitContext<Self>,
    ) -> bool {
        let sr = buffer_config.sample_rate.round().max(8_000.0) as u32;
        self.sample_rate = sr;
        self.engine.set_sample_rate(buffer_config.sample_rate as f64);
        self.peak_decay_weight = 0.25f64
            .powf((buffer_config.sample_rate as f64 * PEAK_DECAY_MS / 1000.0).recip())
            as f32;

        // Restore persisted Soundbank + KitSelection from DAW project state (or legacy kit-path)
        let restored = self
            .params
            .persisted_state
            .lock()
            .ok()
            .filter(|st| !sanitize_user_path(&st.path).is_empty())
            .map(|st| (sanitize_user_path(&st.path), st.selection.clone()))
            .or_else(|| {
                self.params
                    .persisted_kit_path
                    .lock()
                    .ok()
                    .map(|s| sanitize_user_path(&s))
                    .filter(|s| !s.is_empty())
                    .map(|s| (s, None))
            });

        if let Some((clean_path, pref_sel)) = restored {
            let _ = self.loader_tx.send(LoaderCommand::ScanAndLoadPath {
                path: PathBuf::from(clean_path),
                preferred_selection: pref_sel,
                remember_in_registry: false,
            });
        }
        true
    }

    fn reset(&mut self) {
        self.engine.reset();
    }

    fn editor(&mut self, _async_executor: AsyncExecutor<Self>) -> Option<Box<dyn Editor>> {
        let params = self.params.clone();
        let egui_state = params.editor_state.clone();
        let ui_snapshot = self.ui_snapshot.clone();
        let telemetry = self.telemetry.clone();
        let loader_tx = self.loader_tx.clone();
        let audition_tx = self.audition_tx.clone();
        let peak_meter = self.peak_meter.clone();
        let sr = self.sample_rate;

        let initial_path = params
            .persisted_state
            .lock()
            .ok()
            .map(|s| s.path.clone())
            .filter(|s| !s.is_empty())
            .or_else(|| params.persisted_kit_path.lock().ok().map(|s| s.clone()))
            .unwrap_or_default();

        let (dialog_tx, dialog_rx) = bounded::<PathBuf>(4);

        create_egui_editor(
            self.params.editor_state.clone(),
            EditorGuiState {
                path_input: initial_path.clone(),
                last_synced_path: initial_path,
                audition_vel: 0.85,
                pad_page: 0,
                pad_glow: [0.0; MAX_UI_PADS],
                dialog_busy: Arc::new(AtomicBool::new(false)),
                dialog_rx,
                dialog_tx,
            },
            |_, _| {},
            move |egui_ctx, setter, state| {
                egui_ctx.request_repaint_after(std::time::Duration::from_millis(33));

                // Helper to spawn the native Windows/Linux Folder Picker without blocking the DAW!
                let spawn_folder_picker =
                    |busy: &Arc<AtomicBool>, tx: &Sender<PathBuf>, snap_lock: &Arc<Mutex<KitUiSnapshot>>| {
                        if !busy.swap(true, Ordering::AcqRel) {
                            if let Ok(mut s) = snap_lock.lock() {
                                s.is_error = false;
                                s.status_msg =
                                    "Select your Superior Drummer (SL-*) or Toontrack folder in the dialog window..."
                                        .into();
                            }
                            let busy_flag = busy.clone();
                            let out_tx = tx.clone();
                            let snap_for_cancel = snap_lock.clone();
                            thread::Builder::new()
                                .name("open-sd3-folder-dlg".into())
                                .spawn(move || {
                                    if let Some(chosen) = platform::pick_folder_dialog() {
                                        let _ = out_tx.send(chosen);
                                    } else if let Ok(mut s) = snap_for_cancel.lock() {
                                        if s.status_msg.starts_with("Select your Superior Drummer") {
                                            s.status_msg = "Folder selection cancelled.".into();
                                        }
                                    }
                                    busy_flag.store(false, Ordering::Release);
                                })
                                .ok();
                        }
                    };

                let spawn_file_picker =
                    |busy: &Arc<AtomicBool>, tx: &Sender<PathBuf>, snap_lock: &Arc<Mutex<KitUiSnapshot>>| {
                        if !busy.swap(true, Ordering::AcqRel) {
                            if let Ok(mut s) = snap_lock.lock() {
                                s.is_error = false;
                                s.status_msg = "Select an .obw, .sfz, or kit.json file in the dialog window...".into();
                            }
                            let busy_flag = busy.clone();
                            let out_tx = tx.clone();
                            let snap_for_cancel = snap_lock.clone();
                            thread::Builder::new()
                                .name("open-sd3-file-dlg".into())
                                .spawn(move || {
                                    if let Some(chosen) = platform::pick_kit_file_dialog() {
                                        let _ = out_tx.send(chosen);
                                    } else if let Ok(mut s) = snap_for_cancel.lock() {
                                        if s.status_msg.starts_with("Select an .obw") {
                                            s.status_msg = "File selection cancelled.".into();
                                        }
                                    }
                                    busy_flag.store(false, Ordering::Release);
                                })
                                .ok();
                        }
                    };

                // Receive folder/file selected from native OS dialog
                while let Ok(picked_path) = state.dialog_rx.try_recv() {
                    let s = picked_path.to_string_lossy().into_owned();
                    state.path_input = s.clone();
                    state.last_synced_path = s;
                    state.pad_page = 0;
                    let _ = loader_tx.send(LoaderCommand::ScanAndLoadPath {
                        path: picked_path,
                        preferred_selection: None,
                        remember_in_registry: true,
                    });
                }

                // Also accept drag-and-drop folders or files from Windows Explorer!
                let dropped: Vec<PathBuf> = egui_ctx.input(|i| {
                    i.raw
                        .dropped_files
                        .iter()
                        .filter_map(|df| df.path.clone())
                        .collect()
                });
                if let Some(first_drop) = dropped.into_iter().next() {
                    let s = first_drop.to_string_lossy().into_owned();
                    state.path_input = s.clone();
                    state.last_synced_path = s;
                    state.pad_page = 0;
                    let _ = loader_tx.send(LoaderCommand::ScanAndLoadPath {
                        path: first_drop,
                        preferred_selection: None,
                        remember_in_registry: true,
                    });
                }

                ResizableWindow::new("open-sd3-window")
                    .min_size(Vec2::new(760.0, 520.0))
                    .show(egui_ctx, egui_state.as_ref(), |ui| {
                        let snap = ui_snapshot
                            .lock()
                            .map(|g| g.clone())
                            .unwrap_or_default();

                        if !snap.active_path_display.is_empty()
                            && snap.active_path_display != state.last_synced_path
                        {
                            state.path_input = snap.active_path_display.clone();
                            state.last_synced_path = snap.active_path_display.clone();
                        }

                        for idx in 0..MAX_UI_PADS {
                            let hit_v = telemetry.pad_velocities[idx].swap(0, Ordering::Relaxed);
                            if hit_v > 0 {
                                state.pad_glow[idx] = (hit_v as f32 / 127.0).max(0.25);
                            } else {
                                state.pad_glow[idx] = (state.pad_glow[idx] * 0.84).max(0.0);
                            }
                        }

                        // -----------------------------------------------------------------
                        // ROW 1: Header Bar
                        // -----------------------------------------------------------------
                        ui.horizontal(|ui| {
                            ui.heading(
                                RichText::new("OpenSD3")
                                    .color(Color32::from_rgb(245, 180, 65))
                                    .strong(),
                            );
                            ui.separator();
                            ui.label(RichText::new(&snap.kit_name).strong());
                            ui.label(
                                RichText::new(format!("({})", snap.source_desc))
                                    .color(Color32::GRAY),
                            );
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                let voices = telemetry.active_voices.load(Ordering::Relaxed);
                                ui.label(
                                    RichText::new(format!("Voices: {voices}/64"))
                                        .monospace()
                                        .color(Color32::LIGHT_GREEN),
                                );
                            });
                        });

                        ui.add_space(4.0);

                        // -----------------------------------------------------------------
                        // ROW 2: Primary Soundbank Folder Browser & Soundbank Selector
                        // -----------------------------------------------------------------
                        let dlg_open = state.dialog_busy.load(Ordering::Acquire);
                        ui.horizontal(|ui| {
                            let locate_btn = egui::Button::new(
                                RichText::new("Locate Soundbank Folder...")
                                    .color(Color32::from_rgb(20, 24, 30))
                                    .strong(),
                            )
                            .fill(Color32::from_rgb(240, 178, 65));

                            if ui
                                .add_enabled(!dlg_open, locate_btn)
                                .on_hover_text(
                                    "Browse for any Superior Drummer 3 soundbank folder (e.g. SL-SuperiorDrummer3, SL-Death) or your root Toontrack folder",
                                )
                                .clicked()
                            {
                                spawn_folder_picker(&state.dialog_busy, &state.dialog_tx, &ui_snapshot);
                            }

                            if ui
                                .button("Auto-Find Banks")
                                .on_hover_text(
                                    "Automatically scan standard Toontrack & SL-* directories across all drives",
                                )
                                .clicked()
                            {
                                state.pad_page = 0;
                                let _ = loader_tx.send(LoaderCommand::AutoDiscoverAllDrives);
                            }

                            ui.separator();
                            ui.label(RichText::new("Soundbank:").strong());

                            let current_bank_label = match snap.active_bank_idx {
                                Some(idx) => snap
                                    .discovered_banks
                                    .get(idx)
                                    .map(|b| b.summary_label())
                                    .unwrap_or_else(|| snap.kit_name.clone()),
                                None => snap.kit_name.clone(),
                            };

                            egui::ComboBox::from_id_salt("opensd3_bank_combo")
                                .width(275.0)
                                .selected_text(current_bank_label)
                                .show_ui(ui, |ui| {
                                    for (idx, bank) in snap.discovered_banks.iter().enumerate() {
                                        let is_sel = snap.active_bank_idx == Some(idx);
                                        if ui
                                            .selectable_label(is_sel, bank.summary_label())
                                            .on_hover_text(bank.room_path.display().to_string())
                                            .clicked()
                                            && !is_sel
                                        {
                                            state.pad_page = 0;
                                            let _ = loader_tx
                                                .send(LoaderCommand::SelectDiscoveredBank(idx));
                                        }
                                    }
                                    if !snap.discovered_banks.is_empty() {
                                        ui.separator();
                                    }
                                    if ui
                                        .selectable_label(
                                            snap.active_bank_idx.is_none()
                                                && snap.source_desc.starts_with("built-in"),
                                            "Built-in Studio Reference Kit (Synth)",
                                        )
                                        .clicked()
                                    {
                                        state.pad_page = 0;
                                        let _ = loader_tx.send(LoaderCommand::LoadBuiltIn(sr));
                                    }
                                });

                            if ui
                                .add_enabled(!dlg_open, egui::Button::new("Load File..."))
                                .on_hover_text("Load an individual .sfz, kit.json, or .obw file")
                                .clicked()
                            {
                                spawn_file_picker(&state.dialog_busy, &state.dialog_tx, &ui_snapshot);
                            }
                        });

                        // -----------------------------------------------------------------
                        // ROW 3: Path Input + 1-Click OS Clipboard Paste + Load
                        // -----------------------------------------------------------------
                        ui.horizontal(|ui| {
                            ui.label(RichText::new("Folder / Path:").color(Color32::GRAY));
                            let hint = if cfg!(target_os = "windows") {
                                r"D:\Toontrack\SL-Death  or  D:\Toontrack  (or click Locate Soundbank Folder...)"
                            } else {
                                "/path/to/Toontrack/SL-Death  (or click Locate Soundbank Folder...)"
                            };
                            let edit_resp = ui.add(
                                egui::TextEdit::singleline(&mut state.path_input)
                                    .desired_width(470.0)
                                    .hint_text(hint),
                            );

                            // Right-click menu on the path field for DAWs that intercept Ctrl+V
                            edit_resp.context_menu(|ui| {
                                if ui.button("Paste Path & Load").clicked() {
                                    if let Some(clip) = platform::read_os_clipboard_text() {
                                        let clean = sanitize_user_path(&clip);
                                        if !clean.is_empty() {
                                            state.path_input = clean.clone();
                                            state.pad_page = 0;
                                            let _ = loader_tx.send(LoaderCommand::ScanAndLoadPath {
                                                path: PathBuf::from(clean),
                                                preferred_selection: None,
                                                remember_in_registry: true,
                                            });
                                        }
                                    }
                                    ui.close_menu();
                                }
                                if ui.button("Paste Only").clicked() {
                                    if let Some(clip) = platform::read_os_clipboard_text() {
                                        state.path_input = sanitize_user_path(&clip);
                                    }
                                    ui.close_menu();
                                }
                                if ui.button("Clear").clicked() {
                                    state.path_input.clear();
                                    ui.close_menu();
                                }
                            });

                            let enter_pressed = edit_resp.lost_focus()
                                && ui.input(|i| i.key_pressed(egui::Key::Enter));

                            if ui
                                .button("Paste & Load")
                                .on_hover_text(
                                    "Read folder/file path directly from Windows Clipboard and load immediately",
                                )
                                .clicked()
                            {
                                match platform::read_os_clipboard_text() {
                                    Some(clip) => {
                                        let clean = sanitize_user_path(&clip);
                                        if !clean.is_empty() {
                                            state.path_input = clean.clone();
                                            state.pad_page = 0;
                                            let _ = loader_tx.send(LoaderCommand::ScanAndLoadPath {
                                                path: PathBuf::from(clean),
                                                preferred_selection: None,
                                                remember_in_registry: true,
                                            });
                                        }
                                    }
                                    None => {
                                        if let Ok(mut s) = ui_snapshot.lock() {
                                            s.is_error = true;
                                            s.status_msg =
                                                "Clipboard does not contain a text path.".into();
                                        }
                                    }
                                }
                            }

                            let load_clicked = ui.button("Load").clicked();
                            if load_clicked || enter_pressed {
                                let clean = sanitize_user_path(&state.path_input);
                                if clean.is_empty() {
                                    // Never do nothing when Load is clicked on an empty box: open the Folder Picker!
                                    spawn_folder_picker(
                                        &state.dialog_busy,
                                        &state.dialog_tx,
                                        &ui_snapshot,
                                    );
                                } else {
                                    state.pad_page = 0;
                                    let _ = loader_tx.send(LoaderCommand::ScanAndLoadPath {
                                        path: PathBuf::from(clean),
                                        preferred_selection: None,
                                        remember_in_registry: true,
                                    });
                                }
                            }

                            if ui.button("Demo Kit").clicked() {
                                state.pad_page = 0;
                                let _ = loader_tx.send(LoaderCommand::LoadBuiltIn(sr));
                            }
                        });

                        // -----------------------------------------------------------------
                        // ROW 4: Live Progress Bar / Status Line
                        // -----------------------------------------------------------------
                        if let Some(prog) = snap.loading_progress {
                            ui.add(
                                egui::ProgressBar::new(prog)
                                    .animate(true)
                                    .text(&snap.status_msg),
                            );
                        } else {
                            let status_color = if snap.is_error {
                                Color32::from_rgb(255, 125, 110)
                            } else {
                                Color32::from_rgb(175, 210, 240)
                            };
                            ui.label(RichText::new(&snap.status_msg).small().color(status_color));
                        }

                        // -----------------------------------------------------------------
                        // ROW 5: SD3 Kit Piece & Tool Selector (active when a Soundbank is loaded)
                        // -----------------------------------------------------------------
                        if let Some(ref cat_ui) = snap.catalog_ui {
                            ui.separator();
                            let mut next_sel = cat_ui.selection.clone();
                            let mut changed = false;

                            ui.horizontal_wrapped(|ui| {
                                ui.label(
                                    RichText::new("Kit Pieces:")
                                        .color(Color32::from_rgb(240, 185, 85))
                                        .strong(),
                                );

                                let mut piece_combo = |salt: &str,
                                                       title: &str,
                                                       cur: &mut String,
                                                       opts: &[PieceOption],
                                                       width: f32| {
                                    if opts.len() > 1 {
                                        ui.label(RichText::new(title).small());
                                        let cur_lbl = opts
                                            .iter()
                                            .find(|o| o.code == *cur)
                                            .map(|o| o.label.as_str())
                                            .unwrap_or(cur.as_str());
                                        egui::ComboBox::from_id_salt(salt)
                                            .width(width)
                                            .selected_text(cur_lbl)
                                            .show_ui(ui, |ui| {
                                                for opt in opts {
                                                    let sel = *cur == opt.code;
                                                    if ui.selectable_label(sel, &opt.label).clicked()
                                                        && !sel
                                                    {
                                                        *cur = opt.code.clone();
                                                        changed = true;
                                                    }
                                                }
                                            });
                                    }
                                };

                                piece_combo("sd3_tool", "Tool:", &mut next_sel.tool, &cat_ui.tools, 115.0);
                                piece_combo("sd3_kick", "Kick:", &mut next_sel.kick, &cat_ui.kicks, 110.0);
                                piece_combo("sd3_snare", "Snare:", &mut next_sel.snare, &cat_ui.snares, 115.0);
                                piece_combo("sd3_hihat", "Hat:", &mut next_sel.hihat, &cat_ui.hihats, 105.0);
                                piece_combo("sd3_toms", "Toms:", &mut next_sel.toms, &cat_ui.toms, 95.0);
                                piece_combo("sd3_ride", "Ride:", &mut next_sel.ride, &cat_ui.rides, 105.0);
                                piece_combo("sd3_cym", "Cym:", &mut next_sel.cymbals, &cat_ui.cymbals, 105.0);
                            });

                            if changed {
                                state.pad_page = 0;
                                let _ = loader_tx.send(LoaderCommand::ApplyKitSelection(next_sel));
                            }
                        }

                        ui.separator();

                        // -----------------------------------------------------------------
                        // MAIN BODY: Paged 12-Pad Drum Matrix (Left) + 4-Bus Studio Mixer (Right)
                        // -----------------------------------------------------------------
                        let total_pads = snap.pads.len().max(1);
                        let total_pages = (total_pads + UI_PAD_COUNT - 1) / UI_PAD_COUNT;
                        if state.pad_page >= total_pages {
                            state.pad_page = total_pages - 1;
                        }
                        let page_base = state.pad_page * UI_PAD_COUNT;

                        ui.columns(2, |cols| {
                            cols[0].horizontal(|ui| {
                                ui.label(RichText::new("Drum Pads").strong());
                                if total_pages > 1 {
                                    if ui
                                        .add_enabled(state.pad_page > 0, egui::Button::new("<"))
                                        .clicked()
                                    {
                                        state.pad_page = state.pad_page.saturating_sub(1);
                                    }
                                    ui.label(
                                        RichText::new(format!(
                                            "{}/{} ({} pads)",
                                            state.pad_page + 1,
                                            total_pages,
                                            total_pads
                                        ))
                                        .monospace(),
                                    );
                                    if ui
                                        .add_enabled(
                                            state.pad_page + 1 < total_pages,
                                            egui::Button::new(">"),
                                        )
                                        .clicked()
                                    {
                                        state.pad_page += 1;
                                    }
                                }
                                ui.add(
                                    egui::Slider::new(&mut state.audition_vel, 0.1..=1.0)
                                        .text("Vel"),
                                );
                            });
                            cols[0].add_space(4.0);

                            egui::Grid::new("drum_pad_grid")
                                .spacing([8.0, 8.0])
                                .show(&mut cols[0], |ui| {
                                    for slot in 0..UI_PAD_COUNT {
                                        let pad_idx = page_base + slot;
                                        let (pname, note, alias_lbl) = snap
                                            .pads
                                            .get(pad_idx)
                                            .map(|p| {
                                                (
                                                    p.name.as_str(),
                                                    p.primary_note,
                                                    p.alias_label.as_str(),
                                                )
                                            })
                                            .unwrap_or(("Empty", 0, "—"));
                                        let glow = state
                                            .pad_glow
                                            .get(pad_idx)
                                            .copied()
                                            .unwrap_or(0.0);
                                        let bg = Color32::from_rgb(
                                            (36.0 + 195.0 * glow) as u8,
                                            (43.0 + 115.0 * glow) as u8,
                                            (54.0 + 25.0 * glow) as u8,
                                        );
                                        let label_txt = if note > 0 {
                                            format!("{pname}\n{alias_lbl}")
                                        } else {
                                            "—".into()
                                        };
                                        let btn = egui::Button::new(
                                            RichText::new(label_txt)
                                                .size(11.5)
                                                .color(if note > 0 {
                                                    Color32::WHITE
                                                } else {
                                                    Color32::DARK_GRAY
                                                }),
                                        )
                                        .fill(bg)
                                        .stroke(Stroke::new(
                                            1.0_f32,
                                            Color32::from_rgb(90, 100, 118),
                                        ))
                                        .min_size(Vec2::new(122.0, 58.0));

                                        if ui.add(btn).clicked() && note > 0 {
                                            if pad_idx < MAX_UI_PADS {
                                                state.pad_glow[pad_idx] = state.audition_vel;
                                            }
                                            let _ =
                                                audition_tx.try_send((note, state.audition_vel));
                                        }
                                        if slot % 3 == 2 {
                                            ui.end_row();
                                        }
                                    }
                                });

                            // RIGHT COLUMN: Multi-Mic Studio Mixer
                            cols[1].label(
                                RichText::new("Multi-Mic Studio Mixer (-48..+24 dB)").strong(),
                            );
                            cols[1].add_space(4.0);

                            fn param_row_with_hint(
                                ui: &mut egui::Ui,
                                setter: &ParamSetter,
                                label: &str,
                                hint: &str,
                                param: &FloatParam,
                            ) {
                                ui.horizontal(|ui| {
                                    ui.add_sized([92.0, 20.0], egui::Label::new(label))
                                        .on_hover_text(hint);
                                    ui.add(widgets::ParamSlider::for_param(param, setter));
                                });
                            }

                            param_row_with_hint(
                                &mut cols[1],
                                setter,
                                "Close Mics",
                                &format!("Close channels: {}", snap.bus_mic_summary[0]),
                                &params.close_gain,
                            );
                            param_row_with_hint(
                                &mut cols[1],
                                setter,
                                "Overheads",
                                &format!("Overhead channels: {}", snap.bus_mic_summary[1]),
                                &params.oh_gain,
                            );
                            param_row_with_hint(
                                &mut cols[1],
                                setter,
                                "Room / Amb",
                                &format!("Ambience channels: {}", snap.bus_mic_summary[2]),
                                &params.room_gain,
                            );
                            param_row_with_hint(
                                &mut cols[1],
                                setter,
                                "Bleed",
                                &format!("Bleed channels: {}", snap.bus_mic_summary[3]),
                                &params.bleed_gain,
                            );
                            cols[1].separator();
                            param_row_with_hint(
                                &mut cols[1],
                                setter,
                                "Master Out",
                                "Master stereo bus output gain",
                                &params.master_gain,
                            );
                            param_row_with_hint(
                                &mut cols[1],
                                setter,
                                "Vel Floor",
                                "Minimum MIDI velocity floor",
                                &params.vel_floor,
                            );
                            param_row_with_hint(
                                &mut cols[1],
                                setter,
                                "Humanize",
                                "Subtle round-robin & micro-gain humanization",
                                &params.humanize,
                            );

                            cols[1].add_space(6.0);
                            cols[1].label(
                                RichText::new(format!(
                                    "Mics — Close: {} | OH: {} | Room: {}",
                                    snap.bus_mic_summary[0],
                                    snap.bus_mic_summary[1],
                                    snap.bus_mic_summary[2]
                                ))
                                .small()
                                .color(Color32::GRAY),
                            );

                            cols[1].add_space(6.0);
                            let peak_lin = peak_meter.load(Ordering::Relaxed);
                            let peak_db = util::gain_to_db(peak_lin);
                            let norm = ((peak_db + 60.0) / 60.0).clamp(0.0, 1.0);
                            let txt = if peak_db > util::MINUS_INFINITY_DB {
                                format!("Out Peak: {peak_db:.1} dBFS")
                            } else {
                                "Out Peak: -inf dBFS".into()
                            };
                            cols[1].add(egui::ProgressBar::new(norm).text(txt));
                        });
                    });
            },
        )
    }

    fn process(
        &mut self,
        buffer: &mut Buffer,
        _aux: &mut AuxiliaryBuffers,
        context: &mut impl ProcessContext<Self>,
    ) -> ProcessStatus {
        while let Ok(new_kit) = self.kit_rx.try_recv() {
            let old = self.engine.swap_kit(new_kit);
            let _ = self.garbage_tx.try_send(old);
        }

        let mixer = MixerGains {
            close: self.params.close_gain.value(),
            oh: self.params.oh_gain.value(),
            room: self.params.room_gain.value(),
            bleed: self.params.bleed_gain.value(),
            master: self.params.master_gain.value(),
            vel_floor: self.params.vel_floor.value(),
            humanize: self.params.humanize.value(),
        };

        while let Ok((note, vel)) = self.audition_rx.try_recv() {
            self.engine
                .note_on(note, vel, &mixer, Some(&self.telemetry));
        }

        let num_samples = buffer.samples();
        let raw_outputs = buffer.as_slice();
        if raw_outputs.len() < 2 || num_samples == 0 {
            return ProcessStatus::Normal;
        }

        let (first_ch, rest_ch) = raw_outputs.split_first_mut().unwrap();
        let second_ch = &mut rest_ch[0];

        let mut next_event = context.next_event();
        let mut block_start = 0usize;
        let mut max_block_peak = 0.0f32;

        while block_start < num_samples {
            while let Some(ev) = next_event {
                if (ev.timing() as usize) > block_start {
                    break;
                }
                match ev {
                    NoteEvent::NoteOn { note, velocity, .. } if velocity > 0.0 => {
                        self.engine
                            .note_on(note, velocity, &mixer, Some(&self.telemetry));
                    }
                    NoteEvent::MidiCC { cc, value, .. } => {
                        self.engine
                            .control_change(cc, (value.clamp(0.0, 1.0) * 127.0).round() as u8);
                    }
                    NoteEvent::PolyPressure { note, pressure, .. } => {
                        self.engine
                            .poly_pressure(note, (pressure.clamp(0.0, 1.0) * 127.0).round() as u8);
                    }
                    _ => {}
                }
                next_event = context.next_event();
            }

            let next_ev_idx = next_event
                .map(|e| (e.timing() as usize).min(num_samples))
                .unwrap_or(num_samples);
            let block_end = next_ev_idx
                .min(block_start + 64)
                .max(block_start + 1)
                .min(num_samples);

            let p = self.engine.process_block(
                &mut first_ch[block_start..block_end],
                &mut second_ch[block_start..block_end],
                &mixer,
                Some(&self.telemetry),
            );
            max_block_peak = max_block_peak.max(p);
            block_start = block_end;
        }

        if self.params.editor_state.is_open() {
            let prev = self.peak_meter.load(Ordering::Relaxed);
            let next_p = if max_block_peak >= prev {
                max_block_peak
            } else {
                prev * self.peak_decay_weight
                    + max_block_peak * (1.0 - self.peak_decay_weight)
            };
            self.peak_meter.store(next_p, Ordering::Relaxed);
        }

        ProcessStatus::KeepAlive
    }
}

impl ClapPlugin for OpenSd3Plugin {
    const CLAP_ID: &'static str = "org.opensd3.drum-workstation";
    const CLAP_DESCRIPTION: Option<&'static str> = Some(
        "Native Windows & Linux multi-mic drum workstation for Superior Drummer 3 (SL-* / .obw) & SFZ soundbanks",
    );
    const CLAP_MANUAL_URL: Option<&'static str> = Some(Self::URL);
    const CLAP_SUPPORT_URL: Option<&'static str> = None;
    const CLAP_FEATURES: &'static [ClapFeature] = &[
        ClapFeature::Instrument,
        ClapFeature::Drum,
        ClapFeature::Sampler,
        ClapFeature::Stereo,
    ];
}

impl Vst3Plugin for OpenSd3Plugin {
    const VST3_CLASS_ID: [u8; 16] = *b"OpenSD3DrumWkstn";
    const VST3_SUBCATEGORIES: &'static [Vst3SubCategory] = &[
        Vst3SubCategory::Instrument,
        Vst3SubCategory::Drum,
        Vst3SubCategory::Sampler,
        Vst3SubCategory::Stereo,
    ];
}

nih_export_clap!(OpenSd3Plugin);
nih_export_vst3!(OpenSd3Plugin);
