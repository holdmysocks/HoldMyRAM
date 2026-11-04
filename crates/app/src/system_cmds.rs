//! Window commands about the app itself: start with Windows, first-run state, and the
//! WebView2 runtime check (plus what happens when the window cannot be created).

use crate::{adopt_saved_config, debug_log, rt, set_tray, StateView, ACTIVE};
use holdmyram_core::autostart::{self, AutostartStatus};
use serde::Serialize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use tauri::AppHandle;

pub const WEBVIEW2_URL: &str = "https://developer.microsoft.com/microsoft-edge/webview2/";

/// Last window-creation error (shown by `webview2_status`).
static WINDOW_ERROR: Mutex<Option<String>> = Mutex::new(None);
/// A "WebView2 missing" message box is open; don't stack another one.
static BOX_OPEN: AtomicBool = AtomicBool::new(false);

#[tauri::command]
pub async fn get_autostart() -> Result<AutostartStatus, String> {
    autostart::status()
}

/// On: HKCU Run value "HoldMyRAM" = quoted path of this exe (also fixes a moved copy and
/// clears a Task-Manager disable on our entry). Off: removes our value. No admin.
#[tauri::command]
pub async fn set_autostart(enabled: bool) -> Result<AutostartStatus, String> {
    let s = autostart::set(enabled)?;
    debug_log(&format!("autostart: set {enabled} -> {s:?}"));
    Ok(s)
}

#[tauri::command]
pub fn set_tutorial_seen(seen: bool) -> Result<StateView, String> {
    let mut cfg = rt().config.clone();
    cfg.tutorial_seen = seen;
    adopt_saved_config(cfg)
}

#[derive(Serialize)]
pub struct WebView2Status {
    pub installed: bool,
    /// Runtime version, e.g. "131.0.2903.70".
    pub version: Option<String>,
    /// Why the check or the last window creation failed, if it did.
    pub error: Option<String>,
    pub download_url: &'static str,
}

/// The installed WebView2 runtime version (the loader is linked statically through wry).
fn webview2_version() -> Result<String, String> {
    use webview2_com::Microsoft::Web::WebView2::Win32::GetAvailableCoreWebView2BrowserVersionString;
    let mut v = windows_core::PWSTR::null();
    unsafe { GetAvailableCoreWebView2BrowserVersionString(windows_core::PCWSTR::null(), &mut v) }.map_err(|e| e.message().to_string())?;
    if v.is_null() {
        return Err("no WebView2 runtime found".into());
    }
    let s = webview2_com::take_pwstr(v);
    if s.is_empty() {
        Err("no WebView2 runtime found".into())
    } else {
        Ok(s)
    }
}

fn status_now() -> WebView2Status {
    let last = WINDOW_ERROR.lock().unwrap_or_else(|e| e.into_inner()).clone();
    match webview2_version() {
        Ok(v) => WebView2Status { installed: true, version: Some(v), error: last, download_url: WEBVIEW2_URL },
        Err(e) => WebView2Status { installed: false, version: None, error: Some(last.unwrap_or(e)), download_url: WEBVIEW2_URL },
    }
}

#[tauri::command]
pub fn webview2_status() -> WebView2Status {
    status_now()
}

/// The window could not be created. Log it, say so on the tray, and if the WebView2
/// runtime is missing show a native message box offering the download page.
pub fn window_failed(app: &AppHandle, err: &str) {
    *WINDOW_ERROR.lock().unwrap_or_else(|e| e.into_inner()) = Some(err.to_string());
    let st = status_now();
    debug_log(&format!("window: could not be created: {err}; webview2 installed: {} ({:?})", st.installed, st.version));
    let active = ACTIVE.load(Ordering::SeqCst);
    if st.installed {
        set_tray(app, active, "HoldMyRAM: the window could not be opened, see log");
        return;
    }
    set_tray(app, active, "HoldMyRAM: the WebView2 runtime is missing, so the window cannot open. The hotkey and tray still work.");
    if BOX_OPEN.swap(true, Ordering::SeqCst) {
        return;
    }
    // Off the UI thread: the tray keeps working while the box is open.
    let spawned = std::thread::Builder::new().name("webview2-box".into()).spawn(|| {
        if ask_open_download_page() {
            open_url(WEBVIEW2_URL);
        }
        BOX_OPEN.store(false, Ordering::SeqCst);
    });
    if spawned.is_err() {
        BOX_OPEN.store(false, Ordering::SeqCst);
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Yes/No message box. True when the user chose Yes.
fn ask_open_download_page() -> bool {
    use windows::core::PCWSTR;
    use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, IDYES, MB_ICONWARNING, MB_SETFOREGROUND, MB_YESNO};
    let text = wide(
        "HoldMyRAM needs the Microsoft Edge WebView2 Runtime to show its window, and it is not installed on this PC.\n\n\
         The tray icon and the hotkey keep working without it.\n\n\
         Open the WebView2 download page now? (Choose the Evergreen Bootstrapper or Standalone Installer.)",
    );
    let title = wide("HoldMyRAM: WebView2 runtime missing");
    let r = unsafe { MessageBoxW(None, PCWSTR(text.as_ptr()), PCWSTR(title.as_ptr()), MB_YESNO | MB_ICONWARNING | MB_SETFOREGROUND) };
    r == IDYES
}

pub(crate) fn open_url(url: &str) {
    use windows::core::PCWSTR;
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    // Only ever our own constant https URL.
    debug_assert!(url.starts_with("https://"));
    let verb = wide("open");
    let u = wide(url);
    let h = unsafe { ShellExecuteW(None, PCWSTR(verb.as_ptr()), PCWSTR(u.as_ptr()), PCWSTR::null(), PCWSTR::null(), SW_SHOWNORMAL) };
    if (h.0 as isize) <= 32 {
        debug_log(&format!("window: could not open {url} (ShellExecute {})", h.0 as isize));
    }
}
