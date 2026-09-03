pub mod agent;
pub mod commands;
pub mod config;
pub mod doctor;
pub mod error;
pub mod llm;
pub mod models;
pub mod state;
pub mod storage;
pub mod whisper;
pub mod workspace;

use tauri::Manager;

use crate::state::AppState;
use crate::storage::Paths;

/// Private URI scheme the webview uses to read downloaded Whisper weights.
/// Nothing else is reachable through it — see `whisper::resolve_asset`.
pub const MODEL_SCHEME: &str = "vdmodels";

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
        // Serves downloaded model files to the webview, and nothing else.
        .register_uri_scheme_protocol(MODEL_SCHEME, |ctx, request| {
            let Some(state) = ctx.app_handle().try_state::<AppState>() else {
                return tauri::http::Response::builder()
                    .status(503)
                    .body(Vec::new())
                    .unwrap();
            };
            let path = request.uri().path().to_string();
            match whisper::resolve_asset(&state.paths, &path) {
                Some(file) => match std::fs::read(&file) {
                    Ok(bytes) => tauri::http::Response::builder()
                        .status(200)
                        .header("content-type", whisper::content_type(&file))
                        .header("access-control-allow-origin", "*")
                        .body(bytes)
                        .unwrap(),
                    Err(_) => tauri::http::Response::builder()
                        .status(500)
                        .body(Vec::new())
                        .unwrap(),
                },
                None => tauri::http::Response::builder()
                    .status(404)
                    .body(Vec::new())
                    .unwrap(),
            }
        })
        .setup(|app| {
            // The webview offers to remember and refill every text field, and
            // draws that offer as an oversized panel over the form — the HTML
            // `autocomplete` attribute does not always talk it out of it. None
            // of these fields is a login or an address, so the whole feature is
            // switched off at the source.
            #[cfg(target_os = "windows")]
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.with_webview(|webview| unsafe {
                    use webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2Settings4;
                    use windows::core::Interface;

                    let Ok(core) = webview.controller().CoreWebView2() else {
                        return;
                    };
                    let Ok(settings) = core.Settings() else {
                        return;
                    };
                    if let Ok(settings) = settings.cast::<ICoreWebView2Settings4>() {
                        let _ = settings.SetIsGeneralAutofillEnabled(false);
                        let _ = settings.SetIsPasswordAutosaveEnabled(false);
                    }
                });
            }

            // A separate data directory can be requested with VELVETDESK_DATA_DIR:
            // it keeps a development or test run from touching the operator's
            // real profiles, which live in the platform app-data directory.
            let base = match std::env::var_os("VELVETDESK_DATA_DIR") {
                Some(dir) if !dir.is_empty() => std::path::PathBuf::from(dir),
                _ => app
                    .path()
                    .app_data_dir()
                    .map_err(|e| format!("no app data dir: {e}"))?,
            };
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
            commands::delete_agent_entries,
            commands::delete_master_entries,
            commands::delete_chat_messages,
            commands::save_chat,
            commands::run_agent,
            commands::fetch_image,
            commands::list_trusted_roots,
            commands::trust_folder,
            commands::revoke_folder,
            commands::list_backups,
            commands::restore_backup,
            commands::write_letters,
            commands::master_chat,
            commands::master_context_stats,
            commands::get_master_log,
            commands::clear_master_log,
            commands::context_stats,
            commands::clear_context,
            commands::compact_chat,
            commands::compact_context,
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
            commands::list_local_models,
            commands::download_local_model,
            commands::delete_local_model,
            commands::local_models_base_url,
            commands::test_provider,
            commands::seed_demo,
        ])
        .run(tauri::generate_context!())
        .expect("error while running VelvetDesk");
}
