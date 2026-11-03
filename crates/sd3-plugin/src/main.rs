#![cfg_attr(all(target_os = "windows", not(debug_assertions)), windows_subsystem = "windows")]

fn main() {
    nih_plug::nih_export_standalone::<open_sd3::OpenSd3Plugin>();
}
