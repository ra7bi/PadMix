//! System-wide hotkeys. The handler runs on the Win32 message loop (main thread),
//! so hotkeys keep working while the window is minimized or unfocused.

use crate::engine::Cmd;
use crate::mixer::Clip;
use global_hotkey::hotkey::HotKey;
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy)]
pub enum Action {
    Play(u64),
    StopAll,
}

#[derive(Default)]
pub struct Table {
    pub actions: HashMap<u32, Action>,
    pub clips: HashMap<u64, Arc<Clip>>,
    pub tx: Option<Sender<Cmd>>,
}

pub struct Hotkeys {
    manager: Option<GlobalHotKeyManager>,
    registered: Vec<HotKey>,
    pub table: Arc<Mutex<Table>>,
}

impl Hotkeys {
    pub fn new() -> Self {
        let table = Arc::new(Mutex::new(Table::default()));
        let manager = GlobalHotKeyManager::new().ok();
        let t = table.clone();
        GlobalHotKeyEvent::set_event_handler(Some(move |ev: GlobalHotKeyEvent| {
            if ev.state != HotKeyState::Pressed {
                return;
            }
            let Ok(t) = t.lock() else { return };
            let Some(tx) = t.tx.as_ref() else { return };
            match t.actions.get(&ev.id) {
                Some(Action::Play(id)) => {
                    if let Some(c) = t.clips.get(id) {
                        let _ = tx.send(Cmd::Play(c.clone()));
                    }
                }
                Some(Action::StopAll) => {
                    let _ = tx.send(Cmd::StopAll);
                }
                None => {}
            }
        }));
        Self { manager, registered: Vec::new(), table }
    }

    pub fn available(&self) -> bool {
        self.manager.is_some()
    }

    /// Replace all bindings. Returns the hotkeys that could not be registered
    /// (invalid, or already taken by another app).
    pub fn rebind(&mut self, bindings: &[(String, Action)]) -> Vec<String> {
        let mut failed = Vec::new();
        let Some(m) = self.manager.as_ref() else {
            return bindings.iter().map(|b| b.0.clone()).collect();
        };
        for hk in self.registered.drain(..) {
            let _ = m.unregister(hk);
        }
        let mut actions = HashMap::new();
        for (text, action) in bindings {
            match HotKey::from_str(text) {
                Ok(hk) => {
                    if m.register(hk).is_ok() {
                        actions.insert(hk.id(), *action);
                        self.registered.push(hk);
                    } else {
                        failed.push(text.clone());
                    }
                }
                Err(_) => failed.push(text.clone()),
            }
        }
        if let Ok(mut t) = self.table.lock() {
            t.actions = actions;
        }
        failed
    }
}

/// Result of pressing a key while setting a hotkey.
#[derive(Debug, PartialEq)]
pub enum Captured {
    /// Valid hotkey text, e.g. "ctrl+shift+KeyA".
    Ok(String),
    /// Rejected combination, with a reason to show the user.
    TooSimple(String),
}

/// Turn an egui key press into a hotkey. Letters, digits and other keys need two
/// modifiers (Ctrl+Shift, Ctrl+Alt, Alt+Shift) so PadMix never steals everyday
/// shortcuts like Ctrl+A / Ctrl+C system-wide. F-keys may be used alone.
/// Returns None for keys that can't be hotkeys (ignored while waiting).
pub fn capture(key: egui::Key, m: egui::Modifiers) -> Option<Captured> {
    let text = from_egui(key, m)?;
    let fkey = text.rsplit('+').next().map(|k| k.starts_with('F') && k.len() <= 3).unwrap_or(false);
    let mods = [m.ctrl || m.command, m.alt, m.shift].iter().filter(|b| **b).count();
    if fkey || mods >= 2 {
        Some(Captured::Ok(text))
    } else {
        Some(Captured::TooSimple(format!(
            "{} would block that shortcut in every app — add another key, e.g. Ctrl+Shift",
            pretty(&text)
        )))
    }
}

/// Same rule as `capture`, applied to saved hotkey text (e.g. from an older config).
pub fn is_safe(text: &str) -> bool {
    let parts: Vec<String> = text.split('+').map(|p| p.trim().to_ascii_lowercase()).collect();
    let Some(key) = parts.last() else { return false };
    let fkey = key.starts_with('f') && key.len() <= 3 && key[1..].chars().all(|c| c.is_ascii_digit());
    let mods = parts[..parts.len() - 1]
        .iter()
        .filter(|p| matches!(p.as_str(), "ctrl" | "control" | "alt" | "option" | "shift"))
        .count();
    fkey || mods >= 2
}

