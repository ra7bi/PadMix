//! Settings and sound list, saved to %APPDATA%\PadMix\config.json.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SoundEntry {
    pub id: u64,
    pub path: PathBuf,
    pub name: String,
    #[serde(default)]
    pub hotkey: Option<String>,
    /// Per-sound volume, 0.0 – 2.0.
    #[serde(default = "one")]
    pub volume: f32,
}

fn one() -> f32 {
    1.0
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct Config {
    pub mic: Option<String>,
    pub output: Option<String>,
    pub monitor: Option<String>,
    pub monitor_enabled: bool,
    pub mic_gain: f32,
    pub snd_gain: f32,
    pub mon_gain: f32,
    pub stop_hotkey: Option<String>,
    /// Even out loudness between sound files.
    pub normalize: bool,
    pub sounds: Vec<SoundEntry>,
    pub next_id: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            mic: None,
            output: None,
            monitor: None,
            monitor_enabled: true,
            mic_gain: 1.0,
            snd_gain: 0.8,
            mon_gain: 0.6,
            stop_hotkey: Some("ctrl+shift+Backspace".into()),
            normalize: true,
            sounds: Vec::new(),
            next_id: 1,
        }
    }
}

pub fn dir() -> PathBuf {
    let base = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    base.join("PadMix")
}

fn file() -> PathBuf {
    dir().join("config.json")
}

impl Config {
    pub fn load() -> Self {
        std::fs::read_to_string(file())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) {
        let _ = std::fs::create_dir_all(dir());
        if let Ok(s) = serde_json::to_string_pretty(self) {
            // write to temp then rename so a crash never leaves a half-written file
            let tmp = file().with_extension("json.tmp");
            if std::fs::write(&tmp, s).is_ok() {
                let _ = std::fs::rename(&tmp, file());
            }
        }
    }

    pub fn alloc_id(&mut self) -> u64 {
        let id = self.next_id.max(1);
        self.next_id = id + 1;
        id
    }
}
