//! Toggle on (`apply`) and off (`revert`).
//!
//! apply: split the selection into processes and admin items (services, tasks).
//! Processes: snapshot -> `plan::select_targets` (foreground tree, protected names and
//! self are skipped) -> extra guards (protected dirs, PID reuse, access) -> journal every
//! target as Planned and save -> act -> mark Done/Failed and save.
//! Admin items (after the processes, through the elevated helper): read the item's state
//! (read-only) -> guards (never-touch, foreground tree, HoldMyRAM/helper) -> journal the
//! intent as Planned and save -> ask the helper -> record exactly what it did and save.
//! A helper that dies mid-request leaves a Planned entry that revert can still finish.
//!
//! revert: walk the journal last-in, first-out across processes and admin items and undo
//! each Done (or crash-left Planned) entry, then clear the journal. Entries whose restore
//! failed, or that need the helper when none is available, stay in the journal so a later
//! revert can retry.

use crate::actions::{self, ProcHandle, RestoreStatus, DEFAULT_GRACE};
use crate::admin::client::AdminApi;
use crate::admin::handler::AdminBackend;
use crate::admin::proto::{AdminError, ErrorCode, Op, Reply, StartChoice, StartType};
use crate::journal::{self, AdminEntry, AdminOp, Entry, EntryState, Journal};
use crate::plan::{ancestors, descendants, select_targets, Skipped};
use crate::process::{list_processes, ProcInfo};
use crate::types::{Method, Selection, SelectionKind};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Shell processes whose focus means "no app in use". Their trees contain nearly every
/// app the user started, so treating them as the foreground app would protect everything.
const SHELL_NAMES: &[&str] = &["explorer.exe"];

#[derive(Debug, Clone)]
pub struct Options {
    pub journal_path: PathBuf,
    /// Wait between WM_CLOSE and TerminateProcess for Close.
    pub grace: Duration,
}

impl Default for Options {
    fn default() -> Self {
        Self { journal_path: journal::default_path(), grace: DEFAULT_GRACE }
    }
}

/// Why an item was skipped (or, for some failures, what kind of failure it was).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    /// Part of the foreground app's process tree (or hosts it).
    Foreground,
    /// HoldMyRAM itself or its helper.
    SelfProcess,
    /// Protected process name or directory.
    Protected,
    /// Service/task on the never-touch list, a driver, or otherwise refused by the helper.
    NeverTouch,
    /// Stopping it would also stop a protected service.
    ProtectedDependent,
    /// Windows denies the change even to an administrator.
    Locked,
    /// The service does not accept a stop request.
    NotStoppable,
    NotFound,
    /// Malformed name or path.
    Invalid,
    /// No admin helper was running.
    NotArmed,
    /// The user declined (or dismissed) the UAC prompt.
    UacCancelled,
    /// The helper could not be started, or its channel broke.
    HelperFailed,
    Timeout,
    /// Already in the target state (already stopped, disabled, running, enabled...).
    AlreadyDone,
    /// The user changed it after HoldMyRAM did; left as the user set it.
    ChangedByUser,
    /// Selected twice with different methods.
    Ambiguous,
}

impl Reason {
    pub fn from_code(code: ErrorCode) -> Option<Reason> {
        Some(match code {
            ErrorCode::BadRequest => Reason::Invalid,
            ErrorCode::NeverTouch => Reason::NeverTouch,
            ErrorCode::ProtectedDependent => Reason::ProtectedDependent,
            ErrorCode::NotFound => Reason::NotFound,
            ErrorCode::Locked => Reason::Locked,
            ErrorCode::NotStoppable => Reason::NotStoppable,
            ErrorCode::Timeout => Reason::Timeout,
            ErrorCode::UacCancelled => Reason::UacCancelled,
            ErrorCode::NotArmed => Reason::NotArmed,
            ErrorCode::Transport | ErrorCode::Handshake => Reason::HelperFailed,
            ErrorCode::Failed => return None,
        })
    }
}

/// One line of a report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Item {
    /// Process id; for a service the hosting process id when known, else 0; 0 for tasks.
    pub pid: u32,
    /// Executable name, service short name or full task path.
    pub name: String,
    pub kind: SelectionKind,
    pub method: Option<Method>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<Reason>,
    pub detail: String,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Report {
    /// "apply" or "revert".
    pub action: &'static str,
    pub acted: Vec<Item>,
    pub skipped: Vec<Item>,
    pub failed: Vec<Item>,
    /// Set when the whole operation was refused (e.g. journal unreadable).
    pub error: Option<String>,
    /// Service/task entries left in the journal after this run (to be restored later).
    pub admin_pending: usize,
    /// Of those, how many were left only because no helper was available (UAC declined,
    /// not armed, or the helper died). The next toggle off arms the helper and retries.
    pub needs_admin: usize,
}

impl Report {
    fn new(action: &'static str) -> Self {
        Self { action, ..Default::default() }
    }

    fn fatal(action: &'static str, msg: impl Into<String>) -> Self {
        Self { action, error: Some(msg.into()), ..Default::default() }
    }

    /// One line for logs and the tray tooltip.
    pub fn summary(&self) -> String {
        if let Some(e) = &self.error {
            return format!("{}: error: {e}", self.action);
        }
        let mut s = format!(
            "{}: {} acted, {} skipped, {} failed",
            self.action,
            self.acted.len(),
            self.skipped.len(),
            self.failed.len()
        );
        if self.needs_admin > 0 {
            s.push_str(&format!(", {} service/task item(s) need admin approval", self.needs_admin));
        }
        s
    }

    /// Multi-line detail for logs.
    pub fn details(&self) -> String {
        let mut out = self.summary();
        for (label, items) in [("acted", &self.acted), ("skipped", &self.skipped), ("failed", &self.failed)] {
            for i in items {
                let what = match i.kind {
                    SelectionKind::Process => format!("pid {}", i.pid),
                    SelectionKind::Service => "service".to_string(),
                    SelectionKind::Task => "task".to_string(),
                };
                let why = i.reason.map(|r| format!(" [{}]", reason_code(r))).unwrap_or_default();
                out.push_str(&format!("\n  {label} {} ({what}){why}: {}", i.name, i.detail));
            }
        }
        out
    }
}

fn reason_code(r: Reason) -> String {
    serde_json::to_value(r).ok().and_then(|v| v.as_str().map(String::from)).unwrap_or_default()
}

fn item(pid: u32, name: &str, method: Option<Method>, detail: impl Into<String>) -> Item {
    Item { pid, name: name.to_string(), kind: SelectionKind::Process, method, reason: None, detail: detail.into() }
}

fn admin_item(kind: SelectionKind, name: &str, pid: u32, reason: Option<Reason>, detail: impl Into<String>) -> Item {
    let method = Some(kind.default_method());
    Item { pid, name: name.to_string(), kind, method, reason, detail: detail.into() }
}

fn skip_reason(why: &Skipped) -> (&'static str, Reason) {
    match why {
        Skipped::Foreground => ("foreground app (or part of its process tree); left running", Reason::Foreground),
        Skipped::SelfProcess => ("HoldMyRAM itself", Reason::SelfProcess),
        Skipped::Protected => ("protected system process", Reason::Protected),
    }
}

/// Access to the elevated helper for one apply or revert.
pub enum AdminAccess<'a> {
    /// No helper: every service/task item is skipped (apply) or kept for later (revert)
    /// with the reason taken from this error's code (uac_cancelled, not_armed, ...).
    Unavailable(AdminError),
    Ready {
        api: &'a mut dyn AdminApi,
        /// Read-only, unelevated view of services (the app's own `admin::win::WinBackend`,
        /// or a fake in tests), used to record running dependents in the intent BEFORE a
        /// stop. Without it the intent names only the service itself.
        probe: Option<&'a mut dyn AdminBackend>,
    },
}

impl AdminAccess<'_> {
    pub fn not_armed() -> AdminAccess<'static> {
        AdminAccess::Unavailable(AdminError::new(ErrorCode::NotArmed, "the admin helper is not running"))
    }
}

/// Toggle on with the default journal location and grace period, processes only.
pub fn apply(selections: &[Selection], foreground_pid: Option<u32>, self_pid: u32) -> Report {
    apply_with(&Options::default(), selections, foreground_pid, self_pid)
}

/// Toggle off with the default journal location, without a helper (service/task entries
/// stay in the journal and are reported as needing admin approval).
pub fn revert() -> Report {
    revert_full(&Options::default(), std::process::id(), AdminAccess::not_armed())
}

/// Journal left by a previous session that still needs restoring (crash recovery).
pub fn pending_restore() -> Result<Option<Journal>, String> {
    journal::pending(&journal::default_path()).map_err(|e| e.to_string())
}

/// Number of service/task entries in the journal that still need restoring. Errors (an
/// unreadable journal) count as "unknown" and return None.
pub fn admin_pending_at(path: &std::path::Path) -> Option<usize> {
    match journal::load(path) {
        Ok(Some(j)) => Some(j.admin_pending()),
        Ok(None) => Some(0),
        Err(_) => None,
    }
}

/// True when the selection includes services or tasks (needs the helper).
pub fn has_admin_items(selections: &[Selection]) -> bool {
    selections.iter().any(|s| s.kind.is_admin())
}

/// The foreground PID that actually deserves protection: none when the shell or
/// HoldMyRAM itself has focus.
pub(crate) fn effective_foreground(procs: &[ProcInfo], fg: Option<u32>, self_pid: u32) -> Option<u32> {
    let fg = fg?;
    if fg == self_pid {
        return None;
    }
    match procs.iter().find(|p| p.pid == fg) {
        Some(p) if SHELL_NAMES.iter().any(|s| p.name.eq_ignore_ascii_case(s)) => None,
        _ => Some(fg),
    }
}

