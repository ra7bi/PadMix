//! Process-level helpers: log file, crash capture, single instance.

use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS, HANDLE};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::WindowsAndMessaging::{FindWindowW, SetForegroundWindow, ShowWindow, SW_RESTORE};

static LOG: Mutex<Option<std::fs::File>> = Mutex::new(None);

pub fn log_path() -> PathBuf {
    crate::config::dir().join("padmix.log")
}

/// Open %APPDATA%\PadMix\padmix.log (rotated at 1 MB) and record panics there.
pub fn init_logging() {
    let _ = std::fs::create_dir_all(crate::config::dir());
    let path = log_path();
    if std::fs::metadata(&path).map(|m| m.len() > 1_000_000).unwrap_or(false) {
        let _ = std::fs::rename(&path, path.with_extension("log.old"));
    }
    if let Ok(f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        if let Ok(mut g) = LOG.lock() {
            *g = Some(f);
        }
    }
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        log(&format!("CRASH: {info}"));
        default_hook(info);
    }));
    log(&format!("PadMix {} starting", env!("CARGO_PKG_VERSION")));
}

pub fn log(msg: &str) {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if let Ok(mut g) = LOG.lock() {
        if let Some(f) = g.as_mut() {
            let _ = writeln!(f, "[{secs}] {msg}");
        }
    }
}

/// Holds the single-instance mutex for the life of the process.
pub struct Instance(#[allow(dead_code)] HANDLE);

/// Returns None if PadMix is already running (and brings that window to the front).
pub fn single_instance() -> Option<Instance> {
    unsafe {
        let h = CreateMutexW(None, true, w!("Local\\PadMix.SingleInstance.v1")).ok()?;
        if GetLastError() == ERROR_ALREADY_EXISTS {
            focus_existing();
            return None;
        }
        Some(Instance(h))
    }
}

pub fn focus_existing() {
    unsafe {
        if let Ok(hwnd) = FindWindowW(PCWSTR::null(), w!("PadMix")) {
            let _ = ShowWindow(hwnd, SW_RESTORE);
            let _ = SetForegroundWindow(hwnd);
        }
    }
}
