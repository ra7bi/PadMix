//! Per-user install / uninstall (no admin): %LOCALAPPDATA%\Programs\PadMix,
//! Start menu + desktop shortcuts, and an entry in Settings > Apps.
//! Shared by PadMix.exe (`--uninstall`) and PadMix-Setup.exe (`#[path]` include),
//! so it depends only on std + windows.

#![allow(dead_code)]

use std::ffi::c_void;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};
use windows::core::{w, Interface, HSTRING, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, LPARAM, WPARAM};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, IPersistFile, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
};
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteTreeW, RegSetValueExW, HKEY, HKEY_CURRENT_USER, KEY_WRITE,
    REG_DWORD, REG_OPTION_NON_VOLATILE, REG_SZ,
};
use windows::Win32::System::Threading::{OpenMutexW, SYNCHRONIZATION_SYNCHRONIZE};
use windows::Win32::UI::Shell::{
    FOLDERID_Desktop, FOLDERID_LocalAppData, FOLDERID_Programs, IShellLinkW, SHGetKnownFolderPath, ShellLink,
    KNOWN_FOLDER_FLAG,
};
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowW, MessageBoxW, PostMessageW, MESSAGEBOX_RESULT, MESSAGEBOX_STYLE, WM_CLOSE,
};

pub const APP_NAME: &str = "PadMix";
pub const PUBLISHER: &str = "ra7bi";
pub const URL: &str = "https://github.com/ra7bi/PadMix";
const UNINSTALL_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Uninstall\PadMix";
const MUTEX: &str = "Local\\PadMix.SingleInstance.v1";
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const DETACHED_PROCESS: u32 = 0x0000_0008;

fn known_folder(id: &windows::core::GUID) -> Option<PathBuf> {
    unsafe {
        let p = SHGetKnownFolderPath(id, KNOWN_FOLDER_FLAG(0), None).ok()?;
        let s = p.to_string().ok();
        CoTaskMemFree(Some(p.0 as *const c_void));
        s.map(PathBuf::from)
    }
}

pub fn install_dir() -> PathBuf {
    known_folder(&FOLDERID_LocalAppData)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("LOCALAPPDATA").unwrap_or_default()))
        .join("Programs")
        .join(APP_NAME)
}

pub fn installed_exe() -> PathBuf {
    install_dir().join("PadMix.exe")
}

fn start_menu_link() -> Option<PathBuf> {
    known_folder(&FOLDERID_Programs).map(|p| p.join("PadMix.lnk"))
}

fn desktop_link() -> Option<PathBuf> {
    known_folder(&FOLDERID_Desktop).map(|p| p.join("PadMix.lnk"))
}

/// A simple native message box. Returns the button id (IDOK=1, IDCANCEL=2, IDYES=6, IDNO=7).
pub fn message(text: &str, style: u32) -> i32 {
    unsafe {
        let r: MESSAGEBOX_RESULT = MessageBoxW(None, &HSTRING::from(text), w!("PadMix"), MESSAGEBOX_STYLE(style));
        r.0
    }
}
pub const MB_OK_INFO: u32 = 0x40;
pub const MB_OK_ERROR: u32 = 0x10;
pub const MB_YESNO_Q: u32 = 0x04 | 0x20;
pub const MB_OKCANCEL_INFO: u32 = 0x01 | 0x40;
pub const MB_YESNO_WARN_DEFNO: u32 = 0x04 | 0x30 | 0x100;
pub const IDOK: i32 = 1;
pub const IDYES: i32 = 6;

pub fn is_running() -> bool {
    unsafe {
        match OpenMutexW(SYNCHRONIZATION_SYNCHRONIZE, false, &HSTRING::from(MUTEX)) {
            Ok(h) => {
                let _ = CloseHandle(h);
                true
            }
            Err(_) => false,
        }
    }
}

/// Ask a running PadMix to close normally (so it saves settings). True when it is gone.
pub fn close_running(timeout: Duration) -> bool {
    if !is_running() {
        return true;
    }
    unsafe {
        if let Ok(hwnd) = FindWindowW(PCWSTR::null(), w!("PadMix")) {
            let _ = PostMessageW(hwnd, WM_CLOSE, WPARAM(0), LPARAM(0));
        }
    }
    let start = Instant::now();
    while start.elapsed() < timeout {
        if !is_running() {
            std::thread::sleep(Duration::from_millis(300)); // let the process finish exiting
            return true;
        }
        std::thread::sleep(Duration::from_millis(150));
    }
    !is_running()
}

