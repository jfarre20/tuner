//! Shared between the tuner and its setup tool: settings, channel lineup, WebView2 host.
pub mod settings;
#[cfg(windows)]
pub mod web;
/// No WebView2 off Windows: the weather channel and the setup tool's page are unavailable there.
#[cfg(not(windows))]
pub mod web {
    pub struct Web;
    impl Web {
        pub fn new(_window: &winit::window::Window) -> Result<Web, String> {
            Err("web view not available on this platform".into())
        }
        pub fn navigate(&self, _url: &str) {}
        pub fn navigate_html(&self, _html: &str) {}
        pub fn on_message(&self, _f: impl Fn(String) + 'static) {}
        pub fn post(&self, _json: &str) {}
        pub fn set(&self, _size: Option<(u32, u32)>) {}
    }
}

/// Modal error box (GUI exes have no console to complain in).
#[cfg(not(windows))]
pub fn message_box(title: &str, text: &str, _question: bool) -> bool {
    eprintln!("{title}: {text}");
    false
}

#[cfg(windows)]
pub fn message_box(title: &str, text: &str, question: bool) -> bool {
    use windows::core::HSTRING;
    use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, IDYES, MB_ICONERROR, MB_ICONQUESTION, MB_OK, MB_YESNO};
    unsafe {
        let style = if question { MB_YESNO | MB_ICONQUESTION } else { MB_OK | MB_ICONERROR };
        MessageBoxW(None, &HSTRING::from(text), &HSTRING::from(title), style) == IDYES
    }
}

/// Panics in a windowed exe would otherwise vanish: log them next to the exe and show them.
pub fn install_panic_hook(app: &'static str) {
    std::panic::set_hook(Box::new(move |info| {
        let msg = match info.payload().downcast_ref::<&str>() {
            Some(s) => s.to_string(),
            None => info.payload().downcast_ref::<String>().cloned().unwrap_or_else(|| "unknown error".into()),
        };
        let where_ = info.location().map(|l| format!("{}:{}", l.file(), l.line())).unwrap_or_default();
        let text = format!("{app} stopped: {msg}\n\n({where_})");
        eprintln!("{text}");
        if let Some(dir) = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.to_path_buf())) {
            let _ = std::fs::write(dir.join(format!("{app}_crash.log")), format!("{}\n{text}\n", chrono::Local::now()));
        }
        message_box(app, &text, false);
    }));
}

/// One tuner at a time: a second double-click while it's running just exits.
#[cfg(not(windows))]
pub fn single_instance(_name: &str) -> bool {
    true
}

#[cfg(windows)]
pub fn single_instance(name: &str) -> bool {
    use windows::core::HSTRING;
    use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
    use windows::Win32::System::Threading::CreateMutexW;
    unsafe {
        // Leaked on purpose: the handle must live as long as the process.
        match CreateMutexW(None, false, &HSTRING::from(format!("Local\\{name}"))) {
            Ok(h) => {
                let _ = h; // HANDLE is Copy here; never closed, by design
                GetLastError() != ERROR_ALREADY_EXISTS
            }
            Err(_) => true,
        }
    }
}

/// Start the sibling exe (tuner-setup.exe / tuner.exe) from the same folder.
pub fn launch_sibling(exe_name: &str) -> std::io::Result<()> {
    launch_sibling_with(exe_name, &[])
}

pub fn launch_sibling_with(exe_name: &str, args: &[&str]) -> std::io::Result<()> {
    let exe_name = if cfg!(windows) { exe_name.to_string() } else { exe_name.trim_end_matches(".exe").to_string() };
    let exe = std::env::current_exe().ok().map(|p| p.with_file_name(&exe_name)).unwrap_or_else(|| exe_name.clone().into());
    std::process::Command::new(&exe).args(args).spawn().map(|_| ())
}

#[cfg(not(windows))]
pub fn use_parent_console() {}

/// GUI-subsystem exes get no console. When started from a terminal (stdout not redirected to a
/// pipe/file), attach to the parent's console so println!/eprintln! land there; when
/// double-clicked there is no parent console and nothing happens. Redirected handles are kept.
#[cfg(windows)]
pub fn use_parent_console() {
    use windows::Win32::System::Console::{AttachConsole, GetStdHandle, ATTACH_PARENT_PROCESS, STD_OUTPUT_HANDLE};
    unsafe {
        let redirected = GetStdHandle(STD_OUTPUT_HANDLE).is_ok_and(|h| !h.is_invalid() && !h.0.is_null());
        if !redirected {
            let _ = AttachConsole(ATTACH_PARENT_PROCESS);
        }
    }
}

/// The app icon (assets/icon_64.png) as a winit window icon.
pub fn window_icon() -> Option<winit::window::Icon> {
    let bytes: &[u8] = include_bytes!("../assets/icon_64.png");
    let mut dec = png::Decoder::new(std::io::Cursor::new(bytes));
    dec.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = dec.read_info().ok()?;
    let mut buf = vec![0; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut buf).ok()?;
    let n = (info.width * info.height) as usize;
    let rgba: Vec<u8> = match info.color_type {
        png::ColorType::Rgba => buf[..n * 4].to_vec(),
        png::ColorType::Rgb => buf[..n * 3].chunks_exact(3).flat_map(|p| [p[0], p[1], p[2], 255]).collect(),
        _ => return None,
    };
    winit::window::Icon::from_rgba(rgba, info.width, info.height).ok()
}
