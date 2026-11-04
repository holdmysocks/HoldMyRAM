//! In-app updater commands. The rules live in `holdmyram_core::update`; this module holds
//! the status, runs checks/downloads on short-lived worker threads, and performs the
//! install on the user's click.
//!
//! No timers: one delayed one-shot thread ~60 s after start (only if `auto_check_updates`)
//! and whatever the window asks for. A check younger than 6 hours is answered from the
//! cache file (%LOCALAPPDATA%\HoldMyRAM\update.json), so restarts don't re-hit the API.
//! Network code runs only in a short-lived `--update-worker` child process (WinHTTP and
//! its ~20 DLLs and threads never enter the tray process; see core `update::worker`).

use crate::{blocking, debug_log, helper_lock, revert_locked, rfc3339, rt, set_tray, ACTIVE, ARMED, BUSY, QUITTING};
use holdmyram_core::update::http::WinHttp;
use holdmyram_core::update::install::{self, InstallHost, RestoreOutcome};
use holdmyram_core::update::worker::{self, Line, Reply, Request};
use holdmyram_core::update::{self as up, CheckRecord, Staged, UrlPolicy, UrlRole, Version};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter};

pub const CURRENT: &str = env!("CARGO_PKG_VERSION");
/// Delay of the single automatic check after start.
const FIRST_CHECK_DELAY: Duration = Duration::from_secs(60);
/// How long a just-updated process waits for the old one to exit (hotkey, tray, files).
const PARENT_EXIT_WAIT_MS: u32 = 20_000;
const PROGRESS_EVERY: Duration = Duration::from_millis(250);

#[derive(Default)]
struct St {
    loaded: bool,
    phase: &'static str,
    progress: Option<f64>,
    record: Option<CheckRecord>,
    /// A download or install failure (overrides the record's error in the status).
    op_error: Option<(String, String)>,
    staged: Option<Staged>,
    updated_from: Option<String>,
}

static ST: Mutex<St> = Mutex::new(St {
    loaded: false,
    phase: "idle",
    progress: None,
    record: None,
    op_error: None,
    staged: None,
    updated_from: None,
});
static APP: OnceLock<AppHandle> = OnceLock::new();
/// A check or download worker is running.
static WORKER: AtomicBool = AtomicBool::new(false);
static UPDATED_FROM: OnceLock<String> = OnceLock::new();

fn st() -> MutexGuard<'static, St> {
    ST.lock().unwrap_or_else(|e| e.into_inner())
}

fn current() -> Version {
    Version::parse(CURRENT).expect("crate version is plain semver")
}

fn app_dir() -> Option<PathBuf> {
    std::env::current_exe().ok()?.parent().map(Path::to_path_buf)
}

fn updates_dir() -> PathBuf {
    up::data_dir().join("updates")
}

/// The app folder can be written (self-update possible). Checked once per process.
fn supported() -> bool {
    static S: OnceLock<bool> = OnceLock::new();
    *S.get_or_init(|| app_dir().is_some_and(|d| install::dir_writable(&d)))
}

fn emit() {
    if let Some(a) = APP.get() {
        let _ = a.emit("update-changed", ());
    }
}

fn set_phase(p: &'static str) {
    {
        let mut s = st();
        s.phase = p;
        if p != "downloading" {
            s.progress = None;
        }
    }
    emit();
}

/// Load the cache once (cheap file read).
fn ensure_loaded(s: &mut St) {
    if s.loaded {
        return;
    }
    s.loaded = true;
    let c = up::load_cache(&up::data_dir());
    s.record = c.last;
    s.updated_from = UPDATED_FROM.get().cloned();
}

#[derive(Serialize)]
pub struct UpdateStatus {
    current: &'static str,
    latest: Option<String>,
    available: bool,
    phase: &'static str,
    progress: Option<f64>,
    release_url: Option<String>,
    notes: Option<String>,
    checked_at: Option<String>,
    error: Option<String>,
    error_code: Option<String>,
    supported: bool,
    repo: &'static str,
    updated_from: Option<String>,
}

