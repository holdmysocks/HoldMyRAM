//! Log of PERSISTENT admin changes (service start type, task enabled flag) made from the
//! window, so each can be undone later. Independent of the toggle journal: temporary
//! toggle actions (stop now / restart on toggle off) are journaled there, not here.
//!
//! Rules (same as startup.rs):
//! - The record is written atomically BEFORE the change is applied, so a crash can never
//!   leave a change made by us without a record.
//! - One record per item, holding the state from BEFORE HoldMyRAM first changed it; a
//!   second change keeps that original state, and a change back to it drops the record.
//! - A failed change restores the previous log unless the item's state shows it did
//!   change (or cannot be read), in which case the record stays (conservative).
//! - Undo goes through the helper like any other change.
//!
//! File: `%LOCALAPPDATA%\HoldMyRAM\admin-changes.json`. Corrupt file: kept as `.bad`.

use super::client::AdminApi;
use super::proto::*;
use super::rules;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

pub const LOG_FILE: &str = "admin-changes.json";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum AdminChange {
    ServiceStartType {
        name: String,
        /// Start type before HoldMyRAM first changed it.
        before: StartType,
        before_delayed: Option<bool>,
        /// What HoldMyRAM set it to (latest).
        now: StartChoice,
        /// Unix seconds of the latest change.
        at: u64,
    },
    TaskEnabled {
        path: String,
        before: bool,
        now: bool,
        at: u64,
    },
}

impl AdminChange {
    fn key(&self) -> String {
        match self {
            AdminChange::ServiceStartType { name, .. } => format!("service:{}", name.to_lowercase()),
            AdminChange::TaskEnabled { path, .. } => format!("task:{}", path.to_lowercase()),
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct LogFile {
    version: u32,
    changes: Vec<AdminChange>,
}

static LOCK: Mutex<()> = Mutex::new(());

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    name.push(suffix);
    path.with_file_name(name)
}

/// Missing: empty. Corrupt: renamed to `.bad`, then empty. Other I/O errors are returned
/// so a save can never overwrite a log that could not be read.
fn load(path: &Path) -> io::Result<Vec<AdminChange>> {
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) if e.kind() == io::ErrorKind::InvalidData => String::new(),
        Err(e) => return Err(e),
    };
    match serde_json::from_str::<LogFile>(&text) {
        Ok(f) => Ok(f.changes),
        Err(_) => {
            fs::rename(path, with_suffix(path, ".bad"))?;
            Ok(Vec::new())
        }
    }
}

/// Atomic replace: temp file, flush to disk, rename.
fn save(path: &Path, changes: &[AdminChange]) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let json = serde_json::to_vec_pretty(&LogFile { version: 1, changes: changes.to_vec() }).map_err(io::Error::other)?;
    let tmp = with_suffix(path, ".tmp");
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(&json)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path).inspect_err(|_| {
        let _ = fs::remove_file(&tmp);
    })
}

fn refuse(code: ErrorCode, m: impl Into<String>) -> AdminError {
    AdminError::new(code, m)
}

fn log_err(e: io::Error) -> AdminError {
    refuse(ErrorCode::Failed, format!("could not write the admin change log, nothing changed: {e}"))
}

pub struct ChangeLog {
    path: PathBuf,
}

impl Default for ChangeLog {
    fn default() -> Self {
        ChangeLog::at(Self::default_path())
    }
}

impl ChangeLog {
    pub fn default_path() -> PathBuf {
        let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
        base.join("HoldMyRAM").join(LOG_FILE)
    }

