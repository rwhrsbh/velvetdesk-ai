pub mod agent;
pub mod commands;
pub mod config;
pub mod doctor;
pub mod entitlement;
pub mod error;
pub mod hwid;
pub mod llm;
pub mod models;
pub mod state;
pub mod storage;
pub mod sync;
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

    // Updating in place is a desktop affair; a phone installs its own APK.
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    let builder = builder
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init());

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
            // What the licence allows, before the first window is drawn: the
            // caps are consulted from inside tool calls, and a free copy must
            // not spend its first minute behaving like a paid one.
            state.refresh_entitlement();
            let sync_paths = state.paths.clone();
            app.manage(state);

            // Sync runs by itself for a licence that covers more than one
            // machine. Business leaves sealed records on the gateway, so each
            // round is a visit to the mailbox every few minutes and the other
            // machine may well be off. Pro keeps nothing on the server: the
            // device waits in the relay room for the other one and they
            // exchange directly the moment both are on - then again about a
            // minute later, for as long as both stay on.
            tauri::async_runtime::spawn(async move {
                use std::time::Duration;
                let http = reqwest::Client::new();
                loop {
                    let limits = entitlement::limits();
                    let license = config::Secrets::load(&sync_paths)
                        .ok()
                        .and_then(|secrets| {
                            secrets.for_provider("velvetdesk-cloud").first().cloned()
                        })
                        .unwrap_or_default();
                    let pairing = if limits.sync {
                        sync::pair::for_license(&sync_paths, &license)
                            .ok()
                            .flatten()
                    } else {
                        None
                    };
                    let Some(pairing) = pairing.filter(|p| p.auto) else {
                        tokio::time::sleep(Duration::from_secs(60)).await;
                        continue;
                    };
                    let device = crate::hwid::device_id(&sync_paths);

                    let round = sync::ROUND.lock().await;
                    let (outcome, pause) = if limits.mailbox {
                        (
                            sync::mailbox::run_round(
                                &http,
                                &sync_paths,
                                &pairing,
                                &license,
                                &device,
                            )
                            .await
                            .map(Some),
                            Duration::from_secs(300),
                        )
                    } else {
                        let outcome = sync::transport::run_round(
                            &sync_paths,
                            &pairing,
                            &license,
                            Duration::from_secs(600),
                            Some(&sync::KICK),
                        )
                        .await;
                        // Nobody came (or the button wants the room): straight
                        // back in. After a round, a minute's pause, so two
                        // machines left on do not trade digests non-stop.
                        let pause = match &outcome {
                            Ok(None) => Duration::from_secs(2),
                            _ => Duration::from_secs(60),
                        };
                        (outcome, pause)
                    };
                    drop(round);
                    match outcome {
                        Ok(Some(report)) if report.pulled + report.pushed > 0 => {
                            log::info!(
                                "sync: {} in, {} out, {} conflicts",
                                report.pulled,
                                report.pushed,
                                report.conflicts
                            );
                        }
                        Ok(_) => {}
                        Err(err) => {
                            log::warn!("sync: {err}");
                            sync::record_failure(&sync_paths, &err);
                        }
                    }
                    tokio::time::sleep(pause).await;
                }
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::bootstrap,
            commands::list_profiles,
            commands::create_profile,
            commands::save_profile_folder,
            commands::set_profile_folder,
            commands::save_man_folder,
            commands::set_man_folder,
            commands::get_profile,
            commands::save_profile,
            commands::delete_profile,
            commands::list_men,
            commands::read_raw,
            commands::reorder_profiles,
            commands::reorder_men,
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
            commands::cancel_run,
            commands::save_chat,
            commands::digest_chat,
            commands::apply_digest,
            commands::learn_voice,
            commands::run_agent,
            commands::fetch_image,
            commands::check_update,
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
            commands::revert_step,
            commands::pending_clear,
            commands::doctor_scan,
            commands::doctor_fix,
            commands::get_settings,
            commands::save_settings,
            commands::cloud_status,
            commands::plan_state,
            commands::activate_license,
            commands::deactivate_license,
            commands::cloud_plans,
            commands::cloud_coins,
            commands::cloud_subscribe,
            commands::cloud_buy_credits,
            commands::cloud_order,
            commands::cloud_purchases,
            commands::sync_state,
            commands::sync_create_invite,
            commands::sync_join,
            commands::sync_forget,
            commands::sync_set_auto,
            commands::sync_now,
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
        ])
        .run(tauri::generate_context!())
        .expect("error while running VelvetDesk");
}