/// Order a group's PIDs so roots come first, then children by depth. Roots are the
/// members whose parent is not in the group.
fn order_group(members: &[&ProcInfo]) -> Vec<(u32, bool)> {
    let pids: HashSet<u32> = members.iter().map(|p| p.pid).collect();
    let parent: HashMap<u32, u32> = members.iter().map(|p| (p.pid, p.parent)).collect();
    let depth = |mut pid: u32| {
        let mut d = 0usize;
        let mut seen = HashSet::new();
        while let Some(&par) = parent.get(&pid) {
            if !pids.contains(&par) || !seen.insert(par) {
                break;
            }
            d += 1;
            pid = par;
        }
        d
    };
    let mut out: Vec<(u32, usize)> = members.iter().map(|p| (p.pid, depth(p.pid))).collect();
    out.sort_by_key(|&(pid, d)| (d, pid));
    out.into_iter().map(|(pid, d)| (pid, d == 0)).collect()
}

/// A target that passed every guard, journaled and ready to act on.
struct Prepared {
    handle: ProcHandle,
    entry_idx: usize,
}

/// Toggle on, processes only: any service/task selections are skipped as not_armed.
pub fn apply_with(opts: &Options, selections: &[Selection], foreground_pid: Option<u32>, self_pid: u32) -> Report {
    apply_full(opts, selections, foreground_pid, self_pid, AdminAccess::not_armed())
}

/// Toggle on: processes first (unchanged behaviour), then services and tasks through
/// `admin`.
pub fn apply_full(
    opts: &Options,
    selections: &[Selection],
    foreground_pid: Option<u32>,
    self_pid: u32,
    admin: AdminAccess<'_>,
) -> Report {
    let process_sels: Vec<Selection> = selections.iter().filter(|s| s.kind == SelectionKind::Process).cloned().collect();
    // Admin items, de-duplicated by kind + name (case-insensitive), in selection order.
    let mut seen: HashSet<(SelectionKind, String)> = HashSet::new();
    let admin_sels: Vec<Selection> = selections
        .iter()
        .filter(|s| s.kind.is_admin() && seen.insert((s.kind, s.name.to_lowercase())))
        .cloned()
        .collect();

    let procs = list_processes();
    let fg = effective_foreground(&procs, foreground_pid, self_pid);
    let mut report = apply_processes(opts, &process_sels, &procs, fg, self_pid);
    if admin_sels.is_empty() {
        return report;
    }
    if let Some(e) = &report.error {
        let e = e.clone();
        for s in &admin_sels {
            report.skipped.push(admin_item(s.kind, &s.name, 0, None, format!("not attempted: {e}")));
        }
        return report;
    }
    let mut guard: HashSet<u32> = HashSet::new();
    if let Some(fg) = fg {
        guard.extend(descendants(&procs, fg));
        guard.extend(ancestors(&procs, fg));
    }
    guard.remove(&0);
    apply_admin(opts, &admin_sels, &guard, self_pid, admin, &mut report);
    report
}

fn apply_processes(opts: &Options, selections: &[Selection], procs: &[ProcInfo], fg: Option<u32>, self_pid: u32) -> Report {
    let mut report = Report::new("apply");

    // Never overwrite a journal we cannot read: it may be the only record of what to restore.
    let mut journal = match journal::load(&opts.journal_path) {
        Ok(j) => j.unwrap_or_default(),
        Err(e) => return Report::fatal("apply", format!("{e}; refusing to act until it is restored or removed")),
    };
    if selections.is_empty() {
        return report;
    }

    // Resolve methods; a name selected twice with different methods is ambiguous.
    let mut methods: HashMap<String, Method> = HashMap::new();
    let mut ambiguous: HashSet<String> = HashSet::new();
    for s in selections {
        let key = s.name.to_lowercase();
        match methods.get(&key) {
            Some(m) if *m != s.method => {
                ambiguous.insert(key);
            }
            _ => {
                methods.insert(key, s.method);
            }
        }
    }

    let names: Vec<String> = methods.keys().cloned().collect();
    let (targets, skipped) = select_targets(procs, &names, fg, self_pid);
    for (p, why) in &skipped {
        let (text, reason) = skip_reason(why);
        let mut it = item(p.pid, &p.name, methods.get(&p.name.to_lowercase()).copied(), text);
        it.reason = Some(reason);
        report.skipped.push(it);
    }

    // Group by executable name, preserving first-seen order for determinism.
    let mut group_order: Vec<String> = Vec::new();
    let mut groups: HashMap<String, Vec<&ProcInfo>> = HashMap::new();
    for t in targets {
        let key = t.name.to_lowercase();
        if ambiguous.contains(&key) {
            let mut it = item(t.pid, &t.name, None, "selected twice with different methods; ambiguous");
            it.reason = Some(Reason::Ambiguous);
            report.skipped.push(it);
            continue;
        }
        if !groups.contains_key(&key) {
            group_order.push(key.clone());
        }
        groups.entry(key).or_default().push(t);
    }

    // Prepare: open handles, verify identity, apply hard guards, record entries.
    let first_new = journal.entries.len();
    let mut prepared_groups: Vec<(Method, Vec<Prepared>)> = Vec::new();
    for key in &group_order {
        let method = methods[key];
        let members = &groups[key];
        let by_pid: HashMap<u32, &ProcInfo> = members.iter().map(|p| (p.pid, *p)).collect();
        let mut ready: Vec<(ProcHandle, Entry)> = Vec::new();
        let mut group_skips: Vec<Item> = Vec::new();
        let mut root_failed = false;
        let mut protected_dir = false;
        for (pid, is_root) in order_group(members) {
            let p = by_pid[&pid];
            let fail = |why: String, skips: &mut Vec<Item>| skips.push(item(pid, &p.name, Some(method), why));
            let handle = match ProcHandle::open(pid, ProcHandle::rights_for(method)) {
                Ok(h) => h,
                Err(e) => {
                    root_failed |= is_root;
                    fail(format!("{e} (elevated or protected process?)"), &mut group_skips);
                    continue;
                }
            };
            let Some(ident) = handle.identity() else {
                root_failed |= is_root;
                fail("could not read executable path".into(), &mut group_skips);
                continue;
            };
            if !ident.name.eq_ignore_ascii_case(&p.name) {
                root_failed |= is_root;
                fail("process changed since the snapshot (PID reused)".into(), &mut group_skips);
                continue;
            }
            if actions::is_protected_path(&ident.exe) {
                protected_dir = true;
                fail(format!("runs from a protected directory: {}", ident.exe), &mut group_skips);
                if let Some(last) = group_skips.last_mut() {
                    last.reason = Some(Reason::Protected);
                }
                continue;
            }
            if journal.contains_live(pid, ident.created) {
                fail("already closed or frozen by an earlier toggle".into(), &mut group_skips);
                continue;
            }
            let entry = Entry {
                seq: 0, // assigned below
                pid,
                name: p.name.clone(),
                exe: ident.exe,
                command_line: if is_root { handle.command_line() } else { None },
                created: ident.created,
                method,
                state: EntryState::Planned,
                relaunch: is_root,
                note: None,
            };
            ready.push((handle, entry));
        }
        report.skipped.extend(group_skips);
        if (root_failed || protected_dir) && !ready.is_empty() {
            // Acting on part of an app (e.g. browser helpers without the browser) is
            // ambiguous and can lose data. Leave the whole app alone.
            for (_, e) in ready.drain(..) {
                report.skipped.push(item(e.pid, &e.name, Some(method), "main process could not be handled; left the whole app alone"));
            }
        }
        if ready.is_empty() {
            continue;
        }
        let mut prepared = Vec::new();
        for (handle, mut entry) in ready {
            entry.seq = journal.next_seq();
            journal.entries.push(entry);
            prepared.push(Prepared { handle, entry_idx: journal.entries.len() - 1 });
        }
        prepared_groups.push((method, prepared));
    }

    if prepared_groups.is_empty() {
        return report;
    }

    // Journal BEFORE any change. If it cannot be written, do nothing.
    if let Err(e) = journal::save(&opts.journal_path, &journal) {
        report.error = Some(format!("could not write journal ({e}); nothing was changed"));
        return report;
    }

    let set = |journal: &mut Journal, idx: usize, state: EntryState, note: &str| {
        journal.entries[idx].state = state;
        journal.entries[idx].note = Some(note.to_string());
    };

    // Suspend and Kill act immediately, in group order (roots first).
    for (method, group) in &prepared_groups {
        let result = |p: &Prepared| match method {
            Method::Suspend => p.handle.suspend().map(|_| "suspended"),
            Method::Kill => p.handle.terminate().map(|_| "terminated"),
            // Close is handled below; Stop/Disable never reach the process path.
            Method::Close | Method::Stop | Method::Disable => unreachable!(),
        };
        if !matches!(method, Method::Suspend | Method::Kill) {
            continue;
        }
        for p in group {
            match result(p) {
                Ok(note) => set(&mut journal, p.entry_idx, EntryState::Done, note),
                Err(e) => set(&mut journal, p.entry_idx, EntryState::Failed, &e),
            }
        }
    }
    let _ = journal::save(&opts.journal_path, &journal);

    // Close: ask every app at once, then share one grace period.
    let close_groups: Vec<&Vec<Prepared>> =
        prepared_groups.iter().filter(|(m, _)| *m == Method::Close).map(|(_, g)| g).collect();
    if !close_groups.is_empty() {
        for g in &close_groups {
            let pids: HashSet<u32> = g.iter().map(|p| p.handle.pid).collect();
            actions::post_close(&pids);
        }
        let deadline = Instant::now() + opts.grace;
        for g in &close_groups {
            let handles: Vec<&ProcHandle> = g.iter().map(|p| &p.handle).collect();
            let alive: HashSet<u32> = actions::wait_all_exit(&handles, deadline).into_iter().collect();
            if alive.is_empty() {
                for p in g.iter() {
                    set(&mut journal, p.entry_idx, EntryState::Done, "closed gracefully");
                }
                continue;
            }
            if actions::shows_dialog(&alive) {
                for p in g.iter() {
                    if alive.contains(&p.handle.pid) {
                        set(&mut journal, p.entry_idx, EntryState::Failed,
                            "showing a dialog after close (possible unsaved changes); left running");
                    } else {
                        set(&mut journal, p.entry_idx, EntryState::Done, "closed gracefully");
                    }
                }
                continue;
            }
            for p in g.iter() {
                if !alive.contains(&p.handle.pid) || p.handle.has_exited() {
                    set(&mut journal, p.entry_idx, EntryState::Done, "closed gracefully");
                    continue;
                }
                match p.handle.terminate() {
                    Ok(()) => set(&mut journal, p.entry_idx, EntryState::Done, "forced after grace period"),
                    // It may have exited between the check and the call.
                    Err(_) if p.handle.has_exited() => set(&mut journal, p.entry_idx, EntryState::Done, "closed gracefully"),
                    Err(e) => set(&mut journal, p.entry_idx, EntryState::Failed, &e),
                }
            }
        }
    }

    for e in &journal.entries[first_new..] {
        let detail = e.note.clone().unwrap_or_default();
        let it = item(e.pid, &e.name, Some(e.method), detail);
        match e.state {
            EntryState::Done => report.acted.push(it),
            _ => report.failed.push(it),
        }
    }

    let saved = if journal.has_pending() {
        journal::save(&opts.journal_path, &journal)
    } else {
        journal::clear(&opts.journal_path)
    };
    if let Err(e) = saved {
        report.error = Some(format!("actions done but journal update failed: {e}"));
    }
    report
}

