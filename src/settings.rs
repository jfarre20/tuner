//! Persisted settings (tuner_settings.json) and the optional channel lineup (tuner_lineup.json),
//! shared by tuner and tuner-setup.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::Duration;

// Folder defaults are empty: the set up wizard (tuner-setup.exe --wizard) fills them in.
pub const LOGO_DIRS: &str = "";
pub const BUMPER_DIR: &str = "";

/// Persisted next to the exe as tuner_settings.json; edited from the setup menu (S).
#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct Settings {
    pub crt: bool,
    pub crt_curve: f32,    // barrel distortion 0..0.12
    pub crt_scan: f32,     // scanline depth 0..1
    pub crt_noise: f32,    // interference 0..0.1
    pub crt_vignette: f32, // corner darkening 0..0.5
    pub overscan: f32,     // -0.10 (underscan: picture smaller) .. +0.10 (overscan: cropped like a real set)
    pub aspect: usize,     // index into ASPECTS
    pub hud: usize,        // index into HUD_COLORS
    pub logos: bool,       // network bug in the corner
    pub clocks: bool,      // small clock, morning-news style
    pub ticker: bool,      // lower-third news crawl
    pub bumpers: bool,     // station idents between programs (applies at startup)
    pub snow: bool,        // analog snow between channels (off = digital black)
    pub music: bool,       // hold music on the Prevue channel
    pub clock_24h: bool,
    pub logo_dirs: String,   // ';'-separated folders of PNG logos
    pub bumper_dir: String,  // folder tree of idents, one subfolder per network
    pub weather_dir: String, // ws3kp checkout to run with node (empty = don't start one)
    pub weather_url: String, // where the WeatherStar page is served (empty = no weather channel)
    pub weather_zip: String, // US ZIP typed in the menu; geocoded into the fields below
    pub weather_location: String,
    pub weather_lat: f64,
    pub weather_lon: f64,
    pub content_dir: String, // library folder (CLI bare argument overrides)
    pub channels: usize,     // auto-dealt channel count (--channels overrides)
    pub music_dir: String,   // hold music (--music overrides)
}

pub const MUSIC_DIR: &str = "";

/// A manually defined channel in tuner_lineup.json. Channel numbers not listed are dealt from the
/// library automatically. Every field is optional in the file.
#[derive(Serialize, Deserialize, Clone, Default)]
#[serde(default)]
pub struct LineupChannel {
    pub number: usize,
    pub name: String,         // shown as the channel's network; empty = the logo's name / auto
    pub logo: String,         // network logo name from the logo folders, e.g. "HBO"; empty = none
    pub sources: Vec<String>, // folders, walked recursively; empty = content dealt like an auto channel
    pub shuffle: bool,        // else alphabetical
    pub bug: String,          // "auto" | "off" | "br" | "bl" | "tl"
    pub clock: String,        // "auto" | "on" | "off"
    pub ticker: String,       // "auto" | "on" | "off"
    pub idents: String,       // "auto" (own network's folder, else any) | "off" | a folder name under bumper_dir
}

#[derive(Serialize, Deserialize, Clone, Default)]
#[serde(default)]
pub struct Lineup {
    pub channels: Vec<LineupChannel>,
}

impl Lineup {
    pub fn path() -> PathBuf {
        Settings::path().with_file_name("tuner_lineup.json")
    }

    pub fn load() -> Lineup {
        std::fs::read_to_string(Self::path()).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
    }

    pub fn save(&self) -> std::io::Result<()> {
        std::fs::write(Self::path(), serde_json::to_string_pretty(self).unwrap())
    }
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            crt: false,
            crt_curve: 0.045,
            crt_scan: 1.0,
            crt_noise: 0.035,
            crt_vignette: 0.22,
            overscan: 0.0,
            aspect: 0,
            hud: 0,
            logos: true,
            clocks: true,
            ticker: true,
            bumpers: true,
            snow: true,
            music: true,
            clock_24h: false,
            logo_dirs: LOGO_DIRS.into(),
            bumper_dir: BUMPER_DIR.into(),
            weather_dir: String::new(),
            weather_url: String::new(), // the wizard sets it when a ws3kp folder is chosen
            weather_zip: String::new(),
            weather_location: "Cleveland, OH".into(),
            weather_lat: 41.4993,
            weather_lon: -81.6944,
            content_dir: "content".into(),
            channels: 20,
            music_dir: MUSIC_DIR.into(),
        }
    }
}

