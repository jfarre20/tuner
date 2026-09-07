//! Top-level launcher for the release zip: `TUNER.exe` / `TUNER Setup.exe` next to a `bin\`
//! folder that holds the real exes and the FFmpeg DLLs. Which one to start is read from this
//! file's own name, so one binary serves both (dist.ps1 copies it twice).
#![cfg_attr(windows, windows_subsystem = "windows")]

fn main() {
    let me = std::env::current_exe().unwrap_or_default();
    let dir = me.parent().map(|p| p.to_path_buf()).unwrap_or_default();
    let stem = me.file_stem().map(|s| s.to_string_lossy().to_lowercase()).unwrap_or_default();
    let target = match (stem.contains("setup"), cfg!(windows)) {
        (true, true) => "tuner-setup.exe",
        (true, false) => "tuner-setup",
        (false, true) => "tuner.exe",
        (false, false) => "tuner",
    };
    let bin = dir.join("bin");
    let exe = bin.join(target);
    if let Err(e) = std::process::Command::new(&exe).current_dir(&bin).args(std::env::args().skip(1)).spawn() {
        tuner::message_box("TUNER", &format!("Could not start {}:\n{e}\n\nKeep this launcher next to its bin folder.", exe.display()), false);
    }
}
