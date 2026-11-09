//! PadMix window: modern, simple, direct.

use crate::clip::{self, Decoded};
use crate::config::{Config, SoundEntry};
use crate::engine::{self, Cmd, DeviceLists, Engine, Shared};
use crate::hotkeys::{self, Action, Hotkeys};
use crate::installer;
use crate::mixer::Clip;
use crate::sys;
use crate::winaudio;
use egui::{pos2, vec2, Align2, Color32, FontId, Rect, RichText, Sense, Shape, Stroke};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

// ---- palette ----
const BG: Color32 = Color32::from_rgb(14, 16, 20);
const PANEL: Color32 = Color32::from_rgb(22, 26, 34);
const CARD: Color32 = Color32::from_rgb(29, 34, 44);
const CARD_HOVER: Color32 = Color32::from_rgb(37, 43, 55);
const CARD_PLAY: Color32 = Color32::from_rgb(40, 34, 70);
const ACCENT: Color32 = Color32::from_rgb(124, 92, 255);
const GREEN: Color32 = Color32::from_rgb(34, 211, 166);
const AMBER: Color32 = Color32::from_rgb(245, 165, 36);
const RED: Color32 = Color32::from_rgb(239, 68, 68);
const TEXT: Color32 = Color32::from_rgb(230, 232, 238);
const MUTED: Color32 = Color32::from_rgb(138, 144, 160);

struct SoundItem {
    entry: SoundEntry,
    decoded: Option<Arc<Decoded>>,
    clip: Option<Arc<Clip>>,
    error: Option<String>,
    loading: bool,
}

struct Loaded {
    id: u64,
    rate: u32,
    res: Result<(Arc<Decoded>, Arc<Clip>), String>,
}

#[derive(Clone, Copy, PartialEq)]
enum Capture {
    Sound(u64),
    StopAll,
}

enum UiAction {
    Toggle(u64),
    SetHotkey(u64),
    ClearHotkey(u64),
    Rename(u64),
    Remove(u64),
    /// (id, volume, commit): commit=true reloads the clip with the new volume.
    SetVolume(u64, f32, bool),
}

const SOURCE_URL: &str = "https://github.com/ra7bi/PadMix";

enum WinAction {
    Set(winaudio::Endpoint),
    Recommended,
}

