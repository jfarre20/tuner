//! WebView2 child window for the WeatherStar channel. It sits over the swapchain (a child HWND
//! always paints above its parent), so it is only made visible while that channel is tuned and
//! the snow has cleared. Nothing else in the app composites with it.

use std::cell::Cell;
use std::sync::mpsc;
use webview2_com::Microsoft::Web::WebView2::Win32::*;
use webview2_com::*;
use windows::core::{Error, HSTRING};
use windows::Win32::Foundation::E_POINTER;
use windows::Win32::Foundation::{HWND, RECT};

pub struct Web {
    controller: ICoreWebView2Controller,
    webview: ICoreWebView2,
    shown: Cell<bool>,
    bounds: Cell<(i32, i32)>,
}

impl Web {
    /// Create the control hidden; call `navigate` when the page should load.
    pub fn new(window: &winit::window::Window) -> std::result::Result<Web, String> {
        use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
        let parent = match window.window_handle().map(|h| h.as_raw()) {
            Ok(RawWindowHandle::Win32(h)) => HWND(h.hwnd.get() as *mut std::ffi::c_void),
            _ => return Err("not a Win32 window".into()),
        };
        let env = {
            let (tx, rx) = mpsc::channel();
            CreateCoreWebView2EnvironmentCompletedHandler::wait_for_async_operation(
                Box::new(|handler| unsafe { CreateCoreWebView2Environment(&handler).map_err(webview2_com::Error::WindowsError) }),
                Box::new(move |hr, env| {
                    hr?;
                    tx.send(env.ok_or_else(|| Error::from(E_POINTER))).expect("send");
                    Ok(())
                }),
            )
            .map_err(|e| format!("environment: {e:?}"))?;
            rx.recv().map_err(|e| e.to_string())?.map_err(|e| e.to_string())?
        };
        let controller = {
            let (tx, rx) = mpsc::channel();
            CreateCoreWebView2ControllerCompletedHandler::wait_for_async_operation(
                Box::new(move |handler| unsafe { env.CreateCoreWebView2Controller(parent, &handler).map_err(webview2_com::Error::WindowsError) }),
                Box::new(move |hr, c| {
                    hr?;
                    tx.send(c.ok_or_else(|| Error::from(E_POINTER))).expect("send");
                    Ok(())
                }),
            )
            .map_err(|e| format!("controller: {e:?}"))?;
            rx.recv().map_err(|e| e.to_string())?.map_err(|e| e.to_string())?
        };
        let webview = unsafe { controller.CoreWebView2() }.map_err(|e| e.to_string())?;
        unsafe {
            controller.SetIsVisible(false).map_err(|e| e.to_string())?;
            if let Ok(s) = webview.Settings() {
                let _ = s.SetAreDefaultContextMenusEnabled(false);
                let _ = s.SetAreDevToolsEnabled(false);
                let _ = s.SetIsStatusBarEnabled(false);
                let _ = s.SetIsZoomControlEnabled(false);
            }
        }
        Ok(Web { controller, webview, shown: Cell::new(false), bounds: Cell::new((0, 0)) })
    }

    pub fn navigate(&self, url: &str) {
        unsafe {
            if let Err(e) = self.webview.Navigate(&HSTRING::from(url)) {
                eprintln!("webview2 navigate: {e}");
            }
        }
    }

    /// Load an in-memory page (the setup tool's UI).
    pub fn navigate_html(&self, html: &str) {
        unsafe {
            if let Err(e) = self.webview.NavigateToString(&HSTRING::from(html)) {
                eprintln!("webview2 navigate: {e}");
            }
        }
    }

    /// Page -> host: `window.chrome.webview.postMessage(obj)` arrives here as JSON text.
    pub fn on_message(&self, f: impl Fn(String) + 'static) {
        let handler = WebMessageReceivedEventHandler::create(Box::new(move |_sender, args| {
            if let Some(args) = args {
                let mut s = windows::core::PWSTR::null();
                unsafe { args.WebMessageAsJson(&mut s) }?;
                f(take_pwstr(s));
            }
            Ok(())
        }));
        let mut token: i64 = 0;
        unsafe {
            if let Err(e) = self.webview.add_WebMessageReceived(&handler, &mut token) {
                eprintln!("webview2 message handler: {e}");
            }
        }
    }

    /// Host -> page: delivered as a parsed object on `window.chrome.webview` 'message' events.
    pub fn post(&self, json: &str) {
        unsafe {
            if let Err(e) = self.webview.PostWebMessageAsJson(&HSTRING::from(json)) {
                eprintln!("webview2 post: {e}");
            }
        }
    }


    /// Show the page filling `size` (client pixels), or hide it.
    pub fn set(&self, size: Option<(u32, u32)>) {
        unsafe {
            match size {
                Some((w, h)) => {
                    if self.bounds.get() != (w as i32, h as i32) {
                        self.bounds.set((w as i32, h as i32));
                        let _ = self.controller.SetBounds(RECT { left: 0, top: 0, right: w as i32, bottom: h as i32 });
                    }
                    if !self.shown.get() {
                        self.shown.set(true);
                        let _ = self.controller.SetIsVisible(true);
                    }
                }
                None => {
                    if self.shown.get() {
                        self.shown.set(false);
                        let _ = self.controller.SetIsVisible(false);
                    }
                }
            }
        }
    }
}
