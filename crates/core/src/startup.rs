//! Enable or disable startup items (the same switch Task Manager's Startup tab uses).
//!
//! How it works: Windows decides whether a Run entry or Startup-folder shortcut starts at
//! logon from a 12-byte binary value of the same name under
//! `...\Explorer\StartupApproved\{Run, Run32, StartupFolder}` in the entry's hive.
//! First byte with the low bit clear (0x02) means enabled; set (0x03) means disabled, and
//! bytes 4..12 then hold the FILETIME of when it was disabled. Task Manager writes exactly
//! this, so both tools show the same state.
//!
//! Safety rules this module keeps:
//! - Only that StartupApproved value is ever written. The item's own Run value, shortcut
//!   or file is never modified, renamed or deleted, so enabling restores the original
//!   state exactly.
//! - Only the current user's hive is written. HKLM and all-users sources need admin and
//!   are refused before anything is read or written. RunOnce entries are not covered by
//!   StartupApproved and are refused too.
//! - Every write is read back and compared; a mismatch is an error (and the previous
//!   value is put back, best effort).
//! - Disabling is logged (atomically) BEFORE the registry write, so a crash can never leave
//!   an item disabled by us without a record. Re-enabling removes the record.
//!
//! Log: `%LOCALAPPDATA%\HoldMyRAM\startup-changes.json`. A corrupt log is kept as `.bad`
//! and replaced by an empty one.

use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_MORE_DATA, ERROR_SUCCESS, WIN32_ERROR};
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
    HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_QUERY_VALUE, KEY_SET_VALUE, KEY_WOW64_64KEY,
    REG_BINARY, REG_OPTION_NON_VOLATILE, REG_VALUE_TYPE,
};

/// One change HoldMyRAM made, kept in a log so it can always be undone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartupChange {
    /// `ScanItem.name` of the startup entry.
    pub name: String,
    /// The `source:` text from `ScanItem.detail`, e.g. "HKCU Run".
    pub source: String,
    pub was_enabled: bool,
    pub now_enabled: bool,
    /// When the change was made, as an RFC 3339-like UTC string.
    pub at: String,
}

/// Whether HoldMyRAM can change an item from this source without admin rights.
/// `source_detail` is the full `ScanItem.detail` text of a Kind::Startup row.
pub fn can_change(source_detail: &str) -> bool {
    Source::parse(source_detail).is_some_and(|s| s.hive == Hive::User && s.approved_subkey().is_some())
}

/// Enable or disable one startup item. Must be reversible, must not touch the item's
/// own registry value or file, and must log the change.
pub fn set_enabled(name: &str, source_detail: &str, enabled: bool) -> Result<StartupChange, String> {
    let log = default_log_path();
    set_enabled_in(&Env::real(&log), name, source_detail, enabled)
}

/// Changes made so far that are still in effect (newest last).
pub fn changes() -> Vec<StartupChange> {
    let log = default_log_path();
    changes_in(&Env::real(&log))
}

// ---------------------------------------------------------------------------------------
// Sources
// ---------------------------------------------------------------------------------------

