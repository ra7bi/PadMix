//! One-click VB-CABLE setup.
//!
//! PadMix does not ship VB-CABLE (redistribution needs VB-Audio's license). Instead it
//! uses the official pack the user downloaded from vb-audio.com:
//!   1. find `VBCABLE_Driver_Pack*.zip` in Downloads (or let the user pick it)
//!   2. extract it into a fresh folder under %LOCALAPPDATA%\PadMix\vbcable
//!   3. check `VBCABLE_Setup_x64.exe` has a valid Authenticode signature from VB-Audio
//!   4. start the official installer; Windows shows its admin (UAC) prompt to the user
//! Nothing is installed silently: the user approves UAC and clicks "Install Driver".

use anyhow::{anyhow, bail, Context, Result};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

pub const DOWNLOAD_PAGE: &str = "https://vb-audio.com/Cable/";
/// Subject of VB-Audio's code-signing certificate (Vincent Burel, author of VB-CABLE).
const SIGNER: &str = "BUREL VINCENT";
const SETUP_EXE: &str = "VBCABLE_Setup_x64.exe";
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[derive(Clone, PartialEq)]
pub enum State {
    Idle,
    Working(String),
    /// Installer finished; Windows usually needs a restart before the cable appears.
    Finished,
    Failed(String),
}

pub type Shared = Arc<Mutex<State>>;

pub fn new_state() -> Shared {
    Arc::new(Mutex::new(State::Idle))
}

fn set(s: &Shared, v: State) {
    if let Ok(mut g) = s.lock() {
        *g = v;
    }
}

/// Newest `VBCABLE_Driver_Pack*.zip` in the user's Downloads folder (searched 2 levels deep).
pub fn find_pack() -> Option<PathBuf> {
    let home = std::env::var_os("USERPROFILE")?;
    let downloads = Path::new(&home).join("Downloads");
    let mut best: Option<(SystemTime, PathBuf)> = None;
    let mut visit = |dir: &Path| {
        let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
        let mut subdirs = Vec::new();
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                subdirs.push(p);
                continue;
            }
            if is_pack_name(&p) {
                let t = e.metadata().and_then(|m| m.modified()).unwrap_or(SystemTime::UNIX_EPOCH);
                if best.as_ref().map(|(bt, _)| t > *bt).unwrap_or(true) {
                    best = Some((t, p));
                }
            }
        }
        subdirs
    };
    for sub in visit(&downloads) {
        visit(&sub);
    }
    best.map(|(_, p)| p)
}

fn is_pack_name(p: &Path) -> bool {
    let Some(name) = p.file_name().and_then(|n| n.to_str()) else { return false };
    let n = name.to_ascii_lowercase();
    n.starts_with("vbcable_driver_pack") && n.ends_with(".zip")
}

/// Run the whole install in a background thread. `source` is the pack zip or the
/// extracted `VBCABLE_Setup_x64.exe`.
pub fn start(source: PathBuf, state: Shared) {
    set(&state, State::Working("Preparing VB-CABLE…".into()));
    std::thread::spawn(move || {
        if let Err(e) = run(&source, &state) {
            set(&state, State::Failed(format!("{e:#}")));
        }
    });
}

fn run(source: &Path, state: &Shared) -> Result<()> {
    let setup = prepare(source, |m| set(state, State::Working(m.into())))?;
    set(state, State::Working("Waiting for you: approve the admin prompt, then click \"Install Driver\"".into()));
    launch_elevated(&setup)?;
    set(state, State::Finished);
    Ok(())
}

/// Extract (if a zip) and verify. Returns the path of the verified installer.
/// Does not install anything.
pub fn prepare(source: &Path, mut progress: impl FnMut(&str)) -> Result<PathBuf> {
    let setup = if source.extension().map(|e| e.eq_ignore_ascii_case("zip")).unwrap_or(false) {
        progress("Extracting VB-CABLE pack…");
        extract(source)?.join(SETUP_EXE)
    } else {
        source.to_path_buf()
    };
    if !setup.is_file() {
        bail!("{SETUP_EXE} not found in the pack");
    }
    progress("Checking VB-Audio signature…");
    verify(&setup)?;
    Ok(setup)
}

