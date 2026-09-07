//! Instant-tune PoC with playout. A library of files is dealt across N channels, each a looping
//! playlist. Channel position is a pure function of wall-clock time (live-TV style, resumes
//! across restarts).
//!
//! Every channel owns a pipeline (demuxer + video decoder + audio decoder/resampler). Background
//! workers keep every idle channel's pipeline *primed*: positioned at the keyframe just before
//! "now" with that frame already decoded. Tune = snow on screen, take the pipeline, present the
//! primed frame, keep decoding. No I/O and no decoder setup on the hot path.
//!
//! Usage: tuner [<dir>] [--channels N] [--music DIR] [--crt] [--sw] [--start N] [--bench N] [--shot MS]
//! Keys: Up/Down = +-1 channel, Left/Right = +-10, digits + Enter = direct tune (0 = guide),
//!       C = toggle CRT shader, Esc = quit (prints stats).

// GUI subsystem: no console window on double-click. Started from a terminal, we attach to it
// so the logs still print there (see tuner::use_parent_console).
#![cfg_attr(windows, windows_subsystem = "windows")]

// Presenter: Direct3D 11 (with D3D11VA zero-copy decode) on Windows, wgpu (Vulkan / OpenGL,
// software decode) elsewhere or with `--features wgpu-render`. `d3d` is set by build.rs.
#[cfg(d3d)]
mod gpu;
#[cfg(not(d3d))]
#[path = "gpu_wgpu.rs"]
mod gpu;
use tuner::settings::*;
use tuner::web;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use ffmpeg_next as ff;
use ff::ffi;
use ff::frame::{Audio, Video};
#[cfg(d3d)]
use ff::util::format::Pixel;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use chrono::Timelike;
#[cfg(windows)]
use std::ffi::c_void;
use std::path::{Path, PathBuf};
#[cfg(windows)]
use windows::Win32::Foundation::HWND;
#[cfg(d3d)]
use windows::Win32::Graphics::Direct3D11::ID3D11Device;
#[cfg(windows)]
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, KeyEvent, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{Window, WindowId};

// ---------------------------------------------------------------- library index

#[derive(Serialize, Deserialize, Clone)]
struct FileInfo {
    path: String,
    duration: f64,
    width: u32,
    height: u32,
    /// From container/stream tags (title, description, comment) when the file has any.
    #[serde(default)]
    title: Option<String>,
    /// Station ident / bumper (from the bumpers folder), not a program.
    #[serde(default)]
    bumper: bool,
}

/// Best human-readable title from a file's tags: title, then description, then comment; also
/// "artist - title" for music. None when the file only has encoder boilerplate.
fn title_from_tags(fmt: &ff::DictionaryRef, stream: Option<&ff::DictionaryRef>) -> Option<String> {
    let pick = |d: &ff::DictionaryRef| {
        ["title", "description", "comment"].iter().find_map(|k| d.get(k)).map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
    };
    let title = pick(fmt).or_else(|| stream.and_then(pick))?;
    match fmt.get("artist").map(str::trim).filter(|a| !a.is_empty()) {
        Some(artist) if !title.to_lowercase().contains(&artist.to_lowercase()) => Some(format!("{artist} - {title}")),
        _ => Some(title),
    }
}

fn probe(path: &Path) -> Option<FileInfo> {
    let ictx = ff::format::input(path).ok()?;
    let st = ictx.streams().best(ff::media::Type::Video)?;
    let p = st.parameters();
    let duration = st.duration() as f64 * f64::from(st.time_base());
    let duration = if duration > 0.0 { duration } else { ictx.duration() as f64 / ffi::AV_TIME_BASE as f64 };
    let (w, h) = unsafe { ((*p.as_ptr()).width as u32, (*p.as_ptr()).height as u32) };
    let title = title_from_tags(&ictx.metadata(), Some(&st.metadata()));
    Some(FileInfo { path: path.to_string_lossy().into_owned(), duration, width: w, height: h, title, bumper: false })
}

fn walk_videos(dir: &Path, recursive: bool, out: &mut Vec<PathBuf>) {
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = e.path();
        if p.is_dir() {
            if recursive {
                walk_videos(&p, true, out);
            }
        } else if is_video(&p) {
            out.push(p);
        }
    }
}

fn is_video(p: &Path) -> bool {
    p.extension()
        .and_then(|x| x.to_str())
        .is_some_and(|x| matches!(x.to_ascii_lowercase().as_str(), "mp4" | "m4v" | "mov" | "mkv" | "webm" | "avi" | "mpg" | "mpeg" | "wmv"))
}

/// Load `<dir>/tuner_index_v2.json` if it matches the directory listing, else probe all files
/// (8 threads). `recursive` walks subfolders (the bumpers tree is one folder per network).
fn load_or_build_index(dir: &Path, recursive: bool) -> Vec<FileInfo> {
    let mut paths: Vec<PathBuf> = Vec::new();
    walk_videos(dir, recursive, &mut paths);
    paths.sort();
    let cache = dir.join("tuner_index_v2.json"); // v2: adds titles from tags
    if let Ok(txt) = std::fs::read_to_string(&cache) {
        if let Ok(idx) = serde_json::from_str::<Vec<FileInfo>>(&txt) {
            if idx.len() == paths.len() && idx.iter().zip(&paths).all(|(a, b)| Path::new(&a.path) == b) {
                println!("index: loaded {} files from {}", idx.len(), cache.display());
                return idx;
            }
        }
    }
    println!("index: probing {} files ...", paths.len());
    let t0 = Instant::now();
    let next = AtomicUsize::new(0);
    let out = Mutex::new(vec![None; paths.len()]);
    std::thread::scope(|s| {
        for _ in 0..8 {
            s.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                if i >= paths.len() {
                    break;
                }
                let info = probe(&paths[i]);
                out.lock().unwrap()[i] = info;
                if i % 500 == 0 {
                    println!("  {i}/{} ({:.0}s)", paths.len(), t0.elapsed().as_secs_f64());
                }
            });
        }
    });
    let idx: Vec<FileInfo> = out.into_inner().unwrap().into_iter().flatten().filter(|f| f.duration > 0.5).collect();
    println!("index: {} usable files in {:.1}s", idx.len(), t0.elapsed().as_secs_f64());
    let _ = std::fs::write(&cache, serde_json::to_string(&idx).unwrap());
    idx
}

// ---------------------------------------------------------------- schedule

struct Program {
    file: usize,
    start: f64, // offset within the channel loop
}

struct Schedule {
    programs: Vec<Program>,
    total: f64,
}

impl Schedule {
    /// (program index, offset seconds into that file) for channel `ch` at `now`.
    fn at(&self, ch: usize, now: f64) -> (usize, f64) {
        if self.programs.is_empty() {
            return (0, 0.0);
        }
        let t = (now + ch as f64 * 997.0) % self.total;
        let i = self.programs.partition_point(|p| p.start <= t).saturating_sub(1);
        (i, t - self.programs[i].start)
    }

    /// What a viewer would call "now" and "next": bumpers are skipped, so during a station ident
    /// "now" is the program about to start. Returns (now, next, seconds until now ends, progress
    /// 0..1 through now).
    fn now_next(&self, files: &[FileInfo], ch: usize, now: f64) -> (usize, usize, f64, f32) {
        let (i, off) = self.at(ch, now);
        let m = self.programs.len();
        let dur = |j: usize| files[self.programs[j].file].duration;
        let mut j = i;
        let mut left = dur(i) - off;
        let mut frac = (off / dur(i).max(1e-3)) as f32;
        let mut guard = 0;
        while files[self.programs[j].file].bumper && guard < m {
            j = (j + 1) % m;
            left += dur(j);
            frac = 0.0;
            guard += 1;
        }
        let mut k = (j + 1) % m;
        guard = 0;
        while files[self.programs[k].file].bumper && guard < m {
            k = (k + 1) % m;
            guard += 1;
        }
        (j, k, left.max(0.0), frac.clamp(0.0, 1.0))
    }
}

fn now_epoch() -> f64 {
    SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs_f64()
}

/// A lineup channel resolved to file indices.
struct Manual {
    files: Vec<usize>,
    shuffle: bool,
    idents: String, // "" / "auto" | "off" | bumper folder name
}

/// Channels 1..=n. Lineup channels with their own folders get exactly those files (shuffled or
/// alphabetical); every other channel gets a share of the remaining library, dealt round-robin
/// after a deterministic shuffle. With bumpers, a station ident follows every program: the
/// channel's own network's if that folder exists (or the folder the lineup names), else any.
fn build_schedules(files: &[FileInfo], n: usize, styles: &[ChanStyle], nets: &[Network], manual: &HashMap<usize, Manual>) -> Vec<Schedule> {
    let mut rng: u64 = 0x5EED_CAB1_E700_0001;
    let mut next = move || {
        rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        rng >> 33
    };
    let pinned: std::collections::HashSet<usize> = manual.values().flat_map(|m| m.files.iter().copied()).collect();
    let mut order: Vec<usize> = (0..files.len()).filter(|&i| !files[i].bumper && !pinned.contains(&i)).collect();
    for i in (1..order.len()).rev() {
        let r = next();
        order.swap(i, (r % (i as u64 + 1)) as usize);
    }
    // Bumpers grouped by the network folder they came from (lower-cased cleaned name).
    let all_bumpers: Vec<usize> = (0..files.len()).filter(|&i| files[i].bumper).collect();
    let mut by_net: HashMap<String, Vec<usize>> = HashMap::new();
    for &b in &all_bumpers {
        let folder = Path::new(&files[b].path).parent().and_then(|p| p.file_name()).map(|s| clean_name(&s.to_string_lossy())).unwrap_or_default();
        by_net.entry(folder.to_lowercase()).or_default().push(b);
    }
    let mut chans: Vec<Schedule> = (0..n).map(|_| Schedule { programs: Vec::new(), total: 0.0 }).collect();
    // Program, then (maybe) an ident, appended to channel `ch` (1-based).
    let mut push = |chans: &mut Vec<Schedule>, ch: usize, f: usize| {
        let c = &mut chans[ch - 1];
        c.programs.push(Program { file: f, start: c.total });
        c.total += files[f].duration;
        if all_bumpers.is_empty() {
            return;
        }
        let want = manual.get(&ch).map(|m| m.idents.trim().to_lowercase()).unwrap_or_default();
        if want == "off" {
            return;
        }
        let net = if want.is_empty() || want == "auto" { styles.get(ch).map(|s| nets[s.net].name.to_lowercase()).unwrap_or_default() } else { want };
        let pool = by_net.get(&net).filter(|v| !v.is_empty()).unwrap_or(&all_bumpers);
        let b = pool[(next() % pool.len() as u64) as usize];
        c.programs.push(Program { file: b, start: c.total });
        c.total += files[b].duration;
    };
    let auto: Vec<usize> = (1..=n).filter(|ch| !manual.contains_key(ch)).collect();
    if !auto.is_empty() {
        for (k, &f) in order.iter().enumerate() {
            push(&mut chans, auto[k % auto.len()], f);
        }
    }
    for (&ch, m) in manual {
        let mut list = m.files.clone();
        list.sort_by(|a, b| files[*a].path.to_lowercase().cmp(&files[*b].path.to_lowercase()));
        list.dedup();
        if m.shuffle {
            let mut r = (ch as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
            for i in (1..list.len()).rev() {
                r = r.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                list.swap(i, ((r >> 33) % (i as u64 + 1)) as usize);
            }
        }
        for f in list {
            push(&mut chans, ch, f);
        }
    }
    chans
}

// ---------------------------------------------------------------- networks (logos, per-channel look)

#[derive(Clone)]
struct Logo {
    w: u32,
    h: u32,
    rgba: Vec<u8>,
}

struct Network {
    name: String, // display name, e.g. "MTV"
    logo: Option<Logo>,
}

/// How a channel dresses its picture. Fixed per channel number.
#[derive(Clone, Copy)]
struct ChanStyle {
    net: usize,
    bug: u8, // 0 none, 1 bottom right, 2 bottom left, 3 top left
    bug_alpha: u8,
    clock: bool,
    ticker: bool,
    ticker_color: u32,
}

/// "MTV (US).png" -> "MTV"; "FOX (US)" and "FOX" collapse to one network.
fn clean_name(stem: &str) -> String {
    let s = match stem.find(" (") {
        Some(i) => &stem[..i],
        None => stem,
    };
    s.trim().to_string()
}

fn load_logo(path: &Path) -> Option<Logo> {
    let mut dec = png::Decoder::new(std::io::BufReader::new(std::fs::File::open(path).ok()?));
    dec.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = dec.read_info().ok()?;
    let mut buf = vec![0; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut buf).ok()?;
    let (w, h) = (info.width, info.height);
    let n = (w * h) as usize;
    let rgba = match info.color_type {
        png::ColorType::Rgba => buf[..n * 4].to_vec(),
        png::ColorType::Rgb => buf[..n * 3].chunks_exact(3).flat_map(|p| [p[0], p[1], p[2], 255]).collect(),
        png::ColorType::GrayscaleAlpha => buf[..n * 2].chunks_exact(2).flat_map(|p| [p[0], p[0], p[0], p[1]]).collect(),
        png::ColorType::Grayscale => buf[..n].iter().flat_map(|&g| [g, g, g, 255]).collect(),
        _ => return None,
    };
    Some(Logo { w, h, rgba })
}

/// Networks from every PNG in the logo folders, plus bumper folders that have no logo. Order is a
/// fixed shuffle so channel N is the same network every run (given the same folders).
fn load_networks(logo_dirs: &str, bumper_names: &[String]) -> Vec<Network> {
    let mut nets: Vec<Network> = Vec::new();
    let mut seen: HashMap<String, usize> = HashMap::new();
    for dir in logo_dirs.split(';').map(str::trim).filter(|d| !d.is_empty()) {
        let mut paths: Vec<PathBuf> = std::fs::read_dir(dir).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("png"))).collect();
        paths.sort();
        for p in paths {
            let name = clean_name(&p.file_stem().unwrap_or_default().to_string_lossy());
            if name.is_empty() || seen.contains_key(&name.to_lowercase()) {
                continue;
            }
            if let Some(logo) = load_logo(&p) {
                seen.insert(name.to_lowercase(), nets.len());
                nets.push(Network { name, logo: Some(logo) });
            }
        }
    }
    for b in bumper_names {
        if !b.is_empty() && !seen.contains_key(&b.to_lowercase()) {
            seen.insert(b.to_lowercase(), nets.len());
            nets.push(Network { name: b.clone(), logo: None });
        }
    }
    nets.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    let mut rng: u64 = 0x10C0_5A11_7E1E_0001;
    for i in (1..nets.len()).rev() {
        rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        nets.swap(i, ((rng >> 33) % (i as u64 + 1)) as usize);
    }
    if nets.is_empty() {
        nets.push(Network { name: "TUNER".into(), logo: None });
    }
    nets
}

/// Per-channel dressing, hashed from the channel number. Index 0 (guide) is a dummy.
fn make_styles(n_channels: usize, n_nets: usize) -> Vec<ChanStyle> {
    (0..=n_channels)
        .map(|ch| {
            let mut x = (ch as u64 + 7).wrapping_mul(0x9E37_79B9_7F4A_7C15);
            let mut r = || {
                x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                ((x >> 40) % 1000) as u32
            };
            let (bug_r, alpha_r, clock_r, ticker_r, col_r) = (r(), r(), r(), r(), r());
            ChanStyle {
                net: ch.saturating_sub(1) % n_nets.max(1),
                bug: if bug_r < 450 { 1 } else if bug_r < 550 { 2 } else if bug_r < 620 { 3 } else { 0 },
                bug_alpha: (0x70 + alpha_r % 0x70) as u8,
                clock: clock_r < 250,
                ticker: ticker_r < 200,
                ticker_color: [0x00_a0_10_10, 0x00_10_30_a0, 0x00_20_20_20, 0x00_80_10_60][(col_r % 4) as usize],
            }
        })
        .collect()
}

/// Box-filter downscale (logos are small; upscaling is nearest).
fn scale_logo(l: &Logo, tw: u32, th: u32) -> Logo {
    let (tw, th) = (tw.max(1), th.max(1));
    let mut out = vec![0u8; (tw * th * 4) as usize];
    for y in 0..th {
        let sy0 = (y as u64 * l.h as u64 / th as u64) as u32;
        let sy1 = (((y + 1) as u64 * l.h as u64 / th as u64) as u32).max(sy0 + 1).min(l.h);
        for x in 0..tw {
            let sx0 = (x as u64 * l.w as u64 / tw as u64) as u32;
            let sx1 = (((x + 1) as u64 * l.w as u64 / tw as u64) as u32).max(sx0 + 1).min(l.w);
            let mut acc = [0u64; 4];
            let mut n = 0u64;
            for sy in sy0..sy1 {
                for sx in sx0..sx1 {
                    let p = &l.rgba[((sy * l.w + sx) * 4) as usize..][..4];
                    // Premultiply so transparent pixels don't bleed colour into edges.
                    let a = p[3] as u64;
                    acc[0] += p[0] as u64 * a;
                    acc[1] += p[1] as u64 * a;
                    acc[2] += p[2] as u64 * a;
                    acc[3] += a;
                    n += 1;
                }
            }
            let o = &mut out[((y * tw + x) * 4) as usize..][..4];
            if acc[3] > 0 {
                o[0] = (acc[0] / acc[3]) as u8;
                o[1] = (acc[1] / acc[3]) as u8;
                o[2] = (acc[2] / acc[3]) as u8;
            }
            o[3] = (acc[3] / n.max(1)) as u8;
        }
    }
    Logo { w: tw, h: th, rgba: out }
}

