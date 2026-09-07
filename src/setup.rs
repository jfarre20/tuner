//! tuner-setup: a windowed, mouse-driven editor for tuner_settings.json and tuner_lineup.json.
//! The UI is an HTML page (assets/setup.html) in a WebView2; this side owns the files, the native
//! folder dialog and launching the tuner.
#![cfg_attr(windows, windows_subsystem = "windows")]

use std::rc::Rc;
use std::sync::Arc;
use tuner::settings::{Lineup, Settings};
use tuner::web::Web;
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::window::{Window, WindowId};

const PAGE: &str = include_str!("../assets/setup.html");

struct App {
    window: Option<Arc<Window>>,
    web: Option<Rc<Web>>,
    wizard: bool, // first run (no settings file) or --wizard: step-by-step mode
}

/// Native "pick a folder" dialog. None when cancelled.
#[cfg(not(windows))]
fn pick_folder(_owner: &Window, _start: &str) -> Option<String> {
    None
}

#[cfg(windows)]
fn pick_folder(owner: &Window, start: &str) -> Option<String> {
    use windows::core::HSTRING;
    use windows::Win32::Foundation::HWND;
    use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};
    use windows::Win32::UI::Shell::{FileOpenDialog, IFileOpenDialog, IShellItem, FOS_PICKFOLDERS, SIGDN_FILESYSPATH};
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let owner = match owner.window_handle().map(|h| h.as_raw()) {
        Ok(RawWindowHandle::Win32(h)) => HWND(h.hwnd.get() as *mut std::ffi::c_void),
        _ => return None,
    };
    unsafe {
        let dlg: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;
        let opts = dlg.GetOptions().ok()?;
        dlg.SetOptions(opts | FOS_PICKFOLDERS).ok()?;
        if !start.is_empty() && std::path::Path::new(start).is_dir() {
            if let Ok(item) = windows::Win32::UI::Shell::SHCreateItemFromParsingName::<_, Option<&windows::Win32::System::Com::IBindCtx>, IShellItem>(&HSTRING::from(start), None) {
                let _ = dlg.SetFolder(&item);
            }
        }
        dlg.Show(Some(owner)).ok()?;
        let item = dlg.GetResult().ok()?;
        let p = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
        Some(p.to_string().ok()?)
    }
}

fn state_json(s: &Settings, l: &Lineup, wizard: bool) -> String {
    serde_json::json!({
        "type": "state",
        "wizard": wizard,
        "settings": s,
        "lineup": l,
        "logos": s.logo_names(),
        "dir": Settings::path().parent().map(|p| p.display().to_string()).unwrap_or_default(),
    })
    .to_string()
}

fn handle(msg: &str, web: &Web, window: &Window, wizard: bool) {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(msg) else { return };
    match v.get("cmd").and_then(|c| c.as_str()).unwrap_or("") {
        "load" => web.post(&state_json(&Settings::load(), &Lineup::load(), wizard)),
        "save" | "launch" => {
            let parsed = (|| -> Result<(Settings, Lineup), String> {
                let s: Settings = serde_json::from_value(v.get("settings").cloned().unwrap_or_default()).map_err(|e| format!("settings: {e}"))?;
                let l: Lineup = serde_json::from_value(v.get("lineup").cloned().unwrap_or_default()).map_err(|e| format!("lineup: {e}"))?;
                Ok((s, l))
            })();
            let (s, l) = match parsed {
                Ok(x) => x,
                Err(e) => {
                    web.post(&serde_json::json!({ "type": "saved", "ok": false, "error": e }).to_string());
                    return;
                }
            };
            s.save();
            let r = l.save();
            let ok = r.is_ok();
            web.post(&serde_json::json!({ "type": "saved", "ok": ok, "error": r.err().map(|e| e.to_string()) }).to_string());
            if ok && v["cmd"] == "launch" {
                match tuner::launch_sibling("tuner.exe") {
                    Ok(()) => web.post(&serde_json::json!({ "type": "launched" }).to_string()),
                    Err(e) => web.post(&serde_json::json!({ "type": "error", "text": format!("could not start tuner.exe: {e}") }).to_string()),
                }
            }
        }
        "quit" => std::process::exit(0), // wizard finished and launched the tuner
        "browse" => {
            let field = v.get("field").and_then(|f| f.as_str()).unwrap_or("").to_string();
            let start = v.get("start").and_then(|f| f.as_str()).unwrap_or("");
            if let Some(path) = pick_folder(window, start) {
                web.post(&serde_json::json!({ "type": "picked", "field": field, "path": path }).to_string());
            }
        }
        _ => {}
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = Window::default_attributes().with_title("TUNER set up").with_window_icon(tuner::window_icon()).with_inner_size(winit::dpi::LogicalSize::new(1040, 780));
        let window = Arc::new(el.create_window(attrs).expect("window"));
        let web = match Web::new(&window) {
            Ok(w) => w,
            Err(e) => {
                tuner::message_box("tuner set up", &format!("The WebView2 runtime is required (it ships with Microsoft Edge).\n\n{e}"), false);
                el.exit();
                return;
            }
        };
        let size = window.inner_size();
        web.set(Some((size.width, size.height)));
        web.navigate_html(PAGE);
        let web = Rc::new(web);
        let w2 = web.clone(); // the handler keeps the control alive; fine for a tool that lives as long as the window
        let wizard = self.wizard;
        let win = window.clone();
        web.on_message(move |msg| handle(&msg, &w2, &win, wizard));
        self.web = Some(web);
        self.window = Some(window);
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => el.exit(),
            WindowEvent::Resized(size) => {
                if let Some(web) = &self.web {
                    web.set(Some((size.width.max(1), size.height.max(1))));
                }
            }
            _ => {}
        }
    }
}

fn main() {
    tuner::use_parent_console();
    tuner::install_panic_hook("tuner-setup");
    #[cfg(windows)]
    unsafe {
        use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }
    // Decide before Settings::load() runs (it writes the file).
    let wizard = std::env::args().any(|a| a == "--wizard") || !Settings::path().exists();
    let event_loop = EventLoop::new().expect("event loop");
    let mut app = App { window: None, web: None, wizard };
    event_loop.run_app(&mut app).expect("run");
}