pub struct App {
    cfg: Config,
    sounds: Vec<SoundItem>,
    devices: DeviceLists,
    cable: Option<String>,
    active_output: Option<String>,
    active_mic: Option<String>,
    active_monitor: Option<String>,
    shared: Arc<Shared>,
    engine: Option<Engine>,
    engine_err: Option<String>,
    hotkeys: Hotkeys,
    hotkey_fail: Vec<String>,
    load_tx: Sender<Loaded>,
    load_rx: Receiver<Loaded>,
    search: String,
    settings_open: bool,
    capture: Option<Capture>,
    rename: Option<(u64, String)>,
    toast: Option<(String, Instant)>,
    install: installer::Shared,
    /// Windows' own default speaker/mic.
    win: Option<winaudio::Defaults>,
    /// Device watcher: re-check devices every few seconds.
    last_watch: Instant,
    /// Pending automatic reconnect after an audio device error.
    restart_at: Option<Instant>,
    restart_tries: u32,
    /// About window: time it was opened (drives the animation).
    about_open: Option<f64>,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        apply_theme(&cc.egui_ctx);
        let cfg = Config::load();
        let shared = Arc::new(Shared::new(cfg.mic_gain, cfg.snd_gain, cfg.mon_gain));
        let (load_tx, load_rx) = mpsc::channel();
        let sounds = cfg
            .sounds
            .iter()
            .cloned()
            .map(|entry| SoundItem { entry, decoded: None, clip: None, error: None, loading: false })
            .collect();
        let mut app = Self {
            cfg,
            sounds,
            devices: engine::list_devices(),
            cable: None,
            active_output: None,
            active_mic: None,
            active_monitor: None,
            shared,
            engine: None,
            engine_err: None,
            hotkeys: Hotkeys::new(),
            hotkey_fail: Vec::new(),
            load_tx,
            load_rx,
            search: String::new(),
            settings_open: false,
            capture: None,
            rename: None,
            toast: None,
            install: installer::new_state(),
            win: winaudio::read().ok(),
            last_watch: Instant::now(),
            restart_at: None,
            restart_tries: 0,
            about_open: None,
        };
        // drop hotkeys from older configs that would hijack everyday shortcuts (e.g. Ctrl+A)
        let mut cleared = Vec::new();
        for s in &mut app.sounds {
            if let Some(h) = s.entry.hotkey.clone() {
                if !hotkeys::is_safe(&h) {
                    cleared.push(hotkeys::pretty(&h));
                    s.entry.hotkey = None;
                }
            }
        }
        if !cleared.is_empty() {
            app.save();
            app.toast(format!("Removed hotkey {} — it blocked that shortcut in every app. Set a new one (e.g. Ctrl+Shift+1).", cleared.join(", ")));
        }
        app.restart_engine();
        app.load_all();
        app.sync_hotkeys();
        app
    }

    fn rate(&self) -> u32 {
        self.engine.as_ref().map(|e| e.rate).unwrap_or(48_000)
    }

    fn toast(&mut self, msg: impl Into<String>) {
        self.toast = Some((msg.into(), Instant::now()));
    }

    // ---------- engine ----------
    fn restart_engine(&mut self) {
        let old_rate = self.rate();
        self.engine = None; // release devices first
        self.devices = engine::list_devices();
        self.cable = engine::find_cable(&self.devices.outputs);
        let outs = &self.devices.outputs;
        let ins = &self.devices.inputs;

        // Once VB-CABLE exists, PadMix always sends to a cable — sending to speakers or
        // headphones means nobody in Discord hears anything. Other outputs are only
        // allowed (for testing) when no cable is installed.
        let output = match &self.cable {
            Some(cable) => Some(
                self.cfg
                    .output
                    .clone()
                    .filter(|o| outs.contains(o) && engine::is_cable_playback(o))
                    .unwrap_or_else(|| cable.clone()),
            ),
            None => self.cfg.output.clone().filter(|o| outs.contains(o)),
        };
        // never use the cable's own recording side as the mic (feedback loop)
        let usable = |m: &String| ins.contains(m) && !engine::is_cable_capture(m);
        let mic = self
            .cfg
            .mic
            .clone()
            .filter(|m| usable(m))
            .or_else(|| self.devices.default_input.clone().filter(|m| usable(m)))
            .or_else(|| ins.iter().find(|m| usable(m)).cloned());
        // Monitor = your headphones. Never a cable (that would send the sounds twice,
        // or into the mic), even if Windows' default output is currently the cable.
        let monitor = if self.cfg.monitor_enabled {
            let ok = |m: &String| outs.contains(m) && !engine::is_cable_playback(m) && Some(m) != output.as_ref();
            self.cfg
                .monitor
                .clone()
                .filter(|m| ok(m))
                .or_else(|| self.devices.default_output.clone().filter(|m| ok(m)))
                .or_else(|| outs.iter().find(|m| ok(m) && m.to_lowercase().contains("headphone")).cloned())
                .or_else(|| outs.iter().find(|m| ok(m)).cloned())
        } else {
            None
        };
        self.active_output = output.clone();
        self.active_mic = mic.clone();
        self.active_monitor = monitor.clone();

        let Some(output) = output else {
            self.engine_err = Some("VB-CABLE not found".into());
            self.set_tx(None);
            return;
        };
        match Engine::start(mic.as_deref(), &output, monitor.as_deref(), self.shared.clone()) {
            Ok(e) => {
                self.engine_err = None;
                self.set_tx(Some(e.sender()));
                let new_rate = e.rate;
                self.engine = Some(e);
                if new_rate != old_rate {
                    self.load_all();
                }
            }
            Err(e) => {
                sys::log(&format!("engine start failed: {e:#}"));
                self.engine_err = Some(format!("{e:#}"));
                self.set_tx(None);
            }
        }
        sys::log(&format!(
            "engine: out={:?} mic={:?} monitor={:?} ok={}",
            self.active_output,
            self.active_mic,
            self.active_monitor,
            self.engine.is_some()
        ));
    }

    fn refresh_windows(&mut self) {
        self.win = winaudio::read().ok();
    }

    /// Point Windows' default speaker (all roles) at your headphones and its default
    /// mic at your real mic, so "Windows Default" in any app can't hit the cable.
    fn apply_windows_setup(&mut self) {
        let Some(d) = winaudio::read().ok() else {
            self.toast("Couldn't read Windows sound settings");
            return;
        };
        let mut done = Vec::new();
        if let Some(sp) = winaudio::pick_speaker(&d, self.active_monitor.as_deref()) {
            match winaudio::set_default(&sp.id) {
                Ok(()) => done.push(format!("speaker: {}", sp.name)),
                Err(e) => sys::log(&format!("set default speaker failed: {e:#}")),
            }
        }
        if let Some(mic) = self.active_mic.as_deref().and_then(|m| d.find_mic(m)) {
            match winaudio::set_default(&mic.id) {
                Ok(()) => done.push(format!("mic: {}", mic.name)),
                Err(e) => sys::log(&format!("set default mic failed: {e:#}")),
            }
        }
        self.refresh_windows();
        if done.is_empty() {
            self.toast("Windows didn't accept the change — use Open Sound settings");
        } else {
            sys::log(&format!("windows setup: {}", done.join(", ")));
            self.toast(format!("Windows sound set: {}", done.join(" · ")));
        }
    }

    fn set_windows_default(&mut self, ep: winaudio::Endpoint) {
        match winaudio::set_default(&ep.id) {
            Ok(()) => self.toast(format!("Windows default set to {}", ep.name)),
            Err(e) => self.toast(format!("Couldn't change it: {e}")),
        }
        self.refresh_windows();
    }

    /// Every 3 s: notice plugged/unplugged devices (or a freshly installed cable)
    /// and reconnect automatically; also retry after audio errors.
    fn watch_devices(&mut self) {
        if let Some(at) = self.restart_at {
            if Instant::now() >= at {
                self.restart_at = None;
                self.restart_engine();
                if self.engine.is_none() && self.restart_tries < 5 {
                    self.restart_tries += 1;
                    self.restart_at = Some(Instant::now() + Duration::from_secs(2 * self.restart_tries as u64));
                } else if self.engine.is_some() {
                    if self.restart_tries > 0 || self.toast.is_some() {
                        self.toast("Audio reconnected");
                    }
                    self.restart_tries = 0;
                }
            }
        }
        if self.last_watch.elapsed() < Duration::from_secs(3) {
            return;
        }
        self.last_watch = Instant::now();
        let d = engine::list_devices();
        if d.inputs != self.devices.inputs || d.outputs != self.devices.outputs {
            sys::log("device list changed, reconnecting");
            let had_cable = self.cable.is_some();
            self.restart_engine();
            if !had_cable && self.cable.is_some() {
                self.toast("VB-CABLE found — PadMix is live");
            }
        }
        self.refresh_windows();
    }

    fn set_tx(&self, tx: Option<Sender<Cmd>>) {
        if let Ok(mut t) = self.hotkeys.table.lock() {
            t.tx = tx;
        }
    }

    // ---------- sounds ----------
    fn load_all(&mut self) {
        let ids: Vec<u64> = self.sounds.iter().map(|s| s.entry.id).collect();
        for id in ids {
            self.load(id);
        }
    }

    fn load(&mut self, id: u64) {
        let rate = self.rate();
        let Some(item) = self.sounds.iter_mut().find(|s| s.entry.id == id) else { return };
        item.loading = true;
        item.error = None;
        let path = item.entry.path.clone();
        let decoded = item.decoded.clone();
        let volume = item.entry.volume.clamp(0.0, 2.0);
        let normalize = self.cfg.normalize;
        let tx = self.load_tx.clone();
        std::thread::spawn(move || {
            let res = (|| -> anyhow::Result<(Arc<Decoded>, Arc<Clip>)> {
                let d = match decoded {
                    Some(d) => d,
                    None => Arc::new(clip::decode(&path)?),
                };
                let mut data = clip::resample(&d.stereo, d.rate, rate)?;
                let level = if normalize { clip::loudness_gain(&d.stereo) } else { 1.0 };
                clip::apply_gain(&mut data, level * volume);
                Ok((d, Arc::new(Clip { id, data })))
            })()
            .map_err(|e| format!("{e:#}"));
            let _ = tx.send(Loaded { id, rate, res });
        });
    }

    fn poll_loads(&mut self) {
        let rate = self.rate();
        let mut changed = false;
        while let Ok(l) = self.load_rx.try_recv() {
            if l.rate != rate {
                continue; // stale (engine rate changed meanwhile); a new load is pending
            }
            if let Some(item) = self.sounds.iter_mut().find(|s| s.entry.id == l.id) {
                item.loading = false;
                match l.res {
                    Ok((d, c)) => {
                        item.decoded = Some(d);
                        item.clip = Some(c);
                    }
                    Err(e) => item.error = Some(e),
                }
                changed = true;
            }
        }
        if changed {
            self.sync_clips();
        }
    }

    fn sync_clips(&self) {
        let clips: HashMap<u64, Arc<Clip>> = self
            .sounds
            .iter()
            .filter_map(|s| s.clip.clone().map(|c| (s.entry.id, c)))
            .collect();
        if let Ok(mut t) = self.hotkeys.table.lock() {
            t.clips = clips;
        }
    }

    fn sync_hotkeys(&mut self) {
        let mut b: Vec<(String, Action)> = self
            .sounds
            .iter()
            .filter_map(|s| s.entry.hotkey.clone().map(|h| (h, Action::Play(s.entry.id))))
            .collect();
        if let Some(h) = self.cfg.stop_hotkey.clone() {
            b.push((h, Action::StopAll));
        }
        self.hotkey_fail = self.hotkeys.rebind(&b);
        self.sync_clips();
    }

    fn add_files(&mut self, paths: Vec<PathBuf>) {
        let mut added = 0;
        for p in paths {
            if !clip::is_supported(&p) || self.sounds.iter().any(|s| s.entry.path == p) {
                continue;
            }
            let id = self.cfg.alloc_id();
            let name = p.file_stem().and_then(|s| s.to_str()).unwrap_or("Sound").to_string();
            self.sounds.push(SoundItem {
                entry: SoundEntry { id, path: p, name, hotkey: None, volume: 1.0 },
                decoded: None,
                clip: None,
                error: None,
                loading: false,
            });
            self.load(id);
            added += 1;
        }
        if added > 0 {
            self.save();
            self.toast(format!("Added {added} sound{}", if added == 1 { "" } else { "s" }));
        }
    }

    fn save(&mut self) {
        self.cfg.sounds = self.sounds.iter().map(|s| s.entry.clone()).collect();
        self.cfg.save();
    }

    fn is_playing(&self, id: u64) -> Option<f32> {
        self.shared.playing.lock().ok()?.iter().find(|(i, _)| *i == id).map(|(_, p)| *p)
    }

    fn toggle(&mut self, id: u64) {
        let Some(engine) = self.engine.as_ref() else {
            self.toast("Not connected to an output device");
            return;
        };
        if self.is_playing(id).is_some() {
            engine.send(Cmd::Stop(id));
            return;
        }
        match self.sounds.iter().find(|s| s.entry.id == id) {
            Some(s) if s.clip.is_some() => engine.send(Cmd::Play(s.clip.clone().unwrap())),
            Some(s) if s.loading => self.toast("Still loading…"),
            Some(s) if s.error.is_some() => self.toast("This file can't be played"),
            _ => {}
        }
    }

    fn stop_all(&self) {
        if let Some(e) = self.engine.as_ref() {
            e.send(Cmd::StopAll);
        }
    }

    fn apply(&mut self, a: UiAction) {
        match a {
            UiAction::Toggle(id) => self.toggle(id),
            UiAction::SetHotkey(id) => self.capture = Some(Capture::Sound(id)),
            UiAction::ClearHotkey(id) => {
                if let Some(s) = self.sounds.iter_mut().find(|s| s.entry.id == id) {
                    s.entry.hotkey = None;
                }
                self.sync_hotkeys();
                self.save();
            }
            UiAction::Rename(id) => {
                if let Some(s) = self.sounds.iter().find(|s| s.entry.id == id) {
                    self.rename = Some((id, s.entry.name.clone()));
                }
            }
            UiAction::Remove(id) => {
                if let Some(e) = self.engine.as_ref() {
                    e.send(Cmd::Stop(id));
                }
                self.sounds.retain(|s| s.entry.id != id);
                self.sync_hotkeys();
                self.save();
            }
            UiAction::SetVolume(id, v, commit) => {
                if let Some(s) = self.sounds.iter_mut().find(|s| s.entry.id == id) {
                    s.entry.volume = v.clamp(0.0, 2.0);
                }
                if commit {
                    self.load(id);
                    self.save();
                }
            }
        }
    }

    fn finish_capture(&mut self, text: String) {
        // one hotkey = one action
        for s in &mut self.sounds {
            if s.entry.hotkey.as_deref() == Some(text.as_str()) {
                s.entry.hotkey = None;
            }
        }
        if self.cfg.stop_hotkey.as_deref() == Some(text.as_str()) {
            self.cfg.stop_hotkey = None;
        }
        match self.capture.take() {
            Some(Capture::Sound(id)) => {
                if let Some(s) = self.sounds.iter_mut().find(|s| s.entry.id == id) {
                    s.entry.hotkey = Some(text.clone());
                }
            }
            Some(Capture::StopAll) => self.cfg.stop_hotkey = Some(text.clone()),
            None => {}
        }
        self.sync_hotkeys();
        self.save();
        if self.hotkey_fail.contains(&text) {
            self.toast(format!("{} is used by another app — pick another", hotkeys::pretty(&text)));
        } else {
            self.toast(format!("Hotkey set: {}", hotkeys::pretty(&text)));
        }
    }

    // ---------- UI pieces ----------
    fn status(&self) -> (Color32, String) {
        match (&self.engine, &self.active_output) {
            (Some(_), Some(o)) if Some(o) == self.cable.as_ref() => (GREEN, "Live · sending to VB-CABLE".into()),
            (Some(_), Some(o)) => (AMBER, format!("Live · {o} (not VB-CABLE)")),
            _ if self.cable.is_none() => (AMBER, "VB-CABLE not installed".into()),
            _ => (RED, "Not connected".into()),
        }
    }

    fn top_bar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("top")
            .frame(egui::Frame::none().fill(PANEL).inner_margin(egui::Margin::symmetric(18.0, 12.0)))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("PadMix").size(22.0).strong().color(TEXT));
                    ui.add_space(10.0);
                    let (c, s) = self.status();
                    pill(ui, c, &s);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.add(egui::Button::new(RichText::new("About").size(14.0)).frame(false).min_size(vec2(60.0, 32.0))).clicked() {
                            self.about_open = Some(ui.input(|i| i.time));
                        }
                        let gear = if self.settings_open { "×  Close" } else { "⚙  Settings" };
                        if ui.add(egui::Button::new(RichText::new(gear).size(14.0)).min_size(vec2(104.0, 32.0))).clicked() {
                            self.settings_open = !self.settings_open;
                            if self.settings_open {
                                self.refresh_windows();
                            }
                        }
                        let stop = egui::Button::new(RichText::new("⏹  Stop all").size(14.0).color(TEXT))
                            .fill(Color32::from_rgb(60, 30, 36))
                            .min_size(vec2(110.0, 32.0));
                        if ui.add(stop).clicked() {
                            self.stop_all();
                        }
                    });
                });
            });

        if self.cable.is_none() {
            self.cable_banner(ctx);
        } else if self.win.as_ref().map(|w| w.speaker_is_cable()).unwrap_or(false) {
            self.default_output_banner(ctx);
        }
    }

    /// Windows' default speaker (for apps or for calls) is the cable: every PC sound
    /// (Discord voices, games, music) would be sent into the "microphone".
    fn default_output_banner(&mut self, ctx: &egui::Context) {
        let mut fix = false;
        let target = self.win.as_ref().and_then(|w| winaudio::pick_speaker(w, self.active_monitor.as_deref()));
        egui::TopBottomPanel::top("default_out_banner")
            .frame(egui::Frame::none().fill(Color32::from_rgb(56, 22, 26)).inner_margin(egui::Margin::symmetric(18.0, 12.0)))
            .show(ctx, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new("⚠  Windows is playing PC sound into VB-CABLE").color(RED).strong());
                    ui.label(RichText::new("— others would hear your games and friends' voices.").color(TEXT));
                    if let Some(t) = &target {
                        let b = egui::Button::new(RichText::new(format!("Fix now: use {}", short(&t.name))).color(Color32::WHITE).strong())
                            .fill(ACCENT)
                            .min_size(vec2(150.0, 28.0));
                        if ui.add(b).on_hover_text("Makes it Windows' default speaker for apps and calls").clicked() {
                            fix = true;
                        }
                    }
                    if ui.button("Open Sound settings").clicked() {
                        open_sound_settings();
                    }
                });
            });
        if let (true, Some(t)) = (fix, target) {
            self.set_windows_default(t);
        }
    }

    /// Shown while VB-CABLE is missing: one-click install from the official pack.
    fn cable_banner(&mut self, ctx: &egui::Context) {
        let state = self.install.lock().map(|s| s.clone()).unwrap_or(installer::State::Idle);
        let mut click_install = false;
        let mut refresh = false;
        egui::TopBottomPanel::top("cable_banner")
            .frame(egui::Frame::none().fill(Color32::from_rgb(40, 34, 18)).inner_margin(egui::Margin::symmetric(18.0, 12.0)))
            .show(ctx, |ui| {
                ui.horizontal_wrapped(|ui| match &state {
                    installer::State::Idle | installer::State::Failed(_) => {
                        ui.label(RichText::new("⚠  VB-CABLE is needed so others can hear your sounds.").color(AMBER).strong());
                        let b = egui::Button::new(RichText::new("⬇  Install VB-CABLE").color(Color32::WHITE).strong())
                            .fill(ACCENT)
                            .min_size(vec2(160.0, 30.0));
                        if ui.add(b).on_hover_text("Uses the official pack from vb-audio.com. Windows will ask for admin.").clicked() {
                            click_install = true;
                        }
                        ui.hyperlink_to("Download page", installer::DOWNLOAD_PAGE);
                        if ui.button("Refresh").clicked() {
                            refresh = true;
                        }
                        if let installer::State::Failed(msg) = &state {
                            ui.end_row();
                            ui.label(RichText::new(format!("Install failed: {msg}")).color(RED).size(12.5));
                        }
                    }
                    installer::State::Working(msg) => {
                        ui.add(egui::Spinner::new().color(ACCENT));
                        ui.label(RichText::new(msg).color(TEXT));
                    }
                    installer::State::Finished => {
                        ui.label(RichText::new("✔  VB-CABLE installer finished.").color(GREEN).strong());
                        ui.label(RichText::new("Restart Windows, then open PadMix again — it will go live automatically.").color(TEXT));
                        if ui.button("Refresh").clicked() {
                            refresh = true;
                        }
                    }
                });
            });

        if click_install {
            let source = installer::find_pack().or_else(|| {
                rfd::FileDialog::new()
                    .set_title("Select the VB-CABLE pack (VBCABLE_Driver_Pack*.zip or VBCABLE_Setup_x64.exe)")
                    .add_filter("VB-CABLE pack", &["zip", "exe"])
                    .pick_file()
            });
            match source {
                Some(p) => installer::start(p, self.install.clone()),
                None => {
                    ctx.open_url(egui::OpenUrl::new_tab(installer::DOWNLOAD_PAGE));
                    self.toast("Download the VB-CABLE pack, then click Install again");
                }
            }
        }
        if refresh {
            self.restart_engine();
            if self.cable.is_none() {
                self.toast("VB-CABLE not found yet — a Windows restart is usually needed");
            }
        }
    }

    fn bottom_bar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::bottom("controls")
            .frame(egui::Frame::none().fill(PANEL).inner_margin(egui::Margin::symmetric(18.0, 12.0)))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    // mic
                    let muted = self.shared.mic_muted.load(std::sync::atomic::Ordering::Relaxed);
                    let (icon, col) = if muted { ("🔇  Muted", RED) } else { ("🎤  Mic", TEXT) };
                    let b = egui::Button::new(RichText::new(icon).color(col)).min_size(vec2(92.0, 30.0));
                    if ui.add(b).on_hover_text("Mute / unmute your voice").clicked() {
                        self.shared.mic_muted.store(!muted, std::sync::atomic::Ordering::Relaxed);
                    }
                    meter(ui, if muted { 0.0 } else { self.shared.mic_peak() * self.cfg.mic_gain });
                    if slider(ui, &mut self.cfg.mic_gain, 0.0..=2.0, "Voice") {
                        self.shared.set_mic_gain(self.cfg.mic_gain);
                    }
                    ui.separator();
                    ui.label(RichText::new("🔊  Sounds").color(TEXT));
                    if slider(ui, &mut self.cfg.snd_gain, 0.0..=1.5, "Volume others hear") {
                        self.shared.set_snd_gain(self.cfg.snd_gain);
                    }
                    ui.separator();
                    ui.label(RichText::new("🎧  You hear").color(TEXT));
                    if slider(ui, &mut self.cfg.mon_gain, 0.0..=1.5, "Volume in your headphones") {
                        self.shared.set_mon_gain(self.cfg.mon_gain);
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(RichText::new("Output").color(MUTED).size(12.0));
                        meter(ui, self.shared.out_peak());
                    });
                });
            });
    }

    fn settings_panel(&mut self, ctx: &egui::Context) {
        if !self.settings_open {
            return;
        }
        let mut restart = false;
        let mut win_action: Option<WinAction> = None;
        let mut reload = false;
        egui::SidePanel::right("settings")
            .exact_width(340.0)
            .resizable(false)
            .frame(egui::Frame::none().fill(PANEL).inner_margin(egui::Margin::same(18.0)))
            .show(ctx, |ui| {
              egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                ui.set_width(304.0);
                ui.label(RichText::new("Settings").size(18.0).strong().color(TEXT));
                ui.add_space(10.0);
                heading(ui, "PadMix devices");

                section(ui, "Microphone (your real mic)");
                let cur = self.active_mic.clone().unwrap_or_else(|| "None".into());
                egui::ComboBox::from_id_salt("mic").width(284.0).selected_text(short(&cur)).show_ui(ui, |ui| {
                    for d in self.devices.inputs.clone() {
                        if engine::is_cable_capture(&d) {
                            continue; // PadMix's own output — would loop back
                        }
                        if ui.selectable_label(Some(&d) == self.active_mic.as_ref(), &d).clicked() {
                            self.cfg.mic = Some(d);
                            restart = true;
                        }
                    }
                });
                ui.add_space(12.0);

                section(ui, "Send to (what others hear)");
                let cur = self.active_output.clone().unwrap_or_else(|| "None".into());
                egui::ComboBox::from_id_salt("out").width(284.0).selected_text(short(&cur)).show_ui(ui, |ui| {
                    // With VB-CABLE installed only cables are offered: anything else means
                    // nobody hears the sounds. Without it, any output can be picked for testing.
                    let have_cable = self.cable.is_some();
                    for d in self.devices.outputs.clone() {
                        let is_cable = engine::is_cable_playback(&d);
                        if have_cable && !is_cable {
                            continue;
                        }
                        let label = if is_cable { format!("★ {d}") } else { format!("{d}  (test only)") };
                        if ui.selectable_label(Some(&d) == self.active_output.as_ref(), label).clicked() {
                            self.cfg.output = Some(d);
                            restart = true;
                        }
                    }
                });
                ui.label(RichText::new("In Discord / games choose \"CABLE Output\" as your microphone.").size(12.0).color(MUTED));
                ui.add_space(12.0);

                section(ui, "Monitor (you hear the sounds)");
                if ui.checkbox(&mut self.cfg.monitor_enabled, "Play sounds in my headphones too").changed() {
                    restart = true;
                }
                ui.add_enabled_ui(self.cfg.monitor_enabled, |ui| {
                    let cur = self.active_monitor.clone().unwrap_or_else(|| "Default".into());
                    egui::ComboBox::from_id_salt("mon").width(284.0).selected_text(short(&cur)).show_ui(ui, |ui| {
                        for d in self.devices.outputs.clone() {
                            if engine::is_cable_playback(&d) {
                                continue;
                            }
                            if ui.selectable_label(Some(&d) == self.active_monitor.as_ref(), &d).clicked() {
                                self.cfg.monitor = Some(d);
                                restart = true;
                            }
                        }
                    });
                });
                if ui.add(egui::Button::new("↻  Refresh devices").min_size(vec2(284.0, 28.0))).clicked() {
                    restart = true;
                }
                if let Some(e) = &self.engine_err {
                    ui.label(RichText::new(e).color(AMBER).size(12.0));
                }

                // ---- Windows sound ----
                heading(ui, "Windows sound");
                ui.label(
                    RichText::new("What \"Windows Default\" means in Discord, games and every other app.")
                        .size(12.0)
                        .color(MUTED),
                );
                ui.add_space(4.0);
                match &self.win {
                    None => {
                        ui.label(RichText::new("Couldn't read Windows sound settings.").color(AMBER).size(12.5));
                    }
                    Some(w) => {
                        let w = w.clone();
                        if w.speaker_is_cable() {
                            ui.label(RichText::new("⚠  PC sound goes into VB-CABLE — others hear it").color(RED).size(12.5));
                        } else {
                            ui.label(RichText::new("✔  Windows is set up correctly").color(GREEN).size(12.5));
                        }
                        ui.add_space(4.0);

                        section(ui, "Speaker (apps and calls)");
                        let cur = w.speaker.as_ref().map(|e| e.name.clone()).unwrap_or_else(|| "None".into());
                        egui::ComboBox::from_id_salt("win_spk").width(284.0).selected_text(short(&cur)).show_ui(ui, |ui| {
                            for e in &w.speakers {
                                if engine::is_cable_playback(&e.name) {
                                    continue; // never offer the cable as the PC's speaker
                                }
                                if ui.selectable_label(w.speaker.as_ref() == Some(e), &e.name).clicked() {
                                    win_action = Some(WinAction::Set(e.clone()));
                                }
                            }
                        });
                        if let (Some(c), Some(s)) = (&w.speaker_comms, &w.speaker) {
                            if c != s {
                                ui.label(RichText::new(format!("Calls use: {}", short(&c.name))).size(12.0).color(
                                    if engine::is_cable_playback(&c.name) { RED } else { MUTED },
                                ));
                            }
                        }
                        ui.add_space(6.0);

                        section(ui, "Microphone (apps and calls)");
                        let cur = w.mic.as_ref().map(|e| e.name.clone()).unwrap_or_else(|| "None".into());
                        egui::ComboBox::from_id_salt("win_mic").width(284.0).selected_text(short(&cur)).show_ui(ui, |ui| {
                            for e in &w.mics {
                                let label = if engine::is_cable_capture(&e.name) {
                                    format!("{}  (every app hears PadMix)", e.name)
                                } else {
                                    e.name.clone()
                                };
                                if ui.selectable_label(w.mic.as_ref() == Some(e), label).clicked() {
                                    win_action = Some(WinAction::Set(e.clone()));
                                }
                            }
                        });
                        if let (Some(c), Some(m)) = (&w.mic_comms, &w.mic) {
                            if c != m {
                                ui.label(RichText::new(format!("Calls use: {}", short(&c.name))).size(12.0).color(MUTED));
                            }
                        }
                        ui.add_space(8.0);
                        let rec = egui::Button::new(RichText::new("✔  Recommended setup").color(Color32::WHITE).strong())
                            .fill(ACCENT)
                            .min_size(vec2(284.0, 30.0));
                        if ui
                            .add(rec)
                            .on_hover_text("Windows speaker = your headphones, Windows mic = your real mic (for apps and calls). Discord still uses CABLE Output.")
                            .clicked()
                        {
                            win_action = Some(WinAction::Recommended);
                        }
                        if ui.add(egui::Button::new("Open Windows Sound settings").min_size(vec2(284.0, 28.0))).clicked() {
                            open_sound_settings();
                        }
                    }
                }

                // ---- Sounds ----
                heading(ui, "Sounds");
                if ui
                    .checkbox(&mut self.cfg.normalize, "Even out loudness between sounds")
                    .on_hover_text("Quiet clips are boosted and loud ones turned down, so nothing blasts your friends")
                    .changed()
                {
                    reload = true;
                }
                ui.add_space(4.0);
                section(ui, "Stop-all hotkey");
                let label = self.cfg.stop_hotkey.as_deref().map(hotkeys::pretty).unwrap_or_else(|| "Not set".into());
                if ui.add(egui::Button::new(label).min_size(vec2(284.0, 28.0))).clicked() {
                    self.capture = Some(Capture::StopAll);
                }
                if !self.hotkeys.available() {
                    ui.label(RichText::new("Global hotkeys unavailable on this system.").color(AMBER).size(12.0));
                }
                ui.add_space(14.0);
                ui.label(
                    RichText::new("How it works: your mic + sounds are mixed and sent to VB-CABLE. In Discord / games set Input device to \"CABLE Output\".")
                        .size(12.0)
                        .color(MUTED),
                );
                if ui.add(egui::Button::new(RichText::new("Open log folder").size(12.0)).frame(false)).clicked() {
                    let _ = std::process::Command::new("explorer.exe").arg(crate::config::dir()).spawn();
                }
              });
            });
        if restart {
            self.save();
            self.restart_engine();
        }
        if reload {
            self.save();
            self.load_all();
        }
        match win_action {
            Some(WinAction::Set(e)) => self.set_windows_default(e),
            Some(WinAction::Recommended) => self.apply_windows_setup(),
            None => {}
        }
    }

    fn central(&mut self, ctx: &egui::Context) {
        let mut actions: Vec<UiAction> = Vec::new();
        let mut add_clicked = false;
        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(BG).inner_margin(egui::Margin::same(18.0)))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    let search = egui::TextEdit::singleline(&mut self.search)
                        .hint_text("🔍  Search sounds")
                        .desired_width(260.0)
                        .margin(vec2(10.0, 7.0));
                    ui.add(search);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let add = egui::Button::new(RichText::new("+  Add sounds").color(Color32::WHITE).size(14.0))
                            .fill(ACCENT)
                            .min_size(vec2(130.0, 32.0));
                        if ui.add(add).clicked() {
                            add_clicked = true;
                        }
                        ui.label(RichText::new(format!("{} sounds", self.sounds.len())).color(MUTED));
                    });
                });
                ui.add_space(14.0);

                if self.sounds.is_empty() {
                    empty_state(ui);
                    return;
                }

                let q = self.search.to_lowercase();
                let visible: Vec<usize> = (0..self.sounds.len())
                    .filter(|&i| q.is_empty() || self.sounds[i].entry.name.to_lowercase().contains(&q))
                    .collect();
                let playing: HashMap<u64, f32> = self.shared.playing.lock().map(|p| p.iter().copied().collect()).unwrap_or_default();

                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    let gap = 12.0;
                    let avail = ui.available_width();
                    let cols = (((avail + gap) / (190.0 + gap)).floor() as usize).max(1);
                    let w = ((avail - gap * (cols as f32 - 1.0)) / cols as f32).floor();
                    for row in visible.chunks(cols) {
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = gap;
                            for &i in row {
                                let s = &self.sounds[i];
                                card(ui, s, w, playing.get(&s.entry.id).copied(), self.capture, &mut actions);
                            }
                        });
                        ui.add_space(gap);
                    }
                });
            });
        if add_clicked {
            if let Some(files) = rfd::FileDialog::new().add_filter("Audio", clip::SUPPORTED_EXT).pick_files() {
                self.add_files(files);
            }
        }
        for a in actions {
            self.apply(a);
        }
    }

    fn overlays(&mut self, ctx: &egui::Context) {
        // hotkey capture
        if let Some(target) = self.capture {
            let mut got: Option<String> = None;
            let mut rejected: Option<String> = None;
            let mut cancel = false;
            ctx.input(|i| {
                for ev in &i.events {
                    if let egui::Event::Key { key, pressed: true, modifiers, .. } = ev {
                        if *key == egui::Key::Escape {
                            cancel = true;
                        } else {
                            match hotkeys::capture(*key, *modifiers) {
                                Some(hotkeys::Captured::Ok(t)) => got = Some(t),
                                Some(hotkeys::Captured::TooSimple(why)) => rejected = Some(why),
                                None => {}
                            }
                        }
                    }
                }
            });
            if let Some(why) = rejected {
                self.toast(why);
            }
            let what = match target {
                Capture::StopAll => "Stop all".to_string(),
                Capture::Sound(id) => self.sounds.iter().find(|s| s.entry.id == id).map(|s| s.entry.name.clone()).unwrap_or_default(),
            };
            egui::Window::new("hotkey")
                .title_bar(false)
                .resizable(false)
                .anchor(Align2::CENTER_CENTER, vec2(0.0, 0.0))
                .frame(egui::Frame::window(&ctx.style()).fill(PANEL).inner_margin(egui::Margin::same(24.0)).rounding(14.0))
                .show(ctx, |ui| {
                    ui.label(RichText::new(format!("Hotkey for “{what}”")).size(17.0).strong().color(TEXT));
                    ui.add_space(8.0);
                    ui.label(RichText::new("Press two modifiers + a key, e.g. Ctrl+Shift+1 — or an F-key like F9").color(MUTED));
                    ui.label(RichText::new("Esc to cancel").color(MUTED).size(12.0));
                    ui.add_space(8.0);
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                });
            if cancel {
                self.capture = None;
            } else if let Some(t) = got {
                self.finish_capture(t);
            }
        }

        // rename
        if let Some((id, mut name)) = self.rename.take() {
            let mut done = false;
            let mut keep = true;
            egui::Window::new("rename")
                .title_bar(false)
                .resizable(false)
                .anchor(Align2::CENTER_CENTER, vec2(0.0, 0.0))
                .frame(egui::Frame::window(&ctx.style()).fill(PANEL).inner_margin(egui::Margin::same(24.0)).rounding(14.0))
                .show(ctx, |ui| {
                    ui.label(RichText::new("Rename sound").size(17.0).strong().color(TEXT));
                    ui.add_space(8.0);
                    let r = ui.add(egui::TextEdit::singleline(&mut name).desired_width(280.0).margin(vec2(10.0, 7.0)));
                    r.request_focus();
                    if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        done = true;
                    }
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.add(egui::Button::new(RichText::new("Save").color(Color32::WHITE)).fill(ACCENT)).clicked() {
                            done = true;
                        }
                        if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                            keep = false;
                        }
                    });
                });
            if done {
                let n = name.trim().to_string();
                if !n.is_empty() {
                    if let Some(s) = self.sounds.iter_mut().find(|s| s.entry.id == id) {
                        s.entry.name = n;
                    }
                    self.save();
                }
            } else if keep {
                self.rename = Some((id, name));
            }
        }

        // toast + stream errors
        let stream_err = self.shared.error.lock().ok().and_then(|mut e| e.take());
        if let Some(e) = stream_err {
            sys::log(&format!("stream error: {e}"));
            self.toast("Audio device changed — reconnecting…");
            if self.restart_at.is_none() {
                self.restart_at = Some(Instant::now() + Duration::from_millis(1500));
            }
        }
        if let Some((msg, t)) = &self.toast {
            if t.elapsed() > Duration::from_secs(4) {
                self.toast = None;
            } else {
                let msg = msg.clone();
                egui::Area::new(egui::Id::new("toast"))
                    .anchor(Align2::CENTER_BOTTOM, vec2(0.0, -76.0))
                    .show(ctx, |ui| {
                        egui::Frame::none()
                            .fill(Color32::from_rgb(45, 50, 64))
                            .rounding(10.0)
                            .inner_margin(egui::Margin::symmetric(16.0, 10.0))
                            .show(ui, |ui| ui.label(RichText::new(msg).color(TEXT)));
                    });
            }
        }
    }

    /// About window, in the same style as MediaCraft's: block logo drops in, bounces,
    /// then the lines fade in one after another.
    fn about_window(&mut self, ctx: &egui::Context) {
        let Some(t0) = self.about_open else { return };
        let t = (ctx.input(|i| i.time) - t0) as f32;
        let mut close = ctx.input(|i| i.key_pressed(egui::Key::Escape));
        let mut source = false;
        // line i fades in at 950 ms + i*70 ms over 250 ms
        let fade = |i: usize| ((t - (0.95 + i as f32 * 0.07)) / 0.25).clamp(0.0, 1.0);

        egui::Window::new("about")
            .title_bar(false)
            .resizable(false)
            .collapsible(false)
            .anchor(Align2::CENTER_CENTER, vec2(0.0, 0.0))
            .frame(egui::Frame::window(&ctx.style()).fill(PANEL).inner_margin(egui::Margin::same(26.0)).rounding(16.0))
            .show(ctx, |ui| {
                ui.set_width(400.0);
                ui.vertical_centered(|ui| {
                    about_logo(ui, t);
                    ui.add_space(10.0);
                    let a = fade(0);
                    ui.label(
                        RichText::new(format!("PadMix  ·  v{}", env!("CARGO_PKG_VERSION")))
                            .size(19.0)
                            .strong()
                            .color(TEXT.gamma_multiply(a)),
                    );
                    ui.add_space(2.0);
                    ui.label(RichText::new("Press button. Friends laugh. Repeat.").italics().color(MUTED.gamma_multiply(fade(1))));
                    ui.add_space(10.0);
                    crafted_line(ui, fade(2));
                    ui.label(
                        RichText::new("Developer, Chief Tester, and the entire Customer Support department.")
                            .color(TEXT.gamma_multiply(fade(2))),
                    );
                    ui.add_space(8.0);
                    ui.label(
                        RichText::new(
                            "Because we all love free, I decided to open source this. You're welcome. \
                             Powered by too much karak and not enough sleep.",
                        )
                        .color(TEXT.gamma_multiply(fade(3))),
                    );
                    ui.add_space(8.0);
                    ui.label(
                        RichText::new("VB-CABLE, cpal and symphonia do the heavy lifting. I take the credit.")
                            .color(TEXT.gamma_multiply(fade(4))),
                    );
                    ui.add_space(8.0);
                    ui.label(
                        RichText::new(
                            "Prank responsibly. If your friends start muting you, that's between you and them. I was never here.",
                        )
                        .size(12.5)
                        .color(MUTED.gamma_multiply(fade(5))),
                    );
                    ui.add_space(14.0);
                    ui.horizontal(|ui| {
                        // center the two buttons
                        let w = 120.0 + 96.0 + ui.spacing().item_spacing.x;
                        ui.add_space(((ui.available_width() - w) / 2.0).max(0.0));
                        if ui.add(egui::Button::new("Source code").min_size(vec2(120.0, 30.0))).clicked() {
                            source = true;
                        }
                        let c = egui::Button::new(RichText::new("Close").color(Color32::WHITE)).fill(ACCENT).min_size(vec2(96.0, 30.0));
                        if ui.add(c).clicked() {
                            close = true;
                        }
                    });
                });
            });
        if source {
            ctx.open_url(egui::OpenUrl::new_tab(SOURCE_URL));
        }
        if close {
            self.about_open = None;
        } else if t < 2.0 {
            ctx.request_repaint(); // keep the animation smooth
        }
    }
}