const APPROVED: &str = r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved";
const RUN: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_WOW: &str = r"Software\WOW6432Node\Microsoft\Windows\CurrentVersion\Run";
const LOG_FILE: &str = "startup-changes.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Hive {
    User,
    Machine,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Location {
    Run,
    Run32,
    RunOnce,
    RunOnce32,
    Folder,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Source {
    hive: Hive,
    loc: Location,
}

/// Every source label scan.rs emits (after "source: "), and what it means.
const SOURCES: [(&str, Source); 10] = [
    ("HKCU Run", Source { hive: Hive::User, loc: Location::Run }),
    ("HKCU Run (32-bit)", Source { hive: Hive::User, loc: Location::Run32 }),
    ("HKCU RunOnce", Source { hive: Hive::User, loc: Location::RunOnce }),
    ("HKCU RunOnce (32-bit)", Source { hive: Hive::User, loc: Location::RunOnce32 }),
    ("HKLM Run", Source { hive: Hive::Machine, loc: Location::Run }),
    ("HKLM Run (32-bit)", Source { hive: Hive::Machine, loc: Location::Run32 }),
    ("HKLM RunOnce", Source { hive: Hive::Machine, loc: Location::RunOnce }),
    ("HKLM RunOnce (32-bit)", Source { hive: Hive::Machine, loc: Location::RunOnce32 }),
    ("user Startup folder", Source { hive: Hive::User, loc: Location::Folder }),
    ("all-users Startup folder", Source { hive: Hive::Machine, loc: Location::Folder }),
];

/// The source label from a detail string: the text after the LAST "source: " (the
/// command line comes first and could itself contain that text), or the whole string
/// when it is already a bare label such as "HKCU Run".
fn source_label(detail: &str) -> &str {
    match detail.rfind("source: ") {
        Some(i) => detail[i + "source: ".len()..].trim(),
        None => detail.trim(),
    }
}

impl Source {
    /// Exact match against the known labels; anything else is unknown (refused).
    fn parse(detail: &str) -> Option<Source> {
        let label = source_label(detail);
        SOURCES.iter().find(|(l, _)| *l == label).map(|(_, s)| *s)
    }

    fn label(self) -> &'static str {
        SOURCES.iter().find(|(_, s)| *s == self).map(|(l, _)| *l).unwrap_or("unknown")
    }

    /// StartupApproved subkey for this source; None when StartupApproved does not apply.
    fn approved_subkey(self) -> Option<&'static str> {
        match self.loc {
            Location::Run => Some("Run"),
            Location::Run32 => Some("Run32"),
            Location::Folder => Some("StartupFolder"),
            Location::RunOnce | Location::RunOnce32 => None,
        }
    }
}

// ---------------------------------------------------------------------------------------
// Value encoding
// ---------------------------------------------------------------------------------------

/// Seconds between 1601-01-01 and 1970-01-01, in 100 ns units.
const FILETIME_UNIX_EPOCH: u64 = 116_444_736_000_000_000;

pub(crate) fn filetime_now() -> u64 {
    let d = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    FILETIME_UNIX_EPOCH + d.as_secs() * 10_000_000 + u64::from(d.subsec_nanos()) / 100
}

/// The 12-byte StartupApproved value Task Manager writes.
pub(crate) fn approved_bytes(enabled: bool, filetime: u64) -> [u8; 12] {
    let mut b = [0u8; 12];
    if enabled {
        b[0] = 0x02;
    } else {
        b[0] = 0x03;
        b[4..12].copy_from_slice(&filetime.to_le_bytes());
    }
    b
}

/// Absent or empty value: enabled. Otherwise the low bit of byte 0 is "disabled"
/// (same rule as scan.rs).
pub(crate) fn bytes_enabled(data: Option<&[u8]>) -> bool {
    match data.and_then(|d| d.first()) {
        None => true,
        Some(b) => b & 0x01 == 0,
    }
}

