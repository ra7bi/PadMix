//! PadMix-Setup.exe — per-user installer (no admin).
//!
//!   PadMix-Setup.exe        install or update, with native dialogs
//!   PadMix-Setup.exe /S     silent install (no dialogs, no launch)

#![windows_subsystem = "windows"]

#[path = "../../src/setup.rs"]
mod setup;

use std::time::Duration;

const PAYLOAD: &[u8] = include_bytes!(env!("PADMIX_PAYLOAD"));
const VERSION: &str = env!("PADMIX_VERSION");

fn main() {
    let silent = std::env::args().skip(1).any(|a| a.eq_ignore_ascii_case("/S") || a == "--silent");
    let upgrading = setup::installed_exe().exists();

    if !silent {
        let verb = if upgrading { "Update to" } else { "Install" };
        let text = format!(
            "{verb} PadMix {VERSION}?\n\n\
             PadMix installs just for you (no admin needed) into:\n{}\n\n\
             It adds Start menu and desktop shortcuts. Remove it any time from Settings > Apps.",
            setup::install_dir().display()
        );
        if setup::message(&text, setup::MB_OKCANCEL_INFO) != setup::IDOK {
            return;
        }
    }

    if setup::is_running() && !setup::close_running(Duration::from_secs(10)) {
        if !silent {
            setup::message("PadMix is still open. Please close it and run Setup again.", setup::MB_OK_ERROR);
        }
        std::process::exit(2);
    }

    match setup::install(PAYLOAD, VERSION) {
        Ok(exe) => {
            if silent {
                return;
            }
            let text = format!(
                "PadMix {VERSION} is installed.\n\nOpen it now?\n\n\
                 First time? PadMix will help you install VB-CABLE, the free virtual cable \
                 that lets Discord and games hear your sounds."
            );
            if setup::message(&text, setup::MB_YESNO_Q) == setup::IDYES {
                let _ = std::process::Command::new(&exe).current_dir(setup::install_dir()).spawn();
            }
        }
        Err(e) => {
            if !silent {
                setup::message(&format!("PadMix could not be installed:\n\n{e:#}"), setup::MB_OK_ERROR);
            }
            std::process::exit(1);
        }
    }
}