/// Composite an RGBA logo into the straight-alpha ARGB overlay at (x, y), scaled by `alpha`.
fn blit_logo(buf: &mut [u32], w: u32, h: u32, x: i32, y: i32, l: &Logo, alpha: u8) {
    for ly in 0..l.h {
        let dy = y + ly as i32;
        if dy < 0 || dy >= h as i32 {
            continue;
        }
        for lx in 0..l.w {
            let dx = x + lx as i32;
            if dx < 0 || dx >= w as i32 {
                continue;
            }
            let p = &l.rgba[((ly * l.w + lx) * 4) as usize..][..4];
            let a = p[3] as u32 * alpha as u32 / 255;
            if a == 0 {
                continue;
            }
            let d = &mut buf[(dy as u32 * w + dx as u32) as usize];
            let (a0, r0, g0, b0) = (*d >> 24, (*d >> 16) & 255, (*d >> 8) & 255, *d & 255);
            let ao = a + a0 * (255 - a) / 255;
            let mix = |c: u32, c0: u32| (c * a + c0 * a0 * (255 - a) / 255) / ao.max(1);
            *d = (ao << 24) | (mix(p[0] as u32, r0) << 16) | (mix(p[1] as u32, g0) << 8) | mix(p[2] as u32, b0);
        }
    }
}

const HEADLINES: &str = include_str!("../assets/headlines.txt");

// ---------------------------------------------------------------- audio output

const AUDIO_LEAD: f64 = 0.10; // seconds of audio kept queued ahead of the schedule clock
const SNOW_GAIN: f32 = 0.12;

const VOL_MAX: u32 = 20; // TV-style volume steps (one OSD bar segment each)
const VOL_DEFAULT: u32 = 12;

struct AudioOut {
    ring: Mutex<VecDeque<f32>>, // interleaved, device rate/channels
    snow: AtomicU32,            // 0..=255 white-noise level mixed in (matches the picture snow)
    volume: AtomicU32,          // 0..=VOL_MAX
    muted: AtomicBool,
    rate: u32,
    channels: u32,
}

impl AudioOut {
    /// Linear gain for the current volume/mute (squared steps feel like a real TV's knob).
    fn gain(&self) -> f32 {
        if self.muted.load(Ordering::Relaxed) {
            return 0.0;
        }
        let v = self.volume.load(Ordering::Relaxed) as f32 / VOL_MAX as f32;
        v * v
    }
}

fn start_audio() -> Option<(Arc<AudioOut>, cpal::Stream)> {
    let dev = cpal::default_host().default_output_device()?;
    let cfg = dev.default_output_config().ok()?;
    let rate = cfg.sample_rate().0;
    let channels = cfg.channels() as u32;
    let out = Arc::new(AudioOut {
        ring: Mutex::new(VecDeque::with_capacity(rate as usize)),
        snow: AtomicU32::new(0),
        volume: AtomicU32::new(VOL_DEFAULT),
        muted: AtomicBool::new(false),
        rate,
        channels,
    });
    let cb = out.clone();
    let mut rng: u64 = 0x1234_5678_9ABC_DEF1;
    let stream = dev
        .build_output_stream(
            &cfg.config(),
            move |data: &mut [f32], _| {
                let snow = cb.snow.load(Ordering::Relaxed) as f32 / 255.0;
                let gain = cb.gain();
                let mut ring = cb.ring.lock().unwrap();
                for s in data.iter_mut() {
                    let v = ring.pop_front().unwrap_or(0.0);
                    rng ^= rng << 13;
                    rng ^= rng >> 7;
                    rng ^= rng << 17;
                    let n = ((rng >> 40) as f32 / 8388608.0 - 1.0) * SNOW_GAIN;
                    *s = (v * (1.0 - snow) + n * snow) * gain;
                }
            },
            |e| eprintln!("audio: {e}"),
            None,
        )
        .ok()?;
    stream.play().ok()?;
    println!("audio: {} Hz, {} ch", rate, channels);
    Some((out, stream))
}

fn make_resampler(f: &Audio, out_channels: u32, out_rate: u32) -> ff::software::resampling::Context {
    let layout = if f.channel_layout().is_empty() { ff::ChannelLayout::default(f.channels() as i32) } else { f.channel_layout() };
    let out_layout = if out_channels == 1 { ff::ChannelLayout::MONO } else { ff::ChannelLayout::STEREO };
    ff::software::resampling::Context::get(f.format(), layout, f.rate(), ff::format::Sample::F32(ff::format::sample::Type::Packed), out_layout, out_rate)
        .expect("resampler")
}

// ---------------------------------------------------------------- guide music

const MUSIC_GAIN: f32 = 0.7;

struct MusicSrc {
    ictx: ff::format::context::Input,
    idx: usize,
    dec: ff::decoder::Audio,
    rs: Option<ff::software::resampling::Context>,
    eof: bool,
}

/// Random tracks from a folder, streamed straight into the device ring while the guide is up.
struct Music {
    files: Vec<PathBuf>,
    rng: u64,
    cur: Option<MusicSrc>,
    now_playing: String, // from tags (artist - title) or the file name
    changed: bool,       // now_playing changed since last read
}

impl Music {
    fn new(dir: Option<&Path>) -> Music {
        let files: Vec<PathBuf> = dir
            .and_then(|d| std::fs::read_dir(d).ok())
            .map(|rd| {
                rd.flatten()
                    .map(|e| e.path())
                    .filter(|p| {
                        p.extension()
                            .and_then(|x| x.to_str())
                            .is_some_and(|x| matches!(x.to_ascii_lowercase().as_str(), "mp3" | "mp4" | "m4a" | "ogg" | "webm" | "flac" | "wav"))
                    })
                    .collect()
            })
            .unwrap_or_default();
        if let Some(d) = dir {
            println!("music: {} tracks in {}", files.len(), d.display());
        }
        // Seeded from the clock so the guide doesn't open on the same track every launch.
        let rng = (now_epoch() * 1000.0) as u64 ^ 0x00C0_FFEE_1234_5678;
        Music { files, rng, cur: None, now_playing: String::new(), changed: false }
    }

    fn open_random(&mut self) {
        self.cur = None;
        for _ in 0..5 {
            if self.files.is_empty() {
                return;
            }
            self.rng = self.rng.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let path = &self.files[((self.rng >> 33) % self.files.len() as u64) as usize];
            let Ok(ictx) = ff::format::input(path) else { continue };
            let Some(st) = ictx.streams().best(ff::media::Type::Audio) else { continue };
            let idx = st.index();
            let Ok(cctx) = ff::codec::context::Context::from_parameters(st.parameters()) else { continue };
            let Ok(dec) = cctx.decoder().audio() else { continue };
            let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            self.now_playing = title_from_tags(&ictx.metadata(), Some(&st.metadata())).unwrap_or(stem);
            self.changed = true;
            let mut ictx = ictx;
            // Drop in partway through, like tuning into a station: somewhere in the first 70%.
            let dur = ictx.duration();
            if dur > 0 {
                self.rng = self.rng.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                let ts = ((self.rng >> 11) % (dur as u64 * 7 / 10).max(1)) as i64;
                unsafe {
                    ffi::av_seek_frame(ictx.as_mut_ptr(), -1, ts, ffi::AVSEEK_FLAG_BACKWARD);
                }
            }
            println!("music: {}", self.now_playing);
            self.cur = Some(MusicSrc { ictx, idx, dec, rs: None, eof: false });
            return;
        }
    }

    /// Keep ~150 ms of music queued in the device ring. Opens the next random track at EOF.
    fn feed(&mut self, ao: &AudioOut) {
        let per_sec = (ao.rate * ao.channels) as usize;
        let mut guard = 0;
        while ao.ring.lock().unwrap().len() < per_sec * 15 / 100 && guard < 200 {
            guard += 1;
            if self.cur.as_ref().is_none_or(|s| s.eof) {
                self.open_random();
                if self.cur.is_none() {
                    return;
                }
            }
            let s = self.cur.as_mut().unwrap();
            let Some((stream, packet)) = s.ictx.packets().next() else {
                s.eof = true;
                continue;
            };
            if stream.index() != s.idx {
                continue;
            }
            let _ = s.dec.send_packet(&packet);
            let mut f = Audio::empty();
            while s.dec.receive_frame(&mut f).is_ok() {
                let rs = s.rs.get_or_insert_with(|| make_resampler(&f, ao.channels, ao.rate));
                let mut out = Audio::empty();
                if rs.run(&f, &mut out).is_ok() && out.samples() > 0 {
                    let n = out.samples() * ao.channels as usize;
                    let data = unsafe { std::slice::from_raw_parts(out.data(0).as_ptr() as *const f32, n) };
                    ao.ring.lock().unwrap().extend(data.iter().map(|v| v * MUSIC_GAIN));
                }
            }
        }
    }
}

// ---------------------------------------------------------------- pipeline (demuxer + decoders)

/// Hardware decode device. Only the D3D11VA path exists so far; elsewhere `create` says no and
/// everything decodes in software.
#[cfg(not(d3d))]
struct HwDev;
#[cfg(not(d3d))]
impl HwDev {
    fn create() -> Option<HwDev> {
        None
    }
}

/// ffmpeg's D3D11VA device context: one D3D11 device shared by every decoder and the presenter.
#[cfg(d3d)]
struct HwDev(*mut ffi::AVBufferRef);
#[cfg(d3d)]
unsafe impl Send for HwDev {}
#[cfg(d3d)]
unsafe impl Sync for HwDev {}

/// libavutil/hwcontext_d3d11va.h AVD3D11VADeviceContext (not in the generated bindings).
#[cfg(d3d)]
#[repr(C)]
struct AVD3D11VADeviceContext {
    device: *mut c_void,
    device_context: *mut c_void,
    video_device: *mut c_void,
    video_context: *mut c_void,
    lock: *mut c_void,
    unlock: *mut c_void,
    lock_ctx: *mut c_void,
}

#[cfg(d3d)]
impl HwDev {
    fn create() -> Option<HwDev> {
        unsafe {
            let mut r: *mut ffi::AVBufferRef = std::ptr::null_mut();
            if ffi::av_hwdevice_ctx_create(&mut r, ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_D3D11VA, std::ptr::null(), std::ptr::null_mut(), 0) < 0 {
                return None;
            }
            Some(HwDev(r))
        }
    }

    /// The ID3D11Device* inside (borrowed; ffmpeg owns the reference).
    fn d3d_device(&self) -> *mut c_void {
        unsafe {
            let hw = (*self.0).data as *mut ffi::AVHWDeviceContext;
            let d3d = (*hw).hwctx as *mut AVD3D11VADeviceContext;
            (*d3d).device
        }
    }
}

/// Pick D3D11 output when the decoder offers it, else the first *software* format. (Returning
/// another hwaccel format here would make ffmpeg try d3d11va_vld / vaapi and log errors before
/// falling back; when D3D11 setup fails ffmpeg calls this again without it.)
#[cfg(d3d)]
unsafe extern "C" fn get_hw_format(_ctx: *mut ffi::AVCodecContext, fmts: *const ffi::AVPixelFormat) -> ffi::AVPixelFormat {
    let mut p = fmts;
    while *p != ffi::AVPixelFormat::AV_PIX_FMT_NONE {
        if *p == ffi::AVPixelFormat::AV_PIX_FMT_D3D11 {
            return *p;
        }
        p = p.add(1);
    }
    let mut p = fmts;
    while *p != ffi::AVPixelFormat::AV_PIX_FMT_NONE {
        let desc = ffi::av_pix_fmt_desc_get(*p);
        if !desc.is_null() && (*desc).flags & ffi::AV_PIX_FMT_FLAG_HWACCEL as u64 == 0 {
            return *p;
        }
        p = p.add(1);
    }
    ffi::AVPixelFormat::AV_PIX_FMT_NONE
}

fn make_vdecoder(params: ff::codec::Parameters, hw: Option<&HwDev>) -> Result<ff::decoder::Video, ff::Error> {
    let mut cctx = ff::codec::context::Context::from_parameters(params)?;
    match hw {
        #[cfg(d3d)]
        Some(h) => unsafe {
            let p = cctx.as_mut_ptr();
            (*p).hw_device_ctx = ffi::av_buffer_ref(h.0);
            (*p).get_format = Some(get_hw_format);
            (*p).extra_hw_frames = 8; // frames we hold in queues beyond the decoder's own needs
            (*p).thread_count = 1;
        },
        #[cfg(not(d3d))]
        Some(_) => unreachable!("no hardware decode path on this platform"),
        None => unsafe {
            // Slice threading, set on the raw context: ffmpeg-next's Config struct differs between
            // FFmpeg 4.x and 6.x.
            let p = cctx.as_mut_ptr();
            (*p).thread_type = ffi::FF_THREAD_SLICE as i32;
            (*p).thread_count = 4;
        },
    }
    cctx.decoder().video()
}

struct AudioPath {
    idx: usize,
    tb: f64,
    dec: ff::decoder::Audio,
    resampler: Option<ff::software::resampling::Context>,
    out_rate: u32,
    out_channels: u32,
}

struct Pipeline {
    file: usize,
    ictx: ff::format::context::Input,
    vidx: usize,
    tb: f64,
    vdec: ff::decoder::Video,
    audio: Option<AudioPath>,
    /// Decoded video frames not yet presented (front = oldest).
    vq: VecDeque<Video>,
    /// Decoded, resampled audio (interleaved) and the timestamp of its first sample.
    aq: VecDeque<f32>,
    aq_start: f64,
    eof: bool,
    primed_ts: i64,
    hw_wanted: bool, // decoder was created with a D3D11VA device
    hw_warned: bool, // already logged that this file fell back to software
}
// Only one thread touches a Pipeline at a time (it lives in a Mutex slot or in the App).
unsafe impl Send for Pipeline {}

impl Pipeline {
    fn open(file: usize, path: &str, audio: Option<&AudioOut>, hw: Option<&HwDev>) -> Result<Pipeline, ff::Error> {
        let ictx = ff::format::input(path)?;
        let st = ictx.streams().best(ff::media::Type::Video).ok_or(ff::Error::StreamNotFound)?;
        let vidx = st.index();
        let tb = f64::from(st.time_base());
        let vdec = make_vdecoder(st.parameters(), hw)?;
        let audio = match (audio, ictx.streams().best(ff::media::Type::Audio)) {
            (Some(ao), Some(ast)) => {
                let cctx = ff::codec::context::Context::from_parameters(ast.parameters())?;
                let dec = cctx.decoder().audio()?;
                Some(AudioPath { idx: ast.index(), tb: f64::from(ast.time_base()), dec, resampler: None, out_rate: ao.rate, out_channels: ao.channels })
            }
            _ => None,
        };
        Ok(Pipeline {
            file,
            ictx,
            vidx,
            tb,
            vdec,
            audio,
            vq: VecDeque::new(),
            aq: VecDeque::new(),
            aq_start: f64::NAN,
            eof: false,
            primed_ts: i64::MIN,
            hw_wanted: hw.is_some(),
            hw_warned: false,
        })
    }

    /// Seek to the keyframe at/before `offset` seconds and reset decoders and queues.
    fn seek(&mut self, offset: f64) {
        let ts = (offset / self.tb) as i64;
        unsafe {
            ffi::av_seek_frame(self.ictx.as_mut_ptr(), self.vidx as i32, ts, ffi::AVSEEK_FLAG_BACKWARD);
        }
        self.vdec.flush();
        if let Some(a) = &mut self.audio {
            a.dec.flush();
        }
        self.vq.clear();
        self.aq.clear();
        self.aq_start = f64::NAN;
        self.eof = false;
    }

    /// Keyframe timestamp the demuxer would land on for `offset` (index lookup only, no I/O).
    fn keyframe_ts(&self, offset: f64) -> i64 {
        let ts = (offset / self.tb) as i64;
        unsafe {
            let st = *(*self.ictx.as_ptr()).streams.add(self.vidx);
            let i = ffi::av_index_search_timestamp(st, ts, ffi::AVSEEK_FLAG_BACKWARD);
            if i < 0 {
                return ts;
            }
            #[cfg(ffmpeg_ge_5)]
            {
                (*ffi::avformat_index_get_entry(st, i)).timestamp
            }
            #[cfg(not(ffmpeg_ge_5))]
            {
                // FFmpeg 4.x: the index is a public array on AVStream.
                (*(*st).index_entries.add(i as usize)).timestamp
            }
        }
    }

    /// Read one packet and route it. Returns false at EOF.
    fn pump(&mut self) -> bool {
        if self.eof {
            return false;
        }
        let Some((stream, packet)) = self.ictx.packets().next() else {
            self.eof = true;
            return false;
        };
        let sidx = stream.index();
        if sidx == self.vidx {
            let _ = self.vdec.send_packet(&packet);
            let mut f = Video::empty();
            while self.vdec.receive_frame(&mut f).is_ok() {
                if self.hw_wanted && !self.hw_warned && !is_hw_frame(&f) {
                    self.hw_warned = true;
                    eprintln!("hw decode fell back to software for file #{} ({}x{} {:?})", self.file, f.width(), f.height(), f.format());
                }
                self.vq.push_back(std::mem::replace(&mut f, Video::empty()));
            }
        } else if let Some(a) = self.audio.as_mut().filter(|a| a.idx == sidx) {
            let _ = a.dec.send_packet(&packet);
            let mut f = Audio::empty();
            while a.dec.receive_frame(&mut f).is_ok() {
                let pts = f.timestamp().unwrap_or(0) as f64 * a.tb;
                let rs = a.resampler.get_or_insert_with(|| make_resampler(&f, a.out_channels, a.out_rate));
                let mut out = Audio::empty();
                if rs.run(&f, &mut out).is_ok() && out.samples() > 0 {
                    let n = out.samples() * a.out_channels as usize;
                    let data = unsafe { std::slice::from_raw_parts(out.data(0).as_ptr() as *const f32, n) };
                    if self.aq.is_empty() {
                        self.aq_start = pts;
                    }
                    self.aq.extend(data);
                }
            }
        }
        true
    }

    fn pts_secs(&self, f: &Video) -> f64 {
        f.timestamp().unwrap_or(0) as f64 * self.tb
    }