fn validate_name(name: &str, source: Source) -> Result<(), String> {
    if name.is_empty() || name.trim().is_empty() {
        return Err("startup item has no name; refusing to change it".into());
    }
    if name.contains('\0') {
        return Err("startup item name contains a NUL character; refusing to change it".into());
    }
    // Registry value names are limited to 16383 characters.
    if name.encode_utf16().count() > 16_383 {
        return Err("startup item name is too long; refusing to change it".into());
    }
    if source.loc == Location::Folder && (name.contains(['\\', '/', ':']) || name == "." || name == "..") {
        return Err(format!("\"{name}\" is not a plain Startup-folder file name; refusing to change it"));
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------
// Environment (injectable so tests never touch the real keys or log)
// ---------------------------------------------------------------------------------------

struct Env<'a> {
    /// Root + path of the current user's StartupApproved key (the only place written).
    user_root: HKEY,
    user_base: &'a str,
    /// Root + path of the all-users StartupApproved key (read only).
    machine_root: HKEY,
    machine_base: &'a str,
    log: &'a Path,
    /// Whether the item's own entry (Run value or Startup-folder file) exists. Read only.
    entry_exists: &'a dyn Fn(Source, &str) -> bool,
}

impl<'a> Env<'a> {
    fn real(log: &'a Path) -> Env<'a> {
        Env {
            user_root: HKEY_CURRENT_USER,
            user_base: APPROVED,
            machine_root: HKEY_LOCAL_MACHINE,
            machine_base: APPROVED,
            log,
            entry_exists: &real_entry_exists,
        }
    }
}

fn real_entry_exists(source: Source, name: &str) -> bool {
    if source.hive != Hive::User {
        return false;
    }
    match source.loc {
        Location::Run => matches!(read_value(HKEY_CURRENT_USER, RUN, name), Ok(Some(_))),
        Location::Run32 => matches!(read_value(HKEY_CURRENT_USER, RUN_WOW, name), Ok(Some(_))),
        Location::Folder => std::env::var_os("APPDATA")
            .map(|a| {
                Path::new(&a)
                    .join(r"Microsoft\Windows\Start Menu\Programs\Startup")
                    .join(name)
                    .is_file()
            })
            .unwrap_or(false),
        Location::RunOnce | Location::RunOnce32 => false,
    }
}

/// Serialises every read-modify-write of the log and of the registry values.
static LOCK: Mutex<()> = Mutex::new(());

fn set_enabled_in(env: &Env, name: &str, detail: &str, enabled: bool) -> Result<StartupChange, String> {
    let source = Source::parse(detail)
        .ok_or_else(|| format!("unknown startup source \"{}\"; refusing to change it", source_label(detail)))?;
    let label = source.label();
    if source.hive != Hive::User {
        return Err(format!(
            "\"{name}\" comes from {label}, which applies to all users; changing it needs administrator rights"
        ));
    }
    let sub = source
        .approved_subkey()
        .ok_or_else(|| format!("\"{name}\" is a {label} entry; Windows has no on/off switch for those"))?;
    validate_name(name, source)?;

    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let user_path = format!(r"{}\{}", env.user_base, sub);
    let machine_path = format!(r"{}\{}", env.machine_base, sub);
    let prev = read_value(env.user_root, &user_path, name)?;
    let user_enabled = bytes_enabled(prev.as_deref());
    // A matching all-users value can also disable it (scan.rs counts either). Reading it
    // needs no admin; an unreadable key counts as "not disabled there".
    let machine_enabled = bytes_enabled(read_value(env.machine_root, &machine_path, name).ok().flatten().as_deref());
    let effective = user_enabled && machine_enabled;

    let change = |was: bool, now: bool| StartupChange {
        name: name.to_string(),
        source: label.to_string(),
        was_enabled: was,
        now_enabled: now,
        at: rfc3339_now(),
    };

    if enabled {
        if user_enabled && !machine_enabled {
            return Err(format!(
                "\"{name}\" is turned off for all users; turning it back on needs administrator rights"
            ));
        }
        if user_enabled {
            return Ok(change(true, true));
        }
        if !machine_enabled {
            return Err(format!(
                "\"{name}\" is also turned off for all users; turning it back on needs administrator rights"
            ));
        }
        write_verified(env.user_root, &user_path, name, &approved_bytes(true, 0), prev.as_deref())?;
        // The item is enabled now. A failure to update the log only leaves a stale record,
        // which changes() filters out by reading the real state, so it is not an error.
        let _ = update_log(env.log, |log| log.retain(|c| !(c.name == name && c.source == label)));
        Ok(change(false, true))
    } else {
        if !effective {
            return Ok(change(false, false));
        }
        if !(env.entry_exists)(source, name) {
            return Err(format!("\"{name}\" was not found in {label}; refusing to change it"));
        }
        let record = change(true, false);
        // Log first: never leave an item disabled by us without a record.
        update_log(env.log, |log| {
            log.retain(|c| !(c.name == name && c.source == label));
            log.push(record.clone());
        })
        .map_err(|e| format!("could not write the startup change log, nothing changed: {e}"))?;
        if let Err(e) = write_verified(env.user_root, &user_path, name, &approved_bytes(false, filetime_now()), prev.as_deref()) {
            let _ = update_log(env.log, |log| log.retain(|c| !(c.name == name && c.source == label)));
            return Err(e);
        }
        Ok(record)
    }
}

fn changes_in(env: &Env) -> Vec<StartupChange> {
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let Ok(log) = load_log(env.log) else { return Vec::new() };
    log.into_iter()
        .filter(|c| {
            // Keep only records that are still in effect: the item is still disabled in
            // our hive. If the state cannot be read, keep the record (conservative).
            let Some(src) = Source::parse(&c.source) else { return true };
            let Some(sub) = src.approved_subkey() else { return true };
            if src.hive != Hive::User {
                return true;
            }
            match read_value(env.user_root, &format!(r"{}\{}", env.user_base, sub), &c.name) {
                Ok(v) => !bytes_enabled(v.as_deref()),
                Err(_) => true,
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------------------
// Registry access
// ---------------------------------------------------------------------------------------

struct Key(HKEY);

impl Drop for Key {
    fn drop(&mut self) {
        unsafe {
            let _ = RegCloseKey(self.0);
        }
    }
}

pub(crate) fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn reg_err(what: &str, r: WIN32_ERROR) -> String {
    format!("{what} failed (Windows error {})", r.0)
}

/// Raw data of one value. `Ok(None)` when the key or the value does not exist.
pub(crate) fn read_value(root: HKEY, path: &str, name: &str) -> Result<Option<Vec<u8>>, String> {
    let p = wide(path);
    let mut h = HKEY::default();
    let r = unsafe { RegOpenKeyExW(root, PCWSTR(p.as_ptr()), Some(0), KEY_QUERY_VALUE | KEY_WOW64_64KEY, &mut h) };
    if r == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    if r != ERROR_SUCCESS {
        return Err(reg_err("opening the startup setting", r));
    }
    let key = Key(h);
    let n = wide(name);
    for _ in 0..4 {
        let mut ty = REG_VALUE_TYPE::default();
        let mut size = 0u32;
        let r = unsafe { RegQueryValueExW(key.0, PCWSTR(n.as_ptr()), None, Some(&mut ty), None, Some(&mut size)) };
        if r == ERROR_FILE_NOT_FOUND {
            return Ok(None);
        }
        if r != ERROR_SUCCESS {
            return Err(reg_err("reading the startup setting", r));
        }
        let mut data = vec![0u8; size as usize];
        let r = unsafe {
            RegQueryValueExW(key.0, PCWSTR(n.as_ptr()), None, Some(&mut ty), Some(data.as_mut_ptr()), Some(&mut size))
        };
        if r == ERROR_MORE_DATA {
            continue; // grew between the two calls
        }
        if r == ERROR_FILE_NOT_FOUND {
            return Ok(None);
        }
        if r != ERROR_SUCCESS {
            return Err(reg_err("reading the startup setting", r));
        }
        data.truncate(size as usize);
        return Ok(Some(data));
    }
    Err("reading the startup setting failed: value kept changing".into())
}

/// Write a REG_BINARY value (creating the key if needed), read it back and compare.
/// On a failed write or mismatch, put `prev` back (or remove the value if there was
/// none), best effort, and return an error.
pub(crate) fn write_verified(root: HKEY, path: &str, name: &str, data: &[u8], prev: Option<&[u8]>) -> Result<(), String> {
    let p = wide(path);
    let mut h = HKEY::default();
    let r = unsafe {
        RegCreateKeyExW(
            root,
            PCWSTR(p.as_ptr()),
            Some(0),
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_QUERY_VALUE | KEY_SET_VALUE | KEY_WOW64_64KEY,
            None,
            &mut h,
            None,
        )
    };
    if r != ERROR_SUCCESS {
        return Err(reg_err("opening the startup setting for writing", r));
    }
    let key = Key(h);
    let n = wide(name);
    let restore = || unsafe {
        match prev {
            Some(old) => {
                let _ = RegSetValueExW(key.0, PCWSTR(n.as_ptr()), Some(0), REG_BINARY, Some(old));
            }
            None => {
                let _ = RegDeleteValueW(key.0, PCWSTR(n.as_ptr()));
            }
        }
    };
    let r = unsafe { RegSetValueExW(key.0, PCWSTR(n.as_ptr()), Some(0), REG_BINARY, Some(data)) };
    if r != ERROR_SUCCESS {
        restore();
        return Err(reg_err("writing the startup setting", r));
    }
    match read_value(root, path, name) {
        Ok(Some(back)) if back == data => Ok(()),
        Ok(_) => {
            restore();
            Err("the startup setting did not read back as written; the previous value was put back".into())
        }
        Err(e) => {
            restore();
            Err(format!("could not verify the startup setting ({e}); the previous value was put back"))
        }
    }
}

// ---------------------------------------------------------------------------------------
// Change log
// ---------------------------------------------------------------------------------------

#[derive(Debug, Default, Serialize, Deserialize)]
struct LogFile {
    version: u32,
    changes: Vec<StartupChange>,
}

fn default_log_path() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join("HoldMyRAM").join(LOG_FILE)
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    name.push(suffix);
    path.with_file_name(name)
}

/// Load the log. Missing file: empty. Corrupt file: renamed to `.bad`, then empty.
/// Other I/O errors are returned so a later save cannot overwrite a log we could not read.
fn load_log(path: &Path) -> io::Result<Vec<StartupChange>> {
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) if e.kind() == io::ErrorKind::InvalidData => String::new(), // not UTF-8: corrupt
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

/// Atomically replace the log (temp file + rename, as in journal.rs).
fn save_log(path: &Path, changes: &[StartupChange]) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let file = LogFile { version: 1, changes: changes.to_vec() };
    let json = serde_json::to_vec_pretty(&file).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
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

fn update_log(path: &Path, f: impl FnOnce(&mut Vec<StartupChange>)) -> io::Result<()> {
    let mut log = load_log(path)?;
    f(&mut log);
    save_log(path, &log)
}

// ---------------------------------------------------------------------------------------
// Time
// ---------------------------------------------------------------------------------------

/// Current UTC time as `YYYY-MM-DDTHH:MM:SSZ`.
fn rfc3339_now() -> String {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    rfc3339(secs)
}

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
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem / 60) % 60,
        rem % 60
    )
}

// ---------------------------------------------------------------------------------------
// Tests: everything that writes uses a throwaway key under
// HKCU\Software\HoldMyRAMTest\<random> and a temp log file. Nothing here reads or writes
// the real StartupApproved keys, Run entries or log.
// ---------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::journal::tests::temp_dir;
    use std::sync::atomic::{AtomicU32, Ordering};
    use windows::Win32::System::Registry::{RegDeleteKeyW, RegDeleteTreeW};

    const TEST_PARENT: &str = r"Software\HoldMyRAMTest";

    /// A throwaway HKCU key, deleted (with everything under it) on drop.
    struct TestKey {
        path: String,
    }

    impl TestKey {
        fn new() -> TestKey {
            static N: AtomicU32 = AtomicU32::new(0);
            let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().subsec_nanos();
            let path = format!(
                r"{TEST_PARENT}\{}-{}-{}",
                std::process::id(),
                nanos,
                N.fetch_add(1, Ordering::SeqCst)
            );
            TestKey { path }
        }
        fn user_base(&self) -> String {
            format!(r"{}\User\StartupApproved", self.path)
        }
        fn machine_base(&self) -> String {
            format!(r"{}\Machine\StartupApproved", self.path)
        }
    }

    impl Drop for TestKey {
        fn drop(&mut self) {
            // Never delete anything outside our own test parent.
            let prefix = format!(r"{TEST_PARENT}\");
            assert!(self.path.starts_with(&prefix) && self.path.len() > prefix.len());
            let p = wide(&self.path);
            let parent = wide(TEST_PARENT);
            unsafe {
                let _ = RegDeleteTreeW(HKEY_CURRENT_USER, PCWSTR(p.as_ptr()));
                let _ = RegDeleteKeyW(HKEY_CURRENT_USER, PCWSTR(p.as_ptr()));
                // Only succeeds once no other test key is left under it.
                let _ = RegDeleteKeyW(HKEY_CURRENT_USER, PCWSTR(parent.as_ptr()));
            }
        }
    }

    struct Fixture {
        key: TestKey,
        user: String,
        machine: String,
        log: PathBuf,
    }

    fn fixture(tag: &str) -> Fixture {
        let key = TestKey::new();
        Fixture {
            user: key.user_base(),
            machine: key.machine_base(),
            log: temp_dir(tag).join(LOG_FILE),
            key,
        }
    }

    fn exists_unless_missing(_: Source, name: &str) -> bool {
        name != "missing"
    }

    fn env(fx: &Fixture) -> Env<'_> {
        Env {
            user_root: HKEY_CURRENT_USER,
            user_base: &fx.user,
            machine_root: HKEY_CURRENT_USER,
            machine_base: &fx.machine,
            log: &fx.log,
            entry_exists: &exists_unless_missing,
        }
    }

    fn user_value(fx: &Fixture, sub: &str, name: &str) -> Option<Vec<u8>> {
        read_value(HKEY_CURRENT_USER, &format!(r"{}\{}", fx.user, sub), name).unwrap()
    }

    const ALL_DETAILS: [(&str, bool, Option<&str>); 10] = [
        ("HKCU Run", true, Some("Run")),
        ("HKCU Run (32-bit)", true, Some("Run32")),
        ("HKCU RunOnce", false, None),
        ("HKCU RunOnce (32-bit)", false, None),
        ("HKLM Run", false, Some("Run")),
        ("HKLM Run (32-bit)", false, Some("Run32")),
        ("HKLM RunOnce", false, None),
        ("HKLM RunOnce (32-bit)", false, None),
        ("user Startup folder", true, Some("StartupFolder")),
        ("all-users Startup folder", false, Some("StartupFolder")),
    ];

    #[test]
    fn byte_encoding() {
        let ft = 0x01DB_1234_5678_9ABC_u64;
        let off = approved_bytes(false, ft);
        assert_eq!(off[0], 0x03);
        assert_eq!(&off[1..4], &[0, 0, 0]);
        assert_eq!(&off[4..12], &ft.to_le_bytes());
        assert_eq!(approved_bytes(true, ft), [0x02, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);

        assert!(bytes_enabled(None));
        assert!(bytes_enabled(Some(&[])));
        assert!(bytes_enabled(Some(&[0x02])));
        assert!(bytes_enabled(Some(&[0x06, 0, 0])));
        assert!(!bytes_enabled(Some(&[0x03])));
        assert!(!bytes_enabled(Some(&[0x07])));
        assert!(!bytes_enabled(Some(&off)));

        // A current FILETIME is after 2020 and decodes back to roughly now.
        let now = filetime_now();
        assert!(now > 132_223_104_000_000_000);
    }

    #[test]
    fn source_parsing_and_can_change_for_every_scan_label() {
        for (label, changeable, sub) in ALL_DETAILS {
            let full = format!(r#""C:\Program Files\X\x.exe" --tray | source: {label}"#);
            let bare_detail = format!("source: {label}");
            for d in [full.as_str(), bare_detail.as_str(), label] {
                let s = Source::parse(d).unwrap_or_else(|| panic!("unparsed: {d}"));
                assert_eq!(s.label(), label);
                assert_eq!(s.approved_subkey(), sub, "{d}");
                assert_eq!(can_change(d), changeable, "{d}");
            }
        }
        // The command line can't spoof the source: the last "source: " wins.
        assert!(!can_change("evil.exe source: HKCU Run | source: HKLM Run"));
        assert!(can_change("x.exe | source: HKLM Run | source: HKCU Run"));
        // Unknown or malformed sources are refused.
        for d in ["", "source: ", "source: HKCU", "source: hkcu run", "x.exe | source: HKCU Run (64-bit)", "HKCU Run extra"] {
            assert!(Source::parse(d).is_none(), "{d}");
            assert!(!can_change(d), "{d}");
        }
    }

    #[test]
    fn refusals_happen_before_any_access() {
        // These go through the PUBLIC function with the real environment, which is safe
        // only because each is refused before any registry or log access.
        for d in [
            "a.exe | source: HKLM Run",
            "a.exe | source: HKLM Run (32-bit)",
            "a.exe | source: all-users Startup folder",
            "a.exe | source: HKCU RunOnce",
            "a.exe | source: HKLM RunOnce (32-bit)",
            "a.exe | source: somewhere else",
            "a.exe",
        ] {
            for on in [true, false] {
                let e = set_enabled("Item", d, on).unwrap_err();
                assert!(!e.is_empty());
            }
        }
        let e = set_enabled("Item", "source: HKLM Run", false).unwrap_err();
        assert!(e.contains("administrator"), "{e}");
    }

    #[test]
    fn hklm_refusal_writes_nothing() {
        let fx = fixture("hklm");
        let e = env(&fx);
        assert!(set_enabled_in(&e, "Item", "x | source: HKLM Run", false).is_err());
        assert!(set_enabled_in(&e, "Item.lnk", "x | source: all-users Startup folder", false).is_err());
        assert!(read_value(HKEY_CURRENT_USER, &fx.key.path, "Item").unwrap().is_none());
        assert!(read_value(HKEY_CURRENT_USER, &format!(r"{}\Run", fx.machine), "Item").unwrap().is_none());
        assert!(!fx.log.exists());
    }

    #[test]
    fn bad_names_are_refused() {
        let fx = fixture("names");
        let e = env(&fx);
        assert!(set_enabled_in(&e, "", "source: HKCU Run", false).is_err());
        assert!(set_enabled_in(&e, "   ", "source: HKCU Run", false).is_err());
        assert!(set_enabled_in(&e, "a\0b", "source: HKCU Run", false).is_err());
        assert!(set_enabled_in(&e, r"..\x.lnk", "source: user Startup folder", false).is_err());
        assert!(set_enabled_in(&e, "C:x.lnk", "source: user Startup folder", false).is_err());
        assert!(set_enabled_in(&e, "missing", "source: HKCU Run", false).is_err());
        assert!(!fx.log.exists());
    }

    #[test]
    fn disable_then_enable_round_trip() {
        let fx = fixture("roundtrip");
        let e = env(&fx);
        let detail = r#""C:\x\Tool.exe" /background | source: HKCU Run"#;

        let c = set_enabled_in(&e, "Tool", detail, false).unwrap();
        assert_eq!((c.name.as_str(), c.source.as_str()), ("Tool", "HKCU Run"));
        assert!(c.was_enabled && !c.now_enabled);
        assert!(c.at.ends_with('Z') && c.at.len() == 20, "{}", c.at);

        let v = user_value(&fx, "Run", "Tool").unwrap();
        assert_eq!(v.len(), 12);
        assert_eq!(v[0], 0x03);
        assert_eq!(&v[1..4], &[0, 0, 0]);
        assert!(u64::from_le_bytes(v[4..12].try_into().unwrap()) > FILETIME_UNIX_EPOCH);
        assert_eq!(changes_in(&e), vec![c.clone()]);
        // Nothing written in the other subkeys or hive.
        assert!(user_value(&fx, "Run32", "Tool").is_none());
        assert!(read_value(HKEY_CURRENT_USER, &format!(r"{}\Run", fx.machine), "Tool").unwrap().is_none());

        let c2 = set_enabled_in(&e, "Tool", detail, true).unwrap();
        assert!(!c2.was_enabled && c2.now_enabled);
        assert_eq!(user_value(&fx, "Run", "Tool").unwrap(), approved_bytes(true, 0).to_vec());
        assert!(changes_in(&e).is_empty());
        assert!(load_log(&fx.log).unwrap().is_empty());
    }

    #[test]
    fn subkey_follows_source() {
        let fx = fixture("subkeys");
        let e = env(&fx);
        set_enabled_in(&e, "Tray32", "x | source: HKCU Run (32-bit)", false).unwrap();
        set_enabled_in(&e, "Tailscale.lnk", r"C:\t.exe | source: user Startup folder", false).unwrap();
        assert_eq!(user_value(&fx, "Run32", "Tray32").unwrap()[0], 0x03);
        assert_eq!(user_value(&fx, "StartupFolder", "Tailscale.lnk").unwrap()[0], 0x03);
        assert!(user_value(&fx, "Run", "Tray32").is_none());
        assert!(user_value(&fx, "Run", "Tailscale.lnk").is_none());
        let names: Vec<_> = changes_in(&e).into_iter().map(|c| (c.name, c.source)).collect();
        assert_eq!(
            names,
            vec![
                ("Tray32".to_string(), "HKCU Run (32-bit)".to_string()),
                ("Tailscale.lnk".to_string(), "user Startup folder".to_string())
            ]
        );
    }

    #[test]
    fn idempotent_when_already_in_state() {
        let fx = fixture("idem");
        let e = env(&fx);
        // Absent value = enabled: enabling writes nothing and logs nothing.
        let c = set_enabled_in(&e, "App", "source: HKCU Run", true).unwrap();
        assert!(c.was_enabled && c.now_enabled);
        assert!(user_value(&fx, "Run", "App").is_none());
        assert!(!fx.log.exists());

        set_enabled_in(&e, "App", "source: HKCU Run", false).unwrap();
        let before = user_value(&fx, "Run", "App").unwrap();
        let c = set_enabled_in(&e, "App", "source: HKCU Run", false).unwrap();
        assert!(!c.was_enabled && !c.now_enabled);
        assert_eq!(user_value(&fx, "Run", "App").unwrap(), before, "no second write");
        assert_eq!(load_log(&fx.log).unwrap().len(), 1);

        // Disabled by Task Manager (not by us): disabling again is a no-op, no log entry.
        write_verified(HKEY_CURRENT_USER, &format!(r"{}\Run", fx.user), "Other", &approved_bytes(false, 1), None).unwrap();
        set_enabled_in(&e, "Other", "source: HKCU Run", false).unwrap();
        assert_eq!(load_log(&fx.log).unwrap().len(), 1);
    }

    #[test]
    fn all_users_disable_blocks_enable() {
        let fx = fixture("machine");
        let e = env(&fx);
        write_verified(HKEY_CURRENT_USER, &format!(r"{}\Run", fx.machine), "App", &approved_bytes(false, 1), None).unwrap();
        // Effectively disabled already: disabling is a no-op.
        let c = set_enabled_in(&e, "App", "source: HKCU Run", false).unwrap();
        assert!(!c.was_enabled && !c.now_enabled);
        assert!(user_value(&fx, "Run", "App").is_none());
        // Enabling can't succeed from the user hive alone.
        let err = set_enabled_in(&e, "App", "source: HKCU Run", true).unwrap_err();
        assert!(err.contains("administrator"), "{err}");
        assert!(user_value(&fx, "Run", "App").is_none());
    }

    #[test]
    fn changes_drops_items_reenabled_elsewhere() {
        let fx = fixture("external");
        let e = env(&fx);
        set_enabled_in(&e, "A", "source: HKCU Run", false).unwrap();
        set_enabled_in(&e, "B", "source: HKCU Run", false).unwrap();
        // Re-enabled outside HoldMyRAM (e.g. Task Manager).
        write_verified(HKEY_CURRENT_USER, &format!(r"{}\Run", fx.user), "A", &approved_bytes(true, 0), None).unwrap();
        let left: Vec<_> = changes_in(&e).into_iter().map(|c| c.name).collect();
        assert_eq!(left, vec!["B".to_string()]);
    }

    #[test]
    fn log_round_trip_and_atomic_write() {
        let path = temp_dir("startuplog").join(LOG_FILE);
        assert!(load_log(&path).unwrap().is_empty());
        let c = StartupChange {
            name: "X".into(),
            source: "HKCU Run".into(),
            was_enabled: true,
            now_enabled: false,
            at: "2026-11-04T10:00:00Z".into(),
        };
        save_log(&path, std::slice::from_ref(&c)).unwrap();
        assert_eq!(load_log(&path).unwrap(), vec![c.clone()]);
        assert!(!with_suffix(&path, ".tmp").exists());
        // A stale temp file from a crash must not break the next save.
        fs::write(with_suffix(&path, ".tmp"), "garbage").unwrap();
        save_log(&path, &[c.clone(), c.clone()]).unwrap();
        assert_eq!(load_log(&path).unwrap().len(), 2);
        assert!(!with_suffix(&path, ".tmp").exists());
    }

    #[test]
    fn corrupt_log_is_kept_as_bad() {
        let path = temp_dir("startupbad").join(LOG_FILE);
        fs::write(&path, "{ not json").unwrap();
        assert!(load_log(&path).unwrap().is_empty());
        assert!(!path.exists());
        assert_eq!(fs::read_to_string(with_suffix(&path, ".bad")).unwrap(), "{ not json");
        // Non-UTF-8 counts as corrupt too.
        fs::write(&path, [0xFF, 0xFE, 0x00, 0xC3]).unwrap();
        assert!(load_log(&path).unwrap().is_empty());
        assert!(!path.exists());

        // And the whole flow keeps working on top of a corrupt log.
        let mut fx = fixture("badflow");
        fx.log = path.clone();
        fs::write(&path, "[[[").unwrap();
        let e = env(&fx);
        assert!(changes_in(&e).is_empty());
        set_enabled_in(&e, "Y", "source: HKCU Run", false).unwrap();
        assert_eq!(changes_in(&e).len(), 1);
    }

    #[test]
    fn timestamps() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(rfc3339(1_793_786_399), "2026-11-04T09:59:59Z");
    }
}
