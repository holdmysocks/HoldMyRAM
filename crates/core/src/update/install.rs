//! Installing a staged update by swapping files, without a helper script.
//!
//! A running exe can be renamed but not overwritten, so: rename each current exe to
//! `<name>.old`, copy the staged file in under a temporary name, verify size and SHA-256,
//! then rename it into place. Any failure rolls back (new files removed, `.old` renamed
//! back). The `.old` files stay until the NEW process has started and the old one has
//! exited; the new process deletes them (`cleanup_old`). If starting the new exe fails,
//! the swap is rolled back too.

use super::{sha256_hex, Staged, UpdateError, HELPER_EXE, MAIN_EXE};
use std::fs;
use std::path::{Path, PathBuf};

/// Files that are swapped (both must be in the staged update).
pub const SWAP: &[&str] = &[MAIN_EXE, HELPER_EXE];

/// A PE32+ image for x64: "MZ", PE signature, machine AMD64, optional header magic 0x20B.
pub fn check_pe_x64(b: &[u8]) -> Result<(), String> {
    if b.len() < 0x40 || &b[..2] != b"MZ" {
        return Err("not a Windows executable (no MZ header)".into());
    }
    let pe = u32::from_le_bytes([b[0x3c], b[0x3d], b[0x3e], b[0x3f]]) as usize;
    let hdr = b.get(pe..pe + 26).ok_or("truncated PE header")?;
    if &hdr[..4] != b"PE\0\0" {
        return Err("not a Windows executable (no PE signature)".into());
    }
    if u16::from_le_bytes([hdr[4], hdr[5]]) != 0x8664 {
        return Err("not an x64 executable".into());
    }
    if u16::from_le_bytes([hdr[24], hdr[25]]) != 0x20B {
        return Err("not a PE32+ (64-bit) image".into());
    }
    Ok(())
}

/// Can we create, rename and delete files in `dir`? (Program Files without admin: no.)
pub fn dir_writable(dir: &Path) -> bool {
    let probe = dir.join(format!(".holdmyram-write-test-{}", std::process::id()));
    let probe2 = probe.with_extension("renamed");
    let ok = fs::write(&probe, b"x").is_ok() && fs::rename(&probe, &probe2).is_ok();
    let _ = fs::remove_file(&probe);
    let ok = ok && fs::remove_file(&probe2).is_ok();
    let _ = fs::remove_file(&probe2);
    ok
}

fn with_suffix(p: &Path, s: &str) -> PathBuf {
    let mut n = p.as_os_str().to_owned();
    n.push(s);
    PathBuf::from(n)
}

/// What a completed swap did, for a rollback after a failed launch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Swapped {
    pub app_dir: PathBuf,
    /// (installed path, its `.old` backup if one existed)
    pub files: Vec<(PathBuf, Option<PathBuf>)>,
}

/// Put back what `swap` replaced. Best effort for each file; returns the problems.
pub fn rollback(s: &Swapped) -> Vec<String> {
    let mut errs = Vec::new();
    for (path, old) in s.files.iter().rev() {
        let _ = fs::remove_file(with_suffix(path, ".new"));
        match old {
            Some(old) => {
                if path.exists() {
                    if let Err(e) = fs::remove_file(path) {
                        errs.push(format!("{}: {e}", path.display()));
                        continue;
                    }
                }
                if let Err(e) = fs::rename(old, path) {
                    errs.push(format!("{} -> {}: {e}", old.display(), path.display()));
                }
            }
            None => {
                let _ = fs::remove_file(path);
            }
        }
    }
    errs
}