/// Extract into a new, empty folder (never over existing files). Uses Windows' built-in tar.
fn extract(zip: &Path) -> Result<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("PadMix")
        .join("vbcable");
    let stem = zip.file_stem().and_then(|s| s.to_str()).unwrap_or("VBCABLE");
    let stamp = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let dir = base.join(format!("{stem}-{stamp}"));
    std::fs::create_dir_all(&dir).context("creating extract folder")?;

    let tar = std::env::var_os("SystemRoot")
        .map(|r| PathBuf::from(r).join("System32").join("tar.exe"))
        .filter(|p| p.is_file())
        .unwrap_or_else(|| PathBuf::from("tar.exe"));
    let out = Command::new(tar)
        .arg("-xf")
        .arg(zip)
        .arg("-C")
        .arg(&dir)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .context("running tar to extract the zip")?;
    if !out.status.success() {
        bail!("could not extract the zip: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(dir)
}

fn ps_quote(p: &Path) -> String {
    format!("'{}'", p.display().to_string().replace('\'', "''"))
}

fn powershell(script: &str) -> Result<std::process::Output> {
    Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", script])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .context("starting PowerShell")
}

/// Require a valid Authenticode signature whose signer is VB-Audio's author.
fn verify(exe: &Path) -> Result<()> {
    let script = format!(
        "$s = Get-AuthenticodeSignature -LiteralPath {}; $s.Status.ToString(); $s.SignerCertificate.Subject",
        ps_quote(exe)
    );
    let out = powershell(&script)?;
    let text = String::from_utf8_lossy(&out.stdout);
    let mut lines = text.lines();
    let status = lines.next().unwrap_or("").trim().to_string();
    let subject = lines.next().unwrap_or("").trim().to_string();
    check_signature(&status, &subject)
}

fn check_signature(status: &str, subject: &str) -> Result<()> {
    if status != "Valid" {
        bail!("installer signature is not valid ({status}). Download the pack again from vb-audio.com");
    }
    if !subject.to_ascii_uppercase().contains(SIGNER) {
        bail!("installer is not signed by VB-Audio (signer: {subject})");
    }
    Ok(())
}

/// Start the official installer with "Run as administrator" and wait for it to close.
fn launch_elevated(exe: &Path) -> Result<()> {
    let dir = exe.parent().ok_or_else(|| anyhow!("bad installer path"))?;
    let script = format!(
        "try {{ $p = Start-Process -FilePath {} -WorkingDirectory {} -Verb RunAs -Wait -PassThru -ErrorAction Stop; 'EXIT ' + $p.ExitCode }} catch {{ 'ERR ' + $_.Exception.Message }}",
        ps_quote(exe),
        ps_quote(dir)
    );
    let out = powershell(&script)?;
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if let Some(msg) = text.strip_prefix("ERR ") {
        if msg.to_lowercase().contains("cancel") {
            bail!("admin permission was declined, so VB-CABLE was not installed");
        }
        bail!("could not start the installer: {msg}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_name_matching() {
        assert!(is_pack_name(Path::new(r"C:\x\VBCABLE_Driver_Pack46.zip")));
        assert!(is_pack_name(Path::new(r"C:\x\vbcable_driver_pack45.ZIP")));
        assert!(!is_pack_name(Path::new(r"C:\x\VBCABLE_Driver_Pack46.exe")));
        assert!(!is_pack_name(Path::new(r"C:\x\other.zip")));
    }

    #[test]
    fn signature_rules() {
        assert!(check_signature("Valid", "CN=BUREL VINCENT Entrepreneur individuel, O=...").is_ok());
        assert!(check_signature("NotSigned", "").is_err());
        assert!(check_signature("HashMismatch", "CN=BUREL VINCENT").is_err());
        assert!(check_signature("Valid", "CN=Someone Else").is_err());
    }

    #[test]
    fn ps_quote_escapes_single_quotes() {
        assert_eq!(ps_quote(Path::new(r"C:\it's\a.exe")), r"'C:\it''s\a.exe'");
    }
}