/// The 5x5 block "P": blocks cycle accent/green/amber/red, drop in 40 ms apart with a
/// springy landing, then the whole logo bounces once (as in MediaCraft's About).
fn about_logo(ui: &mut egui::Ui, t: f32) {
    const LOGO: [&str; 5] = ["XXXX.", "X...X", "XXXX.", "X....", "X...."];
    const COLORS: [Color32; 4] = [ACCENT, GREEN, AMBER, RED];
    let (blk, gap) = (13.0, 3.5);
    let size = 5.0 * blk + 4.0 * gap;
    let (rect, _) = ui.allocate_exact_size(vec2(size, size + 12.0), Sense::hover());
    let p = ui.painter();
    // logo bounce at 950 ms for 450 ms
    let k = (t - 0.95) / 0.45;
    let bounce = if (0.0..1.0).contains(&k) { -9.0 * (k * std::f32::consts::PI).sin() * (1.0 - k * 0.6) } else { 0.0 };
    let origin = rect.min + vec2(0.0, 12.0 + bounce);
    let mut placed = 0usize;
    for (j, line) in LOGO.iter().enumerate() {
        for (i, cell) in line.chars().enumerate() {
            if cell != 'X' {
                continue;
            }
            let local = ((t - placed as f32 * 0.04) / 0.42).clamp(0.0, 1.0);
            // back-out easing: overshoots a little, like cubic-bezier(.3,1.3,.6,1)
            let e = {
                let x = local - 1.0;
                1.0 + 2.2 * x * x * x + 1.2 * x * x
            };
            let y = -90.0 * (1.0 - e);
            let col = COLORS[placed % COLORS.len()].gamma_multiply(local.min(1.0));
            let min = origin + vec2(i as f32 * (blk + gap), j as f32 * (blk + gap) + y);
            p.rect_filled(Rect::from_min_size(min, vec2(blk, blk)), 3.0, col);
            placed += 1;
        }
    }
}