    /// Timestamp just past the last decoded video frame (how far ahead we've read).
    fn video_horizon(&self) -> f64 {
        self.vq.back().map(|f| self.pts_secs(f)).unwrap_or(f64::NEG_INFINITY)
    }

    fn audio_horizon(&self) -> f64 {
        match &self.audio {
            Some(a) if !self.aq_start.is_nan() => self.aq_start + self.aq.len() as f64 / (a.out_rate * a.out_channels) as f64,
            Some(_) => f64::NEG_INFINITY,
            None => f64::INFINITY,
        }
    }

    /// Drop queued audio older than `offset` (e.g. between the keyframe and the schedule point).
    fn trim_audio(&mut self, offset: f64) {
        let Some(a) = &self.audio else { return };
        if self.aq_start.is_nan() || self.aq_start >= offset {
            return;
        }
        let frame = a.out_channels as usize;
        let per_sec = a.out_rate as usize * frame;
        let n = (((offset - self.aq_start) * per_sec as f64) as usize / frame * frame).min(self.aq.len());
        self.aq.drain(..n);
        self.aq_start += n as f64 / per_sec as f64;
    }

    /// Drop video frames already more than 40 ms late, always keeping one.
    fn trim_video(&mut self, offset: f64) -> u64 {
        let mut n = 0;
        while self.vq.len() > 1 && self.pts_secs(&self.vq[0]) - offset < -0.040 {
            self.vq.pop_front();
            n += 1;
        }
        n
    }
}

type Slot = Arc<Mutex<Option<Pipeline>>>;

/// Bring the slot's pipeline to the file + keyframe the schedule wants for `ch` and decode that
/// frame. Cheap when already primed at the same keyframe. Runs on worker threads under the slot lock.
fn prime(slot: &mut Option<Pipeline>, files: &[FileInfo], sched: &Schedule, ch: usize, audio: Option<&AudioOut>, hw: Option<&HwDev>) {
    let (prog, offset) = sched.at(ch, now_epoch());
    let file = sched.programs[prog].file;
    if slot.as_ref().is_none_or(|p| p.file != file) {
        match Pipeline::open(file, &files[file].path, audio, hw) {
            Ok(p) => *slot = Some(p),
            Err(e) => {
                eprintln!("open {} failed: {e}", files[file].path);
                return;
            }
        }
    }
    let p = slot.as_mut().unwrap();
    let kf = p.keyframe_ts(offset);
    if !p.vq.is_empty() && p.primed_ts == kf {
        return;
    }
    p.seek(offset);
    while p.vq.is_empty() && p.pump() {}
    p.primed_ts = kf;
}

/// Hot neighbour: keep the pipeline fully caught up (frames and audio right at the schedule point)
/// so tuning to it is frame-exact with sound already queued. Runs on worker threads under the lock.
fn follow(slot: &mut Option<Pipeline>, files: &[FileInfo], sched: &Schedule, ch: usize, audio: Option<&AudioOut>, hw: Option<&HwDev>) {
    let (prog, offset) = sched.at(ch, now_epoch());
    let file = sched.programs[prog].file;
    if slot.as_ref().is_none_or(|p| p.file != file) {
        match Pipeline::open(file, &files[file].path, audio, hw) {
            Ok(p) => *slot = Some(p),
            Err(e) => {
                eprintln!("open {} failed: {e}", files[file].path);
                return;
            }
        }
    }
    let p = slot.as_mut().unwrap();
    let behind = p.vq.front().map(|f| offset - p.pts_secs(f)).unwrap_or(f64::INFINITY);
    if !(-0.5..=1.5).contains(&behind) && !(p.eof && !p.vq.is_empty()) {
        p.seek(offset);
    }
    let horizon = offset + AUDIO_LEAD + 0.1;
    let mut n = 0;
    // Bounded per pass (called every ~25 ms) so the slot lock is never held long.
    while (p.video_horizon() < horizon || p.audio_horizon() < horizon) && n < 10 && p.pump() {
        n += 1;
    }
    p.trim_video(offset);
    p.trim_audio(offset);
    p.primed_ts = p.keyframe_ts(offset);
}

struct Shared {
    files: Vec<FileInfo>,
    scheds: Vec<Schedule>,
    slots: Vec<Slot>,
    audio: Option<Arc<AudioOut>>,
    hw: Option<HwDev>,   // D3D11VA device; None = software decode
    active: AtomicUsize, // channel the decode thread owns; workers skip it
    stop: AtomicBool,
    nets: Vec<Network>,
    styles: Vec<ChanStyle>, // indexed by channel
    weather: Option<usize>, // channel number of the WeatherStar page (empty schedule)
}

fn cut(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// (network, now, next, seconds left, progress 0..1) for a channel, bumpers skipped.
fn chan_info(sh: &Shared, ch: usize) -> (String, String, String, f64, f32) {
    if sh.weather == Some(ch) {
        return ("WEATHER".into(), "LOCAL FORECAST".into(), "WEATHERSTAR 3000".into(), 0.0, 0.0);
    }
    let net = sh.styles.get(ch).map(|s| sh.nets[s.net].name.to_uppercase()).unwrap_or_default();
    let s = &sh.scheds[ch];
    if s.programs.is_empty() {
        return (net, String::new(), String::new(), 0.0, 0.0);
    }
    let (i, k, left, frac) = s.now_next(&sh.files, ch, now_epoch());
    (net, prog_name(&sh.files[s.programs[i].file], usize::MAX), prog_name(&sh.files[s.programs[k].file], usize::MAX), left, frac)
}

fn spawn_workers(shared: &Arc<Shared>, n_threads: usize) {
    for k in 0..n_threads {
        let sh = shared.clone();
        std::thread::spawn(move || {
            let n = sh.slots.len();
            while !sh.stop.load(Ordering::Relaxed) {
                for ch in (k..n).step_by(n_threads) {
                    let act = sh.active.load(Ordering::Relaxed);
                    if ch == 0 || ch == act || sh.scheds[ch].programs.is_empty() {
                        continue; // 0 is the guide, an empty schedule is the weather channel: no pipeline
                    }
                    let hot = act < n && n > 2 && (ch == (act + 1) % n || ch == (act + n - 1) % n);
                    let mut slot = sh.slots[ch].lock().unwrap();
                    if slot.is_none() && sh.active.load(Ordering::Relaxed) == ch {
                        continue; // became active while we waited: the decode thread owns it
                    }
                    if hot {
                        follow(&mut slot, &sh.files, &sh.scheds[ch], ch, sh.audio.as_deref(), sh.hw.as_ref());
                    } else {
                        prime(&mut slot, &sh.files, &sh.scheds[ch], ch, sh.audio.as_deref(), sh.hw.as_ref());
                    }
                }
                std::thread::sleep(Duration::from_millis(25));
            }
        });
    }
}

// ---------------------------------------------------------------- OSD

/// 90s TV on-screen display: VCR OSD Mono, green, black drop shadow, aliased.
const OSD_FONT: &[u8] = include_bytes!("../assets/VCR_OSD_MONO_1.001.ttf");
const OSD_HOLD: Duration = Duration::from_secs(3);
const OSD_GREEN: u32 = 0x00_2c_f5_3c; // 0RGB
const NOCLIP: (i32, i32) = (0, i32::MAX);
const VOL_HOLD: Duration = Duration::from_millis(2500);

struct Osd {
    font: fontdue::Font,
    cache: HashMap<(char, u32), (fontdue::Metrics, Vec<u8>)>,
    until: Instant,
    hud: u32, // TV-set OSD colour (settings: HUD COLOR)
}

impl Osd {
    fn new() -> Self {
        let font = fontdue::Font::from_bytes(OSD_FONT, fontdue::FontSettings::default()).expect("font");
        Osd { font, cache: HashMap::new(), until: Instant::now(), hud: OSD_GREEN }
    }

    fn glyph(&mut self, c: char, px: u32) -> &(fontdue::Metrics, Vec<u8>) {
        let font = &self.font;
        self.cache.entry((c, px)).or_insert_with(|| font.rasterize(c, px as f32))
    }

    /// Draw `text` with its right edge at `right`, baseline at `baseline`, into a 0RGB buffer.
    fn draw(&mut self, buf: &mut [u32], w: u32, h: u32, text: &str, px: u32, right: i32, baseline: i32) {
        self.text(buf, w, h, text, px, right, baseline, self.hud, true, NOCLIP);
    }

    fn width(&mut self, text: &str, px: u32) -> f32 {
        text.chars().map(|c| self.glyph(c, px).0.advance_width).sum()
    }

    /// General text: `x` is the right edge when `right_align`, else the left edge. Pixels above
    /// `clip.0` or at/below `clip.1` are not drawn. Always with a black drop shadow.
    #[allow(clippy::too_many_arguments)]
    fn text(&mut self, buf: &mut [u32], w: u32, h: u32, text: &str, px: u32, x: i32, baseline: i32, color: u32, right_align: bool, clip: (i32, i32)) {
        let shadow = (px / 14).max(2) as i32;
        let clip_bottom = clip.1.min(h as i32);
        let mut pen = if right_align { x as f32 - self.width(text, px) } else { x as f32 };
        for c in text.chars() {
            let (m, bitmap) = self.glyph(c, px).clone();
            let x0 = pen as i32 + m.xmin;
            let y0 = baseline - (m.ymin + m.height as i32);
            for (color, dx, dy) in [(0u32, shadow, shadow), (color, 0, 0)] {
                for gy in 0..m.height {
                    let y = y0 + gy as i32 + dy;
                    if y < clip.0 || y >= clip_bottom {
                        continue;
                    }
                    for gx in 0..m.width {
                        if bitmap[gy * m.width + gx] < 128 {
                            continue; // hard threshold = crisp aliased 90s text
                        }
                        let x = x0 + gx as i32 + dx;
                        if x >= 0 && x < w as i32 {
                            buf[(y as u32 * w + x as u32) as usize] = color | 0xFF00_0000;
                        }
                    }
                }
            }
            pen += m.advance_width;
        }
    }
}

// ---------------------------------------------------------------- guide (channel 0)

const PREVIEW_EVERY: Duration = Duration::from_secs(8);
/// Ad box as fractions of the window: (x0, y0, x1, y1).
const GUIDE_AD: (f32, f32, f32, f32) = (0.55, 0.04, 0.97, 0.47);
const GUIDE_BLUE: u32 = 0x00_10_10_a0;
const GUIDE_BLUE2: u32 = 0x00_14_14_b0; // alternate grid rows
const GUIDE_DARK: u32 = 0x00_08_08_60;
const GUIDE_HILITE: u32 = 0x00_e8_d0_40; // cursor row (black text on it)
const GUIDE_YELLOW: u32 = 0x00_ff_e0_40;
const GUIDE_GRAY: u32 = 0x00_c8_c8_c8;
const GUIDE_WHITE: u32 = 0x00_ff_ff_ff;

/// Display name of a library file: its tagged title if it has one, else the stem without a
/// trailing "_raw". Uppercased and cut to `max` chars for the guide grid.
fn prog_name(f: &FileInfo, max: usize) -> String {
    let name = match &f.title {
        Some(t) => t.clone(),
        None => {
            let stem = Path::new(&f.path).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            stem.trim_end_matches("_raw").to_string()
        }
    };
    name.to_uppercase().chars().take(max).collect()
}

fn fill_rect(buf: &mut [u32], w: u32, x0: u32, y0: u32, x1: u32, y1: u32, color: u32) {
    for y in y0..y1 {
        buf[(y * w + x0) as usize..(y * w + x1) as usize].fill(color);
    }
}

/// Blend `color` (0RGB) over the buffer at `alpha` 0..=255: translucent STB banners.
fn tint_rect(buf: &mut [u32], w: u32, x0: u32, y0: u32, x1: u32, y1: u32, color: u32, alpha: u32) {
    let (cr, cg, cb) = ((color >> 16) & 255, (color >> 8) & 255, color & 255);
    for y in y0..y1 {
        for p in &mut buf[(y * w + x0) as usize..(y * w + x1) as usize] {
            let (a0, r0, g0, b0) = (*p >> 24, (*p >> 16) & 255, (*p >> 8) & 255, *p & 255);
            // Straight-alpha "over": the overlay texture is straight alpha too.
            let a = alpha + a0 * (255 - alpha) / 255;
            let mix = |c: u32, d: u32| if a == 0 { 0 } else { (c * alpha + d * a0 * (255 - alpha) / 255) / a };
            *p = (a << 24) | (mix(cr, r0) << 16) | (mix(cg, g0) << 8) | mix(cb, b0);
        }
    }
}

/// Chunky 3D bevel, `t` pixels thick: light top/left, dark bottom/right (raised) or the reverse.
fn bevel(buf: &mut [u32], w: u32, x0: u32, y0: u32, x1: u32, y1: u32, t: u32, raised: bool) {
    let (light, dark) = (0xFF_c0_c0_ff, 0xFF_00_00_28);
    let (tl, br) = if raised { (light, dark) } else { (dark, light) };
    fill_rect(buf, w, x0, y0, x1, y0 + t, tl);
    fill_rect(buf, w, x0, y0, x0 + t, y1, tl);
    fill_rect(buf, w, x0, y1 - t, x1, y1, br);
    fill_rect(buf, w, x1 - t, y0, x1, y1, br);
}

fn fmt_left(secs: f64) -> String {
    let s = secs.max(0.0) as i64;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

// ---------------------------------------------------------------- static (snow) transition

const STATIC_FADE: f32 = 0.10; // seconds to fade snow out once the new channel has a frame
const SETTLE_SECS: f32 = 0.45; // horizontal-hold wobble after the snow clears
const POWER_ON_SECS: f32 = 1.1; // tube warm-up on launch
const POWER_OFF_SECS: f32 = 0.45; // raster collapse on Esc

/// Snow opacity 0..=255. Full until `ready_at`, then linear fade to 0 over STATIC_FADE.
fn static_alpha(ready_at: Instant) -> u32 {
    let now = Instant::now();
    if now < ready_at {
        return 255;
    }
    let t = (now - ready_at).as_secs_f32();
    if t >= STATIC_FADE {
        0
    } else {
        (255.0 * (1.0 - t / STATIC_FADE)) as u32
    }
}


#[cfg(d3d)]
fn is_hw_frame(f: &Video) -> bool {
    f.format() == Pixel::D3D11
}
#[cfg(not(d3d))]
fn is_hw_frame(_f: &Video) -> bool {
    true
}

/// CPU planes of a software-decoded yuv420p frame, as the presenter wants them.
unsafe fn yuv_planes(f: &Video) -> gpu::Picture<'_> {
    let raw = f.as_ptr();
    let h = f.height() as usize;
    let ch = h.div_ceil(2);
    let strides = [(*raw).linesize[0] as usize, (*raw).linesize[1] as usize, (*raw).linesize[2] as usize];
    let planes = [
        std::slice::from_raw_parts((*raw).data[0], strides[0] * h),
        std::slice::from_raw_parts((*raw).data[1], strides[1] * ch),
        std::slice::from_raw_parts((*raw).data[2], strides[2] * ch),
    ];
    gpu::Picture::Yuv420p { planes, strides, width: f.width(), height: f.height() }
}

/// View a decoded frame the way the presenter wants it: a D3D11 NV12 texture slice, or CPU planes.
#[cfg(d3d)]
fn picture_of(f: &Video) -> gpu::Picture<'_> {
    unsafe {
        let raw = f.as_ptr();
        if f.format() == Pixel::D3D11 {
            gpu::Picture::Nv12 { texture: (*raw).data[0] as *mut c_void, index: (*raw).data[1] as usize as u32, width: f.width(), height: f.height() }
        } else {
            yuv_planes(f)
        }
    }
}
#[cfg(not(d3d))]
fn picture_of(f: &Video) -> gpu::Picture<'_> {
    unsafe { yuv_planes(f) }
}

// ---------------------------------------------------------------- app

struct TuneStats {
    total_ms: f64,
    lock_wait_ms: f64,
    primed_hit: bool,
    cold_open: bool,
}

/// Guide state snapshot handed to the drawing code.
struct GuideView<'a> {
    preview: usize,
    sel: usize,
    top: i64,
    step_at: Instant,
    auto: bool,
    music: &'a str,
    s: &'a Settings,
}

struct TuneInfo {
    name: String,
    hit: bool,
    cold: bool,
    lock_ms: f64,
}

/// Decode thread -> UI thread.
enum Msg {
    Tuned { gen: u64, info: TuneInfo },
    Frame { gen: u64, pts: f64, frame: SendFrame },
    Music(String), // guide music track changed
}

/// UI thread -> decode thread.
enum Cmd {
    Tune { src: usize, gen: u64, guide: bool },
    /// Hold music (Prevue channel) vs. the source channel's own audio (interactive guide).
    Music(bool),
    /// No video source (weather channel): park the active pipeline, go quiet.
    Stop,
}

struct SendFrame(Video);
// AVFrame buffers are refcounted and safe to hand to another thread.
unsafe impl Send for SendFrame {}

