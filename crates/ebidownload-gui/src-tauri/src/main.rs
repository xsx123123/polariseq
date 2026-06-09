//! EBIDownload Tauri App
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use ebidownload::app::*;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .manage(AppState::default())
        .invoke_handler(tauri::generate_handler![
            get_config_command,
            load_config_command,
            save_config_command,
            fetch_metadata_command,
            start_download_command,
            start_upload_command,
            cancel_download_command,
            cancel_upload_command,
        ])
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_fs::init())
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

fn main() {
    run();
}
