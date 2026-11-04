//! Start HoldMyRAM with Windows: one value named `HoldMyRAM` under
//! `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` holding the quoted full path of the
//! current exe. Current user only, no admin. Default OFF: nothing here runs unless the user
//! turns it on.
//!
//! Rules:
//! - Only our own value name is ever written or deleted, in the Run key and in
//!   `...\Explorer\StartupApproved\Run` (Task Manager's on/off switch, see startup.rs).
//!   Other Run entries are never touched.
//! - Enabling writes the current exe path (so it also "fixes" a value left by a portable
//!   copy that was moved) and clears a Task-Manager "disabled" flag on our value, because
//!   the user just asked for it to be on. Every write is read back.
//! - Disabling removes our Run value and our StartupApproved value. Reversible: enabling
//!   again recreates them.
//! - Key paths are injectable (`Env`) so tests use a throwaway key, never the real Run key.

use crate::startup::{approved_bytes, bytes_enabled, read_value, wide, write_verified};
use serde::Serialize;
use std::path::{Path, PathBuf};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegOpenKeyExW, RegSetValueExW, HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE,
    KEY_SET_VALUE, KEY_WOW64_64KEY, REG_OPTION_NON_VOLATILE, REG_SZ,
};

pub const VALUE_NAME: &str = "HoldMyRAM";
const RUN: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const APPROVED_RUN: &str = r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AutostartStatus {
    /// Our Run value exists.
    pub enabled: bool,
    /// The value points at the exe that is running now. False when disabled.
    pub path_matches: bool,
    /// Task Manager (StartupApproved) has our entry switched off.
    pub disabled_in_task_manager: bool,
    /// The exe path the Run value names (unquoted), if any.
    pub path: Option<String>,
    /// The exe that is running now (what enabling would write).
    pub current_exe: String,
    /// It will actually start at the next sign-in: enabled, matching and not disabled.
    pub effective: bool,
}

/// Where to read and write. `real()` is the only constructor outside tests.
pub struct Env {
    pub root: HKEY,
    pub run_path: String,
    pub approved_path: String,
    pub exe: PathBuf,
}

impl Env {
    pub fn real() -> Result<Env, String> {
        let exe = std::env::current_exe().map_err(|e| format!("could not find HoldMyRAM's own path: {e}"))?;
        Ok(Env { root: HKEY_CURRENT_USER, run_path: RUN.into(), approved_path: APPROVED_RUN.into(), exe })
    }
}

pub fn status() -> Result<AutostartStatus, String> {
    status_in(&Env::real()?)
}

pub fn set(enabled: bool) -> Result<AutostartStatus, String> {
    set_in(&Env::real()?, enabled)
}

/// REG_SZ bytes -> string (UTF-16LE, NUL-terminated or not).
fn decode_sz(data: &[u8]) -> String {
    let units: Vec<u16> = data.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
    let end = units.iter().position(|&u| u == 0).unwrap_or(units.len());
    String::from_utf16_lossy(&units[..end])
}

/// The program part of a Run command line: the quoted part, or everything when unquoted.
pub fn command_exe(cmd: &str) -> String {
    let t = cmd.trim();
    match t.strip_prefix('"') {
        Some(rest) => rest.split('"').next().unwrap_or("").to_string(),
        None => t.to_string(),
    }
}

fn normalize(p: &str) -> String {
    p.trim().replace('/', "\\").trim_start_matches(r"\\?\").to_lowercase()
}

pub fn same_path(a: &str, b: &Path) -> bool {
    let bs = b.to_string_lossy();
    if normalize(a) == normalize(&bs) {
        return true;
    }
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(x), Ok(y)) => normalize(&x.to_string_lossy()) == normalize(&y.to_string_lossy()),
        _ => false,
    }
}

pub fn status_in(env: &Env) -> Result<AutostartStatus, String> {
    let value = read_value(env.root, &env.run_path, VALUE_NAME)?.map(|d| decode_sz(&d));
    let approved = read_value(env.root, &env.approved_path, VALUE_NAME)?;
    let path = value.as_deref().map(command_exe);
    let enabled = value.is_some();
    let path_matches = path.as_deref().is_some_and(|p| same_path(p, &env.exe));
    let disabled_in_task_manager = !bytes_enabled(approved.as_deref());
    Ok(AutostartStatus {
        enabled,
        path_matches,
        disabled_in_task_manager,
        path,
        current_exe: env.exe.display().to_string(),
        effective: enabled && path_matches && !disabled_in_task_manager,
    })
}

struct Key(HKEY);
impl Drop for Key {
    fn drop(&mut self) {
        unsafe {
            let _ = RegCloseKey(self.0);
        }
    }
}