pub const ASPECTS: &[&str] = &["FIT", "4:3", "16:9", "STRETCH"];
pub const HUD_COLORS: &[(&str, u32)] = &[("GREEN", 0x00_2c_f5_3c), ("WHITE", 0x00_f0_f0_f0), ("AMBER", 0x00_ff_b0_20), ("CYAN", 0x00_30_e0_f0), ("MAGENTA", 0x00_f0_40_d0)];

/// Where the JSON files live: next to the exe when that folder is writable (portable install),
/// otherwise %LOCALAPPDATA%\tuner (e.g. installed under Program Files).
pub fn config_dir() -> PathBuf {
    static DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| {
        let exe_dir = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.to_path_buf())).unwrap_or_else(|| ".".into());
        if exe_dir.join("tuner_settings.json").exists() {
            return exe_dir;
        }
        let probe = exe_dir.join(".tuner_write_test");
        if std::fs::write(&probe, b"").is_ok() {
            let _ = std::fs::remove_file(&probe);
            return exe_dir;
        }
        let base = if cfg!(windows) {
            std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
        } else {
            std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        };
        let local = base.unwrap_or(exe_dir).join("tuner");
        let _ = std::fs::create_dir_all(&local);
        local
    })
    .clone()
}

impl Settings {
    pub fn path() -> PathBuf {
        config_dir().join("tuner_settings.json")
    }

    pub fn load() -> Settings {
        let s: Settings = std::fs::read_to_string(Self::path()).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
        s.save(); // write defaults / new fields so the file is editable
        s
    }

    pub fn save(&self) {
        if let Err(e) = std::fs::write(Self::path(), serde_json::to_string_pretty(self).unwrap()) {
            eprintln!("settings: {e}");
        }
    }

    /// Network names available for the lineup editor: PNG stems in the logo folders, cleaned.
    pub fn logo_names(&self) -> Vec<String> {
        let mut names: Vec<String> = Vec::new();
        for dir in self.logo_dirs.split(';').map(str::trim).filter(|d| !d.is_empty()) {
            for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
                let p = e.path();
                if p.extension().is_some_and(|x| x.eq_ignore_ascii_case("png")) {
                    let stem = p.file_stem().unwrap_or_default().to_string_lossy().to_string();
                    let name = match stem.find(" (") {
                        Some(i) => stem[..i].trim().to_string(),
                        None => stem.trim().to_string(),
                    };
                    if !name.is_empty() && !names.iter().any(|n| n.eq_ignore_ascii_case(&name)) {
                        names.push(name);
                    }
                }
            }
        }
        names.sort_by_key(|n| n.to_lowercase());
        names
    }

    /// Full WeatherStar URL: location + kiosk mode (auto-play, no controls).
    pub fn weather_page(&self) -> Option<String> {
        if self.weather_url.is_empty() {
            return None;
        }
        let q: String = self.weather_location.bytes().map(|b| match b {
            b' ' => "+".to_string(),
            b',' => "%2C".to_string(),
            b => (b as char).to_string(),
        }).collect();
        let sep = if self.weather_url.contains('?') { "&" } else { "?" };
        Some(format!(
            "{}{sep}latLonQuery={q}&latLon=%7B%22lat%22%3A{}%2C%22lon%22%3A{}%7D&settings-kiosk-checkbox=true",
            self.weather_url, self.weather_lat, self.weather_lon
        ))
    }

    pub fn clock(&self, secs: bool) -> String {
        let now = chrono::Local::now();
        let s = match (self.clock_24h, secs) {
            (true, true) => now.format("%H:%M:%S").to_string(),
            (true, false) => now.format("%H:%M").to_string(),
            (false, true) => now.format("%I:%M:%S %p").to_string(),
            (false, false) => now.format("%I:%M %p").to_string(),
        };
        if self.clock_24h { s } else { s.trim_start_matches('0').to_string() }
    }
}

