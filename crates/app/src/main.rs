#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use holdmyram_core::admin::changelog::{AdminChange, ChangeLog};
use holdmyram_core::admin::client::Helper;
use holdmyram_core::admin::handler::AdminBackend;
use holdmyram_core::admin::proto::{AdminError, ErrorCode, StartChoice};
use holdmyram_core::admin::rules;
use holdmyram_core::admin::win::WinBackend;
use holdmyram_core::config::{self, admin_block_reason, Config, DEFAULT_HOTKEY};
use holdmyram_core::journal;
use holdmyram_core::risk::{Kind, Risk, RiskDb};
use holdmyram_core::scan::{scan_all, ScanItem};
use holdmyram_core::startup::{self, StartupChange};
use holdmyram_core::toggle::AdminAccess;
use holdmyram_core::types::SelectionKind;
use holdmyram_core::{foreground_pid, toggle as engine};
use serde::Serialize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::Duration;
use tauri::{
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    image::Image, AppHandle, Emitter, Manager, RunEvent, WebviewUrl, WebviewWindowBuilder, WindowEvent,
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
/// A quit is already in progress (waiting for a toggle and/or restoring).
static QUITTING: AtomicBool = AtomicBool::new(false);
/// How long a quit waits for a running toggle (Close grace period + restore) to finish.
const QUIT_WAIT: Duration = Duration::from_secs(120);

// ---------------------------------------------------------------------------------------
// Elevated helper
//
// One process-wide handle. Nothing elevated runs until something needs it; arming shows
// the UAC prompt and blocks, so it only ever happens on a worker thread (toggle, quit) or
// a blocking task (window commands), never on the tray/UI thread.
//
// Lifetime: the helper stays armed while (the toggle is on AND the journal holds
// service/task entries) OR (the window is open AND an admin action was used in it).
// `disarm_if_unneeded` runs after every toggle, quit, window close and disarm request.
// The helper also exits by itself when this process dies.
// ---------------------------------------------------------------------------------------

static HELPER: Mutex<Option<Helper>> = Mutex::new(None);
/// Mirrors of the helper's state, readable without waiting for HELPER (which is held for
/// the whole of a UAC prompt or a slow service stop).
static ARMED: AtomicBool = AtomicBool::new(false);
static ARMING: AtomicBool = AtomicBool::new(false);
static HELPER_ERROR: Mutex<Option<String>> = Mutex::new(None);
/// The user declined the UAC prompt on a toggle-on. Later toggle-ons do not prompt again
/// (processes only) until the user arms from the window, saves the config, or restarts.
/// Toggle-off with pending service/task entries always asks: that is a restore.
static TOGGLE_DECLINED: AtomicBool = AtomicBool::new(false);
static WINDOW_OPEN: AtomicBool = AtomicBool::new(false);
static WINDOW_ADMIN_USED: AtomicBool = AtomicBool::new(false);

fn helper_lock() -> MutexGuard<'static, Option<Helper>> {
    HELPER.lock().unwrap_or_else(|e| e.into_inner())
}

fn set_helper_error(e: Option<String>) {
    *HELPER_ERROR.lock().unwrap_or_else(|e| e.into_inner()) = e;
}

/// Arm the helper held in `slot` if it is not armed yet (one UAC prompt; blocks).
fn ensure_armed(app: Option<&AppHandle>, slot: &mut Option<Helper>) -> Result<(), AdminError> {
    let h = slot.get_or_insert_with(Helper::new);
    if h.is_armed() {
        ARMED.store(true, Ordering::SeqCst);
        return Ok(());
    }
    ARMING.store(true, Ordering::SeqCst);
    if let Some(app) = app {
        set_tray(app, ACTIVE.load(Ordering::SeqCst), "HoldMyRAM: waiting for admin approval...");
        let _ = app.emit("state-changed", ());
    }
    let r = h.arm();
    ARMING.store(false, Ordering::SeqCst);
    ARMED.store(r.is_ok(), Ordering::SeqCst);
    match &r {
        Ok(()) => {
            set_helper_error(None);
            TOGGLE_DECLINED.store(false, Ordering::SeqCst);
            debug_log("helper: armed");
        }
        Err(e) => {
            set_helper_error(Some(e.to_string()));
            debug_log(&format!("helper: arm failed: {e}"));
        }
    }
    if let Some(app) = app {
        let _ = app.emit("state-changed", ());
    }
    r
}

