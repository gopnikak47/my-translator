pub mod capture;
pub mod microphone;
pub mod resampler;

#[cfg(target_os = "macos")]
pub mod system_audio;

#[cfg(target_os = "windows")]
pub mod wasapi;

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub mod unsupported;

// Re-export SystemAudioCapture from the correct platform module
#[cfg(target_os = "macos")]
pub use system_audio::SystemAudioCapture;

#[cfg(target_os = "windows")]
pub use wasapi::SystemAudioCapture;

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub use unsupported::SystemAudioCapture;

/// Target audio format for Soniox: PCM s16le, 16kHz, mono
#[allow(dead_code)]
pub const TARGET_SAMPLE_RATE: u32 = 16000;
#[allow(dead_code)]
pub const TARGET_CHANNELS: u16 = 1;