struct App {
    window: Option<Arc<Window>>,
    gpu: Option<gpu::Gpu>,
    overlay: Vec<u32>,  // window-sized ARGB, straight alpha
    overlay_live: bool, // something is drawn in the GPU copy of the overlay
    guide_snow_full: bool, // entering the guide from a channel: snow the whole screen, not just the ad box
    last_paint: Instant,
    settings: Settings,
    menu: Option<usize>, // setup menu open, with the cursor row
    menu_zip: String,    // ZIP being typed on the WEATHER ZIP row
    geo_rx: Option<GeoRx>, // ZIP lookup in flight
    web: Option<web::Web>,
    web_navigated: bool,
    bug_cache: Option<(usize, u32, Logo)>, // (network, target height, scaled logo)
    t0: Instant,
    music_now: String, // guide music track, from the decode thread
    shared: Arc<Shared>,
    _audio_stream: Option<cpal::Stream>,
    cmd: std::sync::mpsc::Sender<Cmd>,
    rx: std::sync::mpsc::Receiver<Msg>,
    cur: usize,     // displayed channel (0 = guide)
    preview: usize, // channel shown in the guide's ad box (and the source for channel 0)
    gen: u64,       // bumps on every tune; frames tagged with an older gen are dropped
    tune_t0: Instant,
    tuned: Option<TuneInfo>,
    presented_gen: u64, // gen whose first frame has already been shown
    vq: VecDeque<(f64, Video)>,
    last: Option<Video>,
    stats: Vec<TuneStats>,
    frames_presented: u64,
    frames_dropped: u64,
    osd: Osd,
    ready_at: Instant, // when the current channel first had a frame on screen; snow fades from here
    noise_rng: u64,
    entry: String, // digits typed on the remote, shown on the OSD until they commit
    entry_at: Instant,
    guide_t0: Instant,
    preview_at: Instant,
    // Guide cursor: arrows move it, Enter tunes it, the ad box previews it. Idle for a while and
    // the grid goes back to Prevue-style auto scrolling with the preview rotating.
    guide_sel: usize,
    guide_top: i64,        // 0-based row (channel - 1) at the top of the grid
    guide_step_at: Instant, // when the top row last advanced (drives the one-row ease)
    guide_nav_at: Instant,  // last cursor keypress
    guide_auto: bool,
    last_ch: usize,     // for Recall
    vol_until: Instant, // volume bar visible until
    help: bool,         // remote-control legend overlay
    power_off: Option<Instant>, // Esc pressed: collapse the raster, then exit
    node: Option<std::process::Child>, // local WeatherStar server, killed on exit
    started: bool,
    /// ms (from t0) of the last about_to_wait. A watchdog thread invalidates the window when this
    /// goes stale, which only happens inside Windows' modal move/size loop: WM_PAINT still gets
    /// through there, so RedrawRequested -> tick() keeps the video running while dragging.
    heartbeat: Arc<AtomicU64>,
    // --bench N: automated random tunes at key-repeat speed, then dump + stats + exit.
    bench_left: u32,
    bench_next: Instant,
    bench_rng: u64,
    dump_next: bool,
    start_ch: usize,
    shot_at: Option<Instant>, // --shot MS: dump one frame then exit (for headless checks)
}

impl App {
    /// Keypress path: snow up, tell the decode thread, return. Nothing here touches a file.
    fn tune(&mut self, ch: usize) {
        let t0 = Instant::now();
        let from_guide = self.cur == 0;
        // Channel 0 is the guide: it borrows the preview channel's pipeline for its ad box.
        let src = if ch == 0 { self.preview } else { ch };
        // The weather page has no pipeline; neither does a channel nothing was dealt to (dead air:
        // snow), nor a guide whose preview channel is one of those.
        let weather = self.shared.weather == Some(ch) || (ch != 0 && self.shared.scheds[ch].programs.is_empty()) || self.shared.scheds[src].programs.is_empty();
        if ch == 0 {
            self.preview_at = t0;
            if !from_guide {
                // Channel 0 is the Prevue channel: auto-scrolling grid, rotating preview. G turns
                // it into the interactive guide (open_guide).
                self.guide_t0 = t0;
                self.guide_top = self.preview.max(1) as i64 - 1;
                self.guide_step_at = t0;
                self.guide_auto = true;
            }
        } else if !weather {
            self.preview = ch; // the weather channel has no pipeline to preview
        }
        if ch != self.cur && self.cur != 0 {
            self.last_ch = self.cur;
        }
        self.cur = ch;
        if weather || !self.settings.snow {
            self.last = None; // nothing of the old channel lingers: black until the new picture
        }
        self.gen += 1;
        self.tune_t0 = t0;
        self.tuned = None;
        self.vq.clear();
        self.osd.until = t0 + OSD_HOLD;
        // Snow goes up immediately (picture + sound) and holds until the new channel has a frame.
        // On the guide only the ad box gets snow and the music keeps playing.
        self.ready_at = t0 + Duration::from_secs(3600);
        if let Some(a) = &self.shared.audio {
            if !(from_guide && ch == 0) {
                a.ring.lock().unwrap().clear();
            }
            a.snow.store(if ch == 0 && from_guide { 0 } else { 255 }, Ordering::Relaxed);
        }
        self.guide_snow_full = ch == 0 && !from_guide;
        if !from_guide {
            self.present_snow();
        }
        if weather {
            let _ = self.cmd.send(Cmd::Stop);
            // Snow, then the page; a dead channel just stays snow.
            self.ready_at = if self.shared.weather == Some(ch) { t0 + Duration::from_millis(650) } else { t0 + Duration::from_secs(3600 * 24) };
            if let Some(web) = &self.web {
                if !self.web_navigated {
                    self.web_navigated = true;
                    web.navigate(&self.settings.weather_page().unwrap_or_default());
                }
            }
        } else {
            // Only the Prevue channel plays hold music; the interactive guide plays the preview.
            let _ = self.cmd.send(Cmd::Tune { src, gen: self.gen, guide: ch == 0 && self.guide_auto && self.settings.music });
        }
    }

    /// Full-screen snow, no video: the first thing on screen after a keypress.
    fn present_snow(&mut self) {
        self.present_opt(None);
    }

    fn present(&mut self, frame: &Video) {
        self.present_opt(Some(frame));
    }

    /// Compose one output frame on the GPU: video (aspect-fit, or stretched into the guide's ad
    /// box), snow, and the CPU-drawn overlay (guide chrome / channel OSD).
    fn present_opt(&mut self, frame: Option<&Video>) {
        let Some(window) = &self.window else { return };
        let size = window.inner_size();
        let (w, h) = (size.width, size.height);
        if w == 0 || h == 0 || self.gpu.is_none() {
            return;
        }
        if let Err(e) = self.gpu.as_mut().unwrap().resize(w, h) {
            eprintln!("gpu resize: {e}");
            return;
        }
        if self.overlay.len() != (w * h) as usize {
            self.overlay = vec![0; (w * h) as usize];
            self.overlay_live = true; // force an upload at the new size
        }
        let guide = self.cur == 0;
        let weather = self.shared.weather == Some(self.cur);
        let frame = if weather { None } else { frame };
        let (rx, ry, rw, rh) = if guide {
            let (x0, y0) = ((w as f32 * GUIDE_AD.0) as u32, (h as f32 * GUIDE_AD.1) as u32);
            let (x1, y1) = ((w as f32 * GUIDE_AD.2) as u32, (h as f32 * GUIDE_AD.3) as u32);
            (x0, y0, x1 - x0, y1 - y0)
        } else {
            (0, 0, w, h)
        };
        self.osd.hud = self.settings.hud_color();
        let (dw, dh) = match frame {
            Some(f) if !guide => {
                // ASPECT: FIT keeps the file's shape; 4:3 / 16:9 force a shape; STRETCH fills the
                // window. OVERSCAN zooms the picture past the edges (or shrinks it, if negative).
                let (fw, fh) = (f.width() as f64, f.height() as f64);
                let shape = match self.settings.aspect {
                    1 => 4.0 / 3.0,
                    2 => 16.0 / 9.0,
                    3 => rw as f64 / rh as f64,
                    _ => fw / fh.max(1.0),
                };
                let fit_h = (rw as f64 / shape).min(rh as f64);
                let dh = (fit_h * (1.0 + self.settings.overscan as f64)).max(2.0);
                ((dh * shape).max(2.0) as u32, dh as u32)
            }
            _ => (rw, rh), // guide: stretched to fill the box, Prevue style
        };
        // With overscan the rect can extend past the window; the shader just crops it.
        let (ox, oy) = (rx as i64 + (rw as i64 - dw as i64) / 2, ry as i64 + (rh as i64 - dh as i64) / 2);
        let rect = [ox as f32 / w as f32, oy as f32 / h as f32, (ox + dw as i64) as f32 / w as f32, (oy + dh as i64) as f32 / h as f32];
        let (gx, gy) = (ox.max(0) as u32, oy.max(0) as u32); // guide ad box (always inside)

        // Overlay: drawn on the CPU, uploaded only while something is showing.
        let now = Instant::now();
        let osd_on = (!guide || !self.entry.is_empty()) && now < self.osd.until;
        let vol_on = now < self.vol_until && self.shared.audio.is_some();
        let muted = self.shared.audio.as_ref().is_some_and(|a| a.muted.load(Ordering::Relaxed));
        let blink = self.t0.elapsed().as_millis() % 1000 < 600;
        let nd = self.digits();
        // Per-channel dressing: logo bug, clock, news crawl (not on the guide / weather / menu).
        let style = self.shared.styles.get(self.cur).copied();
        let s = &self.settings;
        let dress = !guide && !weather && style.is_some_and(|st| (s.logos && st.bug > 0) || (s.clocks && st.clock) || (s.ticker && st.ticker));
        let roomy = w >= 320 && h >= 200; // the overlay layout assumes a real window, not a sliver
        if roomy && (guide || osd_on || vol_on || muted || self.help || dress || self.menu.is_some()) {
            let ov = &mut self.overlay;
            if guide {
                ov.fill(GUIDE_BLUE | 0xFF00_0000);
                fill_rect(ov, w, gx, gy, gx + dw, gy + dh, 0); // the ad box shows video
                bevel(ov, w, gx - 3, gy - 3, gx + dw + 3, gy + dh + 3, 3, false);
                let g = GuideView { preview: self.preview, sel: self.guide_sel, top: self.guide_top, step_at: self.guide_step_at, auto: self.guide_auto, music: &self.music_now, s: &self.settings };
                Self::draw_guide(&mut self.osd, &self.shared, &g, self.t0, ov, w, h);
            } else {
                ov.fill(0);
                if dress {
                    Self::draw_dressing(&mut self.osd, &self.shared, &self.settings, &mut self.bug_cache, self.cur, self.t0, ov, w, h);
                }
            }
            if osd_on {
                // Big green channel number, top right: the TV set's own OSD.
                let px = (h as f32 * 0.11) as u32;

                let text = if self.entry.is_empty() { format!("{:0nd$}", self.cur) } else { format!("{}{}", self.entry, "-".repeat(nd.saturating_sub(self.entry.len()))) };
                let right = (w as f32 * 0.94) as i32;
                let baseline = (h as f32 * 0.07) as i32 + px as i32;
                self.osd.draw(ov, w, h, &text, px, right, baseline);
                if !guide && self.entry.is_empty() {
                    Self::draw_banner(&mut self.osd, &self.shared, &self.settings, self.cur, ov, w, h);
                }
            }
            if !guide {
                if vol_on {
                    Self::draw_volume(&mut self.osd, self.shared.audio.as_deref().unwrap(), ov, w, h);
                } else if muted && blink {
                    let px = (h as f32 * 0.06) as u32;
                    self.osd.text(ov, w, h, "MUTE", px, (w as f32 * 0.06) as i32, (h as f32 * 0.72) as i32 + px as i32, self.osd.hud, false, NOCLIP);
                }
            }
            if self.help {
                Self::draw_help(&mut self.osd, ov, w, h);
            }
            if let Some(sel) = self.menu {
                Self::draw_menu(&mut self.osd, &self.settings, sel, &self.menu_zip, self.geo_rx.is_some(), ov, w, h);
            }
            self.gpu.as_ref().unwrap().upload_overlay(&self.overlay).ok();
            self.overlay_live = true;
        } else if self.overlay_live {
            self.overlay.fill(0);
            self.gpu.as_ref().unwrap().upload_overlay(&self.overlay).ok();
            self.overlay_live = false;
        }

        // Snow off = a digital box: black between channels. The guide's ad box always snows.
        let a = if self.settings.snow || guide { static_alpha(self.ready_at) } else { 0 };
        if let Some(ao) = &self.shared.audio {
            ao.snow.store(if guide && !self.guide_snow_full { 0 } else { a }, Ordering::Relaxed);
        }
        if let Some(web) = &mut self.web {
            let show = weather && a == 0 && self.menu.is_none() && !self.help && self.power_off.is_none();
            web.set(show.then_some((w, h)));
        }
        self.noise_rng = self.noise_rng.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        let seed = (self.noise_rng >> 44) as f32 / 1000.0;
        let snow_full = !guide || self.guide_snow_full;
        let t = self.t0.elapsed().as_secs_f32();
        // Tube warm-up on launch, collapse on power-off.
        let power = match self.power_off {
            Some(off) => 1.0 - (off.elapsed().as_secs_f32() / POWER_OFF_SECS).min(1.0),
            None => (t / POWER_ON_SECS).min(1.0),
        };
        // Lock-in wobble on the first frames of a new channel (not in the guide's little ad box).
        let settle = if guide || now < self.ready_at { 0.0 } else { (1.0 - (now - self.ready_at).as_secs_f32() / SETTLE_SECS).max(0.0) };
        let s = &self.settings;
        let crt_params = gpu::CrtParams { curve: s.crt_curve, scan: s.crt_scan, noise: s.crt_noise, vignette: s.crt_vignette };
        let fx = gpu::Fx { snow: a as f32 / 255.0, snow_full, seed, crt: s.crt, crt_params, time: t, power, settle };
        let pic = frame.map(picture_of);
        let gpu = self.gpu.as_mut().unwrap();
        if let Err(e) = gpu.render(pic.as_ref(), rect, fx) {
            eprintln!("gpu render: {e}");
        }
        if self.dump_next {
            self.dump_next = false;
            if let Ok((dw, dh, bgra)) = gpu.read_back() {
                let mut ppm = format!("P6\n{dw} {dh}\n255\n").into_bytes();
                for p in bgra.chunks_exact(4) {
                    ppm.extend_from_slice(&[p[2], p[1], p[0]]);
                }
                std::fs::write("bench_last.ppm", ppm).expect("dump");
            }
        }
        self.frames_presented += 1;
        self.last_paint = Instant::now();
    }

    /// Minimum digits on the channel OSD (2, or more when there are 100+ channels).
    fn digits(&self) -> usize {
        format!("{}", self.shared.slots.len().saturating_sub(1)).len().max(2)
    }

    /// Repaint period needed while something time-based is drawing without new video: fast for
    /// the tube warm-up/collapse and lock-in wobble, slow for OSD timeouts and the MUTE blink.
    fn fx_period(&self) -> Option<Duration> {
        let now = Instant::now();
        let fast = self.power_off.is_some()
            || self.t0.elapsed().as_secs_f32() < POWER_ON_SECS
            || (now >= self.ready_at && (now - self.ready_at).as_secs_f32() < SETTLE_SECS);
        if fast {
            return Some(Duration::from_millis(8));
        }
        let slow = now < self.vol_until + Duration::from_millis(50)
            || now < self.osd.until + Duration::from_millis(50)
            || self.shared.audio.as_ref().is_some_and(|a| a.muted.load(Ordering::Relaxed));
        slow.then_some(Duration::from_millis(50))
    }

    /// Cable-box info banner along the bottom: channel + network, what's on, what's next, how far
    /// in, and the clock. Translucent navy with a raised bevel.
    fn draw_banner(osd: &mut Osd, sh: &Shared, s: &Settings, ch: usize, buf: &mut [u32], w: u32, h: u32) {
        let (wf, hf) = (w as f32, h as f32);
        let (x0, y0, x1, y1) = ((wf * 0.04) as u32, (hf * 0.79) as u32, (wf * 0.96) as u32, (hf * 0.94) as u32);
        tint_rect(buf, w, x0, y0, x1, y1, GUIDE_BLUE, 0xD0);
        bevel(buf, w, x0, y0, x1, y1, 3, true);
        let (net, now_name, next_name, left, frac) = chan_info(sh, ch);
        let px = (hf * 0.05) as u32;
        let px_s = (hf * 0.036) as u32;
        let lx = x0 as i32 + (wf * 0.02) as i32;
        let rx = x1 as i32 - (wf * 0.02) as i32;
        let b1 = y0 as i32 + (hf * 0.062) as i32;
        let b2 = y0 as i32 + (hf * 0.108) as i32;
        let head = format!("{ch:02} {}", cut(&net, 12));
        osd.text(buf, w, h, &head, px, lx, b1, GUIDE_YELLOW, false, NOCLIP);
        let head_w = osd.width(&head, px) as i32 + (wf * 0.025) as i32;
        osd.text(buf, w, h, &cut(&now_name, 26), px, lx + head_w, b1, GUIDE_WHITE, false, NOCLIP);
        osd.text(buf, w, h, &format!("NEXT  {}", cut(&next_name, 30)), px_s, lx, b2, GUIDE_GRAY, false, NOCLIP);
        osd.text(buf, w, h, &s.clock(false), px, rx, b1, GUIDE_WHITE, true, NOCLIP);
        if left > 0.0 {
            osd.text(buf, w, h, &format!("{} LEFT", fmt_left(left)), px_s, rx, b2, GUIDE_GRAY, true, NOCLIP);
        }
        // Progress through the program: sunken track, green fill.
        let (bx0, bx1) = (lx as u32, rx as u32);
        let (by0, by1) = (y1 - (hf * 0.028) as u32, y1 - (hf * 0.014) as u32);
        fill_rect(buf, w, bx0, by0, bx1, by1, GUIDE_DARK | 0xFF00_0000);
        bevel(buf, w, bx0, by0, bx1, by1, 1, false);
        let fx1 = bx0 + 1 + ((bx1 - bx0 - 2) as f32 * frac) as u32;
        fill_rect(buf, w, bx0 + 1, by0 + 1, fx1.max(bx0 + 1), by1 - 1, osd.hud | 0xFF00_0000);
    }