fn status_now() -> UpdateStatus {
    let supported = supported();
    let mut s = st();
    ensure_loaded(&mut s);
    let cur = current();
    let rec = s.record.clone().unwrap_or_default();
    let (error, error_code) = match &s.op_error {
        Some((code, msg)) => (Some(msg.clone()), Some(code.clone())),
        None => (rec.error.clone(), rec.error_code.clone()),
    };
    UpdateStatus {
        current: CURRENT,
        latest: rec.latest.clone(),
        available: rec.available(&cur),
        phase: s.phase,
        progress: s.progress,
        release_url: rec.release_url.clone().or_else(|| rec.latest.as_ref().map(|_| up::releases_page())),
        notes: rec.notes.clone(),
        checked_at: s.record.as_ref().filter(|r| r.checked_at > 0).map(|r| rfc3339(r.checked_at)),
        error,
        error_code,
        supported,
        repo: up::REPO,
        updated_from: s.updated_from.clone(),
    }
}

/// Never blocks on the network.
#[tauri::command]
pub fn update_status() -> UpdateStatus {
    status_now()
}

/// Check (honouring the 6-hour cache unless `force`), then pick up a staged download or
/// start one if `auto_download_updates` is on.
fn check_now(force: bool) {
    {
        let mut s = st();
        ensure_loaded(&mut s);
        if matches!(s.phase, "checking" | "downloading" | "installing") {
            return;
        }
    }
    let now = up::now_unix();
    let cached = st().record.clone().filter(|r| r.fresh(now));
    let rec = match cached {
        Some(r) if !force => r,
        _ => {
            if WORKER.swap(true, Ordering::SeqCst) {
                return;
            }
            set_phase("checking");
            let fail = |code: &str, msg: String| CheckRecord { checked_at: now, error: Some(msg), error_code: Some(code.into()), ..Default::default() };
            let rec = match run_worker(&Request::Check { current: CURRENT.into() }, &mut |_, _| {}) {
                Ok(Reply::Checked { record }) => record,
                Ok(Reply::Failed { code, msg }) => fail(&code, msg),
                Ok(other) => fail("internal", format!("unexpected worker reply {other:?}")),
                Err(e) => fail(e.code, e.msg),
            };
            WORKER.store(false, Ordering::SeqCst);
            match &rec.error {
                Some(e) => debug_log(&format!("update: check: {e}")),
                None => debug_log(&format!("update: check: latest {}", rec.latest.as_deref().unwrap_or("(none)"))),
            }
            let mut cache = up::load_cache(&up::data_dir());
            cache.last = Some(rec.clone());
            if let Err(e) = up::save_cache(&up::data_dir(), &cache) {
                debug_log(&format!("update: could not save the cache: {e}"));
            }
            {
                let mut s = st();
                s.record = Some(rec.clone());
                s.op_error = None;
                s.phase = "idle";
            }
            emit();
            rec
        }
    };
    let cur = current();
    if !rec.installable(&cur) {
        return;
    }
    let v = rec.latest_version().expect("installable has a version");
    if let Some(staged) = up::load_staged(&updates_dir(), &v) {
        let mut s = st();
        if s.phase == "idle" {
            s.staged = Some(staged);
            s.phase = "ready";
            drop(s);
            emit();
        }
        return;
    }
    if rt().config.auto_download_updates {
        start_download();
    }
}

#[tauri::command]
pub async fn check_for_update(force: Option<bool>) -> Result<UpdateStatus, String> {
    blocking(move || {
        check_now(force.unwrap_or(false));
        Ok(status_now())
    })
    .await
}

