#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use holdmyram_core::config::{self, Config, DEFAULT_HOTKEY};
use holdmyram_core::risk::{Kind, Risk, RiskDb};
use holdmyram_core::scan::{scan_all, ScanItem};
use holdmyram_core::startup::{self, StartupChange};
use holdmyram_core::{foreground_pid, toggle as engine};
use serde::Serialize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::Duration;
use tauri::{
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    image::Image, AppHandle, Emitter, Manager, RunEvent, WebviewUrl, WebviewWindowBuilder,
};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut, ShortcutState};

const TRAY_ID: &str = "main";
const WINDOW_LABEL: &str = "main";
const ICON_OFF: &[u8] = include_bytes!("../icons/32x32.png");
const ICON_ON: &[u8] = include_bytes!("../icons/32x32-on.png");

/// Whether the "free RAM" set is currently applied (a journal is pending).
static ACTIVE: AtomicBool = AtomicBool::new(false);
/// A toggle is running on its worker thread; further presses are ignored until it ends.
static BUSY: AtomicBool = AtomicBool::new(false);

/// Settings and status shared between the tray, the hotkey and the window.
struct Runtime {
    config: Config,
    warning: Option<String>,
    hotkey: HotkeyStatus,
    registered: Option<Shortcut>,
    last_report: String,
}

#[derive(Clone, Serialize, Default)]
struct HotkeyStatus {
    /// What the config asked for.
    requested: String,
    /// What is actually registered ("" if nothing).
    active: String,
    error: Option<String>,
}

static RT: OnceLock<Mutex<Runtime>> = OnceLock::new();

fn rt() -> MutexGuard<'static, Runtime> {
    RT.get_or_init(|| {
        Mutex::new(Runtime {
            config: Config::default(),
            warning: None,
            hotkey: HotkeyStatus::default(),
            registered: None,
            last_report: String::new(),
        })
    })
    .lock()
    .unwrap_or_else(|e| e.into_inner())
}

/// (Re)register the global hotkey. On any failure fall back to the default and record why.
/// The lock is never held across calls into the shortcut plugin.
fn apply_hotkey(app: &AppHandle, requested: &str) {
    let gs = app.global_shortcut();
    let old = rt().registered.take();
    if let Some(old) = old {
        let _ = gs.unregister(old);
    }
    let requested = requested.trim().to_string();
    let mut error: Option<String> = None;
    let mut registered: Option<Shortcut> = None;
    let mut active = String::new();

    match requested.parse::<Shortcut>() {
        Ok(s) => match gs.register(s) {
            Ok(()) => {
                registered = Some(s);
                active = requested.clone();
            }
            Err(e) => error = Some(format!("could not register \"{requested}\": {e}")),
        },
        Err(e) => error = Some(format!("could not parse hotkey \"{requested}\": {e}")),
    }
    if registered.is_none() {
        let fb = DEFAULT_HOTKEY;
        let why = error.clone().unwrap_or_default();
        let tried = fb
            .parse::<Shortcut>()
            .map_err(|e| e.to_string())
            .and_then(|s| gs.register(s).map(|_| s).map_err(|e| e.to_string()));
        match tried {
            Ok(s) => {
                registered = Some(s);
                active = fb.to_string();
                error = Some(format!("{why}; using {fb} instead"));
            }
            Err(e) => {
                error = Some(format!(
                    "{why}; fallback {fb} also failed: {e}. No hotkey is active, use the tray menu."
                ))
            }
        }
    }
    if let Some(e) = &error {
        debug_log(&format!("hotkey: {e}"));
    }
    let mut r = rt();
    r.registered = registered;
    r.hotkey = HotkeyStatus { requested, active, error };
}

#[derive(Serialize)]
struct StateView {
    config: Config,
    warning: Option<String>,
    active: bool,
    busy: bool,
    hotkey: HotkeyStatus,
    last_report: String,
    config_path: String,
    app_dir: String,
}

fn state_view() -> StateView {
    let r = rt();
    StateView {
        config: r.config.clone(),
        warning: r.warning.clone(),
        active: ACTIVE.load(Ordering::SeqCst),
        busy: BUSY.load(Ordering::SeqCst),
        hotkey: r.hotkey.clone(),
        last_report: r.last_report.clone(),
        config_path: config::config_path().display().to_string(),
        app_dir: config::app_dir().display().to_string(),
    }
}

#[tauri::command]
fn get_state() -> StateView {
    state_view()
}

#[tauri::command]
fn save_config(app: AppHandle, config: Config) -> Result<StateView, String> {
    config.save().map_err(|e| e.to_string())?;
    let hotkey = config.hotkey.clone();
    {
        let mut r = rt();
        r.config = config;
        r.warning = None;
    }
    apply_hotkey(&app, &hotkey);
    Ok(state_view())
}

