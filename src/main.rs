#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
//! PadMix — free open source soundboard for Windows.
//! Your mic + sounds are mixed and sent to VB-CABLE; others pick "CABLE Output" as microphone.
//!
//!   padmix                      start the app
//!   padmix --list               print audio devices
//!   padmix --selftest <output>  play a 1 s test tone into an output whose name contains <output>
//!                               [--mic] also capture the default mic, [--file <path>] play a file

mod app;
mod clip;
mod config;
mod engine;
mod hotkeys;
mod installer;
mod mixer;
mod setup;
mod sys;
mod winaudio;

use std::sync::Arc;
use std::time::{Duration, Instant};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(|a| a == "--version").unwrap_or(false) {
        attach_console();
        println!("PadMix {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    if args.first().map(|a| a == "--uninstall").unwrap_or(false) {
        uninstall(args.iter().any(|a| a == "--quiet"));
        return;
    }
    if args.first().map(|a| a == "--audio-defaults").unwrap_or(false) {
        attach_console();
        match winaudio::read() {
            Ok(d) => {
                let n = |e: &Option<winaudio::Endpoint>| e.as_ref().map(|e| e.name.clone()).unwrap_or_else(|| "-".into());
                println!("speaker       : {}", n(&d.speaker));
                println!("speaker calls : {}", n(&d.speaker_comms));
                println!("mic           : {}", n(&d.mic));
                println!("mic calls     : {}", n(&d.mic_comms));
                println!("cable as PC speaker: {}", d.speaker_is_cable());
            }
            Err(e) => eprintln!("FAILED: {e:#}"),
        }
        return;
    }
    if args.first().map(|a| a == "--list").unwrap_or(false) {
        attach_console();
        list();
        return;
    }
    if args.first().map(|a| a == "--check-cable-pack").unwrap_or(false) {
        // find + extract + verify the VB-CABLE pack, without installing anything
        attach_console();
        let src = args.get(1).map(std::path::PathBuf::from).or_else(installer::find_pack);
        let Some(src) = src else {
            eprintln!("no VBCABLE_Driver_Pack*.zip found in Downloads");
            std::process::exit(1);
        };
        println!("pack   : {}", src.display());
        match installer::prepare(&src, |m| println!("step   : {m}")) {
            Ok(setup) => println!("verified installer: {}\nOK (not installed)", setup.display()),
            Err(e) => {
                eprintln!("FAILED: {e:#}");
                std::process::exit(1);
            }
        }
        return;
    }
    if args.first().map(|a| a == "--selftest").unwrap_or(false) {
        attach_console();
        if let Err(e) = selftest(&args[1..]) {
            eprintln!("SELFTEST FAILED: {e:#}");
            std::process::exit(1);
        }
        return;
    }

    // One PadMix at a time: a second launch just brings the open window forward.
    let Some(_instance) = sys::single_instance() else { return };
    sys::init_logging();

    let icon = egui::IconData { rgba: include_bytes!("../assets/icon_256.rgba").to_vec(), width: 256, height: 256 };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("PadMix")
            .with_icon(icon)
            .with_inner_size([1000.0, 660.0])
            .with_min_inner_size([640.0, 440.0])
            .with_drag_and_drop(true),
        ..Default::default()
    };
    if let Err(e) = eframe::run_native("PadMix", options, Box::new(|cc| Ok(Box::new(app::App::new(cc))))) {
        sys::log(&format!("window failed: {e}"));
        setup::message(&format!("PadMix couldn't open its window:\n\n{e}\n\nDetails are in {}", sys::log_path().display()), setup::MB_OK_ERROR);
    }
    sys::log("PadMix closed");
}

/// `PadMix.exe --uninstall [--quiet]`, launched from Settings > Apps.
fn uninstall(quiet: bool) {
    if !quiet
        && setup::message(
            "Remove PadMix from this PC?\n\nYour sound files are never deleted.",
            setup::MB_YESNO_Q,
        ) != setup::IDYES
    {
        return;
    }
    if !setup::close_running(Duration::from_secs(8)) && !quiet {
        setup::message("Please close PadMix first, then try again.", setup::MB_OK_ERROR);
        return;
    }
    let remove_data = !quiet
        && setup::message(
            "Also delete your PadMix sound list and settings?\n\n(Choose No to keep them for a future install.)",
            setup::MB_YESNO_WARN_DEFNO,
        ) == setup::IDYES;
    match setup::uninstall(remove_data) {
        Ok(()) => {
            if !quiet {
                setup::message(
                    "PadMix was removed.\n\nVB-CABLE is still installed. To remove it too, run VBCABLE_Setup_x64.exe \
                     as administrator and click \"Remove Driver\".",
                    setup::MB_OK_INFO,
                );
            }
        }
        Err(e) => {
            if !quiet {
                setup::message(&format!("Uninstall failed:\n\n{e:#}"), setup::MB_OK_ERROR);
            }
        }
    }
}

