pub mod agent;
pub mod commands;
pub mod config;
pub mod doctor;
pub mod error;
pub mod llm;
pub mod models;
pub mod state;
pub mod storage;

use tauri::Manager;

use crate::state::AppState;
use crate::storage::Paths;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let builder = tauri::Builder::default();

    #[cfg(desktop)]
    let builder = builder.plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
        if let Some(window) = app.get_webview_window("main") {
            let _ = window.set_focus();
        }
    }));

    builder
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            let base = app
                .path()
                .app_data_dir()
                .map_err(|e| format!("no app data dir: {e}"))?;
            let paths = Paths::new(base).map_err(|e| e.to_string())?;
            // Warm the index so the first render is instant.
            let _ = storage::rebuild_index(&paths);
            let state = AppState::new(paths).map_err(|e| e.to_string())?;
            app.manage(state);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::bootstrap,
            commands::list_profiles,
            commands::create_profile,
            commands::get_profile,
            commands::save_profile,
            commands::delete_profile,
            commands::list_men,
            commands::get_man,
            commands::save_man,
            commands::create_man,
            commands::delete_man,
            commands::get_chat,
            commands::append_message,
            commands::get_agent_log,
            commands::clear_agent_log,
            commands::run_agent,
            commands::master_route,
            commands::global_search,
            commands::rebuild_index,
            commands::pending_list,
            commands::pending_approve,
            commands::pending_reject,
            commands::pending_clear,
            commands::doctor_scan,
            commands::doctor_fix,
            commands::get_settings,
            commands::save_settings,
            commands::list_keys,
            commands::set_keys,
            commands::add_key,
            commands::remove_key,
            commands::list_provider_models,
            commands::transcribe,
            commands::test_provider,
            commands::seed_demo,
        ])
        .run(tauri::generate_context!())
        .expect("error while running VelvetDesk");
}
