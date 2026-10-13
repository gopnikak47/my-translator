mod audio;
mod commands;
mod settings;
mod sysinfo;

use audio::capture::AudioCapture;
use commands::audio::AudioState;
use commands::live::LiveState;
use commands::local_pipeline::LocalPipelineState;
use commands::local_tts::LocalTtsState;
use commands::openai_realtime::OpenAiState;
use commands::qwen_realtime::QwenState;
use settings::{Settings, SettingsState};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

// Set once the frontend has flushed the session (or the exit deadline elapsed),
// so the ExitRequested handler stops preventing exit and the app can quit.
static EXIT_ALLOWED: AtomicBool = AtomicBool::new(false);

#[tauri::command]
fn get_platform_info() -> String {
    // `std::env::consts::ARCH` is the arch of THIS binary, not the CPU. On an
    // Apple Silicon Mac running the x64 build under Rosetta it reports
    // "x86_64", which wrongly blocked the Local MLX engine (MLX runs as a
    // separate native-ARM Python subprocess, so it works fine there).
    // Ask the hardware directly so detection is Rosetta-proof.
    let is_arm_hardware = is_apple_silicon_hardware();
    format!(
        r#"{{"os":"{}","arch":"{}","is_arm_hardware":{},"version":"0.3.0"}}"#,
        std::env::consts::OS,
        std::env::consts::ARCH,
        is_arm_hardware
    )
}

/// True only on Apple Silicon hardware (macOS), even when the current process
/// is x86_64 under Rosetta. Uses `sysctl hw.optional.arm64`, which reads the
/// real CPU, not the process translation state. Non-macOS → false.
#[cfg(target_os = "macos")]
fn is_apple_silicon_hardware() -> bool {
    std::process::Command::new("sysctl")
        .args(["-n", "hw.optional.arm64"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim() == "1")
        .unwrap_or(false)
}

#[cfg(not(target_os = "macos"))]
fn is_apple_silicon_hardware() -> bool {
    false
}

// Called by the frontend after it has flushed the session on exit. Force-exits
// the process; the flag keeps a subsequent ExitRequested from being prevented.
#[tauri::command]
async fn exit_app(app: tauri::AppHandle, live: tauri::State<'_, LiveState>) -> Result<(), ()> {
    // Close translation sessions gracefully (no orphaned billing sessions) and
    // release the overlay port before exiting.
    let inner = live.0.clone();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), inner.shutdown()).await;
    EXIT_ALLOWED.store(true, Ordering::SeqCst);
    app.exit(0);
    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Load settings from disk (or defaults)
    let initial_settings = Settings::load();
    let live_settings = initial_settings.live.clone().sanitized();
    let metrics = Arc::new(live_core::Metrics::new());
    let capture = Arc::new(AudioCapture::new(metrics.clone()));
    let live_state = LiveState::new(&live_settings, metrics, capture.clone());
    let live_for_setup = live_state.clone();
    let live_for_exit = live_state.clone();

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .setup(move |app| {
            #[cfg(desktop)]
            {
                app.handle()
                    .plugin(tauri_plugin_updater::Builder::new().build())?;
                app.handle().plugin(tauri_plugin_process::init())?;
            }
            // Live captions: housekeeping ticker + overlay server for vMix/OBS.
            {
                let inner = live_for_setup.0.clone();
                tauri::async_runtime::spawn(live_core::CaptionHub::run_ticker(Arc::downgrade(&inner.hub)));
                if live_settings.server.auto_start {
                    tauri::async_runtime::spawn(async move {
                        if let Err(e) = inner.ensure_server(&live_settings).await {
                            eprintln!("[live] overlay server not started: {e}");
                        }
                    });
                }
            }
            // Dev builds (compiled with the `devtools` feature): auto-open the WebView
            // inspector so JS/console errors are visible. Never compiled into release.
            #[cfg(feature = "devtools")]
            {
                use tauri::Manager;
                if let Some(win) = app.get_webview_window("main") {
                    win.open_devtools();
                }
            }
            Ok(())
        })
        .manage(SettingsState(Mutex::new(initial_settings)))
        .manage(AudioState { capture })
        .manage(live_state)
        .manage(LocalPipelineState {
            process: Mutex::new(None),
        })
        .manage(LocalTtsState::default())
        .manage(OpenAiState::default())
        .manage(QwenState::default())
        .invoke_handler(tauri::generate_handler![
            commands::settings::get_settings,
            commands::settings::save_settings,
            commands::audio::start_capture,
            commands::audio::stop_capture,
            commands::audio::check_permissions,
            commands::audio::audio_list_input_devices,
            commands::audio::audio_status,
            commands::live::live_start,
            commands::live::live_stop,
            commands::live::live_control,
            commands::live::live_soniox_ingest,
            commands::live::live_subscribe_segments,
            commands::live::live_status,
            commands::live::live_server_restart,
            commands::live::live_server_stop,
            commands::live::live_reset_metrics,
            commands::transcript::save_transcript,
            commands::transcript::open_transcript_dir,
            commands::transcript::list_transcripts,
            commands::transcript::read_transcript,
            commands::session_store::save_session,
            commands::session_store::list_sessions,
            commands::session_store::read_session,
            commands::session_store::read_legacy_session,
            commands::session_store::delete_session,
            commands::session_store::update_session_title,
            commands::session_store::export_session_srt,
            commands::session_store::export_session_txt,
            commands::session_store::search_sessions,
            commands::local_pipeline::start_local_pipeline,
            commands::local_pipeline::send_audio_to_pipeline,
            commands::local_pipeline::stop_local_pipeline,
            commands::local_pipeline::check_mlx_setup,
            commands::local_pipeline::run_mlx_setup,
            commands::edge_tts::edge_tts_speak,
            commands::microsoft_tts::microsoft_list_voices,
            commands::google_free_tts::google_free_tts_speak,
            commands::tiktok_tts::tiktok_tts_speak,
            commands::local_tts::local_tts_speak,
            commands::local_tts::local_tts_list_models,
            commands::local_tts::local_tts_models_dir_path,
            commands::local_tts::local_tts_download_model,
            commands::local_tts::local_tts_delete_model,
            commands::openai_realtime::openai_realtime_start,
            commands::openai_realtime::openai_realtime_send_audio,
            commands::openai_realtime::openai_realtime_stop,
            commands::qwen_realtime::qwen_realtime_start,
            commands::qwen_realtime::qwen_realtime_send_audio,
            commands::qwen_realtime::qwen_realtime_stop,
            get_platform_info,
            exit_app,
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(move |app_handle, event| {
            if let tauri::RunEvent::Exit = event {
                // Last chance: stop device capture threads deterministically.
                live_for_exit.0.capture.stop_all();
            }
            // Cmd+Q and Dock → Quit fire app-level ExitRequested (not the
            // window's onCloseRequested). Prevent the first exit, ask the
            // frontend to flush the session, and force-exit after a deadline so
            // a hung flush can never make the app unquittable.
            if let tauri::RunEvent::ExitRequested { api, .. } = event {
                if !EXIT_ALLOWED.load(Ordering::SeqCst) {
                    use tauri::{Emitter, Manager};
                    api.prevent_exit();
                    if let Some(win) = app_handle.get_webview_window("main") {
                        let _ = win.emit("app-exit-requested", ());
                    }
                    let handle = app_handle.clone();
                    std::thread::spawn(move || {
                        std::thread::sleep(std::time::Duration::from_secs(3));
                        EXIT_ALLOWED.store(true, Ordering::SeqCst);
                        handle.exit(0);
                    });
                }
            }
        });
}
