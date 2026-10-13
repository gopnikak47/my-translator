// System-audio capture placeholder for platforms without a loopback backend
// (e.g. Linux). Keeps the crate compiling there so the app can be built and
// tested; selecting "System" audio returns a clear error instead.

use std::sync::mpsc;

pub struct SystemAudioCapture;

impl SystemAudioCapture {
    pub fn new() -> Self {
        Self
    }

    pub fn start(&self) -> Result<mpsc::Receiver<Vec<u8>>, String> {
        Err("System audio capture is not supported on this platform. Use the microphone input.".into())
    }

    pub fn stop(&self) {}

    #[allow(dead_code)]
    pub fn is_capturing(&self) -> bool {
        false
    }
}

impl Default for SystemAudioCapture {
    fn default() -> Self {
        Self::new()
    }
}