/// Install the staged exes into `app_dir`. On any failure everything is rolled back and
/// the error says so ("swap_failed").
pub fn swap(app_dir: &Path, staged: &Staged) -> Result<Swapped, UpdateError> {
    super::verify_staged(staged)?;
    let mut done = Swapped { app_dir: app_dir.to_path_buf(), files: Vec::new() };
    let fail = |done: &Swapped, what: String| {
        let errs = rollback(done);
        let tail = if errs.is_empty() { "; everything was put back".to_string() } else { format!("; ROLLBACK INCOMPLETE: {}", errs.join("; ")) };
        Err(UpdateError::new("swap_failed", format!("{what}{tail}")))
    };
    for name in SWAP {
        let f = staged.files.iter().find(|f| f.name == *name).expect("verified above");
        let target = app_dir.join(name);
        let old = with_suffix(&target, ".old");
        let tmp = with_suffix(&target, ".new");
        // A leftover .old from an earlier update (it is no longer running).
        if old.exists() {
            if let Err(e) = fs::remove_file(&old) {
                return fail(&done, format!("could not remove leftover {}: {e}", old.display()));
            }
        }
        let backup = if target.exists() {
            if let Err(e) = fs::rename(&target, &old) {
                return fail(&done, format!("could not rename {} to .old: {e}", target.display()));
            }
            Some(old)
        } else {
            None
        };
        done.files.push((target.clone(), backup));
        if let Err(e) = fs::copy(staged.dir.join(name), &tmp) {
            return fail(&done, format!("could not copy the new {name}: {e}"));
        }
        let ok = fs::read(&tmp).ok().is_some_and(|d| d.len() as u64 == f.size && sha256_hex(&d).ok().as_deref() == Some(f.sha256.as_str()));
        if !ok {
            return fail(&done, format!("the copied {name} does not match the verified file"));
        }
        if let Err(e) = fs::rename(&tmp, &target) {
            return fail(&done, format!("could not put the new {name} in place: {e}"));
        }
    }
    // Text files (README etc.): not locked, not critical; copied best effort.
    for f in staged.files.iter().filter(|f| !SWAP.contains(&f.name.as_str())) {
        let _ = fs::copy(staged.dir.join(&f.name), app_dir.join(&f.name));
    }
    Ok(done)
}

/// Delete `*.old` leftovers of a previous update in `app_dir` (best effort). Only the
/// names this updater creates.
pub fn cleanup_old(app_dir: &Path) -> Vec<PathBuf> {
    let mut removed = Vec::new();
    for name in SWAP {
        for suffix in [".old", ".new"] {
            let p = with_suffix(&app_dir.join(name), suffix);
            if p.exists() && fs::remove_file(&p).is_ok() {
                removed.push(p);
            }
        }
    }
    removed
}

/// Restoring before an install.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestoreOutcome {
    /// The toggle was off.
    NotNeeded,
    Restored,
    /// Services/tasks need admin approval that was declined or is unavailable.
    NeedsAdmin,
    /// Some items could not be restored.
    Failed(String),
}

/// The app side of an install (tests use a fake).
pub trait InstallHost {
    fn restore(&mut self) -> RestoreOutcome;
    /// Shut the admin helper down and wait for it to exit.
    fn stop_helper(&mut self);
    /// Start `exe` (the new version) with `--updated-from <old>`.
    fn launch(&mut self, exe: &Path, old_version: &str) -> Result<(), String>;
}

/// The whole install. Ok means the new exe was started; the caller exits now.
pub fn install(host: &mut dyn InstallHost, app_dir: &Path, staged: &Staged, current_version: &str) -> Result<Swapped, UpdateError> {
    let v = super::Version::parse(&staged.version).ok_or_else(|| UpdateError::new("bad_archive", "staged version unreadable"))?;
    let cur = super::Version::parse(current_version).ok_or_else(|| UpdateError::new("internal", "running version unreadable"))?;
    if v <= cur {
        return Err(UpdateError::new("no_update", format!("{v} is not newer than {cur}")));
    }
    if !dir_writable(app_dir) {
        return Err(UpdateError::new("not_writable", format!("{} is not writable without administrator rights", app_dir.display())));
    }
    super::verify_staged(staged)?;
    match host.restore() {
        RestoreOutcome::NotNeeded | RestoreOutcome::Restored => {}
        RestoreOutcome::NeedsAdmin => {
            return Err(UpdateError::new(
                "restore_needed_admin",
                "turn the toggle off first: restoring services/tasks needs admin approval, which was declined. Nothing was changed.",
            ))
        }
        RestoreOutcome::Failed(m) => return Err(UpdateError::new("restore_failed", format!("could not restore everything ({m}); nothing was installed"))),
    }
    host.stop_helper();
    let swapped = swap(app_dir, staged)?;
    if let Err(e) = host.launch(&app_dir.join(MAIN_EXE), current_version) {
        let errs = rollback(&swapped);
        let tail = if errs.is_empty() { "; the previous version was put back".to_string() } else { format!("; ROLLBACK INCOMPLETE: {}", errs.join("; ")) };
        return Err(UpdateError::new("launch_failed", format!("could not start the new version: {e}{tail}")));
    }
    Ok(swapped)
}
