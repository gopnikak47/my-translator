use crate::commands::live::LiveState;
use crate::settings::{Settings, SettingsState};
use tauri::State;

/// Get current settings
#[tauri::command]
pub fn get_settings(state: State<'_, SettingsState>) -> Result<Settings, String> {
    let settings = state.0.lock().map_err(|e| format!("Lock error: {}", e))?;
    Ok(settings.clone())
}

/// Save settings
#[tauri::command]
pub fn save_settings(
    mut new_settings: Settings,
    state: State<'_, SettingsState>,
    live: State<'_, LiveState>,
) -> Result<(), String> {
    let mut settings = state.0.lock().map_err(|e| format!("Lock error: {}", e))?;
    new_settings.live = new_settings.live.sanitized();
    // Keep the generated control token if the frontend sent an empty one.
    if new_settings.live.server.access_token.len() < 16 {
        new_settings.live.server.access_token = settings.live.server.access_token.clone();
    }

    // Save to disk
    new_settings.save()?;

    // Live captions pick up layout/caption changes immediately (overlays
    // receive the new layout over SSE without reloading).
    live.0.apply_settings(&new_settings.live);

    // Update in-memory state
    *settings = new_settings;

    Ok(())
}
