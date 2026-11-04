//! Risk database: how safe it is to stop a given process, service, startup item or task.
//!
//! A curated database ships embedded in the binary (`data/risk.toml`). The user can
//! override or extend it with `%APPDATA%\HoldMyRAM\risk-user.toml`, which wins on conflict.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

static BUILTIN: &str = include_str!("../../../data/risk.toml");

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Risk {
    /// Usually safe to stop; little or no lasting effect.
    Suggested,
    /// Stopping has a visible side effect; read the explanation first.
    Caution,
    /// Can break the system or hardware features. Requires confirmation.
    Dangerous,
    /// Not in the database.
    #[default]
    Unrated,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Process,
    Service,
    Startup,
    Task,
    App,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RiskEntry {
    pub name: String,
    pub kind: Kind,
    #[serde(default)]
    pub risk: Risk,
    /// One or two sentences shown to the user.
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub notes: Option<String>,
}

#[derive(Deserialize)]
struct File {
    #[serde(default)]
    entry: Vec<RiskEntry>,
}

/// Parse a risk TOML document (`[[entry]]` tables).
pub fn parse_entries(text: &str) -> Result<Vec<RiskEntry>, String> {
    toml::from_str::<File>(text)
        .map(|f| f.entry)
        .map_err(|e| e.to_string())
}

/// Path of the user override file.
pub fn user_path() -> PathBuf {
    crate::config::app_dir().join("risk-user.toml")
}

#[derive(Debug, Clone, Default)]
pub struct RiskDb {
    map: HashMap<(String, Kind), RiskEntry>,
}

impl RiskDb {
    /// Only the embedded database. If it somehow fails to parse (a test guards
    /// against that) the result is empty, so everything reads as Unrated.
    pub fn builtin() -> RiskDb {
        let mut db = RiskDb::default();
        if let Ok(entries) = parse_entries(BUILTIN) {
            db.extend(entries);
        }
        db
    }

    /// Embedded database plus the user file at the default path. Returns a
    /// warning if the user file exists but cannot be used.
    pub fn load() -> (RiskDb, Option<String>) {
        Self::load_with_user(&user_path())
    }

    /// Embedded database plus the user file at `user_file` (missing file is fine).
    pub fn load_with_user(user_file: &Path) -> (RiskDb, Option<String>) {
        let mut db = Self::builtin();
        let warning = match std::fs::read_to_string(user_file) {
            Ok(text) => match parse_entries(&text) {
                Ok(entries) => {
                    db.extend(entries);
                    None
                }
                Err(e) => Some(format!(
                    "ignoring {}: {e}",
                    user_file.display()
                )),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => Some(format!("could not read {}: {e}", user_file.display())),
        };
        (db, warning)
    }

    /// Add entries; later entries replace earlier ones with the same name+kind.
    pub fn extend(&mut self, entries: impl IntoIterator<Item = RiskEntry>) {
        for e in entries {
            self.map.insert((e.name.to_lowercase(), e.kind), e);
        }
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn entries(&self) -> impl Iterator<Item = &RiskEntry> {
        self.map.values()
    }

    /// Case-insensitive lookup by name and kind.
    pub fn get(&self, name: &str, kind: Kind) -> Option<&RiskEntry> {
        self.map.get(&(name.trim().to_lowercase(), kind))
    }

    /// Like `get`, but unknown names come back as an Unrated entry.
    pub fn lookup(&self, name: &str, kind: Kind) -> RiskEntry {
        self.get(name, kind).cloned().unwrap_or_else(|| RiskEntry {
            name: name.to_string(),
            kind,
            risk: Risk::Unrated,
            summary: "Not in the risk database. Check what it does before stopping it.".into(),
            notes: None,
        })
    }

    pub fn risk(&self, name: &str, kind: Kind) -> Risk {
        self.get(name, kind).map(|e| e.risk).unwrap_or(Risk::Unrated)
    }

    /// Service lookup that also matches per-user instances: Windows appends a random
    /// hex suffix (`CDPUserSvc_8155c` -> `CDPUserSvc`). An exact entry wins over the
    /// stripped one. Unknown names come back as Unrated, keeping the original name.
    pub fn lookup_service(&self, name: &str) -> RiskEntry {
        if let Some(e) = self.get(name, Kind::Service) {
            return e.clone();
        }
        if let Some(base) = strip_instance_suffix(name) {
            if let Some(e) = self.get(base, Kind::Service) {
                return e.clone();
            }
        }
        self.lookup(name, Kind::Service)
    }

    /// Lookup that applies the per-kind matching rules (service suffix stripping).
    pub fn lookup_for(&self, name: &str, kind: Kind) -> RiskEntry {
        if kind == Kind::Service {
            self.lookup_service(name)
        } else {
            self.lookup(name, kind)
        }
    }
}

/// `Name_8155c` -> `Name` when the part after the last underscore is 4 to 8 hex digits.
fn strip_instance_suffix(name: &str) -> Option<&str> {
    let name = name.trim();
    let (base, suffix) = name.rsplit_once('_')?;
    let ok = (4..=8).contains(&suffix.len()) && suffix.chars().all(|c| c.is_ascii_hexdigit());
    if ok && !base.is_empty() {
        Some(base)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static N: AtomicU32 = AtomicU32::new(0);

    fn entry(name: &str, kind: Kind, risk: Risk) -> RiskEntry {
        RiskEntry { name: name.into(), kind, risk, summary: "s".into(), notes: None }
    }

    #[test]
    fn builtin_parses_and_is_sane() {
        let entries = parse_entries(BUILTIN).expect("data/risk.toml must parse");
        assert!(entries.len() > 50);
        let mut seen = std::collections::HashSet::new();
        for e in &entries {
            assert!(!e.name.trim().is_empty());
            assert!(!e.summary.trim().is_empty(), "{} lacks a summary", e.name);
            assert!(
                seen.insert((e.name.to_lowercase(), e.kind)),
                "duplicate entry {} ({:?})",
                e.name,
                e.kind
            );
            if e.risk == Risk::Unrated {
                assert!(e.notes.is_some(), "{} is Unrated and needs a note", e.name);
            }
        }
        assert_eq!(RiskDb::builtin().len(), entries.len());
    }

    #[test]
    fn critical_items_are_dangerous() {
        let db = RiskDb::builtin();
        for p in ["svchost.exe", "explorer.exe", "dwm.exe", "csrss.exe", "lsass.exe", "winlogon.exe", "MsMpEng.exe", "GHelper.exe"] {
            assert_eq!(db.risk(p, Kind::Process), Risk::Dangerous, "{p}");
        }
        for s in ["WinDefend", "NvContainerLocalSystem", "AsusPTPService", "AsusOptimization"] {
            assert_eq!(db.risk(s, Kind::Service), Risk::Dangerous, "{s}");
        }
        assert_eq!(db.risk("SysMain", Kind::Service), Risk::Suggested);
    }

    #[test]
    fn lookup_is_case_insensitive_and_kind_specific() {
        let db = RiskDb::builtin();
        assert_eq!(db.risk("LSASS.EXE", Kind::Process), Risk::Dangerous);
        assert_eq!(db.risk("asusoptimization", Kind::Service), Risk::Dangerous);
        assert_eq!(db.risk("ASUSOptimization", Kind::Service), Risk::Dangerous);
        // Same name under a different kind is not a match.
        assert_eq!(db.risk("lsass.exe", Kind::Service), Risk::Unrated);
    }

    #[test]
    fn service_instance_suffix_matches() {
        let db = RiskDb::builtin();
        let base = db.get("CDPUserSvc", Kind::Service).expect("CDPUserSvc in db").clone();
        let e = db.lookup_service("CDPUserSvc_8155c");
        assert_eq!(e.risk, base.risk);
        assert_eq!(e.summary, base.summary);
        assert_eq!(db.lookup_service("cdpusersvc_8155C").risk, base.risk);
        assert_eq!(db.lookup_for("CDPUserSvc_8155c", Kind::Service).risk, base.risk);
        // Exact names still work, and bad suffixes do not match.
        assert_eq!(db.lookup_service("CDPUserSvc").risk, base.risk);
        assert_eq!(db.lookup_service("CDPUserSvc_zzzzz").risk, Risk::Unrated);
        assert_eq!(db.lookup_service("CDPUserSvc_ab").risk, Risk::Unrated);
        // Suffix stripping is for services only.
        assert_eq!(db.lookup_for("CDPUserSvc_8155c", Kind::Process).risk, Risk::Unrated);
        // Unknown stays unrated with the original name.
        let u = db.lookup_service("Nope_12345");
        assert_eq!(u.risk, Risk::Unrated);
        assert_eq!(u.name, "Nope_12345");
    }

    #[test]
    fn exact_service_entry_beats_stripped_one() {
        let mut db = RiskDb::default();
        db.extend([
            entry("Foo", Kind::Service, Risk::Suggested),
            entry("Foo_1234", Kind::Service, Risk::Dangerous),
        ]);
        assert_eq!(db.lookup_service("Foo_1234").risk, Risk::Dangerous);
        assert_eq!(db.lookup_service("Foo_9999").risk, Risk::Suggested);
    }

    #[test]
    fn unknown_is_unrated() {
        let db = RiskDb::builtin();
        assert_eq!(db.risk("totally-unknown.exe", Kind::Process), Risk::Unrated);
        let e = db.lookup("totally-unknown.exe", Kind::Process);
        assert_eq!(e.risk, Risk::Unrated);
        assert_eq!(e.name, "totally-unknown.exe");
        assert!(db.get("totally-unknown.exe", Kind::Process).is_none());
    }

    #[test]
    fn user_entries_win_on_conflict() {
        let mut db = RiskDb::builtin();
        assert_eq!(db.risk("Spotify.exe", Kind::Process), Risk::Suggested);
        db.extend([entry("spotify.EXE", Kind::Process, Risk::Dangerous), entry("mine.exe", Kind::Process, Risk::Caution)]);
        assert_eq!(db.risk("Spotify.exe", Kind::Process), Risk::Dangerous);
        assert_eq!(db.risk("mine.exe", Kind::Process), Risk::Caution);
    }

    #[test]
    fn user_file_loading() {
        let dir = std::env::temp_dir().join(format!(
            "holdmyram-risk-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&dir).unwrap();

        // Missing: no warning.
        let (db, w) = RiskDb::load_with_user(&dir.join("none.toml"));
        assert!(w.is_none());
        assert_eq!(db.len(), RiskDb::builtin().len());

        // Valid override.
        let f = dir.join("risk-user.toml");
        std::fs::write(
            &f,
            "[[entry]]\nname = \"Spotify.exe\"\nkind = \"process\"\nrisk = \"caution\"\nsummary = \"I use it for timers.\"\n",
        )
        .unwrap();
        let (db, w) = RiskDb::load_with_user(&f);
        assert!(w.is_none());
        assert_eq!(db.risk("spotify.exe", Kind::Process), Risk::Caution);
        assert_eq!(db.lookup("spotify.exe", Kind::Process).notes, None);

        // Corrupt: warning, builtin still intact.
        std::fs::write(&f, "[[entry]] garbage = =").unwrap();
        let (db, w) = RiskDb::load_with_user(&f);
        assert!(w.is_some());
        assert_eq!(db.risk("spotify.exe", Kind::Process), Risk::Suggested);
    }
}