/// "Crafted in Oman [flag] by Mr.Coder:" with the flag drawn like MediaCraft's SVG.
fn crafted_line(ui: &mut egui::Ui, alpha: f32) {
    let font = FontId::proportional(14.5);
    let text = TEXT.gamma_multiply(alpha);
    let g1 = ui.painter().layout_no_wrap("Crafted in Oman ".into(), font.clone(), text);
    let g2 = ui.painter().layout_no_wrap(" by ".into(), font.clone(), text);
    let g3 = ui.painter().layout_no_wrap("Mr.Coder:".into(), FontId::proportional(14.5), Color32::WHITE.gamma_multiply(alpha));
    let flag = vec2(21.0, 11.0);
    let h = g1.size().y;
    let w = g1.size().x + flag.x + g2.size().x + g3.size().x;
    let (rect, _) = ui.allocate_exact_size(vec2(w, h), Sense::hover());
    let p = ui.painter();
    let mut x = rect.left();
    let y = rect.top();
    let w1 = g1.size().x;
    p.galley(pos2(x, y), g1, text);
    x += w1;
    let f = Rect::from_min_size(pos2(x, y + (h - flag.y) / 2.0), flag);
    // Oman: red band on the hoist, white / red / green stripes
    p.rect_filled(f, 2.0, Color32::from_rgb(200, 16, 46).gamma_multiply(alpha));
    let fx = f.left() + flag.x / 3.0;
    let band = flag.y / 3.0;
    p.rect_filled(Rect::from_min_max(pos2(fx, f.top()), pos2(f.right(), f.top() + band)), 0.0, Color32::WHITE.gamma_multiply(alpha));
    p.rect_filled(Rect::from_min_max(pos2(fx, f.bottom() - band), pos2(f.right(), f.bottom())), 0.0, Color32::from_rgb(0, 128, 0).gamma_multiply(alpha));
    x += flag.x;
    let w2 = g2.size().x;
    p.galley(pos2(x, y), g2, text);
    x += w2;
    p.galley(pos2(x, y), g3, text);
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_loads();
        self.watch_devices();
        let dropped: Vec<PathBuf> = ctx.input(|i| i.raw.dropped_files.iter().filter_map(|f| f.path.clone()).collect());
        if !dropped.is_empty() {
            self.add_files(dropped);
        }
        self.top_bar(ctx);
        self.bottom_bar(ctx);
        self.settings_panel(ctx);
        self.central(ctx);
        self.overlays(ctx);
        self.about_window(ctx);
        ctx.request_repaint_after(Duration::from_millis(33)); // meters + progress
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.save();
    }
}

