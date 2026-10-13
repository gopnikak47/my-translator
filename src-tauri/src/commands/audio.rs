use crate::audio::capture::AudioCapture;
use crate::audio::microphone::{list_input_devices, AudioStatusView, InputDeviceInfo};
use serde::Serialize;
use std::sync::Arc;
use tauri::ipc::{Channel, InvokeResponseBody};
use tauri::State;

/// State for audio capture (one shared capture, many consumers).
pub struct AudioState {
    pub capture: Arc<AudioCapture>,
}

#[derive(Serialize, Clone)]
pub struct PermissionStatus {
    pub screen_recording: String,
    pub microphone: String,
}

/// Start audio capture and forward 16 kHz PCM16 to the frontend via an IPC
/// channel. Audio arrives in JS as an `ArrayBuffer` every ~100 ms.
/// `device` optionally selects an input device by name (microphone source).
#[tauri::command]
pub fn start_capture(
    source: String,
    device: Option<String>,
    channel: Channel<InvokeResponseBody>,
    state: State<'_, AudioState>,
) -> Result<(), String> {
    state.capture.attach_legacy(&source, device, channel)
}

/// Stop the webview's audio stream (capture keeps running if live mode uses it).
#[tauri::command]
pub fn stop_capture(state: State<'_, AudioState>) -> Result<(), String> {
    state.capture.detach_legacy();
    Ok(())
}

#[tauri::command]
pub fn audio_list_input_devices() -> Vec<InputDeviceInfo> {
    list_input_devices()
}

/// Level meter / health of the current capture (peak-hold resets on read).
#[tauri::command]
pub fn audio_status(state: State<'_, AudioState>) -> AudioStatusView {
    state.capture.stats.view()
}

/// Software input gain (dB, 0..+40) and channel mode ("auto" | "mix" |
/// "left" | "right"). Applies immediately to the running capture.
#[tauri::command]
pub fn audio_set_input_options(gain_db: f32, channel_mode: String, state: State<'_, AudioState>) {
    state
        .capture
        .stats
        .set_input_options(gain_db, live_core::audio::ChannelMode::from_code(&channel_mode));
}

/// Check audio capture permissions
#[tauri::command]
pub fn check_permissions() -> PermissionStatus {
    // Note: Actual permission checking on macOS requires Objective-C interop
    // For now, we return "unknown" and permissions will be prompted on first use
    PermissionStatus {
        screen_recording: "unknown".to_string(),
        microphone: "unknown".to_string(),
    }
}