/// Service/task entries waiting in the journal (unreadable journal: 0, nothing we could do).
fn journal_admin_pending() -> usize {
    engine::admin_pending_at(&journal::default_path()).unwrap_or(0)
}

fn helper_needed() -> bool {
    (ACTIVE.load(Ordering::SeqCst) && journal_admin_pending() > 0)
        || (WINDOW_OPEN.load(Ordering::SeqCst) && WINDOW_ADMIN_USED.load(Ordering::SeqCst))
}

/// Shut the helper down unless something still needs it. Blocks while another admin
/// operation holds the helper; call off the UI thread.
fn disarm_if_unneeded() {
    let mut g = helper_lock();
    if helper_needed() {
        return;
    }
    if let Some(mut h) = g.take() {
        if h.is_armed() {
            debug_log("helper: disarmed (no longer needed)");
        }
        h.disarm();
    }
    ARMED.store(false, Ordering::SeqCst);
}

/// Run `f` with an armed helper (arming it first if needed), as an admin action from the
/// window. Blocking; call from a blocking task.
fn with_window_helper<T>(app: &AppHandle, f: impl FnOnce(&mut Helper) -> Result<T, AdminError>) -> Result<T, String> {
    let mut g = helper_lock();
    if let Err(e) = ensure_armed(Some(app), &mut g) {
        drop(g);
        restore_idle_tooltip(app);
        return Err(e.to_string());
    }
    WINDOW_ADMIN_USED.store(true, Ordering::SeqCst);
    let h = g.as_mut().expect("armed");
    let r = f(h);
    ARMED.store(h.is_armed(), Ordering::SeqCst);
    if let Err(e) = &r {
        if matches!(e.code, ErrorCode::Transport | ErrorCode::NotArmed) {
            set_helper_error(Some(e.to_string()));
        }
    }
    drop(g);
    restore_idle_tooltip(app);
    r.map_err(|e| e.to_string())
}

/// After an arm prompt from the window, put the normal tooltip back.
fn restore_idle_tooltip(app: &AppHandle) {
    if BUSY.load(Ordering::SeqCst) {
        return; // the running toggle sets its own tooltip when it ends
    }
    let active = ACTIVE.load(Ordering::SeqCst);
    let last = rt().last_report.clone();
    let tip = if last.is_empty() {
        format!("HoldMyRAM: {}", if active { "on" } else { "off" })
    } else {
        format!("HoldMyRAM: {} ({last})", if active { "on" } else { "off" })
    };
    set_tray(app, active, &tip);
}

/// Blocking work for an async command, off the async runtime's worker threads.
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T, String> + Send + 'static) -> Result<T, String> {
    tauri::async_runtime::spawn_blocking(f).await.map_err(|e| format!("internal error: {e}"))?
}

#[derive(Serialize)]
struct HelperStatus {
    /// The elevated helper is running and connected.
    armed: bool,
    /// A UAC prompt is open right now.
    arming: bool,
    /// Last arm or channel error ("uac_cancelled: ...", "handshake: ..."), if any.
    last_error: Option<String>,
    /// Toggle-on will not prompt again this session (the user declined); arm from the
    /// window to re-enable services/tasks in the toggle.
    toggle_declined: bool,
    /// Service/task entries waiting in the journal for a restore.
    admin_pending: usize,
}

