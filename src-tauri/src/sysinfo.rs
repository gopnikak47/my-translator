//! Process memory usage for the live-event health panel.

/// Resident memory of this process in MiB, where the platform exposes it cheaply.
#[cfg(target_os = "windows")]
pub fn process_memory_mb() -> Option<f64> {
    use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
    use windows::Win32::System::Threading::GetCurrentProcess;
    let mut counters = PROCESS_MEMORY_COUNTERS::default();
    let size = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
    // SAFETY: valid out-pointer to a correctly sized struct; pseudo-handle of
    // the current process never needs closing.
    let ok = unsafe { GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, size) };
    ok.ok().map(|_| counters.WorkingSetSize as f64 / (1024.0 * 1024.0))
}

#[cfg(target_os = "linux")]
pub fn process_memory_mb() -> Option<f64> {
    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    let pages: f64 = statm.split_whitespace().nth(1)?.parse().ok()?;
    Some(pages * 4096.0 / (1024.0 * 1024.0))
}

#[cfg(not(any(target_os = "windows", target_os = "linux")))]
pub fn process_memory_mb() -> Option<f64> {
    None
}
