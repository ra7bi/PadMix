<img src="assets/icon_256.png" alt="PadMix logo" width="96" align="right">

# PadMix

**A free, open source soundboard for Windows.** Press a key and your friends hear the sound in Discord or in your game, mixed with your voice, as if it came out of your microphone.

I wanted a soundboard that was small, had no ads and didn't nag me to upgrade. Because we all love free, I decided to open source it.

## What it does

- **Plays sounds through your mic**, mixed with your voice, so everyone in the call hears them
- **You hear them too**, in your own headphones, at your own volume
- **Hotkeys that work anywhere**, even while you're in a game or the window is minimized
- **Even loudness**, so a quiet clip and an ear-blaster end up at about the same level
- **A volume for each sound**, plus separate sliders for your voice, what others hear and what you hear
- **Drag and drop** mp3, wav, ogg, flac and m4a files straight onto the window
- **Sorts out Windows sound for you**: one click makes your headphones and your real mic the Windows defaults, and it warns you if Windows is sending your PC's sound to your friends
- **Fixes itself** when you unplug a headset or change devices

## Install

**[Download the latest installer](https://github.com/ra7bi/PadMix/releases/latest)** and run it. It's about 8 MB and doesn't need admin rights.

Windows SmartScreen will probably warn you, because the installer isn't code-signed. Signing certificates cost money, and this project is free. If you're happy to continue, choose **More info**, then **Run anyway**. Each release lists its SHA-256 checksum so you can check your download, and if you'd rather not trust my build, you can make your own.

PadMix installs just for you, adds Start menu and desktop shortcuts, and can be removed any time from **Settings → Apps**. Run the installer again to update. For a silent install, use `PadMix-Setup.exe /S`.

## The first time you open it

PadMix needs one free helper: [VB-CABLE](https://vb-audio.com/Cable/), a virtual audio cable. PadMix sends your voice and your sounds into it, and Discord listens to the other end.

1. Download VB-CABLE from [vb-audio.com](https://vb-audio.com/Cable/) into your Downloads folder.
2. In PadMix, click **Install VB-CABLE**. It finds the download, checks it's really signed by VB-Audio, and starts the official installer. Windows asks for admin once. Click **Install Driver**, then restart Windows.
3. In PadMix, open **Settings → Windows sound → Recommended setup**.
4. In Discord, go to **Voice & Video → Input Device** and choose **CABLE Output (VB-Audio Virtual Cable)**.

Discord's **Let's Check** button is the easy test: talk, then click a sound. The meter moves and you hear the sound twice, once from PadMix and once played back by Discord.

VB-CABLE isn't bundled with PadMix because its license doesn't allow that. It's donationware by VB-Audio, so if it's useful to you, consider supporting them.

## Using it

- **Play or stop** a sound: click its card. Clicking a playing sound again restarts it.
- **Right-click a card** to set its volume, its hotkey, rename it or remove it.
- **Hotkeys** need two modifier keys plus a key, like `Ctrl+Shift+1`, or an F-key on its own. Shortcuts like `Ctrl+A` are refused, because a global hotkey would take them away from every other app.
- **Stop everything** with the Stop all button or `Ctrl+Shift+Backspace`.

Your sound list and settings live in `%APPDATA%\PadMix`. Your sound files stay where they are.

## What it doesn't do

- It only runs on Windows.
- It needs VB-CABLE, and apps have to use **CABLE Output** as their microphone.
- It doesn't change your voice. It's a soundboard, not a voice changer.

Prank responsibly. If your friends start muting you, that's between you and them.

## For developers

You'll need [Rust](https://rustup.rs) 1.87 or newer and the Windows SDK, for `rc.exe`, which embeds the icon and version info.

```bash
git clone https://github.com/ra7bi/PadMix.git
```

```bash
cd PadMix
```

```bash
cargo test
```

```bash
powershell -ExecutionPolicy Bypass -File build-release.ps1
```

`build-release.ps1` runs the tests, builds the app and the installer, and puts both in `dist\` with their SHA-256 checksums.

The app is plain Rust with [egui](https://github.com/emilk/egui) for the window, [cpal](https://github.com/RustAudio/cpal) for audio, [symphonia](https://github.com/pdeljanov/Symphonia) to decode files and [rubato](https://github.com/HEnquist/rubato) to resample them. Start with `src/mixer.rs`, which mixes your voice and the sounds inside the audio callback, and `src/engine.rs`, which wires up the devices. `src/winaudio.rs` reads and changes the Windows defaults, and `installer/` is the small setup program.

The exe has a few commands that help when something's wrong:

```bash
padmix --list
```

```bash
padmix --audio-defaults
```

```bash
padmix --selftest "CABLE Input" --loopback
```

The last one sends a tone into VB-CABLE and checks that it comes out the other end. Problems are also written to `%APPDATA%\PadMix\padmix.log`.

Found a bug or have an idea? Please [open an issue](https://github.com/ra7bi/PadMix/issues). Customer support is one person in Oman, so be patient.

## License

PadMix is under the [MIT License](LICENSE). VB-CABLE is a separate product by VB-Audio with its own license. It isn't bundled with PadMix, which only starts the official installer you downloaded.