fn helper_status_now() -> HelperStatus {
    HelperStatus {
        armed: ARMED.load(Ordering::SeqCst),
        arming: ARMING.load(Ordering::SeqCst),
        last_error: HELPER_ERROR.lock().unwrap_or_else(|e| e.into_inner()).clone(),
        toggle_declined: TOGGLE_DECLINED.load(Ordering::SeqCst),
        admin_pending: journal_admin_pending(),
    }
}

#[tauri::command]
fn helper_status() -> HelperStatus {
    helper_status_now()
}

/// Arm the helper now (UAC prompt). Keeps it armed while the window stays open.
#[tauri::command]
async fn arm_helper(app: AppHandle) -> Result<HelperStatus, String> {
    blocking(move || {
        TOGGLE_DECLINED.store(false, Ordering::SeqCst);
        with_window_helper(&app, |_| Ok(()))?;
        Ok(helper_status_now())
    })
    .await
}

/// The window no longer needs the helper. It still stays armed while the toggle is on
/// with service/task entries (so turning the toggle off needs no second prompt).
#[tauri::command]
async fn disarm_helper() -> Result<HelperStatus, String> {
    blocking(|| {
        WINDOW_ADMIN_USED.store(false, Ordering::SeqCst);
        disarm_if_unneeded();
        Ok(helper_status_now())
    })
    .await
}

#[derive(Serialize)]
struct AdminChangeView {
    /// Pass to `admin_undo`. "service:<name, lowercase>" or "task:<path, lowercase>".
    id: String,
    /// "service_start_type" or "task_enabled".
    kind: &'static str,
    /// Service short name or full task path.
    name: String,
    /// Service: "Auto", "AutoDelayed", "Manual", "Disabled" (or "Boot"/"System"/"Unknown"
    /// as read). Task: "enabled" / "disabled".
    before: String,
    now: String,
    /// When the latest change was made, RFC 3339-like UTC (`2026-11-04T09:59:59Z`).
    at: String,
}

fn change_id(c: &AdminChange) -> String {
    match c {
        AdminChange::ServiceStartType { name, .. } => format!("service:{}", name.to_lowercase()),
        AdminChange::TaskEnabled { path, .. } => format!("task:{}", path.to_lowercase()),
    }
}

fn change_view(c: &AdminChange) -> AdminChangeView {
    let onoff = |b: bool| if b { "enabled" } else { "disabled" }.to_string();
    match c {
        AdminChange::ServiceStartType { name, before, before_delayed, now, at } => AdminChangeView {
            id: change_id(c),
            kind: "service_start_type",
            name: name.clone(),
            before: before.to_choice(*before_delayed).map(|ch| format!("{ch:?}")).unwrap_or_else(|| format!("{before:?}")),
            now: format!("{now:?}"),
            at: rfc3339(*at),
        },
        AdminChange::TaskEnabled { path, before, now, at } => AdminChangeView {
            id: change_id(c),
            kind: "task_enabled",
            name: path.clone(),
            before: onoff(*before),
            now: onoff(*now),
            at: rfc3339(*at),
        },
    }
}

/// Persistent start-type change from the window (logged first, undoable). `start` is
/// "Auto", "AutoDelayed", "Manual" or "Disabled". Ok(None): it already had that type.
#[tauri::command]
async fn admin_set_service_start(app: AppHandle, name: String, start: StartChoice) -> Result<Option<AdminChangeView>, String> {
    if let Some(why) = admin_block_reason(SelectionKind::Service, &name) {
        return Err(why);
    }
    rules::validate_service_name(&name)?;
    blocking(move || {
        with_window_helper(&app, |h| ChangeLog::default().set_service_start_type(h, &name, start))
            .map(|c| c.as_ref().map(change_view))
    })
    .await
}