    /// What a channel puts over its own picture: a translucent network bug in a corner, a small
    /// clock, and/or a lower-third news crawl. Which ones is fixed per channel (ChanStyle).
    #[allow(clippy::too_many_arguments)]
    fn draw_dressing(osd: &mut Osd, sh: &Shared, s: &Settings, cache: &mut Option<(usize, u32, Logo)>, ch: usize, t0: Instant, buf: &mut [u32], w: u32, h: u32) {
        let Some(st) = sh.styles.get(ch).copied() else { return };
        let (wf, hf) = (w as f32, h as f32);
        let net = &sh.nets[st.net];
        if s.ticker && st.ticker {
            // Crawl: coloured band, network tag box on the left, headlines scrolling through.
            let (ty0, ty1) = ((hf * 0.935) as u32, h);
            tint_rect(buf, w, 0, ty0, w, ty1, st.ticker_color, 0xE0);
            fill_rect(buf, w, 0, ty0, w, ty0 + 2, 0xFF_ff_ff_ff);
            let px = (hf * 0.04) as u32;
            let tb = (ty0 as f32 + (ty1 - ty0) as f32 * 0.74) as i32;
            let tag = format!(" {} NEWS ", cut(&net.name.to_uppercase(), 10));
            let tag_w = osd.width(&tag, px) as u32 + (wf * 0.01) as u32;
            let msg: String = HEADLINES.lines().map(str::trim).filter(|l| !l.is_empty()).collect::<Vec<_>>().join("   *   ") + "   *   ";
            let tw = osd.width(&msg, px);
            let shift = (t0.elapsed().as_secs_f32() * wf * 0.11) % tw;
            let mut sx = tag_w as f32 - shift;
            while sx < wf {
                // Only draw glyphs that will land right of the tag box (text() clips to the window).
                if sx + tw > tag_w as f32 {
                    osd.text(buf, w, h, &msg, px, sx as i32, tb, GUIDE_WHITE, false, (ty0 as i32, ty1 as i32));
                }
                sx += tw;
            }
            fill_rect(buf, w, 0, ty0 + 2, tag_w, ty1, 0xFF_f0_e0_20);
            osd.text(buf, w, h, &tag, px, (wf * 0.005) as i32, tb, 0x00_10_10_10, false, NOCLIP);
        }
        // Corner clock only during the hours a real station runs one (morning and evening news
        // blocks), plus the occasional program that carries it all through.
        let clock_now = s.clocks && st.clock && {
            let hour = chrono::Local::now().hour();
            let news_hours = (5..10).contains(&hour) || (17..20).contains(&hour);
            let prog_clock = sh.scheds.get(ch).filter(|sc| !sc.programs.is_empty()).is_some_and(|sc| {
                let (i, _) = sc.at(ch, now_epoch());
                (sc.programs[i].file as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 62 == 0 // 1 in 4 programs
            });
            news_hours || prog_clock
        };
        if clock_now {
            let px = (hf * 0.045) as u32;
            let (cx, cy) = ((wf * 0.04) as i32, (hf * 0.05) as i32 + px as i32);
            osd.text(buf, w, h, &s.clock(false), px, cx, cy, GUIDE_WHITE, false, NOCLIP);
        }
        if s.logos && st.bug > 0 {
            if let Some(logo) = &net.logo {
                let th = (hf * 0.09) as u32;
                if cache.as_ref().is_none_or(|c| c.0 != st.net || c.1 != th) {
                    let tw = ((logo.w as f32 * th as f32 / logo.h.max(1) as f32) as u32).min((wf * 0.2) as u32).max(1);
                    let th2 = (logo.h as f32 * tw as f32 / logo.w.max(1) as f32) as u32;
                    *cache = Some((st.net, th, scale_logo(logo, tw, th2.max(1))));
                }
                let l = &cache.as_ref().unwrap().2;
                let (mx, my) = ((wf * 0.05) as i32, (hf * 0.07) as i32);
                let (x, y) = match st.bug {
                    1 => (w as i32 - mx - l.w as i32, h as i32 - my - l.h as i32),
                    2 => (mx, h as i32 - my - l.h as i32),
                    _ => (mx, my + if clock_now { (hf * 0.07) as i32 } else { 0 }),
                };
                blit_logo(buf, w, h, x, y, l, st.bug_alpha);
            }
        }
    }

    /// VCR setup menu: black screen, rows in the HUD colour, cursor, values on the right. Scrolls
    /// when there are more rows than fit.
    #[allow(clippy::too_many_arguments)]
    fn draw_menu(osd: &mut Osd, s: &Settings, sel: usize, zip: &str, geocoding: bool, buf: &mut [u32], w: u32, h: u32) {
        let (wf, hf) = (w as f32, h as f32);
        buf.fill(0xFF_00_00_00);
        let px = (hf * 0.042) as u32;
        let row = (px as f32 * 1.55) as i32;
        let lx = (wf * 0.14) as i32;
        let vx = (wf * 0.86) as i32;
        let mut b = (hf * 0.08) as i32 + px as i32;
        osd.text(buf, w, h, "SET UP MENU", (px as f32 * 1.3) as u32, lx, b, GUIDE_WHITE, false, NOCLIP);
        b += row + row / 2;
        let visible = 9usize;
        let top = sel.saturating_sub(visible / 2).min(MENU.len().saturating_sub(visible));
        if top > 0 {
            osd.text(buf, w, h, "^", px, vx, b - row, GUIDE_GRAY, true, NOCLIP);
        }
        for (i, (label, restart)) in MENU.iter().enumerate().skip(top).take(visible) {
            let cur = i == sel;
            let color = if cur { GUIDE_YELLOW } else { osd.hud };
            if cur {
                osd.text(buf, w, h, ">", px, lx - (px as f32 * 0.9) as i32, b, GUIDE_YELLOW, false, NOCLIP);
            }
            osd.text(buf, w, h, label, px, lx, b, color, false, NOCLIP);
            let val = if i == MENU_ZIP && cur { format!("{zip}{}", "-".repeat(5usize.saturating_sub(zip.len()))) } else { s.value(i) };
            let val = if cur && i != MENU_ZIP { format!("< {val} >") } else { format!("  {val}  ") };
            osd.text(buf, w, h, &val, px, vx, b, color, true, NOCLIP);
            if *restart {
                let lw = osd.width(label, px) as i32;
                osd.text(buf, w, h, "*", px, lx + lw + (px / 2) as i32, b, GUIDE_GRAY, false, NOCLIP);
            }
            b += row;
        }
        if top + visible < MENU.len() {
            osd.text(buf, w, h, "v", px, vx, b, GUIDE_GRAY, true, NOCLIP);
        }
        let px_s = (hf * 0.028) as u32;
        let mut fb = (hf * 0.80) as i32;
        let hint = if sel == MENU_ZIP {
            if geocoding { "LOOKING UP ZIP..." } else { "TYPE 5 DIGITS, ENTER TO APPLY   BACKSPACE DELETES" }
        } else if MENU[sel].1 {
            "* TAKES EFFECT AT NEXT START"
        } else {
            "UP/DOWN SELECT   LEFT/RIGHT CHANGE   S EXIT"
        };
        osd.text(buf, w, h, hint, px_s, lx, fb, GUIDE_WHITE, false, NOCLIP);
        fb += (px_s as f32 * 1.7) as i32;
        osd.text(buf, w, h, &format!("WEATHER: {}", cut(&s.weather_location.to_uppercase(), 40)), px_s, lx, fb, GUIDE_GRAY, false, NOCLIP);
        fb += (px_s as f32 * 1.7) as i32;
        osd.text(buf, w, h, &format!("FILE: {}", cut(&Settings::path().display().to_string().to_uppercase(), 64)), px_s, lx, fb, GUIDE_GRAY, false, NOCLIP);
    }

    /// WEATHER ZIP row: commit the typed ZIP and look it up on a thread.
    fn apply_zip(&mut self) {
        if self.menu_zip.len() != 5 || self.geo_rx.is_some() {
            return;
        }
        self.settings.weather_zip = self.menu_zip.clone();
        self.settings.save();
        self.geo_rx = Some(start_geocode(&self.settings.weather_zip));
    }

    /// TV-set volume OSD: "VOLUME" and a row of green segments, one per step.
    fn draw_volume(osd: &mut Osd, ao: &AudioOut, buf: &mut [u32], w: u32, h: u32) {
        let (wf, hf) = (w as f32, h as f32);
        let px = (hf * 0.06) as u32;
        let x = (wf * 0.06) as i32;
        let base = (hf * 0.72) as i32 + px as i32;
        let muted = ao.muted.load(Ordering::Relaxed);
        osd.text(buf, w, h, if muted { "MUTE" } else { "VOLUME" }, px, x, base, osd.hud, false, NOCLIP);
        let vol = if muted { 0 } else { ao.volume.load(Ordering::Relaxed) };
        let sx = x + osd.width("VOLUME  ", px) as i32;
        let seg_w = (wf * 0.022) as i32;
        let gap = (seg_w / 4).max(2);
        let (sy0, sy1) = (base - (px as f32 * 0.62) as i32, base);
        let shadow = (px / 14).max(2) as i32;
        for i in 0..VOL_MAX as i32 {
            let ax = sx + i * (seg_w + gap);
            if ax + seg_w + shadow >= w as i32 {
                break;
            }
            fill_rect(buf, w, (ax + shadow) as u32, (sy0 + shadow) as u32, (ax + seg_w + shadow) as u32, (sy1 + shadow) as u32, 0xFF00_0000);
            if i < vol as i32 {
                fill_rect(buf, w, ax as u32, sy0 as u32, (ax + seg_w) as u32, sy1 as u32, osd.hud | 0xFF00_0000);
            } else {
                // Empty segment: just an outline.
                fill_rect(buf, w, ax as u32, sy0 as u32, (ax + seg_w) as u32, sy1 as u32, osd.hud | 0xFF00_0000);
                fill_rect(buf, w, ax as u32 + 2, sy0 as u32 + 2, (ax + seg_w) as u32 - 2, sy1 as u32 - 2, 0);
            }
        }
    }

    /// Remote-control legend (H / F1).
    fn draw_help(osd: &mut Osd, buf: &mut [u32], w: u32, h: u32) {
        let (wf, hf) = (w as f32, h as f32);
        let lines = [
            ("UP/DOWN", "CHANNEL +/-  (GUIDE: MOVE CURSOR)"),
            ("LEFT/RIGHT", "CHANNEL +/-10"),
            ("0-9, ENTER", "DIRECT TUNE   (0 = PREVUE CHANNEL)"),
            ("ENTER", "INFO BANNER / TUNE SELECTION"),
            ("BACKSPACE", "RECALL LAST CHANNEL"),
            ("+ / -", "VOLUME"),
            ("M", "MUTE"),
            ("G", "INTERACTIVE GUIDE (AGAIN: CLOSE)"),
            ("S", "SET UP MENU"),
            ("C", "CRT TUBE"),
            ("H / F1", "THIS SCREEN"),
            ("ESC", "POWER OFF"),
        ];
        let px = (hf * 0.036) as u32;
        let row = (px as f32 * 1.45) as i32;
        let (x0, y0) = ((wf * 0.12) as u32, (hf * 0.10) as u32);
        let (x1, y1) = ((wf * 0.88) as u32, y0 + (row * (lines.len() as i32 + 2)) as u32 + (hf * 0.05) as u32);
        let y1 = y1.min(h);
        tint_rect(buf, w, x0, y0, x1, y1, GUIDE_BLUE, 0xE0);
        bevel(buf, w, x0, y0, x1, y1, 3, true);
        let lx = x0 as i32 + (wf * 0.03) as i32;
        let vx = x0 as i32 + (wf * 0.26) as i32;
        let mut b = y0 as i32 + (hf * 0.03) as i32 + px as i32;
        osd.text(buf, w, h, "REMOTE CONTROL", (px as f32 * 1.25) as u32, lx, b, GUIDE_YELLOW, false, NOCLIP);
        b += row + row / 2;
        for (k, v) in lines {
            osd.text(buf, w, h, k, px, lx, b, osd.hud, false, NOCLIP);
            osd.text(buf, w, h, v, px, vx, b, GUIDE_WHITE, false, NOCLIP);
            b += row;
        }
    }

    /// Prevue-style guide: info panel left of the ad box, clock and date, then a grid of channels
    /// that steps up one row at a time (hold, then a quick scroll) or follows the cursor, and a
    /// scrolling ticker along the bottom.
    fn draw_guide(osd: &mut Osd, sh: &Shared, g: &GuideView, t0: Instant, buf: &mut [u32], w: u32, h: u32) {
        let (wf, hf) = (w as f32, h as f32);
        // (network, now playing, next) for a grid row.
        let info = |ch: usize| -> (String, String, String) {
            let (net, now, next, _, _) = chan_info(sh, ch);
            (cut(&net, 12), cut(&now, 24), cut(&next, 24))
        };

        // Left panel: what the ad box is showing.
        let px = (hf * 0.05) as u32;
        let x = (wf * 0.03) as i32;
        let (pnet, pn, pnext, left, _) = chan_info(sh, g.preview);
        let label = if g.auto { "PREVIEW" } else { "SELECTED" };
        osd.text(buf, w, h, &format!("{label}  CH {:02}  {}", g.preview, cut(&pnet, 14)), px, x, (hf * 0.12) as i32, GUIDE_YELLOW, false, NOCLIP);
        osd.text(buf, w, h, &cut(&pn, 26), px, x, (hf * 0.20) as i32, GUIDE_WHITE, false, NOCLIP);
        osd.text(buf, w, h, &format!("NEXT  {}", cut(&pnext, 26)), px * 4 / 5, x, (hf * 0.27) as i32, GUIDE_GRAY, false, NOCLIP);
        if left > 0.0 {
            osd.text(buf, w, h, &format!("{} LEFT", fmt_left(left)), px * 4 / 5, x, (hf * 0.33) as i32, GUIDE_GRAY, false, NOCLIP);
        }
        let date = chrono::Local::now().format("%a %b %e").to_string().to_uppercase();
        osd.text(buf, w, h, &date, px * 4 / 5, x, (hf * 0.395) as i32, GUIDE_GRAY, false, NOCLIP);
        osd.text(buf, w, h, &g.s.clock(true), (px as f32 * 1.4) as u32, x, (hf * 0.475) as i32, GUIDE_WHITE, false, NOCLIP);

        // Header bar.
        let (hy0, hy1) = ((hf * 0.50) as u32, (hf * 0.56) as u32);
        fill_rect(buf, w, 0, hy0, w, hy1, GUIDE_DARK | 0xFF00_0000);
        bevel(buf, w, 0, hy0, w, hy1, 2, true);
        let px_row = (hf * 0.045) as u32;
        let hb = (hf * 0.55) as i32;
        let (cx, netx, nx, xx) = ((wf * 0.03) as i32, (wf * 0.09) as i32, (wf * 0.245) as i32, (wf * 0.625) as i32);
        osd.text(buf, w, h, "CH", px_row, cx, hb, GUIDE_YELLOW, false, NOCLIP);
        osd.text(buf, w, h, "NOW", px_row, nx, hb, GUIDE_WHITE, false, NOCLIP);
        osd.text(buf, w, h, "NEXT", px_row, xx, hb, GUIDE_GRAY, false, NOCLIP);

        // Ticker bar along the bottom; the grid is clipped above it.
        let ty0 = (hf * 0.935) as u32;
        let n = sh.slots.len() - 1;
        if n == 0 {
            return;
        }

        // Rows: hold 1.5 s after each step, then ease up one row over 0.5 s (auto mode only).
        let ph = g.step_at.elapsed().as_secs_f32();
        let frac = if !g.auto || ph < 1.5 { 0.0 } else { let u = ((ph - 1.5) / 0.5).min(1.0); u * u * (3.0 - 2.0 * u) };
        let row_h = px_row as f32 * 1.5;
        let y0 = hy1 as f32;
        let clip = (y0 as i32, ty0 as i32);
        let visible = ((ty0 as f32 - y0) / row_h).ceil() as i64 + 1;
        for r in 0..visible {
            let ch = 1 + ((g.top + r).rem_euclid(n as i64)) as usize;
            let top = y0 + (r as f32 - frac) * row_h;
            let baseline = (top + row_h * 0.78) as i32;
            if top >= ty0 as f32 {
                break;
            }
            let (ry0, ry1) = ((top.max(y0)) as u32, ((top + row_h).min(ty0 as f32)) as u32);
            if ry1 > ry0 {
                if !g.auto && ch == g.sel {
                    fill_rect(buf, w, 0, ry0, w, ry1, GUIDE_HILITE | 0xFF00_0000);
                } else if (g.top + r).rem_euclid(2) == 1 {
                    fill_rect(buf, w, 0, ry0, w, ry1, GUIDE_BLUE2 | 0xFF00_0000);
                }
            }
            if top >= y0 && (top as u32) < ty0 {
                fill_rect(buf, w, 0, top as u32, w, top as u32 + 1, GUIDE_DARK | 0xFF00_0000);
            }
            let (net, nn, nxt) = info(ch);
            let (c_ch, c_now, c_next) = if !g.auto && ch == g.sel { (0, 0, 0x00_30_30_30) } else { (GUIDE_YELLOW, GUIDE_WHITE, GUIDE_GRAY) };
            osd.text(buf, w, h, &format!("{ch:02}"), px_row, cx, baseline, c_ch, false, clip);
            osd.text(buf, w, h, &cut(&net, 9), (px_row as f32 * 0.7) as u32, netx, baseline, c_next, false, clip);
            osd.text(buf, w, h, &cut(&nn, 20), px_row, nx, baseline, c_now, false, clip);
            osd.text(buf, w, h, &cut(&nxt, 19), px_row, xx, baseline, c_next, false, clip);
        }

        // Ticker.
        fill_rect(buf, w, 0, ty0, w, h, GUIDE_DARK | 0xFF00_0000);
        bevel(buf, w, 0, ty0, w, h, 2, true);
        let px_t = (hf * 0.04) as u32;
        let mut msg = String::from(if g.auto {
            "PREVUE  *  ENTER WATCHES THE PREVIEW  *  G INTERACTIVE GUIDE  *  H HELP"
        } else {
            "GUIDE  *  UP/DOWN SELECT  *  ENTER TUNE  *  G CLOSE  *  H HELP"
        });
        if !g.music.is_empty() {
            msg.push_str(&format!("  *  NOW PLAYING: {}", g.music.to_uppercase()));
        }
        msg.push_str("  *  ");
        let tw = osd.width(&msg, px_t);
        let speed = wf * 0.12; // px/s
        let shift = (t0.elapsed().as_secs_f32() * speed) % tw;
        let tb = (ty0 as f32 + (hf - ty0 as f32) * 0.76) as i32;
        let mut sx = -shift;
        while sx < wf {
            osd.text(buf, w, h, &msg, px_t, sx as i32, tb, GUIDE_WHITE, false, (ty0 as i32, h as i32));
            sx += tw;
        }
    }


    /// UI tick: take finished frames from the decode thread, present the one that is due.
    fn tick(&mut self, el: &ActiveEventLoop) {
        while let Ok(m) = self.rx.try_recv() {
            match m {
                Msg::Tuned { gen, info } if gen == self.gen => {
                    // A switch happened (tune or program rollover): frames queued from the previous
                    // file are stale. Their timestamps would look like the far future.
                    self.vq.clear();
                    self.tuned = Some(info);
                }
                Msg::Frame { gen, pts, frame } if gen == self.gen => self.vq.push_back((pts, frame.0)),
                Msg::Music(s) => self.music_now = s,
                _ => {} // stale generation
            }
        }
        let src = if self.cur == 0 { self.preview } else { self.cur };
        let (_, offset) = self.shared.scheds[src].at(src, now_epoch());
        let first = self.presented_gen != self.gen;
        // Late frames are dropped (keep one so something is always on screen).
        while self.vq.len() > 1 && self.vq[0].0 - offset < -0.040 {
            self.vq.pop_front();
            self.frames_dropped += 1;
        }
        // Anything far in the future is from a previous file; never wait on it.
        while self.vq.front().is_some_and(|f| f.0 - offset > 1.5) {
            self.vq.pop_front();
            self.frames_dropped += 1;
        }
        let mut next_wake = Duration::from_millis(20);
        let mut presented = false;
        if let Some(&(pts, _)) = self.vq.front() {
            let due = pts - offset;
            if due > 0.002 && !first {
                next_wake = Duration::from_secs_f64(due.min(0.02));
            } else {
                presented = true;
                let (_, f) = self.vq.pop_front().unwrap();
                if first {
                    self.ready_at = Instant::now(); // snow starts fading from here
                }
                self.present(&f);
                self.last = Some(f);
                if first {
                    self.presented_gen = self.gen;
                    let total_ms = self.tune_t0.elapsed().as_secs_f64() * 1e3;
                    let info = self.tuned.take().unwrap_or(TuneInfo { name: "?".into(), hit: false, cold: false, lock_ms: 0.0 });
                    println!(
                        "tune CH{:02} {}  {}  lock-wait {:5.2}ms  presented {total_ms:6.2}ms",
                        self.cur,
                        info.name,
                        if info.cold { "COLD " } else if info.hit { "hit  " } else { "miss " },
                        info.lock_ms
                    );
                    if let Some(w) = &self.window {
                        w.set_title(&format!("CH {:02}  {}   tune {total_ms:.1} ms", self.cur, info.name));
                    }
                    self.stats.push(TuneStats { total_ms, lock_wait_ms: info.lock_ms, primed_hit: info.hit, cold_open: info.cold });
                }
                next_wake = Duration::from_millis(1);
            }
        }
        if self.power_off.is_some_and(|t| t.elapsed().as_secs_f32() >= POWER_OFF_SECS + 0.15) {
            self.print_stats();
            el.exit();
            return;
        }
        // Snow, the guide and the analog effects animate on their own clock, not only when video
        // frames arrive.
        if !presented {
            let snowing = static_alpha(self.ready_at) > 0;
            let fx = self.fx_period();
            if snowing || fx.is_some() || self.cur == 0 {
                let mut period = if snowing { Duration::from_millis(8) } else { Duration::from_millis(33) };
                if let Some(p) = fx {
                    period = period.min(p);
                }
                if self.last_paint.elapsed() >= period {
                    match self.last.take() {
                        Some(f) => {
                            self.present(&f);
                            self.last = Some(f);
                        }
                        None => self.present_snow(),
                    }
                }
                next_wake = next_wake.min(period);
            }
        }
        el.set_control_flow(ControlFlow::WaitUntil(Instant::now() + next_wake));
    }

    /// Tune to the typed channel number if it exists; otherwise just drop the entry.
    fn commit_entry(&mut self) {
        let n = self.entry.parse::<usize>();
        self.entry.clear();
        if let Some(n) = n.ok().filter(|&n| n < self.shared.slots.len()) {
            self.tune(n);
        } else {
            self.repaint(); // so the OSD shows the real channel again
        }
    }

    /// G: the interactive guide. Cursor starts on the channel we came from (or the preview).
    fn open_guide(&mut self) {
        if self.cur != 0 {
            self.tune(0);
        }
        let now = Instant::now();
        self.guide_auto = false;
        self.guide_sel = self.preview.max(1);
        self.guide_top = self.guide_sel as i64 - 1;
        self.guide_step_at = now;
        self.guide_nav_at = now;
        let _ = self.cmd.send(Cmd::Music(false));
        self.repaint();
    }

    /// Enter with nothing typed: in the interactive guide it tunes the cursor row, on the Prevue
    /// channel it tunes what's previewing, on a channel it brings up the info banner.
    fn select(&mut self) {
        if self.cur == 0 {
            let ch = if self.guide_auto { self.preview } else { self.guide_sel };
            self.tune(ch);
        } else {
            self.osd.until = Instant::now() + OSD_HOLD;
            self.repaint();
        }
    }

    /// Move the guide cursor by `delta` rows and keep it on screen; the ad box follows (debounced
    /// in about_to_wait so a held key doesn't fire a tune per row).
    fn guide_move(&mut self, delta: isize) {
        let n = self.shared.slots.len() as isize - 1;
        if n <= 0 {
            return;
        }
        let now = Instant::now();
        self.guide_sel = ((self.guide_sel as isize - 1 + delta).rem_euclid(n) + 1) as usize;
        self.guide_nav_at = now;
        self.guide_step_at = now;
        // Keep the cursor within the visible rows (roughly 8 fit); scroll the grid if needed.
        let rel = (self.guide_sel as i64 - 1 - self.guide_top).rem_euclid(n as i64);
        // Full rows that fit between the header and the ticker (same geometry as draw_guide).
        let hf = self.window.as_ref().map(|w| w.inner_size().height as f32).unwrap_or(720.0);
        let row_h = (hf * 0.045) as u32 as f32 * 1.5;
        let rows = (((hf * 0.935) as u32 as f32 - (hf * 0.56) as u32 as f32) / row_h).floor() as i64;
        let rows = rows.max(2);
        if delta == -1 && rel == n as i64 - 1 {
            self.guide_top = self.guide_sel as i64 - 1; // stepped above the top: scroll up one
        } else if delta == 1 && rel == rows - 1 {
            self.guide_top = (self.guide_top + 1).rem_euclid(n as i64); // stepped off the bottom
        } else if rel >= rows - 1 {
            self.guide_top = (self.guide_sel as i64 - 1 - rows / 2).rem_euclid(n as i64); // jumped: recenter
        }
        self.repaint();
    }

    fn set_volume(&mut self, delta: i32) {
        let Some(a) = &self.shared.audio else { return };
        let v = (a.volume.load(Ordering::Relaxed) as i32 + delta).clamp(0, VOL_MAX as i32) as u32;
        a.volume.store(v, Ordering::Relaxed);
        a.muted.store(false, Ordering::Relaxed);
        self.vol_until = Instant::now() + VOL_HOLD;
        self.repaint();
    }

    fn toggle_mute(&mut self) {
        let Some(a) = &self.shared.audio else { return };
        let m = !a.muted.load(Ordering::Relaxed);
        a.muted.store(m, Ordering::Relaxed);
        self.vol_until = Instant::now() + VOL_HOLD;
        self.repaint();
    }

    /// Power off: mute, collapse the raster; tick() exits once the collapse has played out.
    fn power_down(&mut self) {
        if self.power_off.is_some() {
            return;
        }
        self.power_off = Some(Instant::now());
        self.menu = None;
        self.help = false;
        if let Some(a) = &self.shared.audio {
            a.muted.store(true, Ordering::Relaxed);
        }
        self.repaint();
    }

    /// Redraw the last frame (or snow) so an overlay change shows up before the next video frame.
    fn repaint(&mut self) {
        match self.last.take() {
            Some(f) => {
                self.present(&f);
                self.last = Some(f);
            }
            None => self.present_snow(),
        }
    }

    fn print_stats(&self) {
        if self.stats.is_empty() {
            return;
        }
        let mut t: Vec<f64> = self.stats.iter().map(|s| s.total_ms).collect();
        t.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let n = t.len();
        let avg = t.iter().sum::<f64>() / n as f64;
        let p = |q: f64| t[((n as f64 - 1.0) * q).round() as usize];
        let hits = self.stats.iter().filter(|s| s.primed_hit).count();
        let cold = self.stats.iter().filter(|s| s.cold_open).count();
        let lock_max = self.stats.iter().map(|s| s.lock_wait_ms).fold(0.0, f64::max);
        println!("\n=== {n} tunes ===");
        println!(
            "keypress->presented ms: min {:.2}  p50 {:.2}  p90 {:.2}  p99 {:.2}  max {:.2}  avg {:.2}",
            t[0],
            p(0.5),
            p(0.9),
            p(0.99),
            t[n - 1],
            avg
        );
        println!("primed hits {hits}/{n}  cold opens {cold}  max lock wait {lock_max:.2} ms");
        println!("frames presented {}  dropped(catch-up) {}", self.frames_presented, self.frames_dropped);
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, _el: &ActiveEventLoop) {
        // The window and presenter were created by the loading screen; this runs once after it.
        if self.started {
            return;
        }
        self.started = true;
        if let Some(window) = self.window.clone() {
            if self.shared.weather.is_some() {
                match web::Web::new(&window) {
                    Ok(w) => self.web = Some(w),
                    Err(e) => eprintln!("webview2: {e} (weather channel will be blank)"),
                }
            }
            // Drag watchdog (see `heartbeat`); runs until the workers are told to stop.
            let alive = Arc::new(AtomicBool::new(true));
            let sh = self.shared.clone();
            let a2 = alive.clone();
            std::thread::spawn(move || {
                while !sh.stop.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(100));
                }
                a2.store(false, Ordering::Relaxed);
            });
            spawn_drag_watchdog(&window, self.heartbeat.clone(), self.t0, alive);
        }
        let s = self.start_ch;
        self.tune(s);
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => {
                // The X behaves like the power button: collapse the raster, then exit (tick()).
                // A second X while it's collapsing quits at once.
                if self.power_off.is_some() {
                    self.print_stats();
                    el.exit();
                } else {
                    self.power_down();
                }
            }
            WindowEvent::Resized(_) => self.repaint(),
            // Inside a window drag the event loop is stuck in Windows' modal loop and about_to_wait
            // never runs; WM_PAINT still arrives (the watchdog thread keeps requesting it), so
            // advance playback from here.
            WindowEvent::RedrawRequested => self.tick(el),
            WindowEvent::KeyboardInput { event: KeyEvent { physical_key: PhysicalKey::Code(code), state: ElementState::Pressed, .. }, .. } => {
                if self.power_off.is_some() {
                    return;
                }
                let n = self.shared.slots.len() as isize;
                if let Some(i) = self.menu {
                    let len = MENU.len();
                    if i == MENU_ZIP {
                        if let Some(d) = digit_of(code) {
                            if self.menu_zip.len() < 5 {
                                self.menu_zip.push(d);
                            }
                            self.repaint();
                            return;
                        }
                        if code == KeyCode::Backspace {
                            self.menu_zip.pop();
                            self.repaint();
                            return;
                        }
                    }
                    let mut dir = 0;
                    match code {
                        KeyCode::ArrowUp => self.menu = Some((i + len - 1) % len),
                        KeyCode::ArrowDown => self.menu = Some((i + 1) % len),
                        KeyCode::ArrowLeft => dir = -1,
                        KeyCode::ArrowRight => dir = 1,
                        KeyCode::Enter | KeyCode::NumpadEnter => {
                            if i == MENU_ZIP {
                                self.apply_zip();
                            } else {
                                dir = 1;
                            }
                        }
                        KeyCode::KeyS | KeyCode::Escape => self.menu = None,
                        _ => {}
                    }
                    if dir != 0 && i != MENU_ZIP {
                        self.settings.adjust(i, dir);
                        self.settings.save();
                        if self.cur == 0 {
                            let _ = self.cmd.send(Cmd::Music(self.guide_auto && self.settings.music));
                        }
                    }
                    self.repaint();
                    return;
                }
                if self.help && !matches!(code, KeyCode::KeyH | KeyCode::F1 | KeyCode::Escape) {
                    self.help = false; // any key dismisses the legend, then acts normally
                }
                if let Some(d) = digit_of(code) {
                    let nd = self.digits();
                    if self.entry.len() < nd {
                        self.entry.push(d);
                        self.entry_at = Instant::now();
                        self.osd.until = self.entry_at + OSD_HOLD;
                        self.repaint();
                    }
                    if self.entry.len() == nd {
                        self.commit_entry();
                    }
                    return;
                }
                let delta: isize = match code {
                    KeyCode::ArrowUp => 1,
                    KeyCode::ArrowDown => -1,
                    KeyCode::ArrowRight => 10,
                    KeyCode::ArrowLeft => -10,
                    KeyCode::Enter | KeyCode::NumpadEnter => {
                        if self.entry.is_empty() {
                            self.select();
                        } else {
                            self.commit_entry();
                        }
                        return;
                    }
                    KeyCode::Backspace | KeyCode::KeyR => {
                        // Recall: back to the previous channel (never the guide).
                        self.entry.clear();
                        let last = self.last_ch;
                        if last != 0 && last != self.cur && last < n as usize {
                            self.tune(last);
                        }
                        return;
                    }
                    KeyCode::Equal | KeyCode::NumpadAdd => {
                        self.set_volume(1);
                        return;
                    }
                    KeyCode::Minus | KeyCode::NumpadSubtract => {
                        self.set_volume(-1);
                        return;
                    }
                    KeyCode::KeyM => {
                        self.toggle_mute();
                        return;
                    }
                    KeyCode::KeyI => {
                        self.select();
                        return;
                    }
                    KeyCode::KeyG => {
                        self.entry.clear();
                        if self.cur == 0 && !self.guide_auto {
                            // Already in the interactive guide: G closes it.
                            let last = self.last_ch.max(1).min(n as usize - 1);
                            self.tune(last);
                        } else {
                            self.open_guide();
                        }
                        return;
                    }
                    KeyCode::KeyH | KeyCode::F1 => {
                        self.help = !self.help;
                        self.repaint();
                        return;
                    }
                    KeyCode::KeyC => {
                        self.settings.crt = !self.settings.crt;
                        self.settings.save();
                        self.repaint();
                        return;
                    }
                    KeyCode::KeyS => {
                        self.help = false;
                        self.menu = Some(0);
                        self.menu_zip = self.settings.weather_zip.clone();
                        self.repaint();
                        return;
                    }
                    KeyCode::Escape => {
                        if self.help {
                            self.help = false;
                            self.repaint();
                            return;
                        }
                        self.power_down();
                        return;
                    }
                    _ => return,
                };
                self.entry.clear();
                if self.cur == 0 && !self.guide_auto {
                    // Cursor on a list: Up is toward lower channel numbers (up the screen).
                    self.guide_move(-delta);
                    return;
                }
                let next = (self.cur as isize + delta).rem_euclid(n) as usize;
                self.tune(next);
            }
            _ => {}
        }
    }


    fn user_event(&mut self, el: &ActiveEventLoop, _: ()) {
        self.tick(el); // the decode thread delivered frames
    }

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        self.heartbeat.store(self.t0.elapsed().as_millis() as u64, Ordering::Relaxed);
        if !self.entry.is_empty() && self.entry_at.elapsed() > Duration::from_millis(1500) {
            self.commit_entry();
        }
        // ZIP lookup finished: point the weather page at the new place.
        if let Some(rx) = &self.geo_rx {
            if let Ok(r) = rx.try_recv() {
                self.geo_rx = None;
                match r {
                    Some((lat, lon, name)) => {
                        println!("weather: {} -> {name} ({lat:.4}, {lon:.4})", self.settings.weather_zip);
                        self.settings.weather_lat = lat;
                        self.settings.weather_lon = lon;
                        self.settings.weather_location = name;
                        self.settings.save();
                        if let Some(web) = &self.web {
                            self.web_navigated = true;
                            web.navigate(&self.settings.weather_page().unwrap_or_default());
                        }
                    }
                    None => eprintln!("weather: ZIP {} lookup failed; keeping {}", self.settings.weather_zip, self.settings.weather_location),
                }
                if self.menu.is_some() {
                    self.repaint();
                }
            }
        }
        // Preload the WeatherStar page once the local server has had a moment to start (and any
        // startup ZIP lookup is done).
        if !self.web_navigated && self.web.is_some() && self.geo_rx.is_none() && self.t0.elapsed() > Duration::from_millis(2500) {
            self.web_navigated = true;
            self.web.as_ref().unwrap().navigate(&self.settings.weather_page().unwrap_or_default());
        }
        if self.cur == 0 {
            let n = self.shared.slots.len() as i64 - 1;
            if self.guide_auto {
                if self.preview_at.elapsed() > PREVIEW_EVERY {
                    // Guide: rotate the ad box through the channels (the weather channel has no video).
                    let mut p = self.preview;
                    for _ in 0..n.max(1) {
                        p = if p as i64 + 1 > n { 1 } else { p + 1 };
                        if !self.shared.scheds[p].programs.is_empty() {
                            break;
                        }
                    }
                    self.preview = p;
                    self.tune(0);
                }
                if n > 0 && self.guide_step_at.elapsed() >= Duration::from_secs(2) {
                    self.guide_top = (self.guide_top + 1).rem_euclid(n);
                    self.guide_step_at = Instant::now();
                }
            } else {
                // Interactive guide: the ad box follows the cursor once it has rested for a moment.
                if self.preview != self.guide_sel && !self.shared.scheds[self.guide_sel].programs.is_empty() && self.guide_nav_at.elapsed() > Duration::from_millis(350) {
                    self.preview = self.guide_sel;
                    self.tune(0);
                }
            }
        }
        if self.shot_at.is_some_and(|t| Instant::now() >= t) {
            self.shot_at = None;
            self.dump_next = true;
            if let Some(f) = self.last.take() {
                self.present(&f);
            }
            self.print_stats();
            el.exit();
            return;
        }
        if self.bench_left > 0 && self.window.is_some() && Instant::now() >= self.bench_next {
            self.bench_left -= 1;
            if self.bench_left == 0 {
                self.dump_next = true;
            }
            self.bench_rng = self.bench_rng.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let next = ((self.bench_rng >> 33) % self.shared.slots.len() as u64) as usize;
            self.tune(next);
            if self.bench_left == 0 {
                // Let the last tune land before reporting.
                self.shot_at = Some(Instant::now() + Duration::from_millis(300));
            }
            self.bench_next = Instant::now() + Duration::from_millis(33);
        }
        self.tick(el);
    }
}