/// Setup-menu rows: label, and whether changing it needs a restart. Order matches `value`/`adjust`.
pub const MENU: &[(&str, bool)] = &[
    ("CRT TUBE", false),
    ("CRT CURVATURE", false),
    ("CRT SCANLINES", false),
    ("CRT NOISE", false),
    ("CRT VIGNETTE", false),
    ("OVERSCAN", false),
    ("ASPECT", false),
    ("HUD COLOR", false),
    ("CHANNEL LOGOS", false),
    ("CLOCK BUGS", false),
    ("NEWS TICKER", false),
    ("STATION IDENTS", true),
    ("SNOW ON TUNE", false),
    ("HOLD MUSIC", false),
    ("24 HOUR CLOCK", false),
    ("WEATHER ZIP", false),
];
pub const MENU_ZIP: usize = 15;

pub fn onoff(b: bool) -> String {
    (if b { "ON" } else { "OFF" }).to_string()
}

impl Settings {
    /// Display value for menu row `i`.
    pub fn value(&self, i: usize) -> String {
        match i {
            0 => onoff(self.crt),
            1 => format!("{:.0}%", self.crt_curve * 1000.0),
            2 => format!("{:.0}%", self.crt_scan * 100.0),
            3 => format!("{:.0}%", self.crt_noise * 1000.0),
            4 => format!("{:.0}%", self.crt_vignette * 200.0),
            5 => format!("{:+.0}%", self.overscan * 100.0),
            6 => ASPECTS[self.aspect.min(ASPECTS.len() - 1)].into(),
            7 => HUD_COLORS[self.hud.min(HUD_COLORS.len() - 1)].0.into(),
            8 => onoff(self.logos),
            9 => onoff(self.clocks),
            10 => onoff(self.ticker),
            11 => onoff(self.bumpers),
            12 => onoff(self.snow),
            13 => onoff(self.music),
            14 => onoff(self.clock_24h),
            _ => if self.weather_zip.is_empty() { "-----".into() } else { self.weather_zip.clone() },
        }
    }

    /// Left/right on menu row `i` (`dir` = -1 / +1). Numbers step and clamp, choices cycle.
    pub fn adjust(&mut self, i: usize, dir: i32) {
        let step = |v: &mut f32, s: f32, lo: f32, hi: f32| *v = (*v + s * dir as f32).clamp(lo, hi);
        let cycle = |v: &mut usize, n: usize| *v = (*v as i32 + dir).rem_euclid(n as i32) as usize;
        match i {
            0 => self.crt = !self.crt,
            1 => step(&mut self.crt_curve, 0.01, 0.0, 0.12),
            2 => step(&mut self.crt_scan, 0.1, 0.0, 1.0),
            3 => step(&mut self.crt_noise, 0.01, 0.0, 0.10),
            4 => step(&mut self.crt_vignette, 0.05, 0.0, 0.5),
            5 => step(&mut self.overscan, 0.01, -0.10, 0.10),
            6 => cycle(&mut self.aspect, ASPECTS.len()),
            7 => cycle(&mut self.hud, HUD_COLORS.len()),
            8 => self.logos = !self.logos,
            9 => self.clocks = !self.clocks,
            10 => self.ticker = !self.ticker,
            11 => self.bumpers = !self.bumpers,
            12 => self.snow = !self.snow,
            13 => self.music = !self.music,
            14 => self.clock_24h = !self.clock_24h,
            _ => {}
        }
    }

    pub fn hud_color(&self) -> u32 {
        HUD_COLORS[self.hud.min(HUD_COLORS.len() - 1)].1
    }
}

pub type GeoRx = std::sync::mpsc::Receiver<Option<(f64, f64, String)>>;

pub fn start_geocode(zip: &str) -> GeoRx {
    let (tx, rx) = std::sync::mpsc::channel();
    let zip = zip.to_string();
    std::thread::spawn(move || {
        let _ = tx.send(geocode_zip(&zip));
    });
    rx
}

/// ZIP -> (lat, lon, "City, ST") via zippopotam.us (free, no key). Blocking; run on a thread.
pub fn geocode_zip(zip: &str) -> Option<(f64, f64, String)> {
    let body = ureq::get(&format!("https://api.zippopotam.us/us/{zip}")).timeout(Duration::from_secs(8)).call().ok()?.into_string().ok()?;
    let v: serde_json::Value = serde_json::from_str(&body).ok()?;
    let p = v.get("places")?.get(0)?;
    let lat: f64 = p.get("latitude")?.as_str()?.parse().ok()?;
    let lon: f64 = p.get("longitude")?.as_str()?.parse().ok()?;
    let name = format!("{}, {}", p.get("place name")?.as_str()?, p.get("state abbreviation")?.as_str()?);
    Some((lat, lon, name))
}