/// Start the background download if an installable update is known and nothing is
/// running or ready yet. One worker thread; it ends when done.
fn start_download() {
    let rec = {
        let mut s = st();
        ensure_loaded(&mut s);
        if matches!(s.phase, "checking" | "downloading" | "ready" | "installing") {
            return;
        }
        match s.record.clone() {
            Some(r) if r.installable(&current()) => r,
            _ => return,
        }
    };
    if !supported() {
        let mut s = st();
        s.phase = "error";
        s.op_error = Some(("not_writable".into(), "HoldMyRAM's folder is not writable, so it cannot update itself; download the new version from the release page".into()));
        drop(s);
        emit();
        return;
    }
    if WORKER.swap(true, Ordering::SeqCst) {
        return;
    }
    {
        let mut s = st();
        s.phase = "downloading";
        s.progress = Some(0.0);
        s.op_error = None;
    }
    emit();
    let spawned = std::thread::Builder::new().name("update-download".into()).spawn(move || {
        let mut last = Instant::now() - PROGRESS_EVERY;
        let mut on_progress = |done: u64, total: Option<u64>| {
            let total = total.or(rec.zip.as_ref().map(|z| z.size)).filter(|&t| t > 0);
            if let Some(t) = total {
                st().progress = Some((done as f64 / t as f64).clamp(0.0, 1.0));
                if last.elapsed() >= PROGRESS_EVERY {
                    last = Instant::now();
                    emit();
                }
            }
        };
        let req = Request::Download { record: rec.clone(), current: CURRENT.into(), updates_dir: updates_dir() };
        let r = match run_worker(&req, &mut on_progress) {
            Ok(Reply::Staged { staged }) => Ok(staged),
            Ok(Reply::Failed { code, msg }) => Err(up::UpdateError { code: code_str(&code), msg }),
            Ok(other) => Err(up::UpdateError::new("internal", format!("unexpected worker reply {other:?}"))),
            Err(e) => Err(e),
        };
        {
            let mut s = st();
            match r {
                Ok(staged) => {
                    debug_log(&format!("update: {} downloaded, verified and staged in {}", staged.version, staged.dir.display()));
                    s.staged = Some(staged);
                    s.phase = "ready";
                }
                Err(e) => {
                    debug_log(&format!("update: download failed: {e}"));
                    s.phase = "error";
                    s.op_error = Some((e.code.to_string(), e.msg));
                }
            }
            s.progress = None;
        }
        WORKER.store(false, Ordering::SeqCst);
        emit();
    });
    if spawned.is_err() {
        WORKER.store(false, Ordering::SeqCst);
        set_phase("idle");
    }
}

/// Error codes a worker can report, as `&'static str` (unknown ones become "internal").
fn code_str(c: &str) -> &'static str {
    const CODES: &[&str] = &[
        "offline", "network", "not_found", "rate_limited", "forbidden", "http_error", "bad_release", "bad_url", "no_asset",
        "bad_checksum", "bad_archive", "io", "no_update", "internal",
    ];
    CODES.iter().copied().find(|k| *k == c).unwrap_or("internal")
}

/// Run one request in a short-lived `holdmyram.exe --update-worker` child (see
/// `holdmyram_core::update::worker`): the tray process itself never loads WinHTTP, so
/// nothing network-related outlives the check. Blocks until the child exits.
fn run_worker(req: &Request, on_progress: &mut dyn FnMut(u64, Option<u64>)) -> Result<Reply, up::UpdateError> {
    use std::io::{BufRead, BufReader, Write};
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let fail = |m: String| up::UpdateError::new("internal", format!("update worker: {m}"));
    let exe = std::env::current_exe().map_err(|e| fail(e.to_string()))?;
    let mut child = Command::new(exe)
        .arg(worker::ARG)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map_err(|e| fail(format!("could not start: {e}")))?;
    let json = worker::request_line(req);
    if let Some(mut stdin) = child.stdin.take() {
        let _ = writeln!(stdin, "{json}");
    } // dropped: the child sees end of input
    let mut reply = None;
    if let Some(out) = child.stdout.take() {
        for line in BufReader::new(out).lines() {
            let Ok(line) = line else { break };
            match worker::parse_line(&line) {
                Line::Progress(d, t) => on_progress(d, t),
                Line::Done(r) => reply = Some(r),
                Line::Other => {}
            }
        }
    }
    let _ = child.wait();
    reply.ok_or_else(|| fail("ended without an answer".into()))
}