fn digit_of(code: KeyCode) -> Option<char> {
    Some(match code {
        KeyCode::Digit0 | KeyCode::Numpad0 => '0',
        KeyCode::Digit1 | KeyCode::Numpad1 => '1',
        KeyCode::Digit2 | KeyCode::Numpad2 => '2',
        KeyCode::Digit3 | KeyCode::Numpad3 => '3',
        KeyCode::Digit4 | KeyCode::Numpad4 => '4',
        KeyCode::Digit5 | KeyCode::Numpad5 => '5',
        KeyCode::Digit6 | KeyCode::Numpad6 => '6',
        KeyCode::Digit7 | KeyCode::Numpad7 => '7',
        KeyCode::Digit8 | KeyCode::Numpad8 => '8',
        KeyCode::Digit9 | KeyCode::Numpad9 => '9',
        _ => return None,
    })
}

fn arg_num(args: &[String], flag: &str, default: u32) -> u32 {
    args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).and_then(|n| n.parse().ok()).unwrap_or(default)
}

// ---------------------------------------------------------------- decode thread

// Music holds ffmpeg contexts; it is only ever used from the decode thread.
unsafe impl Send for Music {}

/// Move audio from the pipeline into the device ring, keeping it AUDIO_LEAD ahead of `offset`.
fn feed_audio(p: &mut Pipeline, ao: &AudioOut, offset: f64) {
    let (frame, per_sec) = match &p.audio {
        Some(a) => (a.out_channels as usize, (a.out_rate * a.out_channels) as usize),
        None => return,
    };
    if p.aq_start.is_nan() {
        return;
    }
    p.trim_audio(offset);
    let mut ring = ao.ring.lock().unwrap();
    // The ring ends at aq_start (everything before it has been pushed); fill up to offset + lead.
    let want = ((offset + AUDIO_LEAD) - p.aq_start).max(0.0);
    let n = ((want * per_sec as f64) as usize / frame * frame).min(p.aq.len());
    if n > 0 {
        ring.extend(p.aq.drain(..n));
        p.aq_start += n as f64 / per_sec as f64;
    }
    if ring.len() > per_sec / 2 {
        let over = ring.len() - per_sec / 4;
        ring.drain(..over); // device clock drift: hard resync
    }
}