/// Turn an egui key press into the hotkey text global-hotkey understands
/// (e.g. "ctrl+shift+KeyA"). Requires at least one modifier, except F-keys.
pub fn from_egui(key: egui::Key, m: egui::Modifiers) -> Option<String> {
    use egui::Key::*;
    let k = match key {
        A => "KeyA", B => "KeyB", C => "KeyC", D => "KeyD", E => "KeyE", F => "KeyF",
        G => "KeyG", H => "KeyH", I => "KeyI", J => "KeyJ", K => "KeyK", L => "KeyL",
        M => "KeyM", N => "KeyN", O => "KeyO", P => "KeyP", Q => "KeyQ", R => "KeyR",
        S => "KeyS", T => "KeyT", U => "KeyU", V => "KeyV", W => "KeyW", X => "KeyX",
        Y => "KeyY", Z => "KeyZ",
        Num0 => "Digit0", Num1 => "Digit1", Num2 => "Digit2", Num3 => "Digit3", Num4 => "Digit4",
        Num5 => "Digit5", Num6 => "Digit6", Num7 => "Digit7", Num8 => "Digit8", Num9 => "Digit9",
        F1 => "F1", F2 => "F2", F3 => "F3", F4 => "F4", F5 => "F5", F6 => "F6",
        F7 => "F7", F8 => "F8", F9 => "F9", F10 => "F10", F11 => "F11", F12 => "F12",
        Space => "Space", Backspace => "Backspace", Enter => "Enter", Tab => "Tab",
        _ => return None,
    };
    let fkey = k.starts_with('F') && k.len() <= 3;
    let mut parts = Vec::new();
    if m.ctrl || m.command {
        parts.push("ctrl");
    }
    if m.alt {
        parts.push("alt");
    }
    if m.shift {
        parts.push("shift");
    }
    if parts.is_empty() && !fkey {
        return None;
    }
    parts.push(k);
    Some(parts.join("+"))
}

/// Short label for the UI: "ctrl+shift+KeyA" -> "Ctrl+Shift+A".
pub fn pretty(text: &str) -> String {
    text.split('+')
        .map(|t| {
            let t = t.trim();
            let t = t.strip_prefix("Key").or_else(|| t.strip_prefix("Digit")).unwrap_or(t);
            let mut c = t.chars();
            match c.next() {
                Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join("+")
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::{Key, Modifiers};

    fn m(ctrl: bool, alt: bool, shift: bool) -> Modifiers {
        Modifiers { ctrl, alt, shift, ..Default::default() }
    }

    #[test]
    fn everyday_shortcuts_are_rejected() {
        for k in [Key::A, Key::C, Key::V, Key::Z, Key::Num1] {
            assert!(matches!(capture(k, m(true, false, false)), Some(Captured::TooSimple(_))));
        }
    }

    #[test]
    fn two_modifiers_or_fkeys_are_accepted() {
        assert_eq!(capture(Key::A, m(true, false, true)), Some(Captured::Ok("ctrl+shift+KeyA".into())));
        assert_eq!(capture(Key::Num1, m(true, true, false)), Some(Captured::Ok("ctrl+alt+Digit1".into())));
        assert_eq!(capture(Key::F9, m(false, false, false)), Some(Captured::Ok("F9".into())));
    }

    #[test]
    fn plain_letters_are_ignored() {
        assert_eq!(capture(Key::A, m(false, false, false)), None);
    }

    #[test]
    fn saved_hotkey_rule() {
        assert!(!is_safe("ctrl+KeyA"));
        assert!(is_safe("ctrl+shift+KeyA"));
        assert!(is_safe("ctrl+shift+Backspace"));
        assert!(is_safe("F9"));
        assert!(!is_safe("KeyA"));
    }

    #[test]
    fn pretty_labels() {
        assert_eq!(pretty("ctrl+shift+KeyA"), "Ctrl+Shift+A");
        assert_eq!(pretty("ctrl+alt+Digit1"), "Ctrl+Alt+1");
    }
}