// ---------------------------------------------------------------------------------------
// Services and tasks
// ---------------------------------------------------------------------------------------

/// Unavailable helper -> one skipped item per admin selection.
fn unavailable_detail(e: &AdminError) -> String {
    match e.code {
        ErrorCode::UacCancelled => "admin approval was declined; services and tasks were left alone".into(),
        ErrorCode::NotArmed => "admin helper not running; services and tasks were left alone".into(),
        _ => format!("admin helper unavailable ({e}); services and tasks were left alone"),
    }
}

/// State shared by the admin steps of one apply.
struct AdminRun<'r, 'a> {
    opts: &'r Options,
    api: &'r mut dyn AdminApi,
    probe: Option<&'r mut (dyn AdminBackend + 'a)>,
    journal: Journal,
    guard: &'r HashSet<u32>,
    /// HoldMyRAM's own PID and the helper's.
    own: [u32; 2],
    /// The helper stopped answering; later items are skipped without contacting it.
    dead: Option<AdminError>,
}

fn apply_admin(opts: &Options, sels: &[Selection], guard: &HashSet<u32>, self_pid: u32, admin: AdminAccess<'_>, report: &mut Report) {
    let (api, probe) = match admin {
        AdminAccess::Unavailable(e) => {
            let reason = Reason::from_code(e.code).unwrap_or(Reason::HelperFailed);
            for s in sels {
                report.skipped.push(admin_item(s.kind, &s.name, 0, Some(reason), unavailable_detail(&e)));
            }
            return;
        }
        AdminAccess::Ready { api, probe } => (api, probe),
    };
    // Proves the helper is alive and tells us its PID (its own services are off limits).
    let helper_pid = match api.call(Op::Ping) {
        Ok(Reply::Pong { pid, .. }) => pid,
        Ok(r) => return skip_all(sels, report, &AdminError::new(ErrorCode::Transport, format!("unexpected reply {r:?}"))),
        Err(e) => return skip_all(sels, report, &e),
    };
    let journal = match journal::load(&opts.journal_path) {
        Ok(j) => j.unwrap_or_default(),
        Err(e) => {
            report.error = Some(format!("{e}; services and tasks were not touched"));
            return;
        }
    };
    let mut run = AdminRun { opts, api, probe, journal, guard, own: [self_pid, helper_pid], dead: None };
    for s in sels {
        if let Some(e) = &run.dead {
            let e = e.clone();
            report.skipped.push(admin_item(s.kind, &s.name, 0, Some(Reason::HelperFailed), unavailable_detail(&e)));
            continue;
        }
        let r = match s.kind {
            SelectionKind::Service => run.stop_service(&s.name, report),
            SelectionKind::Task => run.disable_task(&s.name, report),
            SelectionKind::Process => Ok(()),
        };
        if let Err(e) = r {
            // The journal could not be written: stop touching anything.
            report.error = Some(e);
            break;
        }
    }
    let saved = if run.journal.has_pending() {
        journal::save(&opts.journal_path, &run.journal)
    } else {
        journal::clear(&opts.journal_path)
    };
    if let Err(e) = saved {
        report.error.get_or_insert(format!("journal update failed: {e}"));
    }
    report.admin_pending = run.journal.admin_pending();
}

fn skip_all(sels: &[Selection], report: &mut Report, e: &AdminError) {
    let reason = Reason::from_code(e.code).unwrap_or(Reason::HelperFailed);
    for s in sels {
        report.skipped.push(admin_item(s.kind, &s.name, 0, Some(reason), unavailable_detail(e)));
    }
}

/// Refusals the helper gives before changing anything: skip, never a failure.
fn is_refusal(code: ErrorCode) -> bool {
    matches!(
        code,
        ErrorCode::NeverTouch | ErrorCode::ProtectedDependent | ErrorCode::Locked | ErrorCode::NotStoppable | ErrorCode::NotFound | ErrorCode::BadRequest
    )
}

fn is_channel_error(code: ErrorCode) -> bool {
    matches!(code, ErrorCode::Transport | ErrorCode::NotArmed | ErrorCode::Handshake)
}

impl AdminRun<'_, '_> {
    fn save(&self) -> Result<(), String> {
        journal::save(&self.opts.journal_path, &self.journal).map_err(|e| format!("could not write journal ({e}); stopped before changing more"))
    }

    fn note_channel(&mut self, e: &AdminError) {
        if is_channel_error(e.code) {
            self.dead = Some(e.clone());
        }
    }

    /// Why a process with this PID must not lose its service, if it must not.
    fn pid_guard(&self, pid: u32) -> Option<(Reason, &'static str)> {
        if pid == 0 {
            None
        } else if self.guard.contains(&pid) {
            Some((Reason::Foreground, "runs in (or hosts) the foreground app's process tree; left running"))
        } else if self.own.contains(&pid) {
            Some((Reason::SelfProcess, "runs in HoldMyRAM's own process"))
        } else {
            None
        }
    }

    /// Running dependents of `name` in stop order, after checking each with the helper.
    /// Err: skip the service with this reason. Ok(None): no probe; intent names only the
    /// service.
    fn running_dependents(&mut self, name: &str) -> Result<Option<Vec<String>>, (Reason, String)> {
        let Some(probe) = self.probe.as_deref_mut() else { return Ok(None) };
        let deps = match probe.service_dependents(name) {
            Ok(d) => d,
            Err(_) => return Ok(None),
        };
        let mut out = Vec::new();
        for d in deps {
            let s = match self.api.service_query(&d) {
                Ok(s) => s,
                Err(e) if e.code == ErrorCode::NotFound => continue,
                Err(e) => {
                    self.note_channel(&e);
                    let r = Reason::from_code(e.code).unwrap_or(Reason::ProtectedDependent);
                    return Err((r, format!("could not check dependent \"{d}\": {e}")));
                }
            };
            if !s.running {
                continue;
            }
            if s.protected {
                return Err((Reason::ProtectedDependent, format!("dependent \"{d}\" is protected and would have to stop too")));
            }
            if let Some((r, why)) = self.pid_guard(s.pid) {
                return Err((r, format!("dependent \"{d}\" {why}")));
            }
            out.push(s.name);
        }
        Ok(Some(out))
    }