/// Swap the decode thread's active pipeline to channel `new_src`: hand the old one back to its
/// slot, take the new one (cold open if the worker had not got to it), ship the first frame.
#[allow(clippy::too_many_arguments)]
fn switch(
    active: &mut Option<Pipeline>,
    cur_src: &mut usize,
    new_src: usize,
    gen: u64,
    sh: &Shared,
    tx: &std::sync::mpsc::Sender<Msg>,
    proxy: &winit::event_loop::EventLoopProxy<()>,
    next: &mut Option<Pipeline>,
) {
    if let Some(p) = active.take() {
        *sh.slots[*cur_src].lock().unwrap() = Some(p);
    }
    *cur_src = new_src;
    sh.active.store(new_src, Ordering::Relaxed);
    let (prog, offset) = sh.scheds[new_src].at(new_src, now_epoch());
    let file = sh.scheds[new_src].programs[prog].file;
    let tl = Instant::now();
    let mut p = sh.slots[new_src].lock().unwrap().take();
    let lock_ms = tl.elapsed().as_secs_f64() * 1e3;
    let mut cold = false;
    if p.as_ref().is_none_or(|p| p.file != file) {
        // Program rollover: use the next program we pre-opened, if it is the right one.
        if next.as_ref().is_some_and(|n| n.file == file) {
            if let Some(old) = p.take() {
                *sh.slots[new_src].lock().unwrap() = Some(old); // give the finished one back; the worker will retarget it
            }
            p = next.take();
        }
    }
    if p.as_ref().is_none_or(|p| p.file != file) {
        cold = true;
        p = Pipeline::open(file, &sh.files[file].path, sh.audio.as_deref(), sh.hw.as_ref()).ok();
    }
    let Some(mut p) = p else { return };
    let hit = !p.vq.is_empty();
    if !hit {
        p.seek(offset);
        while p.vq.is_empty() && p.pump() {}
    }
    p.trim_audio(offset);
    let name = Path::new(&sh.files[file].path).file_name().unwrap().to_string_lossy().into_owned();
    let _ = tx.send(Msg::Tuned { gen, info: TuneInfo { name, hit, cold, lock_ms } });
    if let Some(f) = p.vq.pop_front() {
        let pts = p.pts_secs(&f);
        let _ = tx.send(Msg::Frame { gen, pts, frame: SendFrame(f) });
        let _ = proxy.send_event(());
    }
    *active = Some(p);
}

/// Weather channel: hand the active pipeline back to its slot and go quiet.
fn park(active: &mut Option<Pipeline>, src: usize, sh: &Shared) {
    if let Some(p) = active.take() {
        if src < sh.slots.len() {
            *sh.slots[src].lock().unwrap() = Some(p);
        }
    }
    sh.active.store(usize::MAX, Ordering::Relaxed);
    if let Some(ao) = &sh.audio {
        ao.ring.lock().unwrap().clear();
    }
}

/// Owns the active pipeline. Every read, decode and audio push happens here, so the UI thread
/// never blocks on I/O. Frames due within the next 150 ms are shipped to the UI as they decode.
fn decode_thread(
    sh: Arc<Shared>,
    rx: std::sync::mpsc::Receiver<Cmd>,
    tx: std::sync::mpsc::Sender<Msg>,
    proxy: winit::event_loop::EventLoopProxy<()>,
    mut music: Music,
) {
    let mut active: Option<Pipeline> = None;
    let mut src = usize::MAX;
    let mut gen = 0u64;
    let mut guide = false;
    // The next program on the active channel, opened ahead of time on a helper thread.
    let mut next: Option<Pipeline> = None;
    let mut next_rx: Option<std::sync::mpsc::Receiver<Option<Pipeline>>> = None;
    loop {
        let cmd = if active.is_none() {
            match rx.recv() {
                Ok(c) => Some(c),
                Err(_) => return,
            }
        } else {
            match rx.recv_timeout(Duration::from_millis(3)) {
                Ok(c) => Some(c),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => None,
                Err(_) => return,
            }
        };
        match cmd {
            Some(Cmd::Tune { src: s, gen: g, guide: gd }) => {
                let (mut s, mut g, mut gd) = (s, g, gd);
                let mut stop = false;
                loop {
                    match rx.try_recv() {
                        Ok(Cmd::Tune { src, gen, guide }) => (s, g, gd, stop) = (src, gen, guide, false), // only the latest keypress matters
                        Ok(Cmd::Music(m)) => gd = m,
                        Ok(Cmd::Stop) => stop = true,
                        Err(_) => break,
                    }
                }
                gen = g;
                guide = gd;
                if stop {
                    park(&mut active, src, &sh);
                } else {
                    switch(&mut active, &mut src, s, gen, &sh, &tx, &proxy, &mut next);
                }
            }
            Some(Cmd::Music(m)) => {
                guide = m;
                if let Some(ao) = &sh.audio {
                    ao.ring.lock().unwrap().clear(); // drop the queued music / channel audio
                }
            }
            Some(Cmd::Stop) => park(&mut active, src, &sh),
            None => {}
        }
        if active.is_none() {
            continue;
        }
        let (prog, offset) = sh.scheds[src].at(src, now_epoch());
        let rollover = {
            let p = active.as_ref().unwrap();
            sh.scheds[src].programs[prog].file != p.file
                || (p.eof && p.vq.is_empty())
                || p.vq.front().is_some_and(|f| p.pts_secs(f) - offset > 1.0)
        };
        if rollover {
            let s = src;
            switch(&mut active, &mut src, s, gen, &sh, &tx, &proxy, &mut next);
            continue;
        }
        let p = active.as_mut().unwrap();
        // Read ahead until video and audio both cover the near future (bounded per pass).
        let horizon = offset + AUDIO_LEAD + 0.15;
        let mut n = 0;
        while (p.video_horizon() < horizon || p.audio_horizon() < horizon) && n < 8 && p.pump() {
            n += 1;
        }
        p.trim_video(offset);
        // Pre-open the next program ~3 s before this one ends, on a helper thread: an SMB open
        // plus decoder creation would otherwise stall decoding at every program boundary.
        let sched = &sh.scheds[src];
        let remaining = sh.files[p.file].duration - offset;
        let next_file = sched.programs[(prog + 1) % sched.programs.len()].file;
        if remaining < 3.0 && next_rx.is_none() && next.as_ref().is_none_or(|n| n.file != next_file) {
            let (ntx, nrx) = std::sync::mpsc::channel();
            next_rx = Some(nrx);
            let sh2 = sh.clone();
            std::thread::spawn(move || {
                let mut np = Pipeline::open(next_file, &sh2.files[next_file].path, sh2.audio.as_deref(), sh2.hw.as_ref()).ok();
                if let Some(p) = np.as_mut() {
                    p.seek(0.0);
                    while p.vq.is_empty() && p.pump() {}
                }
                let _ = ntx.send(np);
            });
        }
        if let Some(rx) = &next_rx {
            if let Ok(r) = rx.try_recv() {
                next = r;
                next_rx = None;
            }
        }
        // Ship frames that fall due within the next 150 ms; the UI paces and presents them.
        let mut shipped = false;
        while p.vq.front().is_some_and(|f| p.pts_secs(f) <= offset + 0.15) {
            let f = p.vq.pop_front().unwrap();
            let pts = p.pts_secs(&f);
            let _ = tx.send(Msg::Frame { gen, pts, frame: SendFrame(f) });
            shipped = true;
        }
        if shipped {
            let _ = proxy.send_event(());
        }
        if let Some(ao) = &sh.audio {
            if guide {
                music.feed(ao); // guide: music instead of the preview's audio
                if music.changed {
                    music.changed = false;
                    let _ = tx.send(Msg::Music(music.now_playing.clone()));
                }
            } else {
                feed_audio(p, ao, offset);
            }
        }
    }
}

// ---------------------------------------------------------------- boot (loading screen) + main

/// While the UI loop is stuck in Windows' modal move/size loop its heartbeat goes stale; asking
/// for a repaint from another thread still gets WM_PAINT delivered there, and the RedrawRequested
/// handlers draw (Boot) or tick (App). Idle otherwise: the loop normally ticks every few ms.
#[cfg(not(windows))]
fn spawn_drag_watchdog(_window: &Window, _heartbeat: Arc<AtomicU64>, _t0: Instant, _alive: Arc<AtomicBool>) {
    // X11 / Wayland don't park the event loop while a window is dragged.
}

#[cfg(windows)]
fn spawn_drag_watchdog(window: &Window, heartbeat: Arc<AtomicU64>, t0: Instant, alive: Arc<AtomicBool>) {
    let Some(hwnd) = hwnd_of(window) else { return };
    let raw = hwnd.0 as isize;
    std::thread::spawn(move || {
        while alive.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(16));
            let now = t0.elapsed().as_millis() as u64;
            if now.saturating_sub(heartbeat.load(Ordering::Relaxed)) > 40 {
                unsafe {
                    let _ = windows::Win32::Graphics::Gdi::InvalidateRect(Some(HWND(raw as *mut c_void)), None, false);
                }
            }
        }
    });
}

#[cfg(windows)]
fn hwnd_of(window: &Window) -> Option<HWND> {
    match window.window_handle().map(|h| h.as_raw()) {
        Ok(RawWindowHandle::Win32(h)) => Some(HWND(h.hwnd.get() as *mut c_void)),
        _ => None,
    }
}

/// Everything the loader thread produces once the library is indexed.
struct Loaded {
    shared: Arc<Shared>,
    music: Music,
    nchan: usize,
    weather: Option<usize>,
}

enum Progress {
    Status(String),
    Done(Box<Loaded>),
    /// (message, offer to open the set up tool)
    Fail(String, bool),
}

/// Inputs for the loader thread.
struct LoadJob {
    args: Vec<String>,
    settings: Settings,
    audio: Option<Arc<AudioOut>>,
    hw: Option<HwDev>,
}

/// Index the library (and idents, lineup folders), load logos, build the schedule. Slow on a
/// network share, so it runs on a thread while the loading screen shows; `status` lines land there.
fn load_everything(job: LoadJob, tx: &std::sync::mpsc::Sender<Progress>) -> Result<Loaded, (String, bool)> {
    let status = |t: String| {
        println!("{t}");
        let _ = tx.send(Progress::Status(t.to_uppercase()));
    };
    let LoadJob { args, settings, audio, hw } = job;
    let music_dir = args.iter().position(|a| a == "--music").and_then(|i| args.get(i + 1)).cloned().unwrap_or_else(|| settings.music_dir.clone());
    let lineup = Lineup::load();
    // Enough channels for the setting / flag and for every lineup entry.
    let nchan = (arg_num(&args, "--channels", settings.channels as u32) as usize).max(lineup.channels.iter().map(|c| c.number).max().unwrap_or(0)).max(1);
    // First bare argument (not a flag or a flag's value) is the content dir.
    let mut dir = settings.content_dir.clone();
    let mut i = 0;
    while i < args.len() {
        if args[i].starts_with("--") {
            i += if matches!(args[i].as_str(), "--crt" | "--sw") { 1 } else { 2 };
            continue;
        }
        dir = args[i].clone();
        i += 1;
    }
    let mut dir = PathBuf::from(dir);
    if !dir.is_dir() && dir.is_relative() {
        // A relative content folder is looked for next to the exe too.
        if let Some(exe_dir) = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf)) {
            let alt = exe_dir.join(&dir);
            if alt.is_dir() {
                dir = alt;
            }
        }
    }

    status(format!("indexing {}", dir.display()));
    let mut files = load_or_build_index(&dir, false);
    // Lineup channels with their own folders: indexed (recursively, cached per folder) and pinned
    // to that channel; those files are left out of the automatic deal.
    let mut manual: HashMap<usize, Manual> = HashMap::new();
    {
        let mut by_path: HashMap<String, usize> = files.iter().enumerate().map(|(i, f)| (f.path.to_lowercase(), i)).collect();
        for lc in lineup.channels.iter().filter(|c| c.number > 0 && !c.sources.is_empty()) {
            let mut idx = Vec::new();
            for src in &lc.sources {
                if !Path::new(src).is_dir() {
                    eprintln!("lineup: channel {} source {src} is not a folder", lc.number);
                    continue;
                }
                status(format!("indexing channel {} : {src}", lc.number));
                for f in load_or_build_index(Path::new(src), true) {
                    let key = f.path.to_lowercase();
                    let i = match by_path.get(&key) {
                        Some(&i) => i,
                        None => {
                            files.push(f);
                            by_path.insert(key, files.len() - 1);
                            files.len() - 1
                        }
                    };
                    idx.push(i);
                }
            }
            println!("lineup: channel {} \"{}\": {} files from {} folder(s)", lc.number, lc.name, idx.len(), lc.sources.len());
            manual.insert(lc.number, Manual { files: idx, shuffle: lc.shuffle, idents: lc.idents.clone() });
        }
    }
    if files.is_empty() {
        return Err((format!("No video files found in\n{}\n\nSet the content library folder in the set up tool. Open it now?", dir.display()), true));
    }
    // Station idents: one folder per network, appended to the library and flagged.
    if settings.bumpers && !settings.bumper_dir.is_empty() && Path::new(&settings.bumper_dir).is_dir() {
        status(format!("indexing idents in {}", settings.bumper_dir));
        let mut b = load_or_build_index(Path::new(&settings.bumper_dir), true);
        for f in &mut b {
            f.bumper = true;
        }
        println!("bumpers: {} idents from {}", b.len(), settings.bumper_dir);
        files.extend(b);
    }
    let mut bumper_names: Vec<String> = files
        .iter()
        .filter(|f| f.bumper)
        .filter_map(|f| Path::new(&f.path).parent()?.file_name().map(|s| clean_name(&s.to_string_lossy())))
        .collect();
    bumper_names.sort();
    bumper_names.dedup();
    status("loading network logos".into());
    let mut nets = load_networks(&settings.logo_dirs, &bumper_names);
    println!("networks: {} ({} with logos)", nets.len(), nets.iter().filter(|n| n.logo.is_some()).count());
    let mut styles = make_styles(nchan + 1, nets.len());
    // Lineup overrides: name / logo / dressing per channel.
    for lc in lineup.channels.iter().filter(|c| c.number > 0 && c.number <= nchan) {
        let ch = lc.number;
        let logo_idx = if lc.logo.is_empty() { None } else { nets.iter().position(|n| n.name.eq_ignore_ascii_case(&lc.logo)) };
        let named = !lc.name.is_empty() && !logo_idx.is_some_and(|i| nets[i].name.eq_ignore_ascii_case(&lc.name));
        if named {
            let logo = logo_idx.and_then(|i| nets[i].logo.clone());
            nets.push(Network { name: lc.name.clone(), logo });
            styles[ch].net = nets.len() - 1;
        } else if let Some(i) = logo_idx {
            styles[ch].net = i;
        }
        match lc.bug.as_str() {
            "off" => styles[ch].bug = 0,
            "br" => styles[ch].bug = 1,
            "bl" => styles[ch].bug = 2,
            "tl" => styles[ch].bug = 3,
            _ => {}
        }
        match lc.clock.as_str() {
            "on" => styles[ch].clock = true,
            "off" => styles[ch].clock = false,
            _ => {}
        }
        match lc.ticker.as_str() {
            "on" => styles[ch].ticker = true,
            "off" => styles[ch].ticker = false,
            _ => {}
        }
    }
    {
        let with_logo = |s: &ChanStyle| s.bug > 0 && nets[s.net].logo.is_some();
        let bugs: Vec<usize> = (1..=nchan).filter(|&c| with_logo(&styles[c])).collect();
        println!(
            "dressing: {} channels with a logo bug (first: {:?}), {} clocks, {} tickers",
            bugs.len(),
            bugs.first(),
            (1..=nchan).filter(|&c| styles[c].clock).count(),
            (1..=nchan).filter(|&c| styles[c].ticker).count()
        );
    }
    status(format!("building {} channels", nchan));
    // Channel 0 is the guide (no schedule); real channels are 1..=N; the weather page, if
    // configured, is N+1 (no schedule either).
    let mut scheds = build_schedules(&files, nchan, &styles, &nets, &manual);
    scheds.insert(0, Schedule { programs: Vec::new(), total: 0.0 });
    let weather = settings.weather_page().is_some().then(|| {
        scheds.push(Schedule { programs: Vec::new(), total: 0.0 });
        scheds.len() - 1
    });
    let total_h: f64 = scheds.iter().map(|c| c.total).sum::<f64>() / 3600.0;
    println!(
        "playout: {} files across {} channels (+ guide on 0), {:.1} h of programming, ~{:.0} min per channel loop",
        files.len(),
        scheds.len() - 1,
        total_h,
        scheds[1].total / 60.0
    );
    let mut music = Music::new(Some(Path::new(&music_dir)));
    music.open_random(); // first track ready before the guide is ever opened
    let slots: Vec<Slot> = (0..scheds.len()).map(|_| Arc::new(Mutex::new(None))).collect();
    let shared = Arc::new(Shared { files, scheds, slots, audio, hw, active: AtomicUsize::new(usize::MAX), stop: AtomicBool::new(false), nets, styles, weather });
    status("priming channels".into());
    spawn_workers(&shared, 4);
    Ok(Loaded { shared, music, nchan, weather })
}