#[tauri::command]
pub fn download_update() -> UpdateStatus {
    start_download();
    status_now()
}

/// The release page (or the releases list), in the default browser. GitHub only.
#[tauri::command]
pub fn open_release_page() -> Result<(), String> {
    let url = st().record.as_ref().and_then(|r| r.release_url.clone()).unwrap_or_else(up::releases_page);
    UrlPolicy::RELEASE.check(&url, UrlRole::Page).map_err(|e| e.msg)?;
    crate::system_cmds::open_url(&url);
    Ok(())
}

/// The app side of the install.
struct AppHost {
    app: AppHandle,
}

impl InstallHost for AppHost {
    fn restore(&mut self) -> RestoreOutcome {
        if !ACTIVE.load(Ordering::SeqCst) {
            return RestoreOutcome::NotNeeded;
        }
        // Same path as quit_restoring: one admin prompt if services/tasks are pending.
        let mut guard = None;
        let report = revert_locked(&self.app, &mut guard);
        drop(guard);
        debug_log(&format!("update: restore before install: {}", report.details()));
        let still_active = report.needs_admin > 0;
        ACTIVE.store(still_active, Ordering::SeqCst);
        let tip = format!("HoldMyRAM: {} ({})", if still_active { "on" } else { "off" }, report.summary());
        set_tray(&self.app, still_active, &tip);
        let outcome = if report.needs_admin > 0 {
            RestoreOutcome::NeedsAdmin
        } else if !report.failed.is_empty() || report.error.is_some() {
            RestoreOutcome::Failed(format!("{} item(s) could not be restored", report.failed.len().max(1)))
        } else {
            RestoreOutcome::Restored
        };
        {
            let mut r = rt();
            r.last_report = report.summary();
            r.last_detail = Some(report);
        }
        let _ = self.app.emit("state-changed", ());
        outcome
    }

    fn stop_helper(&mut self) {
        if let Some(mut h) = helper_lock().take() {
            h.disarm(); // waits up to 3 s for it to exit
        }
        ARMED.store(false, Ordering::SeqCst);
    }

    fn launch(&mut self, exe: &Path, old_version: &str) -> Result<(), String> {
        let dir = exe.parent().ok_or("no folder")?;
        std::process::Command::new(exe)
            .arg("--updated-from")
            .arg(old_version)
            .arg("--parent-pid")
            .arg(std::process::id().to_string())
            .current_dir(dir)
            .spawn()
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}

/// Install the staged update (user's click only). On success the new version starts and
/// this process exits; otherwise everything is as it was and the error is returned.
#[tauri::command]
pub async fn install_update(app: AppHandle) -> Result<(), String> {
    blocking(move || {
        let staged = st().staged.clone().ok_or("no update is ready to install")?;
        let fail = |code: &str, msg: String, phase: &'static str| {
            let mut s = st();
            s.phase = phase;
            s.op_error = Some((code.to_string(), msg.clone()));
            drop(s);
            emit();
            Err::<(), String>(msg)
        };
        if !supported() {
            let _ = open_release_page();
            return fail("not_writable", "HoldMyRAM's folder is not writable, so it cannot replace itself; the release page was opened instead".into(), "error");
        }
        let Some(dir) = app_dir() else { return fail("swap_failed", "could not find HoldMyRAM's folder".into(), "error") };
        if BUSY.swap(true, Ordering::SeqCst) {
            return fail("busy", "a toggle is running; try again in a moment".into(), "ready");
        }
        set_phase("installing");
        let mut host = AppHost { app: app.clone() };
        match install::install(&mut host, &dir, &staged, CURRENT) {
            Ok(_) => {
                debug_log(&format!("update: installed {}; starting it and exiting {CURRENT}", staged.version));
                QUITTING.store(true, Ordering::SeqCst);
                app.exit(0);
                Ok(())
            }
            Err(e) => {
                BUSY.store(false, Ordering::SeqCst);
                debug_log(&format!("update: install failed: {e}"));
                // Restore problems leave the update ready; the user can retry after fixing them.
                let phase = if e.code.starts_with("restore_") || e.code == "no_update" { "ready" } else { "error" };
                if e.code == "not_writable" {
                    let _ = open_release_page();
                }
                fail(e.code, e.msg, phase)
            }
        }
    })
    .await
}