    fn stop_service(&mut self, name: &str, report: &mut Report) -> Result<(), String> {
        let k = SelectionKind::Service;
        let cur = match self.api.service_query(name) {
            Ok(s) => s,
            Err(e) => {
                self.note_channel(&e);
                report.skipped.push(admin_item(k, name, 0, Reason::from_code(e.code), e.to_string()));
                return Ok(());
            }
        };
        if cur.protected {
            let why = "on the never-touch list, a driver, or HoldMyRAM's own; the helper refuses to change it";
            report.skipped.push(admin_item(k, name, cur.pid, Some(Reason::NeverTouch), why));
            return Ok(());
        }
        if let Some((r, why)) = self.pid_guard(cur.pid) {
            report.skipped.push(admin_item(k, name, cur.pid, Some(r), why));
            return Ok(());
        }
        if !cur.running {
            report.skipped.push(admin_item(k, name, 0, Some(Reason::AlreadyDone), "already stopped"));
            return Ok(());
        }
        let deps = match self.running_dependents(name) {
            Ok(d) => d,
            Err((r, why)) => {
                report.skipped.push(admin_item(k, name, cur.pid, Some(r), why));
                return Ok(());
            }
        };
        let probed = deps.is_some();
        let mut planned = deps.unwrap_or_default();
        planned.push(name.to_string());

        // Intent first.
        let seq = self.journal.next_seq();
        self.journal.admin.push(AdminEntry {
            seq,
            state: EntryState::Planned,
            op: AdminOp::ServiceStop {
                name: name.to_string(),
                before_running: true,
                before_start: cur.start,
                before_delayed: cur.delayed_auto,
                planned: planned.clone(),
                stopped: Vec::new(),
            },
            note: (!probed).then(|| "dependents could not be listed beforehand".into()),
        });
        let idx = self.journal.admin.len() - 1;
        if let Err(e) = self.save() {
            self.journal.admin.pop();
            report.failed.push(admin_item(k, name, cur.pid, None, "not stopped: journal could not be written"));
            return Err(e);
        }

        let result = self.api.service_stop(name);
        let entry = &mut self.journal.admin[idx];
        let set_stopped = |entry: &mut AdminEntry, list: Vec<String>| {
            if let AdminOp::ServiceStop { stopped, .. } = &mut entry.op {
                *stopped = list;
            }
        };
        match result {
            Ok(rep) if rep.stopped.is_empty() => {
                entry.state = EntryState::Failed;
                entry.note = Some("was already stopped".into());
                report.skipped.push(admin_item(k, name, 0, Some(Reason::AlreadyDone), "already stopped"));
            }
            Ok(rep) => {
                let n = rep.stopped.len();
                let detail = if n > 1 {
                    format!("stopped, with {} running dependent(s) first: {}", n - 1, rep.stopped[..n - 1].join(", "))
                } else {
                    "stopped".to_string()
                };
                entry.state = EntryState::Done;
                entry.note = Some(detail.clone());
                set_stopped(entry, rep.stopped);
                report.acted.push(admin_item(k, name, cur.pid, None, detail));
            }
            Err(e) if !e.stopped.is_empty() => {
                // Failed part-way and the helper's rollback left some stopped: restore them later.
                let detail = format!("{e}; left stopped and journaled for restore: {}", e.stopped.join(", "));
                entry.state = EntryState::Done;
                entry.note = Some(detail.clone());
                set_stopped(entry, e.stopped.clone());
                report.failed.push(admin_item(k, name, cur.pid, Reason::from_code(e.code), detail));
            }
            Err(e) if is_refusal(e.code) => {
                entry.state = EntryState::Failed;
                entry.note = Some(e.to_string());
                report.skipped.push(admin_item(k, name, cur.pid, Reason::from_code(e.code), e.to_string()));
            }
            Err(e) => {
                // Unknown outcome (helper died, timed out, or failed): if the helper can
                // still tell us everything planned is running, nothing needs undoing;
                // otherwise keep the intent (Planned) for revert to check.
                self.note_channel(&e);
                let all_running = self.dead.is_none()
                    && planned.iter().all(|p| matches!(self.api.service_query(p), Ok(s) if s.running));
                let entry = &mut self.journal.admin[idx];
                let detail = if all_running {
                    entry.state = EntryState::Failed;
                    format!("{e}; nothing was left stopped")
                } else {
                    format!("{e}; outcome unknown, will be checked on restore")
                };
                entry.note = Some(detail.clone());
                report.failed.push(admin_item(k, name, cur.pid, Reason::from_code(e.code), detail));
            }
        }
        let _ = self.save();
        Ok(())
    }