    pub fn at(path: impl Into<PathBuf>) -> ChangeLog {
        ChangeLog { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Changes in effect, oldest first. An unreadable log reads as empty.
    pub fn changes(&self) -> Vec<AdminChange> {
        let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        load(&self.path).unwrap_or_default()
    }

    /// Set a service's start type through the helper, logged first.
    /// Ok(None): it already had that start type (nothing written).
    pub fn set_service_start_type(
        &self,
        api: &mut dyn AdminApi,
        name: &str,
        choice: StartChoice,
    ) -> Result<Option<AdminChange>, AdminError> {
        rules::validate_service_name(name).map_err(|e| refuse(ErrorCode::BadRequest, e))?;
        if let Some(hit) = rules::never_touch_service(name) {
            return Err(refuse(ErrorCode::NeverTouch, format!("\"{name}\" is on the never-touch list ({hit})")));
        }
        let cur = api.service_query(name)?;
        if cur.protected {
            return Err(refuse(ErrorCode::NeverTouch, format!("\"{name}\" is protected")));
        }
        if choice.matches(cur.start, cur.delayed_auto) {
            return Ok(None);
        }
        if cur.start.to_choice(cur.delayed_auto).is_none() {
            return Err(refuse(ErrorCode::Failed, format!("\"{name}\" has start type {:?}, which could not be restored", cur.start)));
        }
        let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = load(&self.path).map_err(log_err)?;
        let probe = AdminChange::ServiceStartType { name: name.into(), before: cur.start, before_delayed: cur.delayed_auto, now: choice, at: 0 };
        let key = probe.key();
        let existing = prev.iter().find(|c| c.key() == key).cloned();
        let (before, before_delayed) = match &existing {
            Some(AdminChange::ServiceStartType { before, before_delayed, .. }) => (*before, *before_delayed),
            _ => (cur.start, cur.delayed_auto),
        };
        let record = AdminChange::ServiceStartType { name: name.into(), before, before_delayed, now: choice, at: now_secs() };
        let back_to_original = choice.matches(before, before_delayed);
        let mut next: Vec<AdminChange> = prev.iter().filter(|c| c.key() != key).cloned().collect();
        // Log first, even when going back to the original: if that change then fails the
        // old record must still be there (it is, since `prev` is restored on failure).
        next.push(record.clone());
        save(&self.path, &next).map_err(log_err)?;

        match api.service_set_start_type(name, choice) {
            Ok(rep) => {
                let mut final_log: Vec<AdminChange> = next.into_iter().filter(|c| c.key() != key).collect();
                // No earlier record: the helper's own reading of "before" is authoritative.
                let record = if existing.is_none() {
                    AdminChange::ServiceStartType {
                        name: name.into(),
                        before: rep.before.start,
                        before_delayed: rep.before.delayed_auto,
                        now: choice,
                        at: now_secs(),
                    }
                } else {
                    record
                };
                if !back_to_original {
                    final_log.push(record.clone());
                }
                // The change is applied; failing to tidy the log only leaves a record
                // whose undo is a no-op, so it is not an error.
                let _ = save(&self.path, &final_log);
                Ok(Some(record))
            }
            Err(e) => {
                // Restore the old log only if the service is provably unchanged.
                if let Ok(now) = api.service_query(name) {
                    if now.start == cur.start && now.delayed_auto == cur.delayed_auto {
                        let _ = save(&self.path, &prev);
                    }
                }
                Err(e)
            }
        }
    }

    /// Enable or disable a scheduled task through the helper, logged first.
    /// Ok(None): it was already in that state.
    pub fn set_task_enabled(&self, api: &mut dyn AdminApi, path: &str, enabled: bool) -> Result<Option<AdminChange>, AdminError> {
        rules::validate_task_path(path).map_err(|e| refuse(ErrorCode::BadRequest, e))?;
        if let Some(f) = rules::never_touch_task(path) {
            return Err(refuse(ErrorCode::NeverTouch, format!("\"{path}\" is under a never-touch folder ({f})")));
        }
        let cur = api.task_query(path)?;
        if cur.protected {
            return Err(refuse(ErrorCode::NeverTouch, format!("\"{path}\" is protected")));
        }
        if cur.enabled == enabled {
            return Ok(None);
        }
        let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = load(&self.path).map_err(log_err)?;
        // Key by the canonical path the scheduler reported.
        let canonical = cur.path.clone();
        let key = AdminChange::TaskEnabled { path: canonical.clone(), before: false, now: false, at: 0 }.key();
        let original = match prev.iter().find(|c| c.key() == key) {
            Some(AdminChange::TaskEnabled { before, .. }) => *before,
            _ => cur.enabled,
        };
        let record = AdminChange::TaskEnabled { path: canonical.clone(), before: original, now: enabled, at: now_secs() };
        let mut next: Vec<AdminChange> = prev.iter().filter(|c| c.key() != key).cloned().collect();
        next.push(record.clone());
        save(&self.path, &next).map_err(log_err)?;

        match api.task_set_enabled(&canonical, enabled) {
            Ok(_) => {
                if enabled == original {
                    next.retain(|c| c.key() != key);
                    let _ = save(&self.path, &next);
                }
                Ok(Some(record))
            }
            Err(e) => {
                if let Ok(now) = api.task_query(&canonical) {
                    if now.enabled == cur.enabled {
                        let _ = save(&self.path, &prev);
                    }
                }
                Err(e)
            }
        }
    }

    /// Put one logged item back to its original state (through the helper) and drop its
    /// record.
    pub fn undo(&self, api: &mut dyn AdminApi, change: &AdminChange) -> Result<(), AdminError> {
        match change {
            AdminChange::ServiceStartType { name, before, before_delayed, .. } => {
                let choice = before
                    .to_choice(*before_delayed)
                    .ok_or_else(|| refuse(ErrorCode::Failed, format!("cannot restore start type {before:?}")))?;
                self.set_service_start_type(api, name, choice)?;
            }
            AdminChange::TaskEnabled { path, before, .. } => {
                self.set_task_enabled(api, path, *before)?;
            }
        }
        // set_* drop the record when the item is back at its original state; also drop it
        // when the item was already there (set_* returned None and wrote nothing).
        let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let key = change.key();
        let log = load(&self.path).map_err(log_err)?;
        if log.iter().any(|c| c.key() == key) {
            let rest: Vec<AdminChange> = log.into_iter().filter(|c| c.key() != key).collect();
            save(&self.path, &rest).map_err(log_err)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::client::LocalApi;
    use super::super::fake::{FakeBackend, FakeService, FakeTask};
    use super::super::handler::{Handler, Policy};
    use super::*;
    use crate::journal::tests::temp_dir;

    fn api() -> LocalApi<FakeBackend> {
        let mut b = FakeBackend::default();
        b.add(FakeService::running("SysMain", StartType::Auto).delayed(Some(true)));
        b.add(FakeService::running("DiagTrack", StartType::Auto));
        b.add(FakeService::running("RpcSs", StartType::Auto));
        b.add(FakeService::running("CoworkVMService", StartType::Auto).locked());
        b.add_task(FakeTask::new(r"\Microsoft\Windows\Defrag\ScheduledDefrag", true));
        b.add_task(FakeTask::new(r"\Microsoft\Windows\Windows Defender\Windows Defender Scheduled Scan", true));
        b.add_task(FakeTask::new(r"\Locked", true).locked());
        LocalApi::new(Handler::new(b, Policy::new(vec![])))
    }

    fn log(tag: &str) -> ChangeLog {
        ChangeLog::at(temp_dir(tag).join(LOG_FILE))
    }

    #[test]
    fn default_path_is_under_localappdata() {
        let p = ChangeLog::default_path();
        assert!(p.ends_with(r"HoldMyRAM\admin-changes.json"), "{}", p.display());
    }

    #[test]
    fn service_start_type_logged_and_undone() {
        let l = log("adm-svc");
        let mut a = api();
        let c = l.set_service_start_type(&mut a, "SysMain", StartChoice::Disabled).unwrap().unwrap();
        match &c {
            AdminChange::ServiceStartType { name, before, before_delayed, now, at } => {
                assert_eq!(name, "SysMain");
                assert_eq!((*before, *before_delayed, *now), (StartType::Auto, Some(true), StartChoice::Disabled));
                assert!(*at > 1_700_000_000);
            }
            _ => panic!(),
        }
        assert_eq!(l.changes(), vec![c.clone()]);
        assert_eq!(a.handler.backend().service("SysMain").start, StartType::Disabled);

        // A second change keeps the ORIGINAL before-state.
        let c2 = l.set_service_start_type(&mut a, "sysmain", StartChoice::Manual).unwrap().unwrap();
        let AdminChange::ServiceStartType { before, before_delayed, now, .. } = c2 else { panic!() };
        assert_eq!((before, before_delayed, now), (StartType::Auto, Some(true), StartChoice::Manual));
        assert_eq!(l.changes().len(), 1);

        // Undo restores delayed-auto and empties the log.
        l.undo(&mut a, &l.changes()[0]).unwrap();
        let s = a.handler.backend().service("SysMain");
        assert_eq!((s.start, s.delayed_auto), (StartType::Auto, Some(true)));
        assert!(l.changes().is_empty());
        assert!(!with_suffix(l.path(), ".tmp").exists());
    }

    #[test]
    fn same_state_writes_nothing_and_protected_refused_without_log() {
        let l = log("adm-same");
        let mut a = api();
        assert_eq!(l.set_service_start_type(&mut a, "DiagTrack", StartChoice::Auto).unwrap(), None);
        assert_eq!(l.set_task_enabled(&mut a, r"\Microsoft\Windows\Defrag\ScheduledDefrag", true).unwrap(), None);
        for name in ["RpcSs", "rpcss", "AudioSrv_abc12"] {
            assert_eq!(l.set_service_start_type(&mut a, name, StartChoice::Disabled).unwrap_err().code, ErrorCode::NeverTouch);
        }
        let e = l.set_task_enabled(&mut a, r"\Microsoft\Windows\Windows Defender\Windows Defender Scheduled Scan", false).unwrap_err();
        assert_eq!(e.code, ErrorCode::NeverTouch);
        assert_eq!(l.set_service_start_type(&mut a, r"..\x", StartChoice::Disabled).unwrap_err().code, ErrorCode::BadRequest);
        assert!(!l.path().exists(), "nothing logged");
        assert!(a.handler.backend().mutations().is_empty());
    }

    #[test]
    fn failed_change_restores_log_and_locked_is_reported() {
        let l = log("adm-locked");
        let mut a = api();
        l.set_service_start_type(&mut a, "DiagTrack", StartChoice::Manual).unwrap();
        let e = l.set_service_start_type(&mut a, "CoworkVMService", StartChoice::Disabled).unwrap_err();
        assert_eq!(e.code, ErrorCode::Locked);
        let e = l.set_task_enabled(&mut a, r"\Locked", false).unwrap_err();
        assert_eq!(e.code, ErrorCode::Locked);
        // Only the successful change is logged.
        let names: Vec<String> = l.changes().iter().map(|c| c.key()).collect();
        assert_eq!(names, vec!["service:diagtrack"]);
    }

    #[test]
    fn record_is_written_before_the_change() {
        // An API that checks the log at the moment the change is applied.
        struct Spy<'a> {
            inner: LocalApi<FakeBackend>,
            log: &'a ChangeLog,
            seen: Option<usize>,
        }
        impl AdminApi for Spy<'_> {
            fn call(&mut self, op: Op) -> Result<Reply, AdminError> {
                if op.is_mutating() {
                    // LOCK is held by the caller; read the file directly.
                    self.seen = Some(load(self.log.path()).unwrap().len());
                }
                self.inner.call(op)
            }
        }
        let l = log("adm-order");
        let mut spy = Spy { inner: api(), log: &l, seen: None };
        l.set_task_enabled(&mut spy, r"\Microsoft\Windows\Defrag\ScheduledDefrag", false).unwrap();
        assert_eq!(spy.seen, Some(1));
        spy.seen = None;
        l.set_service_start_type(&mut spy, "DiagTrack", StartChoice::Disabled).unwrap();
        assert_eq!(spy.seen, Some(2));
    }

    #[test]
    fn task_round_trip_and_manual_revert_drops_record() {
        let l = log("adm-task");
        let mut a = api();
        let p = r"\microsoft\windows\defrag\scheduleddefrag";
        let c = l.set_task_enabled(&mut a, p, false).unwrap().unwrap();
        assert!(
            matches!(&c, AdminChange::TaskEnabled { path, before: true, now: false, .. } if path == r"\Microsoft\Windows\Defrag\ScheduledDefrag"),
            "{c:?}"
        );
        assert!(!a.handler.backend().task(p).enabled);
        assert_eq!(l.changes().len(), 1);
        // Turning it back on by hand (not via undo) also clears the record.
        l.set_task_enabled(&mut a, p, true).unwrap();
        assert!(l.changes().is_empty());
        // And via undo.
        l.set_task_enabled(&mut a, p, false).unwrap();
        l.undo(&mut a, &l.changes()[0]).unwrap();
        assert!(a.handler.backend().task(p).enabled);
        assert!(l.changes().is_empty());
        // Undo of something already back in place just drops the record.
        l.set_task_enabled(&mut a, p, false).unwrap();
        let rec = l.changes()[0].clone();
        a.task_set_enabled(p, true).unwrap(); // re-enabled outside the log
        assert_eq!(l.changes().len(), 1, "stale record still listed");
        l.undo(&mut a, &rec).unwrap();
        assert!(l.changes().is_empty());
    }

    #[test]
    fn corrupt_log_kept_as_bad() {
        let l = log("adm-bad");
        fs::write(l.path(), "{ not json").unwrap();
        assert!(l.changes().is_empty());
        assert_eq!(fs::read_to_string(with_suffix(l.path(), ".bad")).unwrap(), "{ not json");
        // Unknown record kinds are corrupt too (strict schema).
        fs::write(l.path(), r#"{"version":1,"changes":[{"kind":"registry_wipe","x":1}]}"#).unwrap();
        assert!(l.changes().is_empty());
        // And the flow works on top of it; a stale temp file does not get in the way.
        fs::write(with_suffix(l.path(), ".tmp"), "junk").unwrap();
        let mut a = api();
        l.set_service_start_type(&mut a, "DiagTrack", StartChoice::Manual).unwrap();
        assert_eq!(l.changes().len(), 1);
        assert!(!with_suffix(l.path(), ".tmp").exists());
    }
}
