//! glass-mic: iPad or iPhone microphone as a Windows microphone. The application lives in app.rs
//! and runs on Windows only (WASAPI, COM, VB-CABLE, tray). On other systems only a stub binary is
//! built, so `cargo check` and CI pass on Linux and macOS; access.rs, audio.rs, i18n.rs and
//! logfile.rs are platform independent and are tested there too.

// No console window: started as a scheduled task, the console variant opened a Windows Terminal
// window; closing it killed glass-mic (and the watchdog opened the next one). For --list/--help
// in a terminal the parent console is attached afterwards.
#![cfg_attr(windows, windows_subsystem = "windows")]
// Without app.rs the platform independent modules are only used by their tests.
#![cfg_attr(not(windows), allow(dead_code))]

mod access;
#[cfg(windows)]
mod app;
mod audio;
mod i18n;
mod logfile;
#[cfg(windows)]
mod micswitch;
#[cfg(windows)]
mod tray;
#[cfg(windows)]
mod webrtc_rx;

#[cfg(windows)]
fn main() {
    app::main();
}

#[cfg(not(windows))]
fn main() {
    eprintln!("glass-mic runs on Windows only (WASAPI, COM, VB-CABLE).");
    std::process::exit(1);
}