fn write_sz(root: HKEY, path: &str, name: &str, value: &str) -> Result<(), String> {
    let p = wide(path);
    let mut h = HKEY::default();
    let r = unsafe {
        RegCreateKeyExW(root, PCWSTR(p.as_ptr()), Some(0), PCWSTR::null(), REG_OPTION_NON_VOLATILE, KEY_QUERY_VALUE | KEY_SET_VALUE | KEY_WOW64_64KEY, None, &mut h, None)
    };
    if r != ERROR_SUCCESS {
        return Err(format!("opening the Run key failed (Windows error {})", r.0));
    }
    let key = Key(h);
    let n = wide(name);
    let data: Vec<u8> = wide(value).iter().flat_map(|u| u.to_le_bytes()).collect();
    let r = unsafe { RegSetValueExW(key.0, PCWSTR(n.as_ptr()), Some(0), REG_SZ, Some(&data)) };
    if r != ERROR_SUCCESS {
        return Err(format!("writing the Run value failed (Windows error {})", r.0));
    }
    Ok(())
}

/// Delete one value; a missing key or value is fine.
fn delete_value(root: HKEY, path: &str, name: &str) -> Result<(), String> {
    let p = wide(path);
    let mut h = HKEY::default();
    let r = unsafe { RegOpenKeyExW(root, PCWSTR(p.as_ptr()), Some(0), KEY_SET_VALUE | KEY_WOW64_64KEY, &mut h) };
    if r == ERROR_FILE_NOT_FOUND {
        return Ok(());
    }
    if r != ERROR_SUCCESS {
        return Err(format!("opening {path} failed (Windows error {})", r.0));
    }
    let key = Key(h);
    let n = wide(name);
    let r = unsafe { RegDeleteValueW(key.0, PCWSTR(n.as_ptr())) };
    if r != ERROR_SUCCESS && r != ERROR_FILE_NOT_FOUND {
        return Err(format!("removing the {name} value failed (Windows error {})", r.0));
    }
    Ok(())
}