// ---------- widgets ----------

fn apply_theme(ctx: &egui::Context) {
    let mut v = egui::Visuals::dark();
    v.panel_fill = BG;
    v.window_fill = PANEL;
    v.extreme_bg_color = Color32::from_rgb(18, 21, 27);
    v.faint_bg_color = CARD;
    v.selection.bg_fill = ACCENT;
    v.hyperlink_color = ACCENT;
    let r = egui::Rounding::same(8.0);
    for w in [&mut v.widgets.noninteractive, &mut v.widgets.inactive, &mut v.widgets.hovered, &mut v.widgets.active, &mut v.widgets.open] {
        w.rounding = r;
    }
    v.widgets.inactive.bg_fill = CARD;
    v.widgets.inactive.weak_bg_fill = CARD;
    v.widgets.inactive.bg_stroke = Stroke::NONE;
    v.widgets.hovered.bg_fill = CARD_HOVER;
    v.widgets.hovered.weak_bg_fill = CARD_HOVER;
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, Color32::from_rgb(70, 76, 92));
    v.widgets.active.bg_fill = ACCENT;
    v.widgets.active.weak_bg_fill = ACCENT;
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, TEXT);
    v.widgets.inactive.fg_stroke = Stroke::new(1.0, TEXT);
    v.window_rounding = egui::Rounding::same(14.0);
    v.window_stroke = Stroke::new(1.0, Color32::from_rgb(45, 50, 62));
    ctx.set_visuals(v);

    let mut style = (*ctx.style()).clone();
    style.spacing.item_spacing = vec2(10.0, 8.0);
    style.spacing.button_padding = vec2(12.0, 6.0);
    style.spacing.slider_width = 120.0;
    use egui::{FontFamily::Proportional, TextStyle};
    style.text_styles = [
        (TextStyle::Heading, FontId::new(20.0, Proportional)),
        (TextStyle::Body, FontId::new(14.5, Proportional)),
        (TextStyle::Button, FontId::new(14.0, Proportional)),
        (TextStyle::Small, FontId::new(12.0, Proportional)),
        (TextStyle::Monospace, FontId::new(13.0, egui::FontFamily::Monospace)),
    ]
    .into();
    ctx.set_style(style);
}