/// Writes the given (possibly unsaved) config to `path`. The saved config is untouched.
#[tauri::command]
fn export_config(path: String, config: Config) -> Result<String, String> {
    let p = std::path::PathBuf::from(path.trim());
    if p.as_os_str().is_empty() {
        return Err("enter a file path".into());
    }
    config.export(&p).map_err(|e| e.to_string())?;
    Ok(p.display().to_string())
}

/// Reads and validates only; the UI adopts the result as unsaved changes.
#[tauri::command]
fn import_config(path: String) -> Result<Config, String> {
    let p = std::path::PathBuf::from(path.trim());
    Config::import(&p).map_err(|e| e.to_string())
}

#[derive(Serialize)]
struct Row {
    name: String,
    display: String,
    kind: Kind,
    running: bool,
    ram_bytes: u64,
    pids: Vec<u32>,
    detail: String,
    risk: Risk,
    summary: String,
    notes: Option<String>,
    /// Only processes can be acted on by the toggle today.
    actionable: bool,
    /// Startup rows only: HoldMyRAM can enable/disable this item without admin rights.
    can_change: bool,
}

/// Inventory joined with the risk database, biggest RAM first. `extra` are process names
/// the UI has selected that are not running now, so they still show up and can be
/// deselected. Nothing is kept after the call returns.
#[tauri::command]
async fn scan(extra: Vec<String>) -> Result<Vec<Row>, String> {
    let mut items: Vec<ScanItem> = scan_all();
    for n in extra {
        let t = n.trim();
        if !t.is_empty()
            && !items.iter().any(|i| i.kind == Kind::Process && i.name.eq_ignore_ascii_case(t))
        {
            items.push(ScanItem {
                name: t.to_string(),
                display: t.to_string(),
                kind: Kind::Process,
                running: false,
                ram_bytes: 0,
                pids: Vec::new(),
                detail: "Selected, but not running right now.".into(),
            });
        }
    }
    let (db, db_warning) = RiskDb::load();
    if let Some(w) = db_warning {
        debug_log(&format!("risk db: {w}"));
    }
    let mut rows: Vec<Row> = items
        .into_iter()
        .map(|i| {
            let e = db.lookup_for(&i.name, i.kind);
            Row {
                actionable: i.kind == Kind::Process,
                can_change: i.kind == Kind::Startup && startup::can_change(&i.detail),
                name: i.name,
                display: i.display,
                kind: i.kind,
                running: i.running,
                ram_bytes: i.ram_bytes,
                pids: i.pids,
                detail: i.detail,
                risk: e.risk,
                summary: e.summary,
                notes: e.notes,
            }
        })
        .collect();
    drop(db);
    rows.sort_by(|a, b| {
        b.ram_bytes
            .cmp(&a.ram_bytes)
            .then_with(|| a.display.to_lowercase().cmp(&b.display.to_lowercase()))
    });
    Ok(rows)
}

/// Enable or disable one startup item (what happens at login; unrelated to the RAM toggle).
#[tauri::command]
async fn set_startup_enabled(name: String, detail: String, enabled: bool) -> Result<StartupChange, String> {
    startup::set_enabled(&name, &detail, enabled)
}

#[tauri::command]
async fn startup_changes() -> Result<Vec<StartupChange>, String> {
    Ok(startup::changes())
}

#[tauri::command]
fn toggle_now(app: AppHandle) {
    toggle(&app);
}

#[tauri::command]
fn quit_app(app: AppHandle) {
    app.exit(0);
}

/// Create the window only when asked. Closing it destroys the webview, which is what
/// keeps the background footprint small.
fn open_window(app: &AppHandle) {
    if let Some(w) = app.get_webview_window(WINDOW_LABEL) {
        let _ = w.unminimize();
        let _ = w.set_focus();
        return;
    }
    let _ = WebviewWindowBuilder::new(app, WINDOW_LABEL, WebviewUrl::App("index.html".into()))
        .title("HoldMyRAM")
        .inner_size(1040.0, 700.0)
        .build();
}

fn set_tray(app: &AppHandle, active: bool, tooltip: &str) {
    if let Some(tray) = app.tray_by_id(TRAY_ID) {
        let _ = tray.set_tooltip(Some(tooltip));
        // The colour change is the reliable signal; tray tooltips often don't refresh.
        let bytes: &[u8] = if active { ICON_ON } else { ICON_OFF };
        if let Ok(img) = Image::from_bytes(bytes) {
            let _ = tray.set_icon(Some(img));
        }
    }
}