pub fn set_in(env: &Env, enabled: bool) -> Result<AutostartStatus, String> {
    if enabled {
        let exe = env.exe.to_string_lossy().to_string();
        if exe.contains('"') || exe.trim().is_empty() {
            return Err(format!("unexpected exe path {exe:?}"));
        }
        let cmd = format!("\"{exe}\"");
        write_sz(env.root, &env.run_path, VALUE_NAME, &cmd)?;
        let back = read_value(env.root, &env.run_path, VALUE_NAME)?.map(|d| decode_sz(&d));
        if back.as_deref() != Some(cmd.as_str()) {
            let _ = delete_value(env.root, &env.run_path, VALUE_NAME);
            return Err("the Run value did not read back as written; it was removed".into());
        }
        // Clear a Task Manager "disabled" flag on OUR value only.
        let prev = read_value(env.root, &env.approved_path, VALUE_NAME)?;
        if !bytes_enabled(prev.as_deref()) {
            write_verified(env.root, &env.approved_path, VALUE_NAME, &approved_bytes(true, 0), prev.as_deref())?;
        }
    } else {
        delete_value(env.root, &env.run_path, VALUE_NAME)?;
        delete_value(env.root, &env.approved_path, VALUE_NAME)?;
        if read_value(env.root, &env.run_path, VALUE_NAME)?.is_some() {
            return Err("the Run value is still there after removing it".into());
        }
    }
    status_in(env)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::startup::filetime_now;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    use windows::Win32::System::Registry::{RegDeleteKeyW, RegDeleteTreeW};

    const TEST_PARENT: &str = r"Software\HoldMyRAMTest";

    /// Throwaway HKCU key, deleted on drop. Never the real Run key.
    struct TestKey(String);
    impl TestKey {
        fn new() -> TestKey {
            static N: AtomicU32 = AtomicU32::new(0);
            let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().subsec_nanos();
            TestKey(format!(r"{TEST_PARENT}\autostart-{}-{nanos}-{}", std::process::id(), N.fetch_add(1, Ordering::SeqCst)))
        }
        fn env(&self, exe: &str) -> Env {
            Env {
                root: HKEY_CURRENT_USER,
                run_path: format!(r"{}\Run", self.0),
                approved_path: format!(r"{}\StartupApproved\Run", self.0),
                exe: PathBuf::from(exe),
            }
        }
    }
    impl Drop for TestKey {
        fn drop(&mut self) {
            let prefix = format!(r"{TEST_PARENT}\");
            assert!(self.0.starts_with(&prefix) && self.0.len() > prefix.len());
            let p = wide(&self.0);
            let parent = wide(TEST_PARENT);
            unsafe {
                let _ = RegDeleteTreeW(HKEY_CURRENT_USER, PCWSTR(p.as_ptr()));
                let _ = RegDeleteKeyW(HKEY_CURRENT_USER, PCWSTR(p.as_ptr()));
                let _ = RegDeleteKeyW(HKEY_CURRENT_USER, PCWSTR(parent.as_ptr()));
            }
        }
    }

    const EXE: &str = r"C:\Tools\HoldMyRAM\holdmyram.exe";

    #[test]
    fn real_env_points_at_the_real_run_key() {
        // Sanity of constants only; nothing is read or written.
        assert_eq!(RUN, r"Software\Microsoft\Windows\CurrentVersion\Run");
        assert!(APPROVED_RUN.ends_with(r"StartupApproved\Run"));
    }

    #[test]
    fn default_off_enable_disable_round_trip() {
        let k = TestKey::new();
        let env = k.env(EXE);
        let s = status_in(&env).unwrap();
        assert!(!s.enabled && !s.path_matches && !s.disabled_in_task_manager && !s.effective && s.path.is_none());

        let s = set_in(&env, true).unwrap();
        assert!(s.enabled && s.path_matches && s.effective, "{s:?}");
        assert_eq!(s.path.as_deref(), Some(EXE));
        let raw = read_value(env.root, &env.run_path, VALUE_NAME).unwrap().unwrap();
        assert_eq!(decode_sz(&raw), format!("\"{EXE}\""));
        // Idempotent.
        assert_eq!(set_in(&env, true).unwrap(), s);

        let s = set_in(&env, false).unwrap();
        assert!(!s.enabled && !s.effective);
        assert!(read_value(env.root, &env.run_path, VALUE_NAME).unwrap().is_none());
        // Disabling twice is fine.
        assert!(!set_in(&env, false).unwrap().enabled);
    }

    #[test]
    fn other_run_entries_are_never_touched() {
        let k = TestKey::new();
        let env = k.env(EXE);
        write_sz(env.root, &env.run_path, "OtherApp", r#""C:\Other\other.exe" --min"#).unwrap();
        let disabled_other = approved_bytes(false, filetime_now());
        write_verified(env.root, &env.approved_path, "OtherApp", &disabled_other, None).unwrap();
        set_in(&env, true).unwrap();
        set_in(&env, false).unwrap();
        let v = read_value(env.root, &env.run_path, "OtherApp").unwrap().unwrap();
        assert_eq!(decode_sz(&v), r#""C:\Other\other.exe" --min"#);
        assert_eq!(read_value(env.root, &env.approved_path, "OtherApp").unwrap().unwrap(), disabled_other.to_vec());
    }

    #[test]
    fn moved_exe_reports_mismatch_and_enable_fixes_it() {
        let k = TestKey::new();
        let old = k.env(r"D:\Old place\holdmyram.exe");
        set_in(&old, true).unwrap();
        let env = k.env(EXE);
        let s = status_in(&env).unwrap();
        assert!(s.enabled && !s.path_matches && !s.effective);
        assert_eq!(s.path.as_deref(), Some(r"D:\Old place\holdmyram.exe"));
        let s = set_in(&env, true).unwrap();
        assert!(s.path_matches && s.effective);
        // Case and slash differences still match.
        assert!(same_path(r"c:/tools/holdmyram/HOLDMYRAM.EXE", Path::new(EXE)));
    }

    #[test]
    fn task_manager_disable_is_reported_and_cleared_on_enable() {
        let k = TestKey::new();
        let env = k.env(EXE);
        set_in(&env, true).unwrap();
        write_verified(env.root, &env.approved_path, VALUE_NAME, &approved_bytes(false, filetime_now()), None).unwrap();
        let s = status_in(&env).unwrap();
        assert!(s.enabled && s.disabled_in_task_manager && !s.effective);
        let s = set_in(&env, true).unwrap();
        assert!(!s.disabled_in_task_manager && s.effective);
        assert_eq!(read_value(env.root, &env.approved_path, VALUE_NAME).unwrap().unwrap()[0], 0x02);
        // Disabling removes our StartupApproved value too.
        set_in(&env, false).unwrap();
        assert!(read_value(env.root, &env.approved_path, VALUE_NAME).unwrap().is_none());
    }

    #[test]
    fn command_line_parsing() {
        assert_eq!(command_exe(r#""C:\A B\x.exe" --flag"#), r"C:\A B\x.exe");
        assert_eq!(command_exe(r"C:\x.exe"), r"C:\x.exe");
        assert_eq!(command_exe(r#"  "C:\x.exe""#), r"C:\x.exe");
    }
}