fn pill(ui: &mut egui::Ui, color: Color32, text: &str) {
    let galley = ui.painter().layout_no_wrap(text.to_string(), FontId::proportional(13.0), TEXT);
    let size = vec2(galley.size().x + 34.0, 26.0);
    let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, 13.0, color.gamma_multiply(0.16));
    p.circle_filled(pos2(rect.left() + 14.0, rect.center().y), 4.5, color);
    p.galley(pos2(rect.left() + 26.0, rect.center().y - galley.size().y / 2.0), galley, TEXT);
}

fn meter(ui: &mut egui::Ui, level: f32) {
    let (rect, _) = ui.allocate_exact_size(vec2(90.0, 8.0), Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, 4.0, Color32::from_rgb(40, 45, 56));
    let l = level.clamp(0.0, 1.0);
    if l > 0.001 {
        let c = if l > 0.95 { RED } else if l > 0.7 { AMBER } else { GREEN };
        let fill = Rect::from_min_size(rect.min, vec2(rect.width() * l, rect.height()));
        p.rect_filled(fill, 4.0, c);
    }
}

/// Slider with a % readout. Returns true when changed.
fn slider(ui: &mut egui::Ui, v: &mut f32, range: std::ops::RangeInclusive<f32>, tip: &str) -> bool {
    let r = ui.add(egui::Slider::new(v, range).show_value(false)).on_hover_text(tip);
    ui.label(RichText::new(format!("{:>3}%", (*v * 100.0).round() as i32)).color(MUTED).size(12.0));
    r.changed()
}