/// Persistent enable/disable of a scheduled task from the window (logged first,
/// undoable). Ok(None): it was already in that state.
#[tauri::command]
async fn admin_set_task_enabled(app: AppHandle, path: String, enabled: bool) -> Result<Option<AdminChangeView>, String> {
    if let Some(why) = admin_block_reason(SelectionKind::Task, &path) {
        return Err(why);
    }
    rules::validate_task_path(&path)?;
    blocking(move || {
        with_window_helper(&app, |h| ChangeLog::default().set_task_enabled(h, &path, enabled)).map(|c| c.as_ref().map(change_view))
    })
    .await
}

/// Persistent changes in effect, oldest first. Reads the log only; never arms.
#[tauri::command]
async fn admin_changes() -> Result<Vec<AdminChangeView>, String> {
    Ok(ChangeLog::default().changes().iter().map(change_view).collect())
}

/// Put one logged change back to its original state (through the helper).
#[tauri::command]
async fn admin_undo(app: AppHandle, id: String) -> Result<Vec<AdminChangeView>, String> {
    blocking(move || {
        let log = ChangeLog::default();
        let change = log
            .changes()
            .into_iter()
            .find(|c| change_id(c) == id)
            .ok_or_else(|| "that change is no longer in the log".to_string())?;
        with_window_helper(&app, |h| log.undo(h, &change))?;
        Ok(log.changes().iter().map(change_view).collect())
    })
    .await
}

/// `YYYY-MM-DDTHH:MM:SSZ` (UTC), same format as startup.rs.
fn rfc3339(unix_secs: u64) -> String {
    let days = (unix_secs / 86_400) as i64;
    let rem = unix_secs % 86_400;
    // Civil-from-days (Howard Hinnant), proleptic Gregorian.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, (rem / 60) % 60, rem % 60)
}

/// Settings and status shared between the tray, the hotkey and the window.
struct Runtime {
    config: Config,
    warning: Option<String>,
    hotkey: HotkeyStatus,
    registered: Option<Shortcut>,
    /// One-line summary of the last toggle (tray tooltip).
    last_report: String,
    /// The full per-item report of the last toggle (shown in the window).
    last_detail: Option<engine::Report>,
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
            last_detail: None,
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
    last_report: Option<engine::Report>,
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
        last_report: r.last_detail.clone(),
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
    // A new selection may well be what the user wants the prompt for.
    TOGGLE_DECLINED.store(false, Ordering::SeqCst);
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
    /// Service and task rows: what the elevated helper may do with this item.
    admin: AdminInfo,
}

#[derive(Serialize)]
struct AdminInfo {
    /// Can be selected for the toggle (Service: stop; Task: disable) and changed from the
    /// window. False for blocked items and for rows that are not services or tasks.
    actionable: bool,
    /// Why HoldMyRAM will never change it (hard-block list), e.g. "Core Windows service,
    /// cannot be changed (never-touch list: RpcSs)".
    blocked: Option<String>,
    /// The name to put in a Selection / pass to admin commands: the service short name, or
    /// the full task path (`\Folder\Task`). None for other kinds.
    target: Option<String>,
}

fn admin_info(kind: Kind, name: &str, detail: &str) -> AdminInfo {
    let none = AdminInfo { actionable: false, blocked: None, target: None };
    let (sel_kind, target, valid) = match kind {
        Kind::Service => (SelectionKind::Service, name.to_string(), rules::validate_service_name(name)),
        Kind::Task => {
            // Task rows carry the folder path in `detail` ("\" for the root folder).
            let path = if detail.ends_with('\\') { format!("{detail}{name}") } else { format!("{detail}\\{name}") };
            let valid = rules::validate_task_path(&path);
            (SelectionKind::Task, path, valid)
        }
        _ => return none,
    };
    if let Some(why) = admin_block_reason(sel_kind, &target) {
        return AdminInfo { actionable: false, blocked: Some(why), target: Some(target) };
    }
    match valid {
        Ok(()) => AdminInfo { actionable: true, blocked: None, target: Some(target) },
        Err(e) => AdminInfo { actionable: false, blocked: Some(format!("HoldMyRAM cannot handle this name: {e}")), target: Some(target) },
    }
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
                admin: admin_info(i.kind, &i.name, &i.detail),
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
    quit_restoring(&app);
}