/// Hotkey / tray / button toggle. The work (including the Close grace period) runs on a
/// worker thread so the tray and hotkey never freeze.
fn toggle(app: &AppHandle) {
    if BUSY.swap(true, Ordering::SeqCst) {
        debug_log("toggle: ignored, previous toggle still running");
        return;
    }
    let turning_on = !ACTIVE.load(Ordering::SeqCst);
    // Capture the foreground app at the moment of the key press, before anything changes.
    let fg = if turning_on { foreground_pid() } else { None };
    let self_pid = std::process::id();
    let (selections, grace) = {
        let r = rt();
        (
            r.config.effective_selections().to_vec(),
            Duration::from_secs(r.config.grace_seconds),
        )
    };
    set_tray(app, turning_on, if turning_on { "HoldMyRAM: turning on..." } else { "HoldMyRAM: restoring..." });
    let _ = app.emit("state-changed", ());

    let worker_app = app.clone();
    let spawned = std::thread::Builder::new().name("toggle".into()).spawn(move || {
        let report = if turning_on {
            let opts = engine::Options { grace, ..Default::default() };
            engine::apply_with(&opts, &selections, fg, self_pid)
        } else {
            engine::revert()
        };
        debug_log(&report.details());
        // A refused apply (e.g. unreadable journal) leaves the state off.
        let active = if turning_on { report.error.is_none() || !report.acted.is_empty() } else { false };
        ACTIVE.store(active, Ordering::SeqCst);
        let state = if active { "on" } else { "off" };
        let mut tip = format!("HoldMyRAM: {state} ({})", report.summary());
        if !turning_on && !report.failed.is_empty() {
            tip = format!("HoldMyRAM: off, {} item(s) could not be restored", report.failed.len());
        }
        rt().last_report = report.summary();
        set_tray(&worker_app, active, &tip);
        BUSY.store(false, Ordering::SeqCst);
        let _ = worker_app.emit("state-changed", ());
    });
    if let Err(e) = spawned {
        debug_log(&format!("toggle: could not start worker: {e}"));
        let was_active = ACTIVE.load(Ordering::SeqCst);
        set_tray(app, was_active, "HoldMyRAM: could not start toggle, see log");
        BUSY.store(false, Ordering::SeqCst);
        let _ = app.emit("state-changed", ());
    }
}

/// Crash recovery: a journal left by a previous session means apps are still closed or
/// frozen. Show it on the tray and arm the toggle so the next press restores them.
fn check_pending_restore(app: &AppHandle) {
    match engine::pending_restore() {
        Ok(Some(j)) => {
            let n = j.entries.iter().filter(|e| e.state != holdmyram_core::journal::EntryState::Failed).count();
            debug_log(&format!("startup: journal pending with {n} item(s); restore offered"));
            ACTIVE.store(true, Ordering::SeqCst);
            set_tray(app, true, &format!("HoldMyRAM: restore pending ({n} item(s)), toggle to restore"));
        }
        Ok(None) => {}
        Err(e) => {
            debug_log(&format!("startup: journal unreadable: {e}"));
            set_tray(app, false, "HoldMyRAM: journal unreadable, see log");
        }
    }
}

/// Temporary: confirms the hotkey handler fires and records toggle reports.
fn debug_log(msg: &str) {
    use std::io::Write;
    let path = std::env::temp_dir().join("holdmyram.log");
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{msg}");
    }
}

fn main() {
    let app = tauri::Builder::default()
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(|app, _shortcut, event| {
                    if event.state() == ShortcutState::Pressed {
                        toggle(app);
                    }
                })
                .build(),
        )
        .invoke_handler(tauri::generate_handler![
            get_state, save_config, scan, export_config, import_config, toggle_now, quit_app,
            set_startup_enabled, startup_changes
        ])
        .setup(|app| {
            let open = MenuItem::with_id(app, "open", "Open HoldMyRAM", true, None::<&str>)?;
            let flip = MenuItem::with_id(app, "toggle", "Toggle on/off", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&open, &flip, &quit])?;

            TrayIconBuilder::with_id(TRAY_ID)
                .icon(app.default_window_icon().cloned().expect("window icon"))
                .tooltip("HoldMyRAM: off")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "open" => open_window(app),
                    "toggle" => toggle(app),
                    "quit" => app.exit(0),
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        open_window(tray.app_handle());
                    }
                })
                .build(app)?;

            check_pending_restore(app.handle());

            let loaded = Config::load();
            if let Some(w) = &loaded.warning {
                debug_log(&format!("config: {w}"));
            }
            let hotkey = loaded.config.hotkey.clone();
            {
                let mut r = rt();
                r.config = loaded.config;
                r.warning = loaded.warning;
            }
            apply_hotkey(app.handle(), &hotkey);
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("failed to build HoldMyRAM");

    app.run(|_app, event| {
        // Closing the last window must not quit: the tray keeps running.
        if let RunEvent::ExitRequested { api, code, .. } = event {
            if code.is_none() {
                api.prevent_exit();
            }
        }
    });
}