/// Release builds have no console window; reuse the parent's when run from a terminal.
fn attach_console() {
    #[cfg(not(debug_assertions))]
    unsafe {
        extern "system" {
            fn AttachConsole(pid: u32) -> i32;
        }
        AttachConsole(0xFFFF_FFFF);
    }
}

fn list() {
    let d = engine::list_devices();
    println!("Inputs (microphones):");
    for n in &d.inputs {
        println!("  {}{}", n, if Some(n) == d.default_input.as_ref() { "   [default]" } else { "" });
    }
    println!("Outputs:");
    for n in &d.outputs {
        println!("  {}{}", n, if Some(n) == d.default_output.as_ref() { "   [default]" } else { "" });
    }
    match engine::find_cable(&d.outputs) {
        Some(c) => println!("VB-CABLE: {c}"),
        None => println!("VB-CABLE: not installed"),
    }
}

fn selftest(args: &[String]) -> anyhow::Result<()> {
    let want = args.first().ok_or_else(|| anyhow::anyhow!("usage: --selftest <output name part> [--mic] [--file path]"))?;
    let use_mic = args.iter().any(|a| a == "--mic");
    let file = args.iter().position(|a| a == "--file").and_then(|i| args.get(i + 1));
    let d = engine::list_devices();
    let out = d
        .outputs
        .iter()
        .find(|n| n.to_lowercase().contains(&want.to_lowercase()))
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("no output matching '{want}'"))?;
    // --loopback: listen on the cable's recording side (what Discord hears) instead of a mic.
    // Mic gain is 0 so the captured signal is only measured, never fed back.
    let loopback = args.iter().any(|a| a == "--loopback");
    let mic = if loopback {
        Some(d.inputs.iter().find(|n| engine::is_cable_capture(n)).cloned().ok_or_else(|| anyhow::anyhow!("CABLE Output not found"))?)
    } else if use_mic {
        d.default_input.clone()
    } else {
        None
    };
    println!("output : {out}");
    println!("{}: {}", if loopback { "listen " } else { "mic    " }, mic.as_deref().unwrap_or("(none)"));

    let shared = Arc::new(engine::Shared::new(if loopback { 0.0 } else { 1.0 }, 0.8, 0.6));
    let eng = engine::Engine::start(mic.as_deref(), &out, None, shared.clone())?;
    println!("rate   : {} Hz", eng.rate);

    let data = match file {
        Some(p) => {
            let t = Instant::now();
            let dec = clip::decode(std::path::Path::new(p))?;
            let data = clip::resample(&dec.stereo, dec.rate, eng.rate)?;
            println!("file   : {p} ({} Hz -> {} Hz, {:.2} s, loaded in {} ms)", dec.rate, eng.rate, data.len() as f32 / 2.0 / eng.rate as f32, t.elapsed().as_millis());
            data
        }
        None => {
            let n = eng.rate as usize;
            (0..n).flat_map(|i| {
                let s = (i as f32 * 440.0 * std::f32::consts::TAU / eng.rate as f32).sin() * 0.5;
                [s, s]
            }).collect()
        }
    };
    let secs = data.len() as f32 / 2.0 / eng.rate as f32;
    let clip = Arc::new(mixer::Clip { id: 1, data });
    eng.send(engine::Cmd::Play(clip));

    let (mut out_max, mut mic_max, mut saw_playing) = (0.0f32, 0.0f32, false);
    let end = Instant::now() + Duration::from_secs_f32(secs.min(10.0) + 0.5);
    while Instant::now() < end {
        std::thread::sleep(Duration::from_millis(50));
        out_max = out_max.max(shared.out_peak());
        mic_max = mic_max.max(shared.mic_peak());
        if shared.playing.lock().map(|p| !p.is_empty()).unwrap_or(false) {
            saw_playing = true;
        }
    }
    let err = shared.error.lock().ok().and_then(|e| e.clone());
    println!("played : {saw_playing}");
    println!("peak   : output {:.3}   mic {:.3}", out_max, mic_max);
    if let Some(e) = err {
        println!("error  : {e}");
    }
    if !saw_playing || out_max < 0.05 {
        anyhow::bail!("no audio reached the output");
    }
    if loopback {
        if mic_max < 0.05 {
            anyhow::bail!("tone was sent to the cable but nothing came out of CABLE Output");
        }
        println!("loopback: CABLE Output received the sound (peak {mic_max:.3}) — apps reading CABLE Output will hear it");
    }
    println!("OK");
    Ok(())
}
