//! Toggle on (`apply`) and off (`revert`).
//!
//! apply: snapshot processes -> `plan::select_targets` (foreground tree, protected names
//! and self are skipped) -> extra guards (protected dirs, PID reuse, access) -> journal
//! every target as Planned and save -> act -> mark Done/Failed and save.
//! revert: walk the journal last-in, first-out and undo each Done (or crash-left
//! Planned) entry, then clear the journal. Entries whose restore failed stay in the
//! journal so a later revert can retry.

use crate::actions::{self, ProcHandle, RestoreStatus, DEFAULT_GRACE};
use crate::journal::{self, Entry, EntryState, Journal};
use crate::plan::{select_targets, Skipped};
use crate::process::{list_processes, ProcInfo};
use crate::types::{Method, Selection};
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

/// One line of a report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Item {
    pub pid: u32,
    pub name: String,
    pub method: Option<Method>,
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
        format!(
            "{}: {} acted, {} skipped, {} failed",
            self.action,
            self.acted.len(),
            self.skipped.len(),
            self.failed.len()
        )
    }

    /// Multi-line detail for logs.
    pub fn details(&self) -> String {
        let mut out = self.summary();
        for (label, items) in [("acted", &self.acted), ("skipped", &self.skipped), ("failed", &self.failed)] {
            for i in items {
                out.push_str(&format!("\n  {label} {} (pid {}): {}", i.name, i.pid, i.detail));
            }
        }
        out
    }
}

fn item(pid: u32, name: &str, method: Option<Method>, detail: impl Into<String>) -> Item {
    Item { pid, name: name.to_string(), method, detail: detail.into() }
}

fn skip_reason(why: &Skipped) -> &'static str {
    match why {
        Skipped::Foreground => "foreground app (or part of its process tree); left running",
        Skipped::SelfProcess => "HoldMyRAM itself",
        Skipped::Protected => "protected system process",
    }
}

/// Toggle on with the default journal location and grace period.
pub fn apply(selections: &[Selection], foreground_pid: Option<u32>, self_pid: u32) -> Report {
    apply_with(&Options::default(), selections, foreground_pid, self_pid)
}

/// Toggle off with the default journal location.
pub fn revert() -> Report {
    revert_with(&Options::default(), std::process::id())
}

/// Journal left by a previous session that still needs restoring (crash recovery).
pub fn pending_restore() -> Result<Option<Journal>, String> {
    journal::pending(&journal::default_path()).map_err(|e| e.to_string())
}

/// The foreground PID that actually deserves protection: none when the shell or
/// HoldMyRAM itself has focus.
fn effective_foreground(procs: &[ProcInfo], fg: Option<u32>, self_pid: u32) -> Option<u32> {
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

pub fn apply_with(opts: &Options, selections: &[Selection], foreground_pid: Option<u32>, self_pid: u32) -> Report {
    let mut report = Report::new("apply");

    // Never overwrite a journal we cannot read: it may be the only record of what to restore.
    let mut journal = match journal::load(&opts.journal_path) {
        Ok(j) => j.unwrap_or_default(),
        Err(e) => return Report::fatal("apply", format!("{e}; refusing to act until it is restored or removed")),
    };

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

    let procs = list_processes();
    let fg = effective_foreground(&procs, foreground_pid, self_pid);
    let names: Vec<String> = methods.keys().cloned().collect();
    let (targets, skipped) = select_targets(&procs, &names, fg, self_pid);
    for (p, why) in &skipped {
        report.skipped.push(item(p.pid, &p.name, methods.get(&p.name.to_lowercase()).copied(), skip_reason(why)));
    }

    // Group by executable name, preserving first-seen order for determinism.
    let mut group_order: Vec<String> = Vec::new();
    let mut groups: HashMap<String, Vec<&ProcInfo>> = HashMap::new();
    for t in targets {
        let key = t.name.to_lowercase();
        if ambiguous.contains(&key) {
            report.skipped.push(item(t.pid, &t.name, None, "selected twice with different methods; ambiguous"));
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
            Method::Close => unreachable!(),
        };
        if *method == Method::Close {
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

pub fn revert_with(opts: &Options, self_pid: u32) -> Report {
    let mut report = Report::new("revert");
    let journal = match journal::load(&opts.journal_path) {
        Ok(Some(j)) => j,
        Ok(None) => return report,
        Err(e) => return Report::fatal("revert", e.to_string()),
    };

    let mut keep: Vec<Entry> = Vec::new();
    for idx in journal.revert_order() {
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

    let result = if keep.is_empty() {
        journal::clear(&opts.journal_path)
    } else {
        keep.sort_by_key(|e| e.seq);
        let mut left = journal.clone();
        left.entries = keep;
        journal::save(&opts.journal_path, &left)
    };
    if let Err(e) = result {
        report.error = Some(format!("journal update failed: {e}"));
    }
    report
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
        vec![Selection { name: name.into(), method }]
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
        assert!(r.skipped.iter().any(|i| i.pid == s.pid() && i.detail.contains("foreground")));
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
}