/// Quit without leaving apps closed: wait for any running toggle, restore if the toggle is
/// on, then exit. Holds BUSY the whole time so the hotkey cannot start a new toggle.
fn quit_restoring(app: &AppHandle) {
    if QUITTING.swap(true, Ordering::SeqCst) {
        return;
    }
    set_tray(app, ACTIVE.load(Ordering::SeqCst), "HoldMyRAM: quitting...");
    let worker_app = app.clone();
    let spawned = std::thread::Builder::new().name("quit".into()).spawn(move || {
        let started = std::time::Instant::now();
        let mut got_lock = false;
        while started.elapsed() < QUIT_WAIT {
            if !BUSY.swap(true, Ordering::SeqCst) {
                got_lock = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        if !got_lock {
            // A toggle is stuck. Don't hang forever; the journal still lets the next launch restore.
            debug_log("quit: timed out waiting for the running toggle; exiting without restore");
        } else if ACTIVE.load(Ordering::SeqCst) {
            // Same rule as toggle off: service/task entries ask for admin approval once.
            let mut guard = None;
            let report = revert_locked(&worker_app, &mut guard);
            drop(guard);
            debug_log(&format!("quit: {}", report.details()));
            if report.needs_admin > 0 {
                debug_log(&format!(
                    "quit: admin approval declined or unavailable; {} service/task item(s) stay in the journal and are restored by the first toggle after the next launch",
                    report.needs_admin
                ));
            }
            if !report.failed.is_empty() {
                debug_log(&format!(
                    "quit: {} item(s) could not be restored; they stay in the journal for the next launch",
                    report.failed.len()
                ));
            }
            ACTIVE.store(false, Ordering::SeqCst);
        }
        // Nothing needs the helper any more.
        if let Some(mut h) = helper_lock().take() {
            h.disarm();
        }
        ARMED.store(false, Ordering::SeqCst);
        worker_app.exit(0);
    });
    if let Err(e) = spawned {
        debug_log(&format!("quit: could not start worker ({e}); exiting without restore"));
        app.exit(0);
    }
}

/// Create the window only when asked. Closing it destroys the webview, which is what
/// keeps the background footprint small.
fn open_window(app: &AppHandle) {
    if let Some(w) = app.get_webview_window(WINDOW_LABEL) {
        let _ = w.unminimize();
        let _ = w.set_focus();
        return;
    }
    let built = WebviewWindowBuilder::new(app, WINDOW_LABEL, WebviewUrl::App("index.html".into()))
        .title("HoldMyRAM")
        .inner_size(1040.0, 700.0)
        .build();
    if built.is_ok() {
        WINDOW_OPEN.store(true, Ordering::SeqCst);
    }
}

/// The window was destroyed: it no longer needs the helper. The check may block on the
/// helper lock (a running admin call), so it runs on a short-lived thread, never on the
/// UI thread.
fn on_window_closed() {
    WINDOW_OPEN.store(false, Ordering::SeqCst);
    WINDOW_ADMIN_USED.store(false, Ordering::SeqCst);
    if !ARMED.load(Ordering::SeqCst) && helper_lock_is_free_and_empty() {
        return;
    }
    let _ = std::thread::Builder::new().name("disarm".into()).spawn(disarm_if_unneeded);
}

/// Cheap check: no helper handle exists at all (nothing to disarm).
fn helper_lock_is_free_and_empty() -> bool {
    HELPER.try_lock().map(|g| g.is_none()).unwrap_or(false)
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
        // Held (when the helper is involved) until ACTIVE is updated, so a concurrent
        // disarm check never sees the helper's journal entries with a stale ACTIVE.
        let mut guard: Option<MutexGuard<'static, Option<Helper>>> = None;
        let report = if turning_on {
            let opts = engine::Options { grace, ..Default::default() };
            apply_locked(&worker_app, &opts, &selections, fg, self_pid, &mut guard)
        } else {
            revert_locked(&worker_app, &mut guard)
        };
        debug_log(&report.details());
        // A refused apply (e.g. unreadable journal) leaves the state off. A restore that
        // could not get admin approval stays on, so the next toggle retries it.
        let active = if turning_on { report.error.is_none() || !report.acted.is_empty() } else { report.needs_admin > 0 };
        ACTIVE.store(active, Ordering::SeqCst);
        drop(guard);
        let state = if active { "on" } else { "off" };
        let mut tip = format!("HoldMyRAM: {state} ({})", report.summary());
        if turning_on && report.skipped.iter().any(|i| i.reason == Some(engine::Reason::UacCancelled)) {
            tip = format!("HoldMyRAM: on, apps only; services/tasks skipped (admin approval declined). {}", report.summary());
        }
        if !turning_on && report.needs_admin > 0 {
            tip = format!(
                "HoldMyRAM: apps restored; {} service/task item(s) still pending restore. Toggle again to retry (needs admin approval).",
                report.needs_admin
            );
        } else if !turning_on && !report.failed.is_empty() {
            tip = format!("HoldMyRAM: off, {} item(s) could not be restored", report.failed.len());
        }
        {
            let mut r = rt();
            r.last_report = report.summary();
            r.last_detail = Some(report);
        }
        set_tray(&worker_app, active, &tip);
        disarm_if_unneeded();
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

/// Toggle on. With services/tasks in the selection, arm the helper first (one UAC prompt,
/// on this worker thread); declined or failed means processes only, with the admin items
/// reported as skipped. A decline is remembered so later presses don't prompt again.
fn apply_locked(
    app: &AppHandle,
    opts: &engine::Options,
    selections: &[holdmyram_core::Selection],
    fg: Option<u32>,
    self_pid: u32,
    guard: &mut Option<MutexGuard<'static, Option<Helper>>>,
) -> engine::Report {
    if !engine::has_admin_items(selections) {
        return engine::apply_full(opts, selections, fg, self_pid, AdminAccess::not_armed());
    }
    if TOGGLE_DECLINED.load(Ordering::SeqCst) && !ARMED.load(Ordering::SeqCst) {
        let e = AdminError::new(
            ErrorCode::UacCancelled,
            "admin approval was declined earlier; open HoldMyRAM and allow admin access to include services and tasks",
        );
        return engine::apply_full(opts, selections, fg, self_pid, AdminAccess::Unavailable(e));
    }
    let g = guard.insert(helper_lock());
    match ensure_armed(Some(app), g) {
        Ok(()) => {
            let h = g.as_mut().expect("armed");
            // Unelevated, read-only: lists running dependents so the journal records the
            // full intent before a stop. Lives only for this toggle.
            let mut probe = WinBackend::new().ok();
            let probe = probe.as_mut().map(|p| p as &mut dyn AdminBackend);
            let r = engine::apply_full(opts, selections, fg, self_pid, AdminAccess::Ready { api: h, probe });
            ARMED.store(h.is_armed(), Ordering::SeqCst);
            r
        }
        Err(e) => {
            if e.code == ErrorCode::UacCancelled {
                TOGGLE_DECLINED.store(true, Ordering::SeqCst);
            }
            engine::apply_full(opts, selections, fg, self_pid, AdminAccess::Unavailable(e))
        }
    }
}

/// Toggle off (also used by quit). Service/task entries in the journal need the helper:
/// arm it (one UAC prompt) if it is not armed. Declined: processes are restored now and
/// the service/task entries stay in the journal (`report.needs_admin`).
fn revert_locked(app: &AppHandle, guard: &mut Option<MutexGuard<'static, Option<Helper>>>) -> engine::Report {
    let opts = engine::Options::default();
    let self_pid = std::process::id();
    if journal_admin_pending() == 0 {
        return engine::revert_full(&opts, self_pid, AdminAccess::not_armed());
    }
    let g = guard.insert(helper_lock());
    match ensure_armed(Some(app), g) {
        Ok(()) => {
            let h = g.as_mut().expect("armed");
            let r = engine::revert_full(&opts, self_pid, AdminAccess::Ready { api: h, probe: None });
            ARMED.store(h.is_armed(), Ordering::SeqCst);
            r
        }
        Err(e) => engine::revert_full(&opts, self_pid, AdminAccess::Unavailable(e)),
    }
}

/// Crash recovery: a journal left by a previous session means apps are still closed or
/// frozen. Show it on the tray and arm the toggle so the next press restores them.
fn check_pending_restore(app: &AppHandle) {
    match engine::pending_restore() {
        Ok(Some(j)) => {
            let a = j.admin_pending();
            let n = j.entries.iter().filter(|e| e.state != journal::EntryState::Failed).count() + a;
            debug_log(&format!("startup: journal pending with {n} item(s) ({a} service/task); restore offered"));
            ACTIVE.store(true, Ordering::SeqCst);
            let admin_note = if a > 0 { ", needs admin approval" } else { "" };
            set_tray(app, true, &format!("HoldMyRAM: restore pending ({n} item(s){admin_note}), toggle to restore"));
        }
        Ok(None) => {}
        Err(e) => {
            debug_log(&format!("startup: journal unreadable: {e}"));
            set_tray(app, false, "HoldMyRAM: journal unreadable, see log");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_admin_info() {
        let s = admin_info(Kind::Service, "SysMain", "start: Auto");
        assert!(s.actionable && s.blocked.is_none());
        assert_eq!(s.target.as_deref(), Some("SysMain"));
        let s = admin_info(Kind::Service, "RpcSs", "");
        assert!(!s.actionable);
        assert!(s.blocked.as_deref().unwrap().starts_with("Core Windows service, cannot be changed"), "{:?}", s.blocked);
        let s = admin_info(Kind::Service, "AudioSrv_1a2b3", "");
        assert!(!s.actionable && s.blocked.is_some());
        let t = admin_info(Kind::Task, "ScheduledDefrag", r"\Microsoft\Windows\Defrag");
        assert!(t.actionable);
        assert_eq!(t.target.as_deref(), Some(r"\Microsoft\Windows\Defrag\ScheduledDefrag"));
        let t = admin_info(Kind::Task, "RootTask", r"\");
        assert_eq!(t.target.as_deref(), Some(r"\RootTask"));
        let t = admin_info(Kind::Task, "Windows Defender Scheduled Scan", r"\Microsoft\Windows\Windows Defender");
        assert!(!t.actionable && t.blocked.as_deref().unwrap().contains("Protected Windows task folder"));
        let t = admin_info(Kind::Task, "bad:name", r"\X");
        assert!(!t.actionable && t.blocked.is_some());
        for k in [Kind::Process, Kind::Startup, Kind::App] {
            let a = admin_info(k, "x.exe", "");
            assert!(!a.actionable && a.blocked.is_none() && a.target.is_none());
        }
    }

    #[test]
    fn timestamps_match_startup_format() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(rfc3339(1_793_786_399), "2026-11-04T09:59:59Z");
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
            set_startup_enabled, startup_changes,
            helper_status, arm_helper, disarm_helper, admin_set_service_start, admin_set_task_enabled,
            admin_changes, admin_undo
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
                    "quit" => quit_restoring(app),
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

    app.run(|_app, event| match event {
        // Closing the last window must not quit: the tray keeps running.
        RunEvent::ExitRequested { api, code, .. } => {
            if code.is_none() {
                api.prevent_exit();
            }
        }
        RunEvent::WindowEvent { label, event: WindowEvent::Destroyed, .. } if label == WINDOW_LABEL => on_window_closed(),
        _ => {}
    });
}