    fn disable_task(&mut self, path: &str, report: &mut Report) -> Result<(), String> {
        let k = SelectionKind::Task;
        let cur = match self.api.task_query(path) {
            Ok(t) => t,
            Err(e) => {
                self.note_channel(&e);
                report.skipped.push(admin_item(k, path, 0, Reason::from_code(e.code), e.to_string()));
                return Ok(());
            }
        };
        if cur.protected {
            report.skipped.push(admin_item(k, path, 0, Some(Reason::NeverTouch), "in a never-touch task folder"));
            return Ok(());
        }
        if !cur.enabled {
            report.skipped.push(admin_item(k, path, 0, Some(Reason::AlreadyDone), "already disabled"));
            return Ok(());
        }
        let canonical = cur.path.clone();
        let seq = self.journal.next_seq();
        self.journal.admin.push(AdminEntry {
            seq,
            state: EntryState::Planned,
            op: AdminOp::TaskDisable { path: canonical.clone(), before_enabled: true },
            note: None,
        });
        let idx = self.journal.admin.len() - 1;
        if let Err(e) = self.save() {
            self.journal.admin.pop();
            report.failed.push(admin_item(k, path, 0, None, "not disabled: journal could not be written"));
            return Err(e);
        }
        match self.api.task_set_enabled(&canonical, false) {
            Ok(_) => {
                let entry = &mut self.journal.admin[idx];
                entry.state = EntryState::Done;
                entry.note = Some("disabled".into());
                report.acted.push(admin_item(k, &canonical, 0, None, "disabled"));
            }
            Err(e) if is_refusal(e.code) => {
                let entry = &mut self.journal.admin[idx];
                entry.state = EntryState::Failed;
                entry.note = Some(e.to_string());
                report.skipped.push(admin_item(k, &canonical, 0, Reason::from_code(e.code), e.to_string()));
            }
            Err(e) => {
                self.note_channel(&e);
                let still_enabled = self.dead.is_none() && matches!(self.api.task_query(&canonical), Ok(t) if t.enabled);
                let entry = &mut self.journal.admin[idx];
                let detail = if still_enabled {
                    entry.state = EntryState::Failed;
                    format!("{e}; task left enabled")
                } else {
                    format!("{e}; outcome unknown, will be checked on restore")
                };
                entry.note = Some(detail.clone());
                report.failed.push(admin_item(k, &canonical, 0, Reason::from_code(e.code), detail));
            }
        }
        let _ = self.save();
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------
// Revert
// ---------------------------------------------------------------------------------------

pub fn revert_with(opts: &Options, self_pid: u32) -> Report {
    revert_full(opts, self_pid, AdminAccess::not_armed())
}

enum Slot {
    Proc(usize),
    Admin(usize),
}

/// Outcome of restoring one admin entry.
enum AdminRestore {
    /// Fully handled; drop from the journal.
    Finished,
    /// Keep this (possibly trimmed) entry for a later retry. `no_helper`: kept only
    /// because the helper is unavailable.
    Keep { entry: AdminEntry, no_helper: bool },
}

pub fn revert_full(opts: &Options, self_pid: u32, admin: AdminAccess<'_>) -> Report {
    let mut report = Report::new("revert");
    let journal = match journal::load(&opts.journal_path) {
        Ok(Some(j)) => j,
        Ok(None) => return report,
        Err(e) => return Report::fatal("revert", e.to_string()),
    };

    // One helper check up front, only when there is something for it to do.
    let mut api: Result<&mut dyn AdminApi, AdminError> = match admin {
        AdminAccess::Unavailable(e) => Err(e),
        AdminAccess::Ready { api, .. } => Ok(api),
    };
    if journal.admin_pending() > 0 {
        if let Ok(a) = &mut api {
            if let Err(e) = a.call(Op::Ping) {
                api = Err(e);
            }
        }
    }

    let mut order: Vec<(u64, Slot)> = Vec::new();
    order.extend(journal.entries.iter().enumerate().map(|(i, e)| (e.seq, Slot::Proc(i))));
    order.extend(journal.admin.iter().enumerate().map(|(i, e)| (e.seq, Slot::Admin(i))));
    order.sort_by(|a, b| b.0.cmp(&a.0));

    let mut keep: Vec<Entry> = Vec::new();
    let mut keep_admin: Vec<AdminEntry> = Vec::new();
    for (_, slot) in order {
        match slot {
            Slot::Proc(idx) => {
                let e = &journal.entries[idx];
                if e.state == EntryState::Failed {
                    continue;
                }
                let r = actions::restore(e, self_pid);
                let mut detail = r.detail.clone();
                if let Some(pid) = r.new_pid {
                    detail.push_str(&format!(" as pid {pid}"));
                }
                let it = item(e.pid, &e.name, Some(e.method), detail);
                match r.status {
                    RestoreStatus::Restored => report.acted.push(it),
                    RestoreStatus::Skipped => report.skipped.push(it),
                    RestoreStatus::Failed => {
                        report.failed.push(it);
                        keep.push(e.clone());
                    }
                }
            }
            Slot::Admin(idx) => {
                let e = &journal.admin[idx];
                if e.state == EntryState::Failed {
                    continue;
                }
                let outcome = match &mut api {
                    Err(err) => {
                        let reason = Reason::from_code(err.code).unwrap_or(Reason::HelperFailed);
                        let kind = admin_kind(e);
                        report.skipped.push(admin_item(kind, e.target(), 0, Some(reason), "needs admin approval; kept for the next restore"));
                        AdminRestore::Keep { entry: e.clone(), no_helper: true }
                    }
                    Ok(a) => restore_admin(&mut **a, e, &mut report),
                };
                if api.is_ok() && matches!(outcome, AdminRestore::Keep { no_helper: true, .. }) {
                    // The helper died: don't talk to it again in this run.
                    api = Err(AdminError::new(ErrorCode::Transport, "the admin helper stopped responding"));
                }
                if let AdminRestore::Keep { entry, no_helper } = outcome {
                    if no_helper {
                        report.needs_admin += 1;
                    }
                    keep_admin.push(entry);
                }
            }
        }
    }

    let result = if keep.is_empty() && keep_admin.is_empty() {
        journal::clear(&opts.journal_path)
    } else {
        keep.sort_by_key(|e| e.seq);
        keep_admin.sort_by_key(|e| e.seq);
        let mut left = journal.clone();
        left.entries = keep;
        left.admin = keep_admin;
        report.admin_pending = left.admin_pending();
        journal::save(&opts.journal_path, &left)
    };
    if let Err(e) = result {
        report.error = Some(format!("journal update failed: {e}"));
    }
    report
}

fn admin_kind(e: &AdminEntry) -> SelectionKind {
    match e.op {
        AdminOp::ServiceStop { .. } => SelectionKind::Service,
        AdminOp::TaskDisable { .. } => SelectionKind::Task,
    }
}

fn start_type_name(s: StartType, delayed: Option<bool>) -> String {
    match s.to_choice(delayed) {
        Some(StartChoice::AutoDelayed) => "Automatic (delayed)".into(),
        _ => format!("{s:?}"),
    }
}

fn restore_admin(api: &mut dyn AdminApi, e: &AdminEntry, report: &mut Report) -> AdminRestore {
    match &e.op {
        AdminOp::ServiceStop { name, before_start, before_delayed, planned, stopped, .. } => {
            // Done: exactly what the helper stopped. Planned: the outcome is unknown, so
            // check everything we meant to stop (each was running when recorded).
            let list: Vec<String> = match e.state {
                EntryState::Done => stopped.clone(),
                _ if planned.is_empty() => vec![name.clone()],
                _ => planned.clone(),
            };
            let unknown = e.state != EntryState::Done;
            let k = SelectionKind::Service;
            let keep = |rest: &[String], no_helper: bool| {
                let mut entry = e.clone();
                if let AdminOp::ServiceStop { planned, stopped, .. } = &mut entry.op {
                    if entry.state == EntryState::Done {
                        *stopped = rest.to_vec();
                    } else {
                        *planned = rest.to_vec();
                    }
                }
                AdminRestore::Keep { entry, no_helper }
            };
            // Reverse stop order: the service itself first, then its dependents.
            for i in (0..list.len()).rev() {
                let n = &list[i];
                let cur = match api.service_query(n) {
                    Ok(s) => s,
                    Err(err) if err.code == ErrorCode::NotFound => {
                        report.skipped.push(admin_item(k, n, 0, Some(Reason::NotFound), "no longer exists"));
                        continue;
                    }
                    Err(err) => {
                        let no_helper = is_channel_error(err.code);
                        let reason = Reason::from_code(err.code);
                        let item = admin_item(k, n, 0, reason, format!("{err}; kept for retry"));
                        if no_helper {
                            report.skipped.push(item);
                        } else {
                            report.failed.push(item);
                        }
                        return keep(&list[..=i], no_helper);
                    }
                };
                if cur.running {
                    let why = if unknown { "running (was not stopped, or already started again)" } else { "already running again (started by someone else); left as is" };
                    report.skipped.push(admin_item(k, n, cur.pid, Some(Reason::AlreadyDone), why));
                    continue;
                }
                if cur.protected {
                    report.skipped.push(admin_item(k, n, 0, Some(Reason::NeverTouch), "the helper refuses to change it"));
                    continue;
                }
                let is_target = n.eq_ignore_ascii_case(name);
                let changed = if is_target {
                    before_start
                        .to_choice(*before_delayed)
                        .is_some_and(|c| !c.matches(cur.start, cur.delayed_auto))
                        || cur.start != *before_start
                } else {
                    cur.start == StartType::Disabled
                };
                if changed {
                    let was = if is_target { start_type_name(*before_start, *before_delayed) } else { "enabled".into() };
                    let detail = format!(
                        "start type changed since HoldMyRAM stopped it (was {was}, now {}); left stopped",
                        start_type_name(cur.start, cur.delayed_auto)
                    );
                    report.skipped.push(admin_item(k, n, 0, Some(Reason::ChangedByUser), detail));
                    continue;
                }
                match api.service_start(n) {
                    Ok(r) if r.started => report.acted.push(admin_item(k, n, 0, None, "started")),
                    Ok(_) => report.skipped.push(admin_item(k, n, 0, Some(Reason::AlreadyDone), "already running")),
                    Err(err) => {
                        let no_helper = is_channel_error(err.code);
                        let item = admin_item(k, n, 0, Reason::from_code(err.code), format!("{err}; kept for retry"));
                        if no_helper {
                            report.skipped.push(item);
                        } else {
                            report.failed.push(item);
                        }
                        // Its dependents (earlier in the list) cannot run without it.
                        return keep(&list[..=i], no_helper);
                    }
                }
            }
            AdminRestore::Finished
        }
        AdminOp::TaskDisable { path, before_enabled } => {
            let k = SelectionKind::Task;
            if !before_enabled {
                return AdminRestore::Finished;
            }
            let cur = match api.task_query(path) {
                Ok(t) => t,
                Err(err) if err.code == ErrorCode::NotFound => {
                    report.skipped.push(admin_item(k, path, 0, Some(Reason::NotFound), "no longer exists"));
                    return AdminRestore::Finished;
                }
                Err(err) => {
                    let no_helper = is_channel_error(err.code);
                    let item = admin_item(k, path, 0, Reason::from_code(err.code), format!("{err}; kept for retry"));
                    if no_helper {
                        report.skipped.push(item);
                    } else {
                        report.failed.push(item);
                    }
                    return AdminRestore::Keep { entry: e.clone(), no_helper };
                }
            };
            if cur.enabled {
                let why = if e.state == EntryState::Done { "already enabled again (changed by someone else)" } else { "enabled (was not disabled)" };
                report.skipped.push(admin_item(k, path, 0, Some(Reason::AlreadyDone), why));
                return AdminRestore::Finished;
            }
            if cur.protected {
                report.skipped.push(admin_item(k, path, 0, Some(Reason::NeverTouch), "in a never-touch task folder"));
                return AdminRestore::Finished;
            }
            match api.task_set_enabled(path, true) {
                Ok(_) => {
                    report.acted.push(admin_item(k, path, 0, None, "enabled"));
                    AdminRestore::Finished
                }
                Err(err) => {
                    let no_helper = is_channel_error(err.code);
                    let item = admin_item(k, path, 0, Reason::from_code(err.code), format!("{err}; kept for retry"));
                    if no_helper {
                        report.skipped.push(item);
                    } else {
                        report.failed.push(item);
                    }
                    AdminRestore::Keep { entry: e.clone(), no_helper }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actions::testutil::{spawn, SERIAL};
    use crate::journal::tests::temp_dir;

    fn p(pid: u32, parent: u32, name: &str) -> ProcInfo {
        ProcInfo { pid, parent, name: name.into(), working_set_bytes: 0 }
    }

    fn opts(tag: &str) -> Options {
        Options { journal_path: temp_dir(tag).join("journal.json"), grace: Duration::from_millis(500) }
    }

    fn sel(name: &str, method: Method) -> Vec<Selection> {
        vec![Selection::process(name, method)]
    }

    #[test]
    fn group_order_puts_roots_first() {
        let procs = [p(30, 20, "c.exe"), p(20, 10, "c.exe"), p(10, 1, "c.exe"), p(40, 10, "c.exe"), p(50, 2, "c.exe")];
        let refs: Vec<&ProcInfo> = procs.iter().collect();
        let order = order_group(&refs);
        assert_eq!(order, vec![(10, true), (50, true), (20, false), (40, false), (30, false)]);
    }

    #[test]
    fn shell_or_self_focus_protects_nothing() {
        let procs = [p(100, 4, "explorer.exe"), p(200, 100, "game.exe"), p(300, 4, "holdmyram.exe")];
        assert_eq!(effective_foreground(&procs, Some(100), 300), None);
        assert_eq!(effective_foreground(&procs, Some(300), 300), None);
        assert_eq!(effective_foreground(&procs, Some(200), 300), Some(200));
        assert_eq!(effective_foreground(&procs, None, 300), None);
    }

    #[test]
    fn foreground_child_is_never_touched() {
        let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let s = spawn("sleep");
        let o = opts("fg");
        let r = apply_with(&o, &sel(&s.name, Method::Kill), Some(s.pid()), std::process::id());
        assert!(r.acted.is_empty(), "{}", r.details());
        assert!(r.skipped.iter().any(|i| i.pid == s.pid() && i.detail.contains("foreground") && i.reason == Some(Reason::Foreground)));
        let h = ProcHandle::open(s.pid(), ProcHandle::rights_for(Method::Kill)).unwrap();
        assert!(!h.has_exited());
        assert!(journal::load(&o.journal_path).unwrap().is_none(), "nothing acted, no journal");
    }

    #[test]
    fn protected_directory_blocks_action() {
        // PING.EXE lives in System32: selected or not, it must be refused.
        let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let mut child = std::process::Command::new("ping")
            .args(["-n", "30", "127.0.0.1"])
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let o = opts("protdir");
        let r = apply_with(&o, &sel("PING.EXE", Method::Kill), None, std::process::id());
        let mine: Vec<_> = r.skipped.iter().filter(|i| i.pid == child.id()).collect();
        assert_eq!(mine.len(), 1, "{}", r.details());
        assert!(mine[0].detail.contains("protected directory"));
        assert!(!r.acted.iter().any(|i| i.pid == child.id()));
        assert!(child.try_wait().unwrap().is_none(), "ping must still be running");
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn kill_then_revert_relaunches() {
        let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let s = spawn("sleep");
        let o = opts("kill");
        let r = apply_with(&o, &sel(&s.name, Method::Kill), None, std::process::id());
        assert_eq!(r.acted.len(), 1, "{}", r.details());
        let j = journal::load(&o.journal_path).unwrap().unwrap();
        assert_eq!(j.entries[0].state, EntryState::Done);
        assert!(j.entries[0].relaunch);
        assert!(j.entries[0].command_line.as_deref().unwrap_or("").contains("sleeper_child_entry"));

        let r = revert_with(&o, std::process::id());
        assert_eq!(r.acted.len(), 1, "{}", r.details());
        assert!(journal::load(&o.journal_path).unwrap().is_none(), "journal cleared after revert");
        assert!(actions::running_with_exe(&s.exe.to_string_lossy(), std::process::id()).is_some());
    }

    #[test]
    fn close_graceful_window_then_revert() {
        let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let s = spawn("window");
        let o = Options { grace: Duration::from_secs(10), ..opts("closewin") };
        let start = Instant::now();
        let r = apply_with(&o, &sel(&s.name, Method::Close), None, std::process::id());
        assert_eq!(r.acted.len(), 1, "{}", r.details());
        assert_eq!(r.acted[0].detail, "closed gracefully");
        assert!(start.elapsed() < Duration::from_secs(9), "should not wait the full grace");
        let r = revert_with(&o, std::process::id());
        assert_eq!(r.acted.len(), 1, "{}", r.details());
    }

    #[test]
    fn close_forces_after_grace_when_no_window() {
        let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let s = spawn("sleep");
        let o = opts("closeforce");
        let r = apply_with(&o, &sel(&s.name, Method::Close), None, std::process::id());
        assert_eq!(r.acted.len(), 1, "{}", r.details());
        assert_eq!(r.acted[0].detail, "forced after grace period");
        let h = ProcHandle::open(s.pid(), ProcHandle::rights_for(Method::Kill));
        assert!(h.map(|h| h.wait_exit(Duration::from_secs(5))).unwrap_or(true));
        let _ = revert_with(&o, std::process::id());
    }

    #[test]
    fn suspend_then_revert_resumes_and_reapply_is_not_doubled() {
        let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let s = spawn("window");
        let o = opts("suspend");
        let r = apply_with(&o, &sel(&s.name, Method::Suspend), None, std::process::id());
        assert_eq!(r.acted.len(), 1, "{}", r.details());
        // Proof it is really frozen: a WM_CLOSE is not processed while suspended.
        let h = ProcHandle::open(s.pid(), ProcHandle::rights_for(Method::Kill)).unwrap();
        actions::post_close(&HashSet::from([s.pid()]));
        assert!(!h.wait_exit(Duration::from_millis(700)), "suspended process handled WM_CLOSE");
        // A second apply must not suspend the same process twice (one resume would not undo it).
        let r2 = apply_with(&o, &sel(&s.name, Method::Suspend), None, std::process::id());
        assert!(r2.acted.is_empty(), "{}", r2.details());
        assert!(r2.skipped.iter().any(|i| i.detail.contains("earlier toggle")));

        let r = revert_with(&o, std::process::id());
        assert_eq!(r.acted.len(), 1, "{}", r.details());
        assert_eq!(r.acted[0].detail, "resumed");
        // Once resumed, the queued WM_CLOSE is handled and the process exits.
        assert!(h.wait_exit(Duration::from_secs(10)), "process did not resume");
    }

    #[test]
    fn crash_left_planned_entries_are_recovered() {
        let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let s = spawn("sleep");
        let o = opts("crash");
        // Simulate a crash right after journaling, before suspending: entry is Planned.
        let h = ProcHandle::open(s.pid(), ProcHandle::rights_for(Method::Suspend)).unwrap();
        let ident = h.identity().unwrap();
        let mut j = Journal::new();
        j.entries.push(Entry {
            seq: 0, pid: s.pid(), name: ident.name, exe: ident.exe, command_line: None,
            created: ident.created, method: Method::Suspend, state: EntryState::Planned,
            relaunch: false, note: None,
        });
        journal::save(&o.journal_path, &j).unwrap();
        h.suspend().unwrap(); // the action did happen before the "crash"
        assert!(journal::pending(&o.journal_path).unwrap().is_some());
        let r = revert_with(&o, std::process::id());
        assert_eq!(r.acted.len(), 1, "{}", r.details());
        assert!(journal::pending(&o.journal_path).unwrap().is_none());
    }

    #[test]
    fn unreadable_journal_blocks_apply() {
        let o = opts("corrupt");
        std::fs::write(&o.journal_path, "{oops").unwrap();
        let r = apply_with(&o, &sel("nothing-real.exe", Method::Kill), None, std::process::id());
        assert!(r.error.is_some());
        assert_eq!(std::fs::read_to_string(&o.journal_path).unwrap(), "{oops", "must not overwrite");
    }

    #[test]
    fn revert_without_journal_is_a_noop() {
        let o = opts("empty");
        let r = revert_with(&o, std::process::id());
        assert!(r.error.is_none() && r.acted.is_empty() && r.failed.is_empty());
    }

    // -----------------------------------------------------------------------------------
    // Services and tasks, against the in-memory fake through the real handler and wire
    // format (LocalApi). No admin rights, no real service or task.
    // -----------------------------------------------------------------------------------

    use crate::admin::client::LocalApi;
    use crate::admin::fake::{FakeBackend, FakeService, FakeTask};
    use crate::admin::handler::{Handler, Policy};

    const DEFRAG: &str = r"\Microsoft\Windows\Defrag\ScheduledDefrag";
    const DEFENDER_SCAN: &str = r"\Microsoft\Windows\Windows Defender\Windows Defender Scheduled Scan";

    /// Fake service PIDs are odd numbers: real Windows PIDs are multiples of 4, so they can
    /// never collide with the real process tree the foreground guard is built from.
    fn world() -> FakeBackend {
        let mut b = FakeBackend::default();
        b.add(FakeService::running("Target", StartType::Auto).pid(3_000_001));
        b.add(FakeService::running("DepA", StartType::Manual).depends_on("Target").pid(3_000_003));
        b.add(FakeService::running("DepB", StartType::Manual).depends_on("DepA").pid(3_000_005));
        b.add(FakeService::stopped("DepStopped", StartType::Manual).depends_on("Target"));
        b.add(FakeService::running("SysMain", StartType::Auto).delayed(Some(true)).pid(3_000_007));
        b.add(FakeService::running("RpcSs", StartType::Auto).pid(3_000_009));
        b.add(FakeService::running("CodexSandboxService.OpenAI.Codex", StartType::Auto).locked().pid(3_000_011));
        b.add(FakeService::stopped("AlreadyOff", StartType::Manual));
        b.add_task(FakeTask::new(DEFRAG, true));
        b.add_task(FakeTask::new(DEFENDER_SCAN, true));
        b.add_task(FakeTask::new(r"\Locked\Task", true).locked());
        b.add_task(FakeTask::new(r"\Off\Task", false));
        b
    }

    fn api(b: FakeBackend) -> LocalApi<FakeBackend> {
        LocalApi::new(Handler::new(b, Policy::new(vec![])))
    }

    fn mutations(a: &LocalApi<FakeBackend>) -> Vec<String> {
        a.handler.backend().mutations()
    }

    fn svc(a: &LocalApi<FakeBackend>, n: &str) -> FakeService {
        a.handler.backend().service(n).clone()
    }

    /// Apply with the fake as both the helper and the unelevated probe.
    fn apply_admin_only(o: &Options, a: &mut LocalApi<FakeBackend>, sels: &[Selection], fg: Option<u32>) -> Report {
        let mut probe = world();
        apply_full(o, sels, fg, u32::MAX - 1, AdminAccess::Ready { api: a, probe: Some(&mut probe) })
    }

    fn revert_admin(o: &Options, a: &mut dyn AdminApi) -> Report {
        revert_full(o, std::process::id(), AdminAccess::Ready { api: a, probe: None })
    }

    fn reasons(items: &[Item]) -> Vec<(String, Option<Reason>)> {
        items.iter().map(|i| (i.name.clone(), i.reason)).collect()
    }

    #[test]
    fn service_stop_with_dependents_then_revert_in_reverse_order() {
        let o = opts("svc-stop");
        let mut a = api(world());
        let r = apply_admin_only(&o, &mut a, &[Selection::service("Target")], None);
        assert_eq!(r.acted.len(), 1, "{}", r.details());
        assert!(r.acted[0].detail.contains("DepB, DepA"), "{}", r.details());
        assert_eq!(mutations(&a), vec!["stop DepB", "stop DepA", "stop Target"]);
        assert_eq!(r.admin_pending, 1);

        let j = journal::load(&o.journal_path).unwrap().unwrap();
        assert_eq!(j.admin.len(), 1);
        let AdminOp::ServiceStop { before_running, before_start, planned, stopped, .. } = &j.admin[0].op else { panic!() };
        assert!(*before_running);
        assert_eq!(*before_start, StartType::Auto);
        assert_eq!(planned, &vec!["DepB", "DepA", "Target"]);
        assert_eq!(stopped, &vec!["DepB", "DepA", "Target"]);
        assert_eq!(j.admin[0].state, EntryState::Done);
        // Start type is never touched by a stop.
        assert_eq!(svc(&a, "Target").start, StartType::Auto);

        let r = revert_admin(&o, &mut a);
        assert_eq!(r.acted.iter().map(|i| i.name.as_str()).collect::<Vec<_>>(), vec!["Target", "DepA", "DepB"], "{}", r.details());
        assert_eq!(&mutations(&a)[3..], ["start Target", "start DepA", "start DepB"]);
        for s in ["Target", "DepA", "DepB"] {
            assert!(svc(&a, s).running, "{s}");
        }
        assert!(!svc(&a, "DepStopped").running, "was stopped before; stays stopped");
        assert!(journal::load(&o.journal_path).unwrap().is_none());
    }

    #[test]
    fn revert_does_not_start_what_the_user_changed() {
        let o = opts("svc-user");
        let mut a = api(world());
        let r = apply_admin_only(&o, &mut a, &[Selection::service("Target"), Selection::service("SysMain")], None);
        assert_eq!(r.acted.len(), 2, "{}", r.details());
        // Meanwhile: the user starts DepA... (which also needs Target running)
        a.handler.backend_mut().set_running("Target", true);
        a.handler.backend_mut().set_running("DepA", true);
        // ...and switches SysMain to Manual.
        a.handler.backend_mut().service_mut("SysMain").start = StartType::Manual;
        let before = mutations(&a).len();

        let r = revert_admin(&o, &mut a);
        // Only DepB is started; SysMain stays stopped with the user's start type.
        assert_eq!(&mutations(&a)[before..], ["start DepB"], "{}", r.details());
        assert!(!svc(&a, "SysMain").running);
        assert_eq!(svc(&a, "SysMain").start, StartType::Manual);
        let rs = reasons(&r.skipped);
        assert!(rs.contains(&("SysMain".into(), Some(Reason::ChangedByUser))), "{rs:?}");
        assert!(rs.contains(&("Target".into(), Some(Reason::AlreadyDone))), "{rs:?}");
        assert!(rs.contains(&("DepA".into(), Some(Reason::AlreadyDone))), "{rs:?}");
        assert!(r.failed.is_empty());
        assert!(journal::load(&o.journal_path).unwrap().is_none(), "nothing left to retry");

        // A delayed-auto service whose flag the user cleared counts as changed too.
        let o = opts("svc-delayed");
        let mut a = api(world());
        apply_admin_only(&o, &mut a, &[Selection::service("SysMain")], None);
        a.handler.backend_mut().service_mut("SysMain").delayed_auto = Some(false);
        let r = revert_admin(&o, &mut a);
        assert!(!svc(&a, "SysMain").running, "{}", r.details());
        assert_eq!(r.skipped[0].reason, Some(Reason::ChangedByUser));
    }

    #[test]
    fn task_disable_then_reenable_and_user_change_respected() {
        let o = opts("task");
        let mut a = api(world());
        let r = apply_admin_only(&o, &mut a, &[Selection::task(r"\microsoft\windows\defrag\scheduleddefrag")], None);
        assert_eq!(r.acted.len(), 1, "{}", r.details());
        assert_eq!(r.acted[0].name, DEFRAG, "canonical path reported");
        assert!(!a.handler.backend().task(DEFRAG).enabled);
        let j = journal::load(&o.journal_path).unwrap().unwrap();
        assert_eq!(j.admin[0].op, AdminOp::TaskDisable { path: DEFRAG.into(), before_enabled: true });
        let r = revert_admin(&o, &mut a);
        assert_eq!(r.acted.len(), 1, "{}", r.details());
        assert!(a.handler.backend().task(DEFRAG).enabled);
        assert!(journal::load(&o.journal_path).unwrap().is_none());

        // The user re-enables it themselves: revert changes nothing.
        apply_admin_only(&o, &mut a, &[Selection::task(DEFRAG)], None);
        a.task_set_enabled(DEFRAG, true).unwrap();
        let n = mutations(&a).len();
        let r = revert_admin(&o, &mut a);
        assert_eq!(mutations(&a).len(), n, "{}", r.details());
        assert_eq!(r.skipped[0].reason, Some(Reason::AlreadyDone));

        // Already disabled: skipped, nothing journaled.
        let r = apply_admin_only(&o, &mut a, &[Selection::task(r"\Off\Task")], None);
        assert_eq!(r.skipped[0].reason, Some(Reason::AlreadyDone));
        assert!(journal::load(&o.journal_path).unwrap().is_none());
    }

    #[test]
    fn never_touch_and_locked_are_skipped_without_failing_the_rest() {
        let o = opts("svc-mixed");
        let mut a = api(world());
        let sels = vec![
            Selection::service("RpcSs"),
            Selection::service("CodexSandboxService.OpenAI.Codex"),
            Selection::task(DEFENDER_SCAN),
            Selection::task(r"\Locked\Task"),
            Selection::service("NoSuchService"),
            Selection::service("AlreadyOff"),
            Selection::service("SysMain"),
            Selection::task(DEFRAG),
        ];
        let r = apply_admin_only(&o, &mut a, &sels, None);
        assert!(r.error.is_none() && r.failed.is_empty(), "{}", r.details());
        let acted: Vec<&str> = r.acted.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(acted, vec!["SysMain", DEFRAG]);
        let rs = reasons(&r.skipped);
        for want in [
            ("RpcSs", Reason::NeverTouch),
            ("CodexSandboxService.OpenAI.Codex", Reason::Locked),
            (DEFENDER_SCAN, Reason::NeverTouch),
            (r"\Locked\Task", Reason::Locked),
            ("NoSuchService", Reason::NotFound),
            ("AlreadyOff", Reason::AlreadyDone),
        ] {
            assert!(rs.contains(&(want.0.to_string(), Some(want.1))), "{want:?} in {rs:?}");
        }
        assert!(svc(&a, "RpcSs").running && svc(&a, "CodexSandboxService.OpenAI.Codex").running);
        assert!(a.handler.backend().task(DEFENDER_SCAN).enabled);
        // Only the two real changes are journaled as pending.
        let j = journal::load(&o.journal_path).unwrap().unwrap();
        assert_eq!(j.admin_pending(), 2);
        let r = revert_admin(&o, &mut a);
        assert_eq!(r.acted.len(), 2, "{}", r.details());
        assert!(journal::load(&o.journal_path).unwrap().is_none());
    }

    #[test]
    fn uac_cancelled_means_processes_only() {
        let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let s = spawn("sleep");
        let o = opts("uac");
        let sels = vec![Selection::process(&s.name, Method::Kill), Selection::service("SysMain"), Selection::task(DEFRAG)];
        let declined = AdminError::new(ErrorCode::UacCancelled, "the administrator prompt was cancelled");
        let r = apply_full(&o, &sels, None, std::process::id(), AdminAccess::Unavailable(declined));
        assert_eq!(r.acted.len(), 1, "{}", r.details());
        assert_eq!(r.acted[0].kind, SelectionKind::Process);
        assert_eq!(reasons(&r.skipped), vec![("SysMain".into(), Some(Reason::UacCancelled)), (DEFRAG.into(), Some(Reason::UacCancelled))]);
        assert!(r.failed.is_empty() && r.error.is_none());
        let j = journal::load(&o.journal_path).unwrap().unwrap();
        assert!(j.admin.is_empty(), "nothing admin journaled");
        let _ = revert_with(&o, std::process::id());
    }

    #[test]
    fn foreground_owned_service_is_skipped() {
        let o = opts("svc-fg");
        let me = std::process::id();
        let mut b = world();
        b.add(FakeService::running("FgHost", StartType::Manual).pid(me));
        b.add(FakeService::running("HasFgDependent", StartType::Manual).pid(3_000_101));
        b.add(FakeService::running("FgDep", StartType::Manual).depends_on("HasFgDependent").pid(me));
        let mut a = api(b);
        let mut probe = world();
        probe.add(FakeService::running("FgHost", StartType::Manual).pid(me));
        probe.add(FakeService::running("HasFgDependent", StartType::Manual).pid(3_000_101));
        probe.add(FakeService::running("FgDep", StartType::Manual).depends_on("HasFgDependent").pid(me));
        // This test process plays the foreground app.
        let sels = [Selection::service("FgHost"), Selection::service("HasFgDependent"), Selection::service("SysMain")];
        let r = apply_full(&o, &sels, Some(me), u32::MAX - 1, AdminAccess::Ready { api: &mut a, probe: Some(&mut probe) });
        let rs = reasons(&r.skipped);
        assert!(rs.contains(&("FgHost".into(), Some(Reason::Foreground))), "{}", r.details());
        assert!(rs.contains(&("HasFgDependent".into(), Some(Reason::Foreground))), "{}", r.details());
        assert_eq!(r.acted.len(), 1);
        assert_eq!(mutations(&a), vec!["stop SysMain"]);
        let _ = revert_admin(&o, &mut a);
    }

    #[test]
    fn helper_pid_and_self_pid_services_are_skipped() {
        let o = opts("svc-self");
        let mut b = world();
        // LocalApi's "helper" is this test process.
        b.add(FakeService::running("InHelper", StartType::Manual).pid(std::process::id()));
        b.add(FakeService::running("InApp", StartType::Manual).pid(4242));
        let mut a = api(b);
        let sels = [Selection::service("InHelper"), Selection::service("InApp")];
        let r = apply_full(&o, &sels, None, 4242, AdminAccess::Ready { api: &mut a, probe: None });
        assert_eq!(reasons(&r.skipped), vec![("InHelper".into(), Some(Reason::SelfProcess)), ("InApp".into(), Some(Reason::SelfProcess))]);
        assert!(mutations(&a).is_empty());
    }

    /// Wraps LocalApi: on the first mutating op the "helper" does the work and then dies
    /// before replying; every later call fails as not armed until `revive`.
    struct Dying {
        inner: LocalApi<FakeBackend>,
        die_on_mutation: bool,
        dead: bool,
    }

    impl AdminApi for Dying {
        fn call(&mut self, op: Op) -> Result<Reply, AdminError> {
            if self.dead {
                return Err(AdminError::new(ErrorCode::NotArmed, "the admin helper is not running"));
            }
            if self.die_on_mutation && op.is_mutating() {
                let _ = self.inner.call(op); // the work happens...
                self.dead = true; // ...but the reply never arrives
                return Err(AdminError::new(ErrorCode::Transport, "admin helper channel failed: pipe broken"));
            }
            self.inner.call(op)
        }
    }

    #[test]
    fn helper_death_mid_apply_leaves_a_journal_that_revert_finishes() {
        let o = opts("svc-dies");
        let mut d = Dying { inner: api(world()), die_on_mutation: true, dead: false };
        let mut probe = world();
        let sels = [Selection::service("Target"), Selection::task(DEFRAG)];
        let r = apply_full(&o, &sels, None, u32::MAX - 1, AdminAccess::Ready { api: &mut d, probe: Some(&mut probe) });
        // The stop happened but its outcome is unknown; the task was not attempted.
        assert_eq!(r.failed.len(), 1, "{}", r.details());
        assert_eq!(r.failed[0].reason, Some(Reason::HelperFailed));
        assert!(r.skipped.iter().any(|i| i.name == DEFRAG && i.reason == Some(Reason::HelperFailed)), "{}", r.details());
        assert_eq!(d.inner.handler.backend().mutations(), vec!["stop DepB", "stop DepA", "stop Target"]);
        assert!(d.inner.handler.backend().task(DEFRAG).enabled);

        let j = journal::load(&o.journal_path).unwrap().unwrap();
        assert_eq!(j.admin.len(), 1);
        assert_eq!(j.admin[0].state, EntryState::Planned, "intent survives the helper's death");
        let AdminOp::ServiceStop { planned, stopped, .. } = &j.admin[0].op else { panic!() };
        assert_eq!(planned, &vec!["DepB", "DepA", "Target"]);
        assert!(stopped.is_empty());

        // Revert while the helper is still dead: kept, needs admin.
        let r = revert_admin(&o, &mut d);
        assert_eq!((r.needs_admin, r.admin_pending), (1, 1), "{}", r.details());
        assert!(journal::pending(&o.journal_path).unwrap().is_some());

        // A new helper: revert restarts all three in reverse stop order.
        d.dead = false;
        d.die_on_mutation = false;
        let r = revert_admin(&o, &mut d);
        assert_eq!(r.acted.iter().map(|i| i.name.as_str()).collect::<Vec<_>>(), vec!["Target", "DepA", "DepB"], "{}", r.details());
        assert_eq!(r.needs_admin, 0);
        assert!(journal::load(&o.journal_path).unwrap().is_none());
    }

    #[test]
    fn helper_death_mid_revert_keeps_the_rest_for_retry() {
        let o = opts("svc-dies-revert");
        let mut a = api(world());
        apply_admin_only(&o, &mut a, &[Selection::service("Target"), Selection::task(DEFRAG)], None);
        let mut d = Dying { inner: a, die_on_mutation: true, dead: false };
        // LIFO: the task (seq 1) is re-enabled first; the helper dies doing that.
        let r = revert_admin(&o, &mut d);
        assert_eq!(r.needs_admin, 2, "{}", r.details());
        let j = journal::load(&o.journal_path).unwrap().unwrap();
        assert_eq!(j.admin.len(), 2);
        d.dead = false;
        d.die_on_mutation = false;
        let r = revert_admin(&o, &mut d);
        assert!(r.failed.is_empty(), "{}", r.details());
        // The task was in fact re-enabled before the helper died: now reported as already done.
        assert!(r.skipped.iter().any(|i| i.name == DEFRAG && i.reason == Some(Reason::AlreadyDone)));
        assert!(journal::load(&o.journal_path).unwrap().is_none());
        assert!(svc(&d.inner, "Target").running && svc(&d.inner, "DepB").running);
    }

    #[test]
    fn revert_without_helper_keeps_admin_entries_and_restores_processes() {
        let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let s = spawn("sleep");
        let o = opts("svc-nohelper");
        let mut a = api(world());
        let mut probe = world();
        let sels = vec![Selection::process(&s.name, Method::Kill), Selection::service("SysMain")];
        let r = apply_full(&o, &sels, None, std::process::id(), AdminAccess::Ready { api: &mut a, probe: Some(&mut probe) });
        assert_eq!(r.acted.len(), 2, "{}", r.details());

        let declined = AdminError::new(ErrorCode::UacCancelled, "cancelled");
        let r = revert_full(&o, std::process::id(), AdminAccess::Unavailable(declined));
        assert_eq!(r.acted.len(), 1, "process relaunched: {}", r.details());
        assert_eq!((r.needs_admin, r.admin_pending), (1, 1));
        assert!(r.summary().contains("need admin approval"));
        assert_eq!(r.skipped.iter().find(|i| i.name == "SysMain").unwrap().reason, Some(Reason::UacCancelled));
        let j = journal::load(&o.journal_path).unwrap().unwrap();
        assert!(j.entries.is_empty() && j.admin.len() == 1, "{j:?}");

        let r = revert_admin(&o, &mut a);
        assert_eq!(r.acted.len(), 1, "{}", r.details());
        assert!(svc(&a, "SysMain").running);
        assert!(journal::load(&o.journal_path).unwrap().is_none());
    }

    #[test]
    fn failed_restart_stays_in_journal_with_the_remainder() {
        let o = opts("svc-startfail");
        let mut a = api(world());
        apply_admin_only(&o, &mut a, &[Selection::service("Target")], None);
        a.handler.backend_mut().service_mut("Target").start_fails = true;
        let r = revert_admin(&o, &mut a);
        assert_eq!(r.failed.len(), 1, "{}", r.details());
        assert_eq!(r.needs_admin, 0);
        let j = journal::load(&o.journal_path).unwrap().unwrap();
        let AdminOp::ServiceStop { stopped, .. } = &j.admin[0].op else { panic!() };
        assert_eq!(stopped, &vec!["DepB", "DepA", "Target"], "nothing started; all kept");
        a.handler.backend_mut().service_mut("Target").start_fails = false;
        let r = revert_admin(&o, &mut a);
        assert_eq!(r.acted.len(), 3, "{}", r.details());
        assert!(journal::load(&o.journal_path).unwrap().is_none());
    }

    #[test]
    fn partial_stop_leftovers_are_journaled() {
        let o = opts("svc-partial");
        let mut b = world();
        b.service_mut("Target").stop_fails = true;
        b.service_mut("DepB").start_fails = true; // helper's rollback cannot restart DepB
        let mut a = api(b);
        let r = apply_admin_only(&o, &mut a, &[Selection::service("Target")], None);
        assert_eq!(r.failed.len(), 1, "{}", r.details());
        let j = journal::load(&o.journal_path).unwrap().unwrap();
        assert_eq!(j.admin[0].state, EntryState::Done);
        let AdminOp::ServiceStop { stopped, .. } = &j.admin[0].op else { panic!() };
        assert_eq!(stopped, &vec!["DepB"]);
        a.handler.backend_mut().service_mut("DepB").start_fails = false;
        let r = revert_admin(&o, &mut a);
        assert_eq!(r.acted.len(), 1, "{}", r.details());
        assert!(svc(&a, "DepB").running);
    }

    #[test]
    fn journal_is_written_before_each_admin_change() {
        struct Spy<'p> {
            inner: LocalApi<FakeBackend>,
            path: &'p std::path::Path,
            seen: Vec<(String, EntryState)>,
        }
        impl AdminApi for Spy<'_> {
            fn call(&mut self, op: Op) -> Result<Reply, AdminError> {
                if op.is_mutating() {
                    let j = journal::load(self.path).unwrap().expect("journal exists before the change");
                    let last = j.admin.last().unwrap();
                    self.seen.push((last.target().to_string(), last.state));
                }
                self.inner.call(op)
            }
        }
        let o = opts("svc-order");
        let mut spy = Spy { inner: api(world()), path: &o.journal_path, seen: Vec::new() };
        let r = apply_full(&o, &[Selection::service("SysMain"), Selection::task(DEFRAG)], None, 1, AdminAccess::Ready { api: &mut spy, probe: None });
        assert_eq!(r.acted.len(), 2, "{}", r.details());
        assert_eq!(spy.seen, vec![("SysMain".into(), EntryState::Planned), (DEFRAG.into(), EntryState::Planned)]);
    }

    #[test]
    fn processes_then_admin_share_one_lifo_sequence() {
        let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let s = spawn("sleep");
        let o = opts("svc-lifo");
        let mut a = api(world());
        let mut probe = world();
        let sels = vec![Selection::service("SysMain"), Selection::process(&s.name, Method::Kill), Selection::task(DEFRAG)];
        let r = apply_full(&o, &sels, None, std::process::id(), AdminAccess::Ready { api: &mut a, probe: Some(&mut probe) });
        assert_eq!(r.acted.len(), 3, "{}", r.details());
        let j = journal::load(&o.journal_path).unwrap().unwrap();
        // Processes first (seq 0), then admin items in selection order.
        assert_eq!(j.entries[0].seq, 0);
        assert_eq!(j.admin.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![1, 2]);
        let r = revert_admin(&o, &mut a);
        let order: Vec<SelectionKind> = r.acted.iter().map(|i| i.kind).collect();
        assert_eq!(order, vec![SelectionKind::Task, SelectionKind::Service, SelectionKind::Process], "{}", r.details());
    }
}