/// Copy the app into place and register it. Caller must close a running PadMix first.
pub fn install(exe_bytes: &[u8], version: &str) -> anyhow::Result<PathBuf> {
    use anyhow::Context;
    let dir = install_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let exe = installed_exe();
    let tmp = dir.join("PadMix.exe.new");
    std::fs::write(&tmp, exe_bytes).context("writing PadMix.exe")?;
    if exe.exists() {
        std::fs::remove_file(&exe).context("replacing the old PadMix.exe (is PadMix still open?)")?;
    }
    std::fs::rename(&tmp, &exe).context("placing PadMix.exe")?;

    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }
    if let Some(l) = start_menu_link() {
        create_shortcut(&l, &exe).context("creating Start menu shortcut")?;
    }
    if let Some(l) = desktop_link() {
        let _ = create_shortcut(&l, &exe); // desktop may be redirected/locked; not fatal
    }
    register_uninstall(&exe, version, exe_bytes.len() as u64).context("registering in Settings > Apps")?;
    Ok(exe)
}

fn create_shortcut(link: &Path, target: &Path) -> anyhow::Result<()> {
    unsafe {
        let sl: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
        sl.SetPath(&HSTRING::from(target.as_os_str()))?;
        if let Some(d) = target.parent() {
            sl.SetWorkingDirectory(&HSTRING::from(d.as_os_str()))?;
        }
        sl.SetDescription(w!("PadMix soundboard"))?;
        sl.SetIconLocation(&HSTRING::from(target.as_os_str()), 0)?;
        let pf: IPersistFile = sl.cast()?;
        pf.Save(&HSTRING::from(link.as_os_str()), true)?;
    }
    Ok(())
}

fn set_sz(key: HKEY, name: &str, value: &str) -> anyhow::Result<()> {
    let wide: Vec<u16> = value.encode_utf16().chain(std::iter::once(0)).collect();
    let bytes = unsafe { std::slice::from_raw_parts(wide.as_ptr() as *const u8, wide.len() * 2) };
    unsafe { RegSetValueExW(key, &HSTRING::from(name), 0, REG_SZ, Some(bytes)).ok()? };
    Ok(())
}

fn set_dword(key: HKEY, name: &str, value: u32) -> anyhow::Result<()> {
    unsafe { RegSetValueExW(key, &HSTRING::from(name), 0, REG_DWORD, Some(&value.to_le_bytes())).ok()? };
    Ok(())
}

fn register_uninstall(exe: &Path, version: &str, size: u64) -> anyhow::Result<()> {
    let mut key = HKEY::default();
    unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            &HSTRING::from(UNINSTALL_KEY),
            0,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE,
            None,
            &mut key,
            None,
        )
        .ok()?;
    }
    let exe_s = exe.display().to_string();
    let r = (|| -> anyhow::Result<()> {
        set_sz(key, "DisplayName", APP_NAME)?;
        set_sz(key, "DisplayVersion", version)?;
        set_sz(key, "Publisher", PUBLISHER)?;
        set_sz(key, "DisplayIcon", &format!("{exe_s},0"))?;
        set_sz(key, "InstallLocation", &install_dir().display().to_string())?;
        set_sz(key, "UninstallString", &format!("\"{exe_s}\" --uninstall"))?;
        set_sz(key, "QuietUninstallString", &format!("\"{exe_s}\" --uninstall --quiet"))?;
        set_sz(key, "URLInfoAbout", URL)?;
        set_dword(key, "EstimatedSize", (size / 1024) as u32)?;
        set_dword(key, "NoModify", 1)?;
        set_dword(key, "NoRepair", 1)?;
        Ok(())
    })();
    unsafe {
        let _ = RegCloseKey(key);
    }
    r
}

/// Remove shortcuts and the Settings > Apps entry, optionally user data, and
/// delete the install folder once this process has exited.
pub fn uninstall(remove_data: bool) -> anyhow::Result<()> {
    for l in [start_menu_link(), desktop_link()].into_iter().flatten() {
        let _ = std::fs::remove_file(l);
    }
    unsafe {
        let _ = RegDeleteTreeW(HKEY_CURRENT_USER, &HSTRING::from(UNINSTALL_KEY));
    }
    let mut dirs = vec![install_dir()];
    if remove_data {
        if let Some(a) = std::env::var_os("APPDATA") {
            dirs.push(PathBuf::from(a).join(APP_NAME));
        }
        if let Some(l) = std::env::var_os("LOCALAPPDATA") {
            dirs.push(PathBuf::from(l).join(APP_NAME));
        }
    }
    // Safety: only ever delete folders literally named "PadMix".
    let dirs: Vec<PathBuf> = dirs
        .into_iter()
        .filter(|d| d.file_name().map(|n| n == APP_NAME).unwrap_or(false) && d.exists())
        .collect();
    if dirs.is_empty() {
        return Ok(());
    }
    // PadMix.exe can't delete itself while running: let cmd do it 2 s after we exit.
    let mut script = String::from("timeout /t 2 /nobreak >nul");
    for d in &dirs {
        script.push_str(&format!(" & rmdir /s /q \"{}\"", d.display()));
    }
    Command::new("cmd.exe")
        .args(["/d", "/c", &script])
        .creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS)
        .spawn()?;
    Ok(())
}
