//! Snapshot journal: what the last toggle changed, written to disk BEFORE each change
//! so a crash mid-toggle can still be undone.
//!
//! Location: `%LOCALAPPDATA%\HoldMyRAM\journal.json` (overridable for tests).
//! Writes are atomic: the JSON goes to a temp file next to the target, is flushed, then
//! renamed over the old file, so a reader sees either the old or the new journal, never
//! a torn one. Entries are kept in apply order; [`Journal::revert_order`] yields them
//! last-in, first-out.

use crate::types::Method;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub const JOURNAL_VERSION: u32 = 1;
const FILE_NAME: &str = "journal.json";

/// Progress of one touched process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EntryState {
    /// Recorded, action not yet confirmed. After a crash the action may or may not
    /// have happened, so revert checks the live process before doing anything.
    Planned,
    /// The action happened; revert must undo it.
    Done,
    /// The action did not happen (or was abandoned); nothing to undo.
    Failed,
}

/// One touched process and everything needed to restore it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// Monotonic within a journal; equals apply order.
    pub seq: u64,
    pub pid: u32,
    /// Executable name as seen in the process list (e.g. `Discord.exe`).
    pub name: String,
    /// Full image path from `QueryFullProcessImageNameW`.
    pub exe: String,
    /// Full command line, if it could be read. Restore falls back to the exe alone.
    #[serde(default)]
    pub command_line: Option<String>,
    /// Process creation time (FILETIME as u64). Together with name and exe path it
    /// guards against acting on a different process that reused the PID.
    #[serde(default)]
    pub created: Option<u64>,
    pub method: Method,
    pub state: EntryState,
    /// Whether restore should start this exe again. Only the root of a multi-process
    /// group is relaunched; its children are spawned by the root itself.
    #[serde(default)]
    pub relaunch: bool,
    /// Free-form outcome text for the UI and logs.
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Journal {
    pub version: u32,
    /// Seconds since the Unix epoch when the journal was first written.
    pub created_unix: u64,
    pub entries: Vec<Entry>,
}

impl Default for Journal {
    fn default() -> Self {
        Self::new()
    }
}

impl Journal {
    pub fn new() -> Self {
        let created_unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        Self { version: JOURNAL_VERSION, created_unix, entries: Vec::new() }
    }

    pub fn next_seq(&self) -> u64 {
        self.entries.iter().map(|e| e.seq + 1).max().unwrap_or(0)
    }

    /// Indices of entries in revert order: highest `seq` first (LIFO). Sorting by seq
    /// rather than trusting vector order keeps this correct for a hand-edited file.
    pub fn revert_order(&self) -> Vec<usize> {
        let mut idx: Vec<usize> = (0..self.entries.len()).collect();
        idx.sort_by(|&a, &b| self.entries[b].seq.cmp(&self.entries[a].seq));
        idx
    }

    /// True when something may still need undoing.
    pub fn has_pending(&self) -> bool {
        self.entries.iter().any(|e| e.state != EntryState::Failed)
    }

    /// Whether an entry for this exact process (pid + creation time) is still pending.
    pub fn contains_live(&self, pid: u32, created: Option<u64>) -> bool {
        self.entries
            .iter()
            .any(|e| e.pid == pid && e.created == created && e.state != EntryState::Failed)
    }
}

/// Default journal path, `%LOCALAPPDATA%\HoldMyRAM\journal.json`.
pub fn default_path() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join("HoldMyRAM").join(FILE_NAME)
}

/// Load the journal. `Ok(None)` when there is no journal file.
/// A corrupt file is an error, never silently discarded: it may be the only record of
/// what needs restoring.
pub fn load(path: &Path) -> io::Result<Option<Journal>> {
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    serde_json::from_str(&text)
        .map(Some)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("journal unreadable: {e}")))
}

/// Atomically replace the journal file with `journal`.
pub fn save(path: &Path, journal: &Journal) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let json = serde_json::to_vec_pretty(journal)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let tmp = tmp_path(path);
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(&json)?;
        // Make sure the bytes are on disk before the rename publishes them.
        f.sync_all()?;
    }
    // std::fs::rename uses MoveFileExW(MOVEFILE_REPLACE_EXISTING) on Windows, which
    // replaces the destination in one step.
    match fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// Remove the journal (nothing left to restore). Missing file is fine.