// ---------------------------------------------------------------------------------------
// Process start
// ---------------------------------------------------------------------------------------

/// Very first thing in `main`: handle `--updated-from <ver> --parent-pid <pid>` (wait for
/// the old process to exit so the hotkey, tray and files are free), delete `*.old`
/// leftovers and staged versions that are not newer than this one. Best effort.
pub fn early_startup() {
    let args: Vec<String> = std::env::args().collect();
    // Worker child: do the one network request and exit (never builds the app).
    if args.get(1).map(String::as_str) == Some(worker::ARG) {
        let code = worker::run(&mut std::io::stdin().lock(), &mut std::io::stdout().lock(), &WinHttp);
        std::process::exit(code);
    }
    let arg = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let from = arg("--updated-from").filter(|v| Version::parse(v).is_some());
    if let Some(pid) = arg("--parent-pid").and_then(|p| p.parse::<u32>().ok()) {
        wait_for_exit(pid);
    }
    if let Some(dir) = app_dir() {
        let removed = install::cleanup_old(&dir);
        if !removed.is_empty() {
            debug_log(&format!("update: removed leftovers {removed:?}"));
        }
    }
    up::prune_updates(&updates_dir(), &current());
    if let Some(from) = from {
        let note = format!("updated from {from} to {CURRENT} at {}", rfc3339(up::now_unix()));
        debug_log(&format!("update: {note}"));
        let mut cache = up::load_cache(&up::data_dir());
        cache.updated = Some(note);
        let _ = up::save_cache(&up::data_dir(), &cache);
        let _ = UPDATED_FROM.set(from);
    }
}

fn wait_for_exit(pid: u32) {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE};
    if pid == std::process::id() {
        return;
    }
    unsafe {
        if let Ok(h) = OpenProcess(PROCESS_SYNCHRONIZE, false, pid) {
            let _ = WaitForSingleObject(h, PARENT_EXIT_WAIT_MS);
            let _ = CloseHandle(h);
        }
    }
}

/// 60 s. Debug builds only: HOLDMYRAM_FIRST_CHECK_DELAY_SECS overrides it (for
/// scripts\measure-idle.ps1 -Debug). A release build has no code that reads it.
fn first_check_delay() -> Duration {
    #[cfg(debug_assertions)]
    if let Some(s) = std::env::var("HOLDMYRAM_FIRST_CHECK_DELAY_SECS").ok().and_then(|s| s.trim().parse::<u64>().ok()) {
        return Duration::from_secs(s.min(3600));
    }
    FIRST_CHECK_DELAY
}

/// In `setup`: remember the handle and start the single delayed check (a one-shot thread
/// that exits; nothing repeats).
pub fn on_setup(app: &AppHandle) {
    let _ = APP.set(app.clone());
    let _ = std::thread::Builder::new().name("update-first-check".into()).spawn(|| {
        std::thread::sleep(first_check_delay());
        if rt().config.auto_check_updates && !QUITTING.load(Ordering::SeqCst) {
            check_now(false);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_codes_map_to_known_codes() {
        for c in ["offline", "not_found", "rate_limited", "bad_checksum", "bad_archive", "no_asset", "network", "io"] {
            assert_eq!(code_str(c), c);
        }
        assert_eq!(code_str("something else"), "internal");
        assert_eq!(worker::ARG, "--update-worker");
    }
}