fn open_sound_settings() {
    let _ = std::process::Command::new("explorer.exe").arg("ms-settings:sound").spawn();
}

fn heading(ui: &mut egui::Ui, t: &str) {
    ui.add_space(10.0);
    ui.separator();
    ui.label(RichText::new(t).size(14.5).strong().color(TEXT));
    ui.add_space(4.0);
}

fn section(ui: &mut egui::Ui, t: &str) {
    ui.label(RichText::new(t).color(MUTED).size(12.5));
    ui.add_space(2.0);
}

fn short(s: &str) -> String {
    truncate(s, 34)
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n.saturating_sub(1)).collect::<String>() + "…"
    }
}

fn empty_state(ui: &mut egui::Ui) {
    let avail = ui.available_size();
    let (rect, _) = ui.allocate_exact_size(vec2(avail.x, avail.y.min(320.0)), Sense::hover());
    let p = ui.painter();
    p.rect_stroke(rect.shrink(4.0), 16.0, Stroke::new(1.5, Color32::from_rgb(55, 60, 75)));
    p.text(rect.center() - vec2(0.0, 18.0), Align2::CENTER_CENTER, "Drop sound files here", FontId::proportional(20.0), TEXT);
    p.text(rect.center() + vec2(0.0, 14.0), Align2::CENTER_CENTER, "or click “+ Add sounds” — mp3, wav, ogg, flac, m4a", FontId::proportional(14.0), MUTED);
}