/// Per-launch options that outlive the loading screen and move into the App.
struct RunArgs {
    bench: u32,
    start_ch: usize,
    shot_ms: u32,
    audio_stream: Option<cpal::Stream>,
    node: Option<std::process::Child>,
    proxy: winit::event_loop::EventLoopProxy<()>,
}

/// The loading screen: window + presenter up at once, "PLEASE STAND BY" over faint snow while
/// the loader thread indexes. Hands the window, presenter and settings to the App when done.
struct Boot {
    #[cfg(d3d)]
    device: ID3D11Device,
    settings: Settings,
    window: Option<Arc<Window>>,
    gpu: Option<gpu::Gpu>,
    osd: Osd,
    overlay: Vec<u32>,
    t0: Instant,
    last_paint: Instant,
    status: String,
    rx: std::sync::mpsc::Receiver<Progress>,
    noise_rng: u64,
    done: Option<Box<Loaded>>,
    run: Option<RunArgs>,
    heartbeat: Arc<AtomicU64>, // same drag watchdog as App (see App::heartbeat)
    alive: Arc<AtomicBool>,    // cleared when the App takes over, stopping the watchdog
}

impl Boot {
    fn present(&mut self) {
        let (Some(window), Some(gpu)) = (&self.window, &mut self.gpu) else { return };
        let size = window.inner_size();
        let (w, h) = (size.width, size.height);
        if w == 0 || h == 0 || gpu.resize(w, h).is_err() {
            return;
        }
        if self.overlay.len() != (w * h) as usize {
            self.overlay = vec![0; (w * h) as usize];
        }
        let ov = &mut self.overlay;
        ov.fill(0);
        self.osd.hud = self.settings.hud_color();
        if w >= 320 && h >= 200 {
            let (wf, hf) = (w as f32, h as f32);
            let big = (hf * 0.11) as u32;
            let tw = self.osd.width("PLEASE STAND BY", big);
            let cx = ((wf - tw) / 2.0) as i32;
            self.osd.text(ov, w, h, "PLEASE STAND BY", big, cx, (hf * 0.47) as i32, self.osd.hud, false, NOCLIP);
            let small = (hf * 0.04) as u32;
            let dots = ".".repeat(((self.t0.elapsed().as_millis() / 400) % 4) as usize);
            let line = format!("{}{dots}", self.status);
            self.osd.text(ov, w, h, &line, small, (wf * 0.08) as i32, (hf * 0.88) as i32, GUIDE_GRAY, false, NOCLIP);
            self.osd.text(ov, w, h, "TUNER", small, (wf * 0.92) as i32, (hf * 0.88) as i32, GUIDE_GRAY, true, NOCLIP);
        }
        let _ = gpu.upload_overlay(&self.overlay);
        self.noise_rng = self.noise_rng.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        let t = self.t0.elapsed().as_secs_f32();
        let s = &self.settings;
        let fx = gpu::Fx {
            snow: 0.22,
            snow_full: true,
            seed: (self.noise_rng >> 44) as f32 / 1000.0,
            crt: s.crt,
            crt_params: gpu::CrtParams { curve: s.crt_curve, scan: s.crt_scan, noise: s.crt_noise, vignette: s.crt_vignette },
            time: t,
            power: (t / POWER_ON_SECS).min(1.0),
            settle: 0.0,
        };
        let _ = gpu.render(None, [0.0, 0.0, 1.0, 1.0], fx);
        self.last_paint = Instant::now();
    }

    /// Drain loader progress. Exits the loop on failure.
    fn poll(&mut self, el: &ActiveEventLoop) {
        loop {
            match self.rx.try_recv() {
                Ok(Progress::Status(s)) => self.status = s,
                Ok(Progress::Done(l)) => self.done = Some(l),
                Ok(Progress::Fail(text, offer)) => {
                    if tuner::message_box("tuner", &text, offer) && offer {
                        let _ = tuner::launch_sibling("tuner-setup.exe");
                    }
                    el.exit();
                    return;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => return,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    if self.done.is_none() {
                        el.exit(); // the loader died; its panic hook already showed why
                    }
                    return;
                }
            }
        }
    }
}

impl ApplicationHandler for Boot {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = Window::default_attributes().with_title("TUNER").with_window_icon(tuner::window_icon()).with_inner_size(winit::dpi::LogicalSize::new(1280, 720));
        let window = Arc::new(el.create_window(attrs).expect("window"));
        let size = window.inner_size();
        #[cfg(d3d)]
        let gpu = gpu::Gpu::new(self.device.clone(), hwnd_of(&window).expect("not a Win32 window"), size.width.max(1), size.height.max(1)).expect("d3d11 presenter");
        #[cfg(not(d3d))]
        let gpu = gpu::Gpu::new(window.clone(), size.width.max(1), size.height.max(1)).expect("presenter");
        self.gpu = Some(gpu);
        spawn_drag_watchdog(&window, self.heartbeat.clone(), self.t0, self.alive.clone());
        self.window = Some(window);
        self.present();
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => el.exit(),
            WindowEvent::Resized(_) => self.present(),
            WindowEvent::RedrawRequested => {
                self.poll(el);
                self.present();
            }
            WindowEvent::KeyboardInput { event: KeyEvent { physical_key: PhysicalKey::Code(KeyCode::Escape), state: ElementState::Pressed, .. }, .. } => el.exit(),
            _ => {}
        }
    }

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        self.heartbeat.store(self.t0.elapsed().as_millis() as u64, Ordering::Relaxed);
        self.poll(el);
        // --shot while still loading: dump the loading screen itself (headless checks).
        if let Some(ms) = self.run.as_ref().map(|r| r.shot_ms).filter(|&ms| ms > 0) {
            if self.done.is_none() && self.t0.elapsed() >= Duration::from_millis(ms as u64) {
                self.present();
                if let Some(Ok((dw, dh, bgra))) = self.gpu.as_ref().map(|g| g.read_back()) {
                    let mut ppm = format!("P6
{dw} {dh}
255
").into_bytes();
                    for p in bgra.chunks_exact(4) {
                        ppm.extend_from_slice(&[p[2], p[1], p[0]]);
                    }
                    let _ = std::fs::write("bench_last.ppm", ppm);
                }
                el.exit();
                return;
            }
        }
        if self.done.is_none() {
            if self.last_paint.elapsed() >= Duration::from_millis(33) {
                self.present();
            }
            el.set_control_flow(ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(16)));
        }
    }
}

impl App {
    /// Take over the window and presenter from the loading screen.
    fn from_boot(b: Boot, loaded: Loaded) -> App {
        let Loaded { shared, music, nchan, weather } = loaded;
        b.alive.store(false, Ordering::Relaxed); // the App starts its own watchdog
        let run = b.run.expect("run args");
        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<Cmd>();
        let (msg_tx, msg_rx) = std::sync::mpsc::channel::<Msg>();
        let proxy = run.proxy.clone();
        {
            let sh = shared.clone();
            std::thread::spawn(move || decode_thread(sh, cmd_rx, msg_tx, proxy, music));
        }
        let settings = b.settings;
        let now = Instant::now();
        App {
            window: b.window,
            gpu: b.gpu,
            overlay: b.overlay,
            overlay_live: true,
            guide_snow_full: false,
            last_paint: now,
            menu: None,
            menu_zip: settings.weather_zip.clone(),
            geo_rx: (!settings.weather_zip.is_empty() && weather.is_some()).then(|| start_geocode(&settings.weather_zip)),
            settings,
            web: None,
            web_navigated: false,
            bug_cache: None,
            t0: b.t0, // the tube warm-up continues from the loading screen
            music_now: String::new(),
            shared: shared.clone(),
            _audio_stream: run.audio_stream,
            cmd: cmd_tx,
            rx: msg_rx,
            cur: 1,
            preview: 1,
            gen: 0,
            tune_t0: now,
            tuned: None,
            presented_gen: 0,
            vq: VecDeque::new(),
            last: None,
            stats: Vec::new(),
            frames_presented: 0,
            frames_dropped: 0,
            osd: b.osd,
            ready_at: now - Duration::from_secs(10),
            noise_rng: b.noise_rng,
            entry: String::new(),
            entry_at: now,
            guide_t0: now,
            preview_at: now,
            guide_sel: 1,
            guide_top: 0,
            guide_step_at: now,
            guide_nav_at: now,
            guide_auto: true,
            last_ch: run.start_ch.clamp(1, nchan),
            vol_until: now,
            help: false,
            power_off: None,
            node: run.node,
            started: false,
            heartbeat: Arc::new(AtomicU64::new(0)),
            bench_left: run.bench,
            bench_next: now + Duration::from_millis(1500),
            bench_rng: 0x9E3779B97F4A7C15,
            dump_next: false,
            start_ch: run.start_ch.min(shared.scheds.len() - 1),
            shot_at: (run.shot_ms > 0).then(|| now + Duration::from_millis(run.shot_ms as u64)),
        }
    }
}

/// A child console process gets no window of its own (we have no console to share).
#[cfg(windows)]
fn hide_console(cmd: &mut std::process::Command) {
    use std::os::windows::process::CommandExt;
    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
}
#[cfg(not(windows))]
fn hide_console(_cmd: &mut std::process::Command) {}

/// Loading screen first, then the tuner, in one event loop.
enum Stage {
    Boot(Boot),
    Run(App),
    Switching,
}

impl ApplicationHandler for Stage {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        match self {
            Stage::Boot(b) => b.resumed(el),
            Stage::Run(a) => a.resumed(el),
            Stage::Switching => {}
        }
    }

    fn window_event(&mut self, el: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        match self {
            Stage::Boot(b) => b.window_event(el, id, event),
            Stage::Run(a) => a.window_event(el, id, event),
            Stage::Switching => {}
        }
    }

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        if let Stage::Boot(b) = self {
            b.about_to_wait(el);
            if b.done.is_some() {
                let Stage::Boot(mut b) = std::mem::replace(self, Stage::Switching) else { unreachable!() };
                let loaded = *b.done.take().unwrap();
                *self = Stage::Run(App::from_boot(b, loaded));
                if let Stage::Run(a) = self {
                    a.resumed(el);
                    a.about_to_wait(el);
                }
            }
            return;
        }
        if let Stage::Run(a) = self {
            a.about_to_wait(el);
        }
    }
}

fn main() {
    ff::init().expect("ffmpeg init");
    ff::log::set_level(ff::log::Level::Error);
    tuner::use_parent_console();
    tuner::install_panic_hook("tuner");
    let args: Vec<String> = std::env::args().skip(1).collect();
    // Plain launches are single-instance; test/bench runs with arguments may overlap a session.
    if args.is_empty() && !tuner::single_instance("tuner") {
        return;
    }
    // First run (no settings file yet): hand over to the set up wizard instead of guessing.
    if args.is_empty() && !Settings::path().exists() {
        if let Err(e) = tuner::launch_sibling_with("tuner-setup.exe", &["--wizard"]) {
            tuner::message_box("tuner", &format!("No settings yet, and tuner-setup.exe could not be started: {e}"), false);
        }
        return;
    }
    let bench = arg_num(&args, "--bench", 0);
    let start_ch = arg_num(&args, "--start", 1) as usize;
    let shot_ms = arg_num(&args, "--shot", 0);
    let mut settings = Settings::load();
    if args.iter().any(|a| a == "--crt") {
        settings.crt = true;
    }
    // Start the local WeatherStar server first: it needs a second or two to come up.
    let mut node = None;
    if settings.weather_page().is_some() && !settings.weather_dir.is_empty() && Path::new(&settings.weather_dir).join("index.mjs").is_file() {
        let port = settings.weather_url.rsplit(':').next().and_then(|p| p.split('/').next()).and_then(|p| p.parse::<u16>().ok()).unwrap_or(8083);
        let mut cmd = std::process::Command::new("node");
        cmd.arg("index.mjs").current_dir(&settings.weather_dir).env("WS3KP_PORT", port.to_string()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
        hide_console(&mut cmd);
        match cmd.spawn() {
            Ok(c) => {
                println!("weather: ws3kp on port {port} from {}", settings.weather_dir);
                node = Some(c);
            }
            Err(e) => eprintln!("weather: could not start node ({e}); using {} as is", settings.weather_url),
        }
    }
    let (audio, audio_stream) = match start_audio() {
        Some((a, s)) => (Some(a), Some(s)),
        None => {
            eprintln!("audio: no output device, running silent");
            (None, None)
        }
    };
    // One D3D11 device for everything: ffmpeg's D3D11VA decoders and our presenter. `--sw` forces
    // software decode (the presenter then uploads yuv420p planes instead).
    let hw = if args.iter().any(|a| a == "--sw") { None } else { HwDev::create() };
    #[cfg(d3d)]
    let device = match &hw {
        Some(h) => unsafe { gpu::Gpu::device_from_raw(h.d3d_device()) },
        None => gpu::Gpu::create_device().expect("d3d11 device"),
    };
    println!("video: {}", if hw.is_some() { "D3D11VA hardware decode" } else { "software decode" });

    // The slow part (indexing over a network share can take a while) runs behind the loading screen.
    let (tx, rx) = std::sync::mpsc::channel::<Progress>();
    {
        let job = LoadJob { args: args.clone(), settings: settings.clone(), audio, hw };
        std::thread::spawn(move || {
            let r = match load_everything(job, &tx) {
                Ok(l) => Progress::Done(Box::new(l)),
                Err((m, offer)) => Progress::Fail(m, offer),
            };
            let _ = tx.send(r);
        });
    }
    let event_loop = EventLoop::<()>::with_user_event().build().unwrap();
    let proxy = event_loop.create_proxy();
    let mut stage = Stage::Boot(Boot {
        #[cfg(d3d)]
        device,
        settings,
        window: None,
        gpu: None,
        osd: Osd::new(),
        overlay: Vec::new(),
        t0: Instant::now(),
        last_paint: Instant::now(),
        status: "STARTING".into(),
        rx,
        noise_rng: 0x2545F4914F6CDD1D,
        done: None,
        run: Some(RunArgs { bench, start_ch, shot_ms, audio_stream, node, proxy }),
        heartbeat: Arc::new(AtomicU64::new(0)),
        alive: Arc::new(AtomicBool::new(true)),
    });
    event_loop.run_app(&mut stage).unwrap();
    match &mut stage {
        Stage::Run(a) => {
            a.shared.stop.store(true, Ordering::Relaxed);
            if let Some(c) = &mut a.node {
                let _ = c.kill();
            }
        }
        Stage::Boot(b) => {
            if let Some(c) = b.run.as_mut().and_then(|r| r.node.as_mut()) {
                let _ = c.kill();
            }
        }
        Stage::Switching => {}
    }
}