pub fn clear(path: &Path) -> io::Result<()> {
    let _ = fs::remove_file(tmp_path(path));
    match fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// Startup check for crash recovery: a journal with pending entries means the previous
/// session closed or froze apps and never restored them.
pub fn pending(path: &Path) -> io::Result<Option<Journal>> {
    Ok(load(path)?.filter(Journal::has_pending))
}

fn tmp_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    name.push(".tmp");
    path.with_file_name(name)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A fresh, empty directory under the system temp dir for one test.
    pub(crate) fn temp_dir(tag: &str) -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "holdmyram-test-{}-{}-{}",
            std::process::id(),
            tag,
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn entry(seq: u64, pid: u32, method: Method) -> Entry {
        Entry {
            seq,
            pid,
            name: "app.exe".into(),
            exe: r"C:\Apps\app.exe".into(),
            command_line: Some(r#""C:\Apps\app.exe" --flag"#.into()),
            created: Some(1234),
            method,
            state: EntryState::Done,
            relaunch: true,
            note: None,
        }
    }

    #[test]
    fn round_trip() {
        let dir = temp_dir("roundtrip");
        let path = dir.join("journal.json");
        let mut j = Journal::new();
        j.entries.push(entry(0, 10, Method::Close));
        j.entries.push(entry(1, 11, Method::Suspend));
        j.entries[1].state = EntryState::Planned;
        j.entries[1].command_line = None;
        save(&path, &j).unwrap();
        let back = load(&path).unwrap().unwrap();
        assert_eq!(back, j);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn missing_file_is_none_and_clear_is_idempotent() {
        let dir = temp_dir("missing");
        let path = dir.join("journal.json");
        assert!(load(&path).unwrap().is_none());
        clear(&path).unwrap();
        clear(&path).unwrap();
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn corrupt_file_is_an_error_not_silently_empty() {
        let dir = temp_dir("corrupt");
        let path = dir.join("journal.json");
        fs::write(&path, "{ not json").unwrap();
        assert!(load(&path).is_err());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn atomic_write_replaces_and_leaves_no_temp() {
        let dir = temp_dir("atomic");
        let path = dir.join("journal.json");
        let mut j = Journal::new();
        j.entries.push(entry(0, 1, Method::Kill));
        save(&path, &j).unwrap();
        j.entries.push(entry(1, 2, Method::Kill));
        save(&path, &j).unwrap();
        assert_eq!(load(&path).unwrap().unwrap().entries.len(), 2);
        assert!(!tmp_path(&path).exists(), "temp file must be renamed away");
        // A stale temp from a crash between write and rename must not break the next save.
        fs::write(tmp_path(&path), "garbage").unwrap();
        save(&path, &j).unwrap();
        assert_eq!(load(&path).unwrap().unwrap(), j);
        assert!(!tmp_path(&path).exists());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn creates_parent_directory() {
        let dir = temp_dir("parent");
        let path = dir.join("nested").join("deeper").join("journal.json");
        save(&path, &Journal::new()).unwrap();
        assert!(path.exists());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn revert_order_is_lifo_by_seq() {
        let mut j = Journal::new();
        // Deliberately out of vector order.
        j.entries.push(entry(1, 101, Method::Close));
        j.entries.push(entry(0, 100, Method::Close));
        j.entries.push(entry(2, 102, Method::Suspend));
        let pids: Vec<u32> = j.revert_order().iter().map(|&i| j.entries[i].pid).collect();
        assert_eq!(pids, vec![102, 101, 100]);
        assert_eq!(j.next_seq(), 3);
    }

    #[test]
    fn pending_ignores_failed_only_journals() {
        let dir = temp_dir("pending");
        let path = dir.join("journal.json");
        let mut j = Journal::new();
        j.entries.push(entry(0, 1, Method::Close));
        j.entries[0].state = EntryState::Failed;
        save(&path, &j).unwrap();
        assert!(pending(&path).unwrap().is_none());
        j.entries[0].state = EntryState::Planned;
        save(&path, &j).unwrap();
        assert!(pending(&path).unwrap().is_some());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn contains_live_matches_pid_and_creation_time() {
        let mut j = Journal::new();
        j.entries.push(entry(0, 50, Method::Suspend));
        assert!(j.contains_live(50, Some(1234)));
        assert!(!j.contains_live(50, Some(9999)), "same pid, different process");
        j.entries[0].state = EntryState::Failed;
        assert!(!j.contains_live(50, Some(1234)));
    }
}