fn card(ui: &mut egui::Ui, s: &SoundItem, w: f32, progress: Option<f32>, capture: Option<Capture>, actions: &mut Vec<UiAction>) {
    let id = s.entry.id;
    let (rect, resp) = ui.allocate_exact_size(vec2(w, 84.0), Sense::click());
    let hovered = resp.hovered();
    let capturing = capture == Some(Capture::Sound(id));
    let p = ui.painter_at(rect);

    let bg = if progress.is_some() { CARD_PLAY } else if hovered { CARD_HOVER } else { CARD };
    p.rect_filled(rect, 12.0, bg);
    if capturing {
        p.rect_stroke(rect.shrink(1.0), 12.0, Stroke::new(2.0, ACCENT));
    }
    if let Some(prog) = progress {
        let bar = Rect::from_min_size(pos2(rect.left(), rect.bottom() - 4.0), vec2(rect.width() * prog.clamp(0.0, 1.0), 4.0));
        p.rect_filled(bar, 2.0, ACCENT);
    }

    // play / stop button
    let c = pos2(rect.left() + 30.0, rect.center().y);
    let btn_col = if s.error.is_some() { Color32::from_rgb(70, 50, 55) } else if progress.is_some() || hovered { ACCENT } else { Color32::from_rgb(52, 58, 74) };
    p.circle_filled(c, 17.0, btn_col);
    if progress.is_some() {
        p.rect_filled(Rect::from_center_size(c, vec2(11.0, 11.0)), 2.0, Color32::WHITE);
    } else {
        let pts = vec![c + vec2(-4.5, -7.0), c + vec2(-4.5, 7.0), c + vec2(7.5, 0.0)];
        p.add(Shape::convex_polygon(pts, Color32::WHITE, Stroke::NONE));
    }

    // name + subtitle
    let tx = rect.left() + 58.0;
    let max_chars = ((w - 70.0) / 8.0).max(6.0) as usize;
    p.text(pos2(tx, rect.top() + 22.0), Align2::LEFT_CENTER, truncate(&s.entry.name, max_chars), FontId::proportional(15.0), TEXT);
    let (sub, col) = if let Some(_e) = &s.error {
        ("⚠ Can't load file".to_string(), AMBER)
    } else if s.loading {
        ("Loading…".to_string(), MUTED)
    } else if capturing {
        ("Press keys…".to_string(), ACCENT)
    } else if let Some(h) = &s.entry.hotkey {
        (format!("⌨ {}", hotkeys::pretty(h)), Color32::from_rgb(170, 160, 255))
    } else {
        ("Right-click to set hotkey".to_string(), MUTED)
    };
    p.text(pos2(tx, rect.top() + 50.0), Align2::LEFT_CENTER, sub, FontId::proportional(12.5), col);

    let resp = resp.on_hover_text(match &s.error {
        Some(e) => format!("{}\n{}", s.entry.path.display(), e),
        None => s.entry.path.display().to_string(),
    });
    if resp.clicked() {
        actions.push(UiAction::Toggle(id));
    }
    resp.context_menu(|ui| {
        ui.horizontal(|ui| {
            ui.label(RichText::new("Volume").color(MUTED).size(12.5));
            let mut v = s.entry.volume;
            let r = ui.add(egui::Slider::new(&mut v, 0.0..=2.0).show_value(false));
            ui.label(RichText::new(format!("{}%", (v * 100.0).round() as i32)).color(MUTED).size(12.0));
            if r.changed() {
                actions.push(UiAction::SetVolume(id, v, false));
            }
            if r.drag_stopped() || (r.changed() && !r.dragged()) {
                actions.push(UiAction::SetVolume(id, v, true));
            }
        });
        ui.separator();
        if ui.button("⌨  Set hotkey…").clicked() {
            actions.push(UiAction::SetHotkey(id));
            ui.close_menu();
        }
        if s.entry.hotkey.is_some() && ui.button("✖  Clear hotkey").clicked() {
            actions.push(UiAction::ClearHotkey(id));
            ui.close_menu();
        }
        if ui.button("✏  Rename…").clicked() {
            actions.push(UiAction::Rename(id));
            ui.close_menu();
        }
        ui.separator();
        if ui.button(RichText::new("🗑  Remove").color(RED)).clicked() {
            actions.push(UiAction::Remove(id));
            ui.close_menu();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every non-ASCII symbol the UI draws must exist in egui's built-in fonts,
    /// otherwise it shows up as an empty box.
    #[test]
    fn ui_symbols_exist_in_fonts() {
        let src = include_str!("app.rs");
        let used: std::collections::BTreeSet<char> =
            src.chars().filter(|c| !c.is_ascii() && !('\u{2500}'..'\u{2580}').contains(c)).collect();
        let ctx = egui::Context::default();
        let _ = ctx.run(Default::default(), |_| {});
        let missing: String = used
            .into_iter()
            .filter(|c| !ctx.fonts(|f| f.has_glyph(&FontId::proportional(14.0), *c)))
            .collect();
        assert!(missing.is_empty(), "symbols missing from fonts: {missing}");
    }
}
